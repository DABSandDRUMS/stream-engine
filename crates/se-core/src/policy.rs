//! Policy layer and moderation (§12.1): every chat-, bits-, or points-originated action runs
//! role gate → cost → cooldowns → content filter → approval → execute → audit.
//!
//! Two layers live here:
//! * pure, reusable pieces for adapters that gate their own actions (`se-bot`, song requests):
//!   [`role_allows`], [`Cooldowns`], [`FilterCfg`] + [`filter_text`];
//! * the [`Policy`] state machine the core runs for every event before rules see it: text
//!   sanitizing, the blocklist, AutoMod holds, the veto window, managed channel point rewards
//!   (`rewards/*.toml`) with refunds, the approval queue, deletion sync, and ad breaks.
//!
//! [`Policy`] never touches the state tree: it returns [`Effect`]s that the core applies in
//! order, which keeps the pipeline deterministic (replayable) and testable on its own. Every
//! decision is announced as a `policy.*` event (`accepted`, `rejected`, `filtered`, `pending`,
//! `approved`), which the engine writes to the audit log.

use crate::config::{Config, Dur};
use se_proto::{Actor, Event, Op, Origin, Role, Ts, Value};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// State: the held items (`[{id, kind, type, user, user_id, text, reward, amount, message_id,
/// created_ns, expires_ns}]`, master-clock ns) and their count.
pub const PENDING: &str = "policy.pending";
pub const PENDING_COUNT: &str = "policy.pending.count";

const MS: Ts = 1_000_000;
/// Upper bound on held items (veto + approval); beyond it new approvals are rejected.
const MAX_PENDING: usize = 500;
/// AutoMod-held message ids remembered (oldest forgotten first).
const MAX_HELD: usize = 1000;

// ---- pure pieces --------------------------------------------------------------------------

/// Role gate: `min` or higher on the ladder everyone < follower < sub < VIP < mod < owner.
/// Follower already includes the minimum follow age (adapters only grant the role once the
/// follow is old enough, see [`PolicyCfg::follower_min_age_ms`]).
pub fn role_allows(actor: Option<&Actor>, min: Role) -> bool {
    min == Role::Everyone || actor.is_some_and(|a| a.top_role() >= min)
}

/// Lowercase role name as written in config (`vip`, `mod`).
pub fn role_name(r: Role) -> String {
    format!("{r:?}").to_lowercase()
}

/// Global and per-user cooldown lengths.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CooldownSpec {
    pub global_ms: Option<u64>,
    pub per_user_ms: Option<u64>,
}

impl CooldownSpec {
    pub fn is_empty(&self) -> bool {
        self.global_ms.is_none() && self.per_user_ms.is_none()
    }
}

/// Cooldown tracker keyed by an action key (`reward:hype`, `cmd:!hype`) and user id.
#[derive(Clone, Debug, Default)]
pub struct Cooldowns {
    global: HashMap<String, u64>,
    user: HashMap<String, u64>,
}

/// Timestamps a [`Cooldowns::commit`] replaced, so a held action that is later rejected
/// gives the cooldown back.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CooldownMark {
    global: Option<u64>,
    user: Option<u64>,
}

fn user_key(key: &str, user: &str) -> String {
    let mut k = String::with_capacity(key.len() + user.len() + 1);
    k.push_str(key);
    k.push('\u{1f}');
    k.push_str(user);
    k
}

impl Cooldowns {
    /// `Ok` when neither cooldown is running, else the remaining milliseconds (the longer one).
    pub fn check(&self, key: &str, user: &str, spec: &CooldownSpec, now_ms: u64) -> Result<(), u64> {
        let mut wait = 0u64;
        if let (Some(cd), Some(t)) = (spec.global_ms, self.global.get(key)) {
            wait = wait.max((t + cd).saturating_sub(now_ms));
        }
        if let Some(cd) = spec.per_user_ms
            && !user.is_empty()
            && let Some(t) = self.user.get(&user_key(key, user))
        {
            wait = wait.max((t + cd).saturating_sub(now_ms));
        }
        if wait == 0 { Ok(()) } else { Err(wait) }
    }

    /// Start both cooldowns for `key` (and `user`) now.
    pub fn commit(&mut self, key: &str, user: &str, now_ms: u64) -> CooldownMark {
        let global = self.global.insert(key.to_string(), now_ms);
        let user = if user.is_empty() { None } else { self.user.insert(user_key(key, user), now_ms) };
        if self.user.len() > 20_000 {
            // bounded memory over a long stream: forget per-user entries older than a day
            self.user.retain(|_, t| now_ms.saturating_sub(*t) < 86_400_000);
        }
        CooldownMark { global, user }
    }

    /// Undo a commit (the action it guarded was rejected).
    pub fn rollback(&mut self, key: &str, user: &str, mark: CooldownMark) {
        match mark.global {
            Some(t) => self.global.insert(key.to_string(), t),
            None => self.global.remove(key),
        };
        if !user.is_empty() {
            let k = user_key(key, user);
            match mark.user {
                Some(t) => self.user.insert(k, t),
                None => self.user.remove(&k),
            };
        }
    }
}

/// Content filter settings (from `[policy]` in `project.toml`).
#[derive(Clone, Debug, PartialEq)]
pub struct FilterCfg {
    /// Blocked terms, already folded. `word` matches a whole word (also spaced out or with
    /// stretched letters), `word*` a prefix, `*word` a suffix, `*word*` anywhere.
    pub blocklist: Vec<String>,
    /// Maximum characters of user text kept for display (longer text is cut with `…`).
    pub max_len: usize,
    /// Combining marks kept per base character (zalgo beyond this is stripped).
    pub max_marks: usize,
}

impl Default for FilterCfg {
    fn default() -> Self {
        FilterCfg { blocklist: Vec::new(), max_len: 300, max_marks: 2 }
    }
}

impl FilterCfg {
    /// The filter configured in the project (defaults when `[policy]` is absent or broken).
    pub fn from_config(c: &Config) -> FilterCfg {
        PolicyCfg::from_config(c).0.filter
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FilterReason {
    /// Matched a blocklist entry (the entry as written, folded).
    Blocked(String),
}

impl fmt::Display for FilterReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FilterReason::Blocked(t) => write!(f, "blocked term `{t}`"),
        }
    }
}

/// Run user text through the content filter: blocklist (homoglyph/leet/spacing-insensitive),
/// then the display form (zalgo stripped, controls removed, whitespace collapsed, length-capped).
pub fn filter_text(s: &str, cfg: &FilterCfg) -> Result<String, FilterReason> {
    if let Some(t) = blocked_term(s, &cfg.blocklist) {
        return Err(FilterReason::Blocked(t));
    }
    Ok(display_text(s, cfg.max_len, cfg.max_marks))
}

fn is_invisible(c: char) -> bool {
    matches!(c,
        '\u{00AD}' | '\u{034F}' | '\u{061C}' | '\u{115F}' | '\u{1160}' | '\u{17B4}' | '\u{17B5}' | '\u{180E}'
        | '\u{200B}' | '\u{200C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}'
        | '\u{2066}'..='\u{206F}' | '\u{3164}' | '\u{FEFF}' | '\u{FFA0}' | '\u{E0001}')
}

/// Display-safe form of user text: NFC, control/bidi/invisible characters removed, at most
/// `max_marks` combining marks per base character, whitespace collapsed, cut to `max_len`
/// characters (with `…`). Idempotent.
pub fn display_text(s: &str, max_len: usize, max_marks: usize) -> String {
    let mut out = String::with_capacity(s.len().min(max_len * 4 + 4));
    let mut marks = 0usize;
    let mut len = 0usize;
    let mut space = false;
    let mut have_base = false;
    let max_len = max_len.max(1);
    // invisibles go first so NFC composes what the viewer actually sees
    let visible: String = s.chars().filter(|c| !is_invisible(*c)).collect();
    for c in visible.nfc() {
        if c.is_whitespace() || c.is_control() {
            if have_base {
                space = true;
            }
            marks = 0;
            continue;
        }
        if is_combining_mark(c) {
            // marks with no base (or only whitespace before them) and zalgo stacks are dropped
            if !have_base || space || marks >= max_marks {
                continue;
            }
            marks += 1;
            out.push(c);
            continue;
        }
        if space {
            if len + 1 >= max_len {
                out.push('…');
                return out;
            }
            out.push(' ');
            len += 1;
            space = false;
        }
        if len >= max_len {
            // cut on the last kept character
            out.push('…');
            return out;
        }
        out.push(c);
        len += 1;
        marks = 0;
        have_base = true;
    }
    out
}

/// [`display_text`] for one chat fragment: a single leading/trailing space is kept so text
/// around emotes stays separated (`"hi "` + emote + `" there"`).
pub fn display_fragment(s: &str, max_len: usize, max_marks: usize) -> String {
    let core = display_text(s, max_len.max(1), max_marks);
    if core.is_empty() {
        return if s.chars().any(char::is_whitespace) { " ".into() } else { core };
    }
    let lead = s.starts_with(char::is_whitespace);
    let trail = s.ends_with(char::is_whitespace) && !core.ends_with('…');
    let mut out = String::with_capacity(core.len() + 2);
    if lead {
        out.push(' ');
    }
    out.push_str(&core);
    if trail {
        out.push(' ');
    }
    out
}

/// Confusable → Latin (Cyrillic, Greek, small capitals, dotless/stroked letters); applied after
/// NFKC + lowercase + mark stripping.
fn confusable(c: char) -> Option<&'static str> {
    Some(match c {
        'а' | 'ɑ' | 'α' | 'ᴀ' => "a",
        'в' | 'β' | 'ʙ' | 'ь' => "b",
        'с' | 'ϲ' | 'ᴄ' => "c",
        'ԁ' | 'ᴅ' | 'đ' => "d",
        'е' | 'ё' | 'є' | 'ε' | 'ᴇ' => "e",
        'ɡ' | 'ɢ' => "g",
        'н' | 'һ' | 'ʜ' => "h",
        'і' | 'ї' | 'ι' | 'ı' | 'ɪ' => "i",
        'ј' | 'ᴊ' => "j",
        'к' | 'κ' | 'ᴋ' => "k",
        'ӏ' | 'ʟ' | 'ł' => "l",
        'м' | 'ᴍ' => "m",
        'η' | 'п' | 'ɴ' => "n",
        'о' | 'ο' | 'σ' | 'ᴏ' | 'ø' => "o",
        'р' | 'ρ' | 'ᴘ' => "p",
        'ԛ' => "q",
        'г' | 'ʀ' => "r",
        'ѕ' | 'ꜱ' => "s",
        'т' | 'τ' | 'ᴛ' => "t",
        'υ' | 'ᴜ' | 'μ' => "u",
        'ν' | 'ᴠ' => "v",
        'ԝ' | 'ω' | 'ᴡ' | 'ш' => "w",
        'х' | 'χ' => "x",
        'у' | 'γ' | 'ү' | 'ʏ' => "y",
        'ᴢ' => "z",
        'ß' => "ss",
        'æ' => "ae",
        'œ' => "oe",
        _ => return None,
    })
}

fn leet(c: char) -> char {
    match c {
        '0' => 'o',
        '1' | '!' | '|' => 'i',
        '3' => 'e',
        '4' | '@' => 'a',
        '5' | '$' => 's',
        '7' | '+' => 't',
        '8' => 'b',
        '9' => 'g',
        _ => c,
    }
}

/// Folded form for matching: NFKC (fullwidth, math alphanumerics, circled letters), lowercase,
/// marks and invisibles removed, confusables mapped to Latin, regional indicators to letters.
/// Returns (plain fold, fold with leetspeak digits/symbols mapped to letters).
pub fn fold(s: &str) -> (String, String) {
    let mut plain = String::with_capacity(s.len());
    for c in s.nfkd() {
        if is_combining_mark(c) || is_invisible(c) {
            continue;
        }
        if ('\u{1F1E6}'..='\u{1F1FF}').contains(&c) {
            plain.push((b'a' + (c as u32 - 0x1F1E6) as u8) as char);
            continue;
        }
        for l in c.to_lowercase() {
            match confusable(l) {
                Some(r) => plain.push_str(r),
                None => plain.push(l),
            }
        }
    }
    let plain: String = plain.nfkc().collect();
    let leeted = plain.chars().map(leet).collect();
    (plain, leeted)
}

fn collapse_repeats(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last = None;
    for c in s.chars() {
        if Some(c) != last {
            out.push(c);
        }
        last = Some(c);
    }
    out
}

/// A letter repeated three or more times in a row (`baaaad`): stretched to dodge filters.
fn is_stretched(s: &str) -> bool {
    let mut last = None;
    let mut run = 0;
    for c in s.chars() {
        run = if Some(c) == last { run + 1 } else { 1 };
        if run >= 3 {
            return true;
        }
        last = Some(c);
    }
    false
}

/// Words of a folded string; runs of single-character words (`b a d`, `b.a.d`) are also joined.
fn words(folded: &str) -> Vec<String> {
    let raw: Vec<&str> = folded.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let mut out: Vec<String> = raw.iter().map(|w| w.to_string()).collect();
    let mut run = String::new();
    for w in raw.iter().chain(std::iter::once(&"")) {
        if w.chars().count() == 1 {
            run.push_str(w);
        } else {
            if run.chars().count() >= 3 {
                out.push(std::mem::take(&mut run));
            }
            run.clear();
        }
    }
    out
}

/// Fold a blocklist entry the way [`blocked_term`] expects (keeps `*` wildcards).
pub fn fold_term(t: &str) -> String {
    let (plain, _) = fold(t.trim());
    plain.chars().filter(|c| c.is_alphanumeric() || *c == '*' || *c == ' ').collect()
}

/// First blocklist entry (folded) that `s` contains, if any.
pub fn blocked_term(s: &str, blocklist: &[String]) -> Option<String> {
    if blocklist.is_empty() || s.is_empty() {
        return None;
    }
    let (plain, leeted) = fold(s);
    let mut words_all = words(&plain);
    words_all.extend(words(&leeted));
    // stretched words are compared with repeats collapsed on both sides (`baaaad` ~ `bad`),
    // ordinary words never are (so `good` never matches `god`)
    let collapsed: Vec<String> = words_all.iter().filter(|w| is_stretched(w)).map(|w| collapse_repeats(w)).collect();
    let squash_plain: String = plain.chars().filter(|c| c.is_alphanumeric()).collect();
    let squash_leet: String = leeted.chars().filter(|c| c.is_alphanumeric()).collect();
    for term in blocklist {
        let (pre, core, suf) = (term.starts_with('*'), term.trim_matches('*'), term.ends_with('*') && term.len() > 1);
        if core.is_empty() {
            continue;
        }
        let hit = if core.contains(' ') {
            // multi-word phrase: whole words on the space-normalized text
            let phrase: String = format!(" {} ", core.split_whitespace().collect::<Vec<_>>().join(" "));
            let norm = |s: &str| format!(" {} ", s.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()).collect::<Vec<_>>().join(" "));
            norm(&plain).contains(&phrase) || norm(&leeted).contains(&phrase)
        } else {
            let cc = collapse_repeats(core);
            let test = |w: &str| match (pre, suf) {
                (true, true) => w.contains(core),
                (false, true) => w.starts_with(core),
                (true, false) => w.ends_with(core),
                (false, false) => w == core,
            };
            words_all.iter().any(|w| test(w))
                || collapsed.iter().any(|w| match (pre, suf) {
                    (true, true) => w.contains(&cc),
                    (false, true) => w.starts_with(&cc),
                    (true, false) => w.ends_with(&cc),
                    (false, false) => *w == cc,
                })
                || (pre && suf && (squash_plain.contains(core) || squash_leet.contains(core)))
        };
        if hit {
            return Some(term.clone());
        }
    }
    None
}

// ---- configuration ------------------------------------------------------------------------

/// `[policy]` in `project.toml`.
#[derive(Clone, Debug, PartialEq)]
pub struct PolicyCfg {
    /// Modes in which chat-originated effects run (§12.1: live; rehearsal mirrors live).
    pub effect_modes: Vec<String>,
    /// Action prefixes that count as effects (paused outside `effect_modes`).
    pub effect_actions: Vec<String>,
    /// Minimum follow age before a viewer gets the follower role (applied by adapters).
    pub follower_min_age_ms: u64,
    pub filter: FilterCfg,
    pub veto: VetoCfg,
    /// Items waiting for mod approval are rejected (and refunded) after this long.
    pub approval_ttl_ms: u64,
    /// Opt-in: `twitch.ad_break` switches the mode to `ad_break` and back after the break.
    pub ad_break_mode: bool,
    /// Modes an ad break never interrupts.
    pub ad_break_skip: Vec<String>,
}

/// Veto window (§12.1): large alerts with user text wait so a mod can kill them.
#[derive(Clone, Debug, PartialEq)]
pub struct VetoCfg {
    pub ms: u64,
    /// Cheers with a message at or above this many bits are held (0 disables).
    pub min_bits: i64,
    /// Tips with a message at or above this amount are held (0 disables).
    pub min_tip: f64,
    /// Resub messages are held.
    pub resub: bool,
    /// Managed redemptions with user input are held.
    pub redeem: bool,
    /// On reject: drop the event entirely instead of delivering it without the text.
    pub drop_on_reject: bool,
}

impl Default for VetoCfg {
    fn default() -> Self {
        VetoCfg { ms: 3000, min_bits: 100, min_tip: 5.0, resub: true, redeem: true, drop_on_reject: false }
    }
}

impl Default for PolicyCfg {
    fn default() -> Self {
        PolicyCfg {
            effect_modes: vec!["live".into(), "rehearsal".into()],
            effect_actions: ["lights", "audio", "patch", "fx", "source", "clips"].iter().map(|s| s.to_string()).collect(),
            follower_min_age_ms: 0,
            filter: FilterCfg::default(),
            veto: VetoCfg::default(),
            approval_ttl_ms: 10 * 60_000,
            ad_break_mode: false,
            ad_break_skip: vec!["offline".into()],
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct PolicyFile {
    effect_modes: Option<Vec<String>>,
    effect_actions: Option<Vec<String>>,
    follower_min_age: Option<Dur>,
    blocklist: Vec<String>,
    max_len: Option<usize>,
    max_marks: Option<usize>,
    approval_ttl: Option<Dur>,
    ad_break_mode: Option<bool>,
    ad_break_skip: Option<Vec<String>>,
    veto: VetoFile,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct VetoFile {
    ms: Option<Dur>,
    min_bits: Option<i64>,
    min_tip: Option<f64>,
    resub: Option<bool>,
    redeem: Option<bool>,
    on_reject: Option<String>,
}

impl PolicyCfg {
    /// Parse `[policy]`; on error the defaults are returned together with the message.
    pub fn from_config(c: &Config) -> (PolicyCfg, Option<String>) {
        match c.project.extra.get("policy") {
            None => (PolicyCfg::default(), None),
            Some(v) => match PolicyCfg::parse(v) {
                Ok(p) => (p, None),
                Err(e) => (PolicyCfg::default(), Some(format!("project.toml [policy]: {e}"))),
            },
        }
    }

    pub fn parse(v: &toml::Value) -> Result<PolicyCfg, String> {
        let f: PolicyFile = v.clone().try_into().map_err(|e: toml::de::Error| e.message().to_string())?;
        let d = PolicyCfg::default();
        let on_reject = match f.veto.on_reject.as_deref() {
            None | Some("strip") => false,
            Some("drop") => true,
            Some(o) => return Err(format!("veto.on_reject: expected `strip` or `drop`, got `{o}`")),
        };
        let filter = FilterCfg {
            blocklist: f.blocklist.iter().map(|t| fold_term(t)).filter(|t| !t.trim_matches('*').is_empty()).collect(),
            max_len: f.max_len.unwrap_or(d.filter.max_len).clamp(1, 5000),
            max_marks: f.max_marks.unwrap_or(d.filter.max_marks).min(8),
        };
        let dv = VetoCfg::default();
        Ok(PolicyCfg {
            effect_modes: f.effect_modes.unwrap_or(d.effect_modes),
            effect_actions: f.effect_actions.unwrap_or(d.effect_actions),
            follower_min_age_ms: f.follower_min_age.map(Dur::ms).unwrap_or(0),
            filter,
            veto: VetoCfg {
                ms: f.veto.ms.map(Dur::ms).unwrap_or(dv.ms).min(60_000),
                min_bits: f.veto.min_bits.unwrap_or(dv.min_bits),
                min_tip: f.veto.min_tip.unwrap_or(dv.min_tip),
                resub: f.veto.resub.unwrap_or(dv.resub),
                redeem: f.veto.redeem.unwrap_or(dv.redeem),
                drop_on_reject: on_reject,
            },
            approval_ttl_ms: f.approval_ttl.map(Dur::ms).unwrap_or(d.approval_ttl_ms).max(1000),
            ad_break_mode: f.ad_break_mode.unwrap_or(d.ad_break_mode),
            ad_break_skip: f.ad_break_skip.unwrap_or(d.ad_break_skip),
        })
    }

    fn effect_action(&self, name: &str) -> bool {
        self.effect_actions.iter().any(|p| name.strip_prefix(p.as_str()).is_some_and(|r| r.is_empty() || r.starts_with('.')))
    }
}

/// A channel point reward we own (`rewards/<key>.toml`, §7). `se-twitch` creates/updates it on
/// Twitch; the core gates its redemptions.
#[derive(Clone, Debug, PartialEq)]
pub struct RewardDef {
    /// File stem.
    pub key: String,
    pub title: String,
    pub cost: i64,
    pub prompt: String,
    /// `#RRGGBB` background colour on Twitch.
    pub color: Option<String>,
    pub enabled: bool,
    pub paused: bool,
    pub input_required: bool,
    /// Global cooldown (also enforced by Twitch).
    pub cooldown_ms: Option<u64>,
    /// Per-viewer cooldown (ours only).
    pub user_cooldown_ms: Option<u64>,
    pub max_per_stream: Option<u32>,
    pub max_per_user_per_stream: Option<u32>,
    /// Commands run for the viewer when a redemption is accepted.
    pub fires: Vec<String>,
    /// Rejected redemptions are refunded (`on_reject = "refund"`, default) or kept (`"keep"`).
    pub refund_on_reject: bool,
    pub role: Role,
    /// A mod must approve each redemption.
    pub approval: bool,
    /// `fulfill = "manual"`: the handler of `fires` fulfills/refunds itself.
    pub manual_fulfill: bool,
    /// Run the content filter on the viewer's input.
    pub filter: bool,
    /// Modes in which redemptions run (default: the policy's effect modes).
    pub modes: Option<Vec<String>>,
    /// `fx = true`: the reward fires effects, so it is paused on Twitch while `fx.enabled` is off.
    pub fx: bool,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Fires {
    One(String),
    Many(Vec<String>),
}

#[derive(Deserialize)]
#[serde(default, deny_unknown_fields)]
struct RewardFile {
    title: String,
    cost: i64,
    prompt: String,
    color: Option<String>,
    enabled: bool,
    paused: bool,
    input_required: bool,
    cooldown: Option<Dur>,
    user_cooldown: Option<Dur>,
    max_per_stream: Option<u32>,
    max_per_user_per_stream: Option<u32>,
    fires: Option<Fires>,
    on_reject: String,
    role: Role,
    approval: bool,
    fulfill: String,
    filter: bool,
    modes: Option<Vec<String>>,
    fx: bool,
}

impl Default for RewardFile {
    fn default() -> Self {
        RewardFile {
            title: String::new(),
            cost: 0,
            prompt: String::new(),
            color: None,
            enabled: true,
            paused: false,
            input_required: false,
            cooldown: None,
            user_cooldown: None,
            max_per_stream: None,
            max_per_user_per_stream: None,
            fires: None,
            on_reject: "refund".into(),
            role: Role::Everyone,
            approval: false,
            fulfill: "auto".into(),
            filter: true,
            modes: None,
            fx: false,
        }
    }
}

/// `preset.<name>` (one token) is shorthand for `preset.fire <name>` (§7 `fires = "preset.hype"`).
fn expand_fires(c: &str) -> String {
    let t = c.trim();
    match t.strip_prefix("preset.") {
        Some(name) if !name.is_empty() && !t.contains(char::is_whitespace) && !matches!(name, "fire" | "release" | "toggle") => {
            format!("preset.fire {name}")
        }
        _ => t.to_string(),
    }
}

impl RewardDef {
    pub fn parse(key: &str, t: &toml::Table) -> Result<RewardDef, String> {
        let f: RewardFile = toml::Value::Table(t.clone()).try_into().map_err(|e: toml::de::Error| e.message().to_string())?;
        let title = f.title.trim().to_string();
        if title.is_empty() {
            return Err("reward needs a `title`".into());
        }
        if title.chars().count() > 45 {
            return Err("`title` is limited to 45 characters by Twitch".into());
        }
        if f.cost < 1 {
            return Err("`cost` must be at least 1".into());
        }
        if f.prompt.chars().count() > 200 {
            return Err("`prompt` is limited to 200 characters by Twitch".into());
        }
        if let Some(c) = &f.color
            && (c.len() != 7 || !c.starts_with('#') || !c[1..].chars().all(|x| x.is_ascii_hexdigit()))
        {
            return Err(format!("`color` must be #RRGGBB, got `{c}`"));
        }
        if let Some(cd) = f.cooldown
            && !(1000..=604_800_000).contains(&cd.ms())
        {
            return Err("`cooldown` must be between 1s and 7 days (Twitch limit)".into());
        }
        if f.max_per_stream == Some(0) || f.max_per_user_per_stream == Some(0) {
            return Err("`max_per_stream` / `max_per_user_per_stream` must be at least 1".into());
        }
        let refund_on_reject = match f.on_reject.as_str() {
            "refund" => true,
            "keep" => false,
            o => return Err(format!("`on_reject` must be `refund` or `keep`, got `{o}`")),
        };
        let manual_fulfill = match f.fulfill.as_str() {
            "auto" => false,
            "manual" => true,
            o => return Err(format!("`fulfill` must be `auto` or `manual`, got `{o}`")),
        };
        let fires: Vec<String> = match f.fires {
            None => Vec::new(),
            Some(Fires::One(s)) => vec![expand_fires(&s)],
            Some(Fires::Many(v)) => v.iter().map(|s| expand_fires(s)).collect(),
        };
        for c in &fires {
            Op::parse(c).map_err(|e| format!("fires `{c}`: {e}"))?;
        }
        Ok(RewardDef {
            key: key.to_string(),
            title,
            cost: f.cost,
            prompt: f.prompt,
            color: f.color,
            enabled: f.enabled,
            paused: f.paused,
            input_required: f.input_required,
            cooldown_ms: f.cooldown.map(Dur::ms),
            user_cooldown_ms: f.user_cooldown.map(Dur::ms),
            max_per_stream: f.max_per_stream,
            max_per_user_per_stream: f.max_per_user_per_stream,
            fires,
            refund_on_reject,
            role: f.role,
            approval: f.approval,
            manual_fulfill,
            filter: f.filter,
            modes: f.modes,
            fx: f.fx,
        })
    }

    /// All rewards in the project plus per-file errors (`rewards/<key>.toml: …`).
    pub fn all(c: &Config) -> (Vec<RewardDef>, Vec<String>) {
        let mut ok = Vec::new();
        let mut errs = Vec::new();
        if let Some(files) = c.other.get("rewards") {
            for (k, t) in files {
                match RewardDef::parse(k, t) {
                    Ok(r) => {
                        if ok.iter().any(|o: &RewardDef| o.title.eq_ignore_ascii_case(&r.title)) {
                            errs.push(format!("rewards/{k}.toml: duplicate title `{}`", r.title));
                        } else {
                            ok.push(r)
                        }
                    }
                    Err(e) => errs.push(format!("rewards/{k}.toml: {e}")),
                }
            }
        }
        (ok, errs)
    }
}

/// Gate for an ad-hoc chat action (`policy.run`, chatbot commands).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GateSpec {
    pub role: Role,
    pub cooldown: CooldownSpec,
    pub approval: bool,
    pub filter: bool,
}

impl GateSpec {
    /// From `policy.run` args: `role`, `cooldown = {global, per_user}`, `approval`, `filter`.
    pub fn from_args(a: &Value) -> Result<GateSpec, String> {
        let role = match a.get_path("role").and_then(Value::as_str) {
            None => Role::Everyone,
            Some(r) => Role::parse(r).ok_or_else(|| format!("unknown role `{r}`"))?,
        };
        let dur = |k: &str| -> Result<Option<u64>, String> {
            match a.get_path(k) {
                None | Some(Value::Null) => Ok(None),
                Some(Value::Str(s)) => se_proto::parse_duration_ms(s).map(Some).ok_or_else(|| format!("bad duration `{s}` for {k}")),
                Some(v) => v.as_f64().filter(|f| *f >= 0.0).map(|f| Some(f as u64)).ok_or_else(|| format!("bad duration for {k}")),
            }
        };
        Ok(GateSpec {
            role,
            cooldown: CooldownSpec { global_ms: dur("cooldown.global")?, per_user_ms: dur("cooldown.per_user")? },
            approval: a.get_path("approval").is_some_and(Value::truthy),
            filter: a.get_path("filter").is_none_or(Value::truthy),
        })
    }
}

// ---- the policy state machine ---------------------------------------------------------------

/// What the core should do next, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Publish the event and run its rules.
    Deliver(Event),
    /// A `policy.*` decision event (published, audited, visible to rules).
    Emit(Event),
    /// Run commands at chat priority for a viewer (approved commands).
    Run { commands: Vec<String>, actor: Option<Actor>, event: Option<Event> },
    /// Run a managed redemption's `fires`, then hand the outcome to [`Policy::settle`] so it
    /// is fulfilled or rejected (and refunded) by what actually happened (§4.3: a preset with
    /// `conflict = "reject"` that is already active refunds the viewer).
    Redeem(Box<Redemption>),
    /// Adapter action (`twitch.refund`, `twitch.fulfill`).
    Action { name: String, args: Value, actor: Option<Actor> },
    /// Switch the show mode (ad breaks).
    SetMode(String),
    /// The pending queue changed (`policy.pending` must be republished).
    Pending,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldKind {
    /// Auto-approved when the window ends unless a mod rejects it.
    Veto,
    /// Waits for a mod; rejected when it expires.
    Approval,
}

impl HoldKind {
    fn as_str(self) -> &'static str {
        match self {
            HoldKind::Veto => "veto",
            HoldKind::Approval => "approval",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct Paid {
    key: String,
    fires: Vec<String>,
    redemption_id: String,
    reward_id: String,
    refund: bool,
    refundable: bool,
    fulfill: bool,
    user: String,
    mark: CooldownMark,
    cd_key: String,
    /// Modes the reward runs in and whether it came from the simulator (exempt).
    modes: Vec<String>,
    sim: bool,
}

/// A managed redemption waiting for its `fires` to run (see [`Effect::Redeem`]).
#[derive(Clone, Debug, PartialEq)]
pub struct Redemption {
    pub commands: Vec<String>,
    pub event: Event,
    paid: Paid,
    approved_by: Option<String>,
}

impl Redemption {
    pub fn actor(&self) -> Option<&Actor> {
        self.event.actor.as_ref()
    }
}

#[derive(Clone, Debug)]
enum Work {
    Event { event: Event, paid: Option<Paid> },
    Commands { commands: Vec<String>, actor: Option<Actor>, event: Option<Event>, rollback: Option<(String, String, CooldownMark)> },
}

#[derive(Clone, Debug)]
struct Pending {
    id: String,
    kind: HoldKind,
    deadline: Ts,
    user_id: String,
    message_id: String,
    work: Work,
    summary: Value,
}

#[derive(Clone, Debug)]
struct AdBreak {
    prev: String,
    until: Ts,
}

/// The policy pipeline run by the core (see the module docs).
#[derive(Default)]
pub struct Policy {
    pub cfg: PolicyCfg,
    rewards: Vec<RewardDef>,
    pub cooldowns: Cooldowns,
    stream_counts: HashMap<String, u32>,
    user_counts: HashMap<String, u32>,
    pending: Vec<Pending>,
    held: VecDeque<String>,
    ad: Option<AdBreak>,
    next_deadline: Option<Ts>,
}

fn s(v: &Value, k: &str) -> String {
    match v.get_path(k) {
        Some(Value::Str(x)) => x.clone(),
        Some(Value::Null) | None => String::new(),
        Some(o) => o.to_string(),
    }
}

/// Stable id for a held item, derived from the input that caused it (replay-safe).
pub fn hold_id(seed: u64) -> String {
    format!("p{seed:x}")
}

/// Events from the platform itself rather than a viewer: rules on them run at normal
/// (preset) priority so they may change modes and scenes.
pub fn is_platform_event(ty: &str) -> bool {
    matches!(ty, "twitch.stream.online" | "twitch.stream.offline" | "twitch.ad_break") || ty.starts_with("twitch.ad.")
}

/// Commands a viewer submits directly (origin `chat`/`relay`) may not moderate or drive the
/// channel unless the viewer is a mod.
pub fn chat_action_allowed(name: &str, actor: Option<&Actor>) -> Result<(), String> {
    const OPEN: &[&str] = &["twitch.chat.send", "twitch.marker"];
    let restricted = (name.starts_with("mod.") || name.starts_with("twitch.")) && !OPEN.contains(&name);
    if restricted && !role_allows(actor, Role::Mod) {
        return Err(format!("`{name}` needs a moderator"));
    }
    Ok(())
}

/// User text present in a cheer message once cheermotes (`Cheer100`) are removed.
fn has_user_text(ty: &str, text: &str) -> bool {
    if ty == "twitch.cheer" {
        text.split_whitespace().any(|w| {
            let digits = w.trim_start_matches(|c: char| c.is_alphabetic());
            !(digits.len() < w.len() && !digits.is_empty() && digits.chars().all(|c| c.is_ascii_digit()))
        })
    } else {
        !text.trim().is_empty()
    }
}

impl Policy {
    /// Apply configuration (initial load and hot reload); returns errors to report.
    pub fn configure(&mut self, c: &Config) -> Vec<String> {
        let (cfg, err) = PolicyCfg::from_config(c);
        let (rewards, mut errs) = RewardDef::all(c);
        if let Some(e) = err {
            errs.insert(0, e);
        }
        self.cfg = cfg;
        self.rewards = rewards;
        errs
    }

    pub fn rewards(&self) -> &[RewardDef] {
        &self.rewards
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// `policy.pending` state: the queue shown to mods.
    pub fn pending_value(&self) -> Value {
        Value::List(self.pending.iter().map(|p| p.summary.clone()).collect())
    }

    /// Is `op` a chat effect that must not run in `mode`?
    pub fn effect_paused(&self, op: &Op, mode: &str) -> bool {
        if self.cfg.effect_modes.iter().any(|m| m == mode) {
            return false;
        }
        match op {
            Op::Set { .. } | Op::Animate { .. } | Op::Trigger { .. } | Op::PresetFire { .. } => true,
            Op::Action { name, .. } => self.cfg.effect_action(name),
            _ => false,
        }
    }

    /// The engine restarted in `mode`; an interrupted ad break returns to `live` shortly.
    pub fn restored(&mut self, mode: &str, now: Ts) {
        if mode == "ad_break" && self.ad.is_none() {
            self.ad = Some(AdBreak { prev: "live".into(), until: now + 5_000 * MS });
        }
    }

    fn managed<'a>(&'a self, ev: &Event) -> Option<&'a RewardDef> {
        let p = &ev.payload;
        if p.get_path("managed").is_some_and(|m| matches!(m, Value::Bool(false))) {
            return None;
        }
        let key = s(p, "reward_key");
        if !key.is_empty() {
            return self.rewards.iter().find(|r| r.key == key);
        }
        let title = s(p, "reward");
        self.rewards.iter().find(|r| r.title.eq_ignore_ascii_case(&title))
    }

    /// Sanitize user text in place; returns the blocklist entry hit, if any.
    fn sanitize(&self, ev: &mut Event, check: bool) -> Option<String> {
        let (max_len, max_marks) = (self.cfg.filter.max_len, self.cfg.filter.max_marks);
        let relay = ev.origin == Origin::Relay;
        let Value::Map(m) = &mut ev.payload else { return None };
        let mut blocked = None;
        for key in ["message", "input"] {
            if let Some(Value::Str(t)) = m.get_mut(key) {
                if check && blocked.is_none() {
                    blocked = blocked_term(t, &self.cfg.filter.blocklist);
                }
                *t = display_text(t, max_len, max_marks);
            }
        }
        if relay && let Some(Value::Str(u)) = m.get_mut("user") {
            if check && blocked.is_none() {
                blocked = blocked_term(u, &self.cfg.filter.blocklist);
            }
            *u = display_text(u, 64, max_marks);
        }
        if let Some(Value::List(frags)) = m.get_mut("fragments") {
            let mut total = 0usize;
            let mut keep = 0usize;
            for f in frags.iter_mut() {
                if total >= max_len {
                    break;
                }
                keep += 1;
                if let Value::Map(fm) = f
                    && let Some(Value::Str(t)) = fm.get_mut("text")
                {
                    let clean = display_fragment(t, max_len - total, max_marks);
                    total += clean.chars().count().max(1);
                    *t = clean;
                }
            }
            frags.truncate(keep);
        }
        blocked
    }

    fn strip_text(ev: &mut Event) {
        if let Value::Map(m) = &mut ev.payload {
            for key in ["message", "input"] {
                if m.contains_key(key) {
                    m.insert(key.into(), Value::Str(String::new()));
                }
            }
            if m.contains_key("fragments") {
                m.insert("fragments".into(), Value::List(Vec::new()));
            }
        }
    }

    fn veto_applies(&self, ev: &Event) -> bool {
        let v = &self.cfg.veto;
        if v.ms == 0 {
            return false;
        }
        let p = &ev.payload;
        let msg = s(p, "message");
        match ev.ty.as_str() {
            "twitch.cheer" => v.min_bits > 0 && p.get_path("bits").and_then(Value::as_i64).unwrap_or(0) >= v.min_bits && has_user_text(&ev.ty, &msg),
            "tip" => v.min_tip > 0.0 && p.get_path("amount").and_then(Value::as_f64).unwrap_or(0.0) >= v.min_tip && has_user_text(&ev.ty, &msg),
            "twitch.resub" | "twitch.sub" => v.resub && has_user_text(&ev.ty, &msg),
            _ => false,
        }
    }

    fn summary(&self, id: &str, kind: HoldKind, ev: &Event, now: Ts, deadline: Ts, reward: Option<&RewardDef>) -> Value {
        let p = &ev.payload;
        let text = if ev.ty == "twitch.redeem" { s(p, "input") } else { s(p, "message") };
        let amount = p.get_path("bits").or_else(|| p.get_path("amount")).or_else(|| p.get_path("cost")).cloned().unwrap_or_default();
        Value::map()
            .with("id", id)
            .with("kind", kind.as_str())
            .with("type", ev.ty.clone())
            .with("user", ev.actor.as_ref().map(|a| a.name.clone()).unwrap_or_else(|| s(p, "user")))
            .with("user_id", ev.actor.as_ref().map(|a| a.id.clone()).unwrap_or_default())
            .with("text", text)
            .with("reward", reward.map(|r| Value::Str(r.title.clone())).unwrap_or_default())
            .with("amount", amount)
            .with("message_id", s(p, "message_id"))
            .with("created_ns", now as i64)
            .with("expires_ns", deadline as i64)
    }

    fn decision(ty: &str, cause: &Event, payload: Value) -> Event {
        let mut e = Event::new(ty, Origin::System, payload);
        e.actor = cause.actor.clone();
        e.causal = Some(cause.id);
        e
    }

    fn push_pending(&mut self, p: Pending, out: &mut Vec<Effect>) {
        self.next_deadline = Some(self.next_deadline.map_or(p.deadline, |d| d.min(p.deadline)));
        let mut announce = p.summary.clone();
        if let Value::Map(m) = &mut announce {
            m.remove("created_ns");
        }
        let cause = match &p.work {
            Work::Event { event, .. } => Some(event.clone()),
            Work::Commands { event, .. } => event.clone(),
        };
        let mut e = Event::new("policy.pending", Origin::System, announce);
        if let Some(c) = &cause {
            e.actor = c.actor.clone();
            e.causal = Some(c.id);
        }
        self.pending.push(p);
        out.push(Effect::Emit(e));
        out.push(Effect::Pending);
    }

    /// Screen one event before it is published and rules run (§12.1).
    pub fn screen(&mut self, mut ev: Event, now: Ts, mode: &str, out: &mut Vec<Effect>) {
        match ev.ty.as_str() {
            "twitch.automod.hold" => {
                let mid = s(&ev.payload, "message_id");
                if !mid.is_empty() {
                    if self.held.len() >= MAX_HELD {
                        self.held.pop_front();
                    }
                    self.held.push_back(mid);
                }
            }
            "twitch.automod.update" => {
                let mid = s(&ev.payload, "message_id");
                self.held.retain(|m| *m != mid);
            }
            "twitch.chat.delete" => {
                let mid = s(&ev.payload, "message_id");
                if !mid.is_empty() {
                    self.drop_where(|p| p.message_id == mid, "message deleted", now, out);
                }
            }
            "twitch.user.purge" => {
                let uid = s(&ev.payload, "user_id");
                if !uid.is_empty() {
                    self.drop_where(|p| p.user_id == uid, "user banned or timed out", now, out);
                }
            }
            "twitch.stream.online" => {
                self.stream_counts.clear();
                self.user_counts.clear();
            }
            "twitch.ad_break" if self.cfg.ad_break_mode => {
                let secs = ev.payload.get_path("duration").and_then(Value::as_i64).unwrap_or(90).clamp(1, 600) as u64;
                let until = now + secs * 1000 * MS;
                if mode == "ad_break" {
                    if let Some(a) = &mut self.ad {
                        a.until = a.until.max(until);
                    }
                } else if !self.cfg.ad_break_skip.iter().any(|m| m == mode) {
                    self.ad = Some(AdBreak { prev: mode.to_string(), until });
                    out.push(Effect::SetMode("ad_break".into()));
                }
            }
            _ => {}
        }
        let external = matches!(ev.origin, Origin::Twitch | Origin::Chat | Origin::Relay | Origin::Sim);
        if !external || ev.actor.is_none() || ev.ty.starts_with("policy.") {
            out.push(Effect::Deliver(ev));
            return;
        }
        let exempt = role_allows(ev.actor.as_ref(), Role::Mod);
        let blocked = self.sanitize(&mut ev, !exempt);
        if ev.ty == "twitch.chat" {
            let mid = s(&ev.payload, "message_id");
            let reason = if !mid.is_empty() && self.held.contains(&mid) {
                Some("held by AutoMod".to_string())
            } else {
                blocked.map(|t| FilterReason::Blocked(t).to_string())
            };
            match reason {
                Some(r) => {
                    let p = &ev.payload;
                    let info = Value::map()
                        .with("type", ev.ty.clone())
                        .with("reason", r)
                        .with("message_id", mid.clone())
                        .with("user", s(p, "user"))
                        .with("user_id", ev.actor.as_ref().map(|a| a.id.clone()).unwrap_or_default())
                        .with("text", s(p, "message"));
                    out.push(Effect::Emit(Self::decision("policy.filtered", &ev, info)));
                }
                None => out.push(Effect::Deliver(ev)),
            }
            return;
        }
        if ev.ty == "twitch.redeem"
            && let Some(r) = self.managed(&ev).cloned()
        {
            self.redeem(ev, &r, blocked, now, mode, out);
            return;
        }
        if let Some(term) = blocked {
            Self::strip_text(&mut ev);
            ev.payload = std::mem::take(&mut ev.payload).with("filtered", "blocklist");
            let info = Value::map()
                .with("type", ev.ty.clone())
                .with("reason", FilterReason::Blocked(term).to_string())
                .with("user", s(&ev.payload, "user"))
                .with("user_id", ev.actor.as_ref().map(|a| a.id.clone()).unwrap_or_default())
                .with("stripped", true);
            out.push(Effect::Emit(Self::decision("policy.filtered", &ev, info)));
        }
        if !exempt && self.veto_applies(&ev) && self.pending.len() < MAX_PENDING {
            let id = hold_id(ev.id);
            let deadline = now + self.cfg.veto.ms * MS;
            let summary = self.summary(&id, HoldKind::Veto, &ev, now, deadline, None);
            let (user_id, message_id) = (ev.actor.as_ref().map(|a| a.id.clone()).unwrap_or_default(), s(&ev.payload, "message_id"));
            self.push_pending(Pending { id, kind: HoldKind::Veto, deadline, user_id, message_id, work: Work::Event { event: ev, paid: None }, summary }, out);
            return;
        }
        out.push(Effect::Deliver(ev));
    }

    /// Managed channel point redemption: role → cost → cooldowns/limits → filter → approval/veto → execute.
    fn redeem(&mut self, ev: Event, r: &RewardDef, blocked: Option<String>, now: Ts, mode: &str, out: &mut Vec<Effect>) {
        let p = &ev.payload;
        let user = ev.actor.as_ref().map(|a| a.id.clone()).unwrap_or_default();
        let status = s(p, "status");
        let refundable = status.is_empty() || status.eq_ignore_ascii_case("unfulfilled");
        let paid_cost = p.get_path("cost").and_then(Value::as_i64).unwrap_or(r.cost);
        let input = s(p, "input");
        let modes = r.modes.as_ref().unwrap_or(&self.cfg.effect_modes);
        let now_ms = now / MS;
        let cd_key = format!("reward:{}", r.key);
        let spec = CooldownSpec { global_ms: r.cooldown_ms, per_user_ms: r.user_cooldown_ms };
        let ukey = user_key(&r.key, &user);
        let reason: Option<String> = if !r.enabled {
            Some("reward is disabled".into())
        } else if ev.origin != Origin::Sim && !modes.iter().any(|m| m == mode) {
            Some(format!("effects are paused (mode `{mode}`)"))
        } else if !role_allows(ev.actor.as_ref(), r.role) {
            Some(format!("needs role `{}`", role_name(r.role)))
        } else if paid_cost < r.cost {
            Some(format!("paid {paid_cost}, costs {}", r.cost))
        } else if let Err(wait) = self.cooldowns.check(&cd_key, &user, &spec, now_ms) {
            Some(format!("cooldown ({}s left)", wait.div_ceil(1000)))
        } else if r.max_per_stream.is_some_and(|m| self.stream_counts.get(&r.key).copied().unwrap_or(0) >= m) {
            Some("limit per stream reached".into())
        } else if r.max_per_user_per_stream.is_some_and(|m| self.user_counts.get(&ukey).copied().unwrap_or(0) >= m) {
            Some("limit per viewer reached".into())
        } else if r.input_required && input.trim().is_empty() {
            Some("input required".into())
        } else if r.filter && blocked.is_some() {
            blocked.map(|t| FilterReason::Blocked(t).to_string())
        } else if self.pending.len() >= MAX_PENDING && (r.approval || self.cfg.veto.redeem) {
            Some("approval queue is full".into())
        } else {
            None
        };
        let paid = Paid {
            key: r.key.clone(),
            fires: r.fires.clone(),
            redemption_id: s(p, "redemption_id"),
            reward_id: s(p, "reward_id"),
            refund: r.refund_on_reject,
            refundable,
            fulfill: !r.manual_fulfill,
            user: user.clone(),
            mark: CooldownMark::default(),
            cd_key: cd_key.clone(),
            modes: modes.clone(),
            sim: ev.origin == Origin::Sim,
        };
        if let Some(reason) = reason {
            self.reject_paid(ev, paid, &reason, "policy", out);
            return;
        }
        let mut paid = paid;
        paid.mark = self.cooldowns.commit(&cd_key, &user, now_ms);
        *self.stream_counts.entry(r.key.clone()).or_default() += 1;
        *self.user_counts.entry(ukey).or_default() += 1;
        let hold = if r.approval {
            Some((HoldKind::Approval, self.cfg.approval_ttl_ms))
        } else if self.cfg.veto.redeem && self.cfg.veto.ms > 0 && !input.trim().is_empty() && !role_allows(ev.actor.as_ref(), Role::Mod) {
            Some((HoldKind::Veto, self.cfg.veto.ms))
        } else {
            None
        };
        match hold {
            Some((kind, ms)) => {
                let id = hold_id(ev.id);
                let deadline = now + ms * MS;
                let summary = self.summary(&id, kind, &ev, now, deadline, Some(r));
                let message_id = s(&ev.payload, "message_id");
                self.push_pending(Pending { id, kind, deadline, user_id: user, message_id, work: Work::Event { event: ev, paid: Some(paid) }, summary }, out);
            }
            None => self.accept_paid(ev, paid, None, out),
        }
    }

    fn accept_paid(&mut self, mut ev: Event, paid: Paid, approved_by: Option<&str>, out: &mut Vec<Effect>) {
        if approved_by.is_some() {
            ev.payload = std::mem::take(&mut ev.payload).with("vetted", true);
        }
        if paid.fires.is_empty() {
            self.fulfilled(ev, paid, approved_by, out);
        } else {
            out.push(Effect::Redeem(Box::new(Redemption { commands: paid.fires.clone(), event: ev, paid, approved_by: approved_by.map(String::from) })));
        }
    }

    /// The core ran a redemption's `fires`: `Ok` fulfills it, `Err(reason)` (the first command
    /// that failed, e.g. a `reject` preset that is already active) rejects and refunds it and
    /// gives back the cooldowns and limits it took.
    pub fn settle(&mut self, r: Redemption, outcome: Result<(), String>, out: &mut Vec<Effect>) {
        match outcome {
            Ok(()) => self.fulfilled(r.event, r.paid, r.approved_by.as_deref(), out),
            Err(reason) => {
                self.release_paid(&r.paid);
                self.reject_paid(r.event, r.paid, &reason, "policy", out);
            }
        }
    }

    fn fulfilled(&mut self, ev: Event, paid: Paid, approved_by: Option<&str>, out: &mut Vec<Effect>) {
        let info = Value::map()
            .with("type", ev.ty.clone())
            .with("reward", s(&ev.payload, "reward"))
            .with("reward_key", paid.key.clone())
            .with("redemption_id", paid.redemption_id.clone())
            .with("user", s(&ev.payload, "user"))
            .with("user_id", paid.user.clone())
            .with("approved_by", approved_by.map(Value::from).unwrap_or_default());
        out.push(Effect::Emit(Self::decision("policy.accepted", &ev, info)));
        let actor = ev.actor.clone();
        out.push(Effect::Deliver(ev));
        if paid.fulfill && !paid.redemption_id.is_empty() {
            let args = Value::map().with("redemption_id", paid.redemption_id).with("reward_id", paid.reward_id);
            out.push(Effect::Action { name: "twitch.fulfill".into(), args, actor });
        }
    }

    fn reject_paid(&mut self, ev: Event, paid: Paid, reason: &str, by: &str, out: &mut Vec<Effect>) {
        let refund = paid.refund && paid.refundable && !paid.redemption_id.is_empty();
        let info = Value::map()
            .with("type", ev.ty.clone())
            .with("reason", reason)
            .with("by", by)
            .with("reward", s(&ev.payload, "reward"))
            .with("reward_key", paid.key.clone())
            .with("redemption_id", paid.redemption_id.clone())
            .with("user", s(&ev.payload, "user"))
            .with("user_id", paid.user.clone())
            .with("refunded", refund);
        out.push(Effect::Emit(Self::decision("policy.rejected", &ev, info)));
        if refund {
            let args = Value::map().with("redemption_id", paid.redemption_id).with("reward_id", paid.reward_id).with("reason", reason);
            out.push(Effect::Action { name: "twitch.refund".into(), args, actor: ev.actor.clone() });
        }
    }

    /// Give back cooldowns and per-stream counts taken by a held redemption.
    fn release_paid(&mut self, paid: &Paid) {
        self.cooldowns.rollback(&paid.cd_key, &paid.user, paid.mark);
        if let Some(n) = self.stream_counts.get_mut(&paid.key) {
            *n = n.saturating_sub(1);
        }
        if let Some(n) = self.user_counts.get_mut(&user_key(&paid.key, &paid.user)) {
            *n = n.saturating_sub(1);
        }
    }

    fn approve(&mut self, p: Pending, by: &str, mode: &str, out: &mut Vec<Effect>) {
        let mut info = p.summary.clone().with("by", by);
        if let Value::Map(m) = &mut info {
            m.remove("created_ns");
            m.remove("expires_ns");
        }
        match p.work {
            Work::Event { event, paid: Some(paid) } => {
                // the show may have left the effect modes while the redemption waited
                if !paid.sim && !paid.modes.iter().any(|m| m == mode) {
                    self.release_paid(&paid);
                    self.reject_paid(event, paid, &format!("effects are paused (mode `{mode}`)"), by, out);
                    return;
                }
                out.push(Effect::Emit(Self::decision("policy.approved", &event, info)));
                self.accept_paid(event, paid, Some(by), out);
            }
            Work::Event { mut event, paid: None } => {
                out.push(Effect::Emit(Self::decision("policy.approved", &event, info)));
                event.payload = std::mem::take(&mut event.payload).with("vetted", true);
                out.push(Effect::Deliver(event));
            }
            Work::Commands { commands, actor, event, .. } => {
                let mut e = Event::new("policy.approved", Origin::System, info);
                e.actor = actor.clone();
                e.causal = event.as_ref().map(|c| c.id);
                out.push(Effect::Emit(e));
                out.push(Effect::Run { commands, actor, event });
            }
        }
    }

    fn reject(&mut self, p: Pending, reason: &str, by: &str, drop: bool, out: &mut Vec<Effect>) {
        match p.work {
            Work::Event { event, paid: Some(paid) } => {
                self.release_paid(&paid);
                self.reject_paid(event, paid, reason, by, out);
            }
            Work::Event { mut event, paid: None } => {
                let drop = drop || self.cfg.veto.drop_on_reject;
                let info = p.summary.clone().with("reason", reason).with("by", by).with("dropped", drop);
                out.push(Effect::Emit(Self::decision("policy.rejected", &event, info)));
                if !drop {
                    Self::strip_text(&mut event);
                    event.payload = std::mem::take(&mut event.payload).with("vetted", true).with("vetoed", true);
                    out.push(Effect::Deliver(event));
                }
            }
            Work::Commands { actor, event, rollback, .. } => {
                if let Some((k, u, m)) = rollback {
                    self.cooldowns.rollback(&k, &u, m);
                }
                let mut e = Event::new("policy.rejected", Origin::System, p.summary.clone().with("reason", reason).with("by", by));
                e.actor = actor;
                e.causal = event.as_ref().map(|c| c.id);
                out.push(Effect::Emit(e));
            }
        }
    }

    fn drop_where(&mut self, f: impl Fn(&Pending) -> bool, reason: &str, _now: Ts, out: &mut Vec<Effect>) {
        let (gone, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending).into_iter().partition(|p| f(p));
        self.pending = keep;
        if gone.is_empty() {
            return;
        }
        for p in gone {
            self.reject(p, reason, "system", true, out);
        }
        out.push(Effect::Pending);
    }

    /// Mod decision on a held item (`mod.approve` / `mod.reject`).
    pub fn resolve(&mut self, id: &str, approve: bool, by: &str, reason: Option<&str>, mode: &str, out: &mut Vec<Effect>) -> Result<(), String> {
        let i = self.pending.iter().position(|p| p.id == id).ok_or_else(|| format!("nothing pending with id `{id}`"))?;
        let p = self.pending.remove(i);
        if approve {
            self.approve(p, by, mode, out);
        } else {
            self.reject(p, reason.unwrap_or("rejected by a moderator"), by, false, out);
        }
        out.push(Effect::Pending);
        Ok(())
    }

    /// Hold gated commands for mod approval (rules with `approval = true`, `policy.run`).
    #[allow(clippy::too_many_arguments)]
    pub fn hold_commands(
        &mut self,
        id: String,
        label: &str,
        commands: Vec<String>,
        actor: Option<Actor>,
        event: Option<Event>,
        rollback: Option<(String, String, CooldownMark)>,
        now: Ts,
        out: &mut Vec<Effect>,
    ) -> Result<(), String> {
        if self.pending.len() >= MAX_PENDING {
            return Err("approval queue is full".into());
        }
        let deadline = now + self.cfg.approval_ttl_ms * MS;
        let text = event.as_ref().map(|e| s(&e.payload, "message")).unwrap_or_default();
        let summary = Value::map()
            .with("id", id.clone())
            .with("kind", HoldKind::Approval.as_str())
            .with("type", label)
            .with("user", actor.as_ref().map(|a| a.name.clone()).unwrap_or_default())
            .with("user_id", actor.as_ref().map(|a| a.id.clone()).unwrap_or_default())
            .with("text", text)
            .with("do", commands.clone())
            .with("message_id", event.as_ref().map(|e| s(&e.payload, "message_id")).unwrap_or_default())
            .with("created_ns", now as i64)
            .with("expires_ns", deadline as i64);
        let user_id = actor.as_ref().map(|a| a.id.clone()).unwrap_or_default();
        let message_id = event.as_ref().map(|e| s(&e.payload, "message_id")).unwrap_or_default();
        self.push_pending(
            Pending { id, kind: HoldKind::Approval, deadline, user_id, message_id, work: Work::Commands { commands, actor, event, rollback }, summary },
            out,
        );
        Ok(())
    }

    /// Gate an ad-hoc chat action (`policy.run`): role → cooldowns → filter → approval → run.
    /// `Err` carries the reason (also announced as `policy.rejected`).
    #[allow(clippy::too_many_arguments)]
    pub fn run_gated(
        &mut self,
        id: String,
        key: &str,
        spec: &GateSpec,
        commands: Vec<String>,
        actor: Option<Actor>,
        mut event: Option<Event>,
        now: Ts,
        out: &mut Vec<Effect>,
    ) -> Result<(), String> {
        let user = actor.as_ref().map(|a| a.id.clone()).unwrap_or_default();
        let label = format!("run {key}");
        let cd_key = format!("run:{key}");
        let reason = if !role_allows(actor.as_ref(), spec.role) {
            Some(format!("needs role `{}`", role_name(spec.role)))
        } else if let Err(wait) = self.cooldowns.check(&cd_key, &user, &spec.cooldown, now / MS) {
            Some(format!("cooldown ({}s left)", wait.div_ceil(1000)))
        } else if spec.filter
            && !role_allows(actor.as_ref(), Role::Mod)
            && let Some(t) = event.as_mut().and_then(|e| self.sanitize(e, true))
        {
            Some(FilterReason::Blocked(t).to_string())
        } else {
            None
        };
        if let Some(reason) = reason {
            let info = Value::map()
                .with("type", label)
                .with("reason", reason.clone())
                .with("user", actor.as_ref().map(|a| a.name.clone()).unwrap_or_default())
                .with("user_id", user);
            let mut e = Event::new("policy.rejected", Origin::System, info);
            e.actor = actor;
            e.causal = event.as_ref().map(|c| c.id);
            out.push(Effect::Emit(e));
            return Err(reason);
        }
        let mark = self.cooldowns.commit(&cd_key, &user, now / MS);
        if spec.approval && !role_allows(actor.as_ref(), Role::Mod) {
            return self.hold_commands(id, &label, commands, actor, event, Some((cd_key, user, mark)), now, out);
        }
        let mut e = Event::new("policy.accepted", Origin::System, Value::map().with("type", label).with("user_id", user));
        e.actor = actor.clone();
        e.causal = event.as_ref().map(|c| c.id);
        out.push(Effect::Emit(e));
        out.push(Effect::Run { commands, actor, event });
        Ok(())
    }

    /// Anything for [`Policy::tick`] to do at `now` (cheap; checked every core tick).
    pub fn due(&self, now: Ts) -> bool {
        self.ad.as_ref().is_some_and(|a| now >= a.until) || self.next_deadline.is_some_and(|d| now >= d)
    }

    /// Per-tick work: veto windows end (auto-approve), approvals expire (reject), ad breaks end.
    pub fn tick(&mut self, now: Ts, mode: &str, out: &mut Vec<Effect>) {
        if let Some(a) = &self.ad
            && now >= a.until
        {
            // a manual mode change during the break wins: only return if still in ad_break
            if mode == "ad_break" {
                out.push(Effect::SetMode(a.prev.clone()));
            }
            self.ad = None;
        }
        if self.next_deadline.is_none_or(|d| now < d) {
            return;
        }
        let (due, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending).into_iter().partition(|p| p.deadline <= now);
        self.pending = keep;
        self.next_deadline = self.pending.iter().map(|p| p.deadline).min();
        for p in due {
            match p.kind {
                HoldKind::Veto => self.approve(p, "veto window", mode, out),
                HoldKind::Approval => self.reject(p, "approval expired", "system", false, out),
            }
        }
        out.push(Effect::Pending);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const S: Ts = 1_000 * MS;

    fn viewer(id: &str, roles: Vec<Role>) -> Actor {
        Actor { platform: "twitch".into(), id: id.into(), name: format!("user{id}"), roles }
    }

    fn ev(ty: &str, payload: Value, actor: Option<Actor>) -> Event {
        let mut e = Event::new(ty, Origin::Twitch, payload);
        e.actor = actor;
        e
    }

    fn cfg_with(project: &str, rewards: &[(&str, &str)]) -> Config {
        let mut files = vec![crate::config::SourceFile {
            kind: "project".into(),
            name: "project".into(),
            path: "project.toml".into(),
            table: toml::from_str(project).unwrap(),
        }];
        for (k, src) in rewards {
            files.push(crate::config::SourceFile {
                kind: "rewards".into(),
                name: k.to_string(),
                path: format!("rewards/{k}.toml"),
                table: toml::from_str(src).unwrap(),
            });
        }
        Config::build(&files)
    }

    fn policy(project: &str, rewards: &[(&str, &str)]) -> Policy {
        let mut p = Policy::default();
        let errs = p.configure(&cfg_with(project, rewards));
        assert!(errs.is_empty(), "{errs:?}");
        p
    }

    fn redeem(id: u64, reward: &str, user: &str, input: &str) -> Event {
        let mut e = ev(
            "twitch.redeem",
            Value::map()
                .with("reward", reward)
                .with("reward_id", "rw-1")
                .with("redemption_id", format!("red-{id}"))
                .with("cost", 2000)
                .with("input", input)
                .with("status", "unfulfilled")
                .with("user", user),
            Some(viewer(user, vec![Role::Follower])),
        );
        e.id = id;
        e
    }

    fn names(out: &[Effect]) -> Vec<String> {
        out.iter()
            .map(|e| match e {
                Effect::Deliver(e) => format!("deliver {}", e.ty),
                Effect::Emit(e) => format!("emit {}", e.ty),
                Effect::Run { commands, .. } => format!("run {}", commands.join(";")),
                Effect::Redeem(r) => format!("redeem {}", r.commands.join(";")),
                Effect::Action { name, .. } => format!("action {name}"),
                Effect::SetMode(m) => format!("mode {m}"),
                Effect::Pending => "pending".into(),
            })
            .collect()
    }

    const HYPE: &str = "title = \"HYPE\"\ncost = 2000\ncooldown = \"5m\"\nmax_per_user_per_stream = 3\nfires = \"preset.hype\"\non_reject = \"refund\"";

    #[test]
    fn roles_ladder() {
        assert!(role_allows(None, Role::Everyone));
        assert!(!role_allows(None, Role::Follower));
        assert!(role_allows(Some(&viewer("1", vec![Role::Sub])), Role::Follower));
        assert!(!role_allows(Some(&viewer("1", vec![Role::Sub])), Role::Vip));
        assert!(role_allows(Some(&viewer("1", vec![Role::Follower, Role::Mod])), Role::Vip));
    }

    #[test]
    fn cooldowns_global_per_user_and_rollback() {
        let mut c = Cooldowns::default();
        let spec = CooldownSpec { global_ms: Some(10_000), per_user_ms: Some(60_000) };
        assert_eq!(c.check("k", "a", &spec, 0), Ok(()));
        let m = c.commit("k", "a", 0);
        assert_eq!(c.check("k", "b", &spec, 4_000), Err(6_000), "global applies to everyone");
        assert_eq!(c.check("k", "b", &spec, 10_000), Ok(()));
        assert_eq!(c.check("k", "a", &spec, 10_000), Err(50_000), "per-user outlasts global");
        assert_eq!(c.check("other", "a", &spec, 1), Ok(()), "keys are independent");
        c.rollback("k", "a", m);
        assert_eq!(c.check("k", "a", &spec, 1), Ok(()), "rollback restores the previous state");
    }

    #[test]
    fn display_text_strips_zalgo_controls_and_caps() {
        let zalgo = "h\u{0301}\u{0302}\u{0303}\u{0304}\u{0305}i\u{0306}\u{0307}\u{0308}\u{0309}";
        let d = display_text(zalgo, 300, 2);
        // NFC composes h+◌́ and i+◌̆; two more marks survive on each base, the rest is stripped
        assert_eq!(d, "h\u{301}\u{302}\u{12d}\u{307}\u{308}", "{d:?}");
        assert_eq!(display_text("a\u{202E}b\u{200B}c\n\n  d\u{0007}", 300, 2), "abc d");
        let long = "x".repeat(400);
        let capped = display_text(&long, 300, 2);
        assert_eq!(capped.chars().count(), 301);
        assert!(capped.ends_with('…'));
        assert_eq!(display_text("  lead and trail  ", 300, 2), "lead and trail");
    }

    #[test]
    fn blocklist_sees_through_homoglyphs_leet_spacing_and_stretching() {
        let bl: Vec<String> = ["badword", "spam*", "*scam*", "two words"].iter().map(|t| fold_term(t)).collect();
        for s in [
            "this is a badword",
            "BADWORD!",
            "ｂａｄｗｏｒｄ",
            "bаdwоrd",
            "b4dw0rd",
            "b a d w o r d",
            "b.a.d.w.o.r.d",
            "baaaadwoooord",
            "𝐛𝐚𝐝𝐰𝐨𝐫𝐝",
            "b\u{0301}a\u{0301}dword",
            "spammer here",
            "it's a total scamola",
            "TWO   words",
        ] {
            assert!(blocked_term(s, &bl).is_some(), "should block {s:?}");
        }
        for s in ["good words only", "badwords are fine as a different word", "spa mm", "i am two", "words two"] {
            assert!(blocked_term(s, &bl).is_none(), "should allow {s:?}");
        }
    }

    #[test]
    fn filter_text_contract() {
        let cfg = FilterCfg { blocklist: vec![fold_term("nope")], max_len: 10, max_marks: 1 };
        assert_eq!(filter_text("hello there friend", &cfg), Ok("hello ther…".into()));
        assert_eq!(filter_text("n0pe", &cfg), Err(FilterReason::Blocked("nope".into())));
    }

    #[test]
    fn reward_file_parses_plan_example_and_validates() {
        let t: toml::Table = toml::from_str(HYPE).unwrap();
        let r = RewardDef::parse("hype", &t).unwrap();
        assert_eq!(r.fires, vec!["preset.fire hype".to_string()]);
        assert_eq!(r.cooldown_ms, Some(300_000));
        assert!(r.refund_on_reject && !r.manual_fulfill);
        for bad in [
            "cost = 5",
            "title = \"x\"",
            "title = \"x\"\ncost = 1\ncolor = \"red\"",
            "title = \"x\"\ncost = 1\ntypo = 1",
            "title = \"x\"\ncost = 1\nfires = \"$$\"",
        ] {
            assert!(RewardDef::parse("x", &toml::from_str(bad).unwrap()).is_err(), "{bad}");
        }
    }

    #[test]
    fn managed_redeem_accepted_runs_fires_and_fulfills() {
        let mut p = policy("[policy]\nveto = { redeem = false }", &[("hype", HYPE)]);
        let mut out = Vec::new();
        p.screen(redeem(1, "HYPE", "a", ""), 10 * S, "live", &mut out);
        assert_eq!(names(&out), ["redeem preset.fire hype"]);
        let Some(Effect::Redeem(r)) = out.pop() else { panic!() };
        p.settle(*r, Ok(()), &mut out);
        assert_eq!(names(&out), ["emit policy.accepted", "deliver twitch.redeem", "action twitch.fulfill"]);
    }

    #[test]
    fn redeem_whose_fires_fail_is_refunded_and_gives_its_limits_back() {
        let once = "title = \"Once\"\ncost = 1\nmax_per_stream = 1\ncooldown = \"5m\"\nfires = \"preset.hype\"";
        let mut p = policy("[policy]\nveto = { redeem = false }", &[("once", once)]);
        let mut out = Vec::new();
        p.screen(redeem(1, "Once", "a", ""), S, "live", &mut out);
        let Some(Effect::Redeem(r)) = out.pop() else { panic!("{:?}", names(&out)) };
        // `conflict = "reject"` preset already active: the viewer paid for nothing
        p.settle(*r, Err("preset `hype` is already active".into()), &mut out);
        assert_eq!(names(&out), ["emit policy.rejected", "action twitch.refund"]);
        let Effect::Emit(e) = &out[0] else { panic!() };
        assert_eq!(e.payload.get_path("reason").and_then(Value::as_str), Some("preset `hype` is already active"));
        assert_eq!(e.payload.get_path("refunded"), Some(&Value::Bool(true)));
        let Effect::Action { args, .. } = &out[1] else { panic!() };
        assert_eq!(args.get_path("redemption_id").and_then(Value::as_str), Some("red-1"));
        // the cooldown and the once-per-stream limit it took are given back
        out.clear();
        p.screen(redeem(2, "Once", "b", ""), 2 * S, "live", &mut out);
        assert_eq!(names(&out), ["redeem preset.fire hype"], "limits were released");
        // a reward without fires is fulfilled right away (nothing can fail)
        let mut p = policy("[policy]\nveto = { redeem = false }", &[("hi", "title = \"Hi\"\ncost = 1")]);
        out.clear();
        p.screen(redeem(3, "Hi", "a", ""), S, "live", &mut out);
        assert_eq!(names(&out), ["emit policy.accepted", "deliver twitch.redeem", "action twitch.fulfill"]);
    }

    #[test]
    fn rejected_redeems_are_refunded_with_reasons() {
        let mut p = policy("", &[("hype", HYPE), ("vip", "title = \"VIP only\"\ncost = 10\nrole = \"vip\"\nfires = \"preset.hype\"")]);
        // wrong mode
        let mut out = Vec::new();
        p.screen(redeem(1, "HYPE", "a", ""), S, "brb", &mut out);
        assert_eq!(names(&out), ["emit policy.rejected", "action twitch.refund"]);
        let Effect::Action { args, .. } = &out[1] else { panic!() };
        assert_eq!(args.get_path("redemption_id").unwrap().as_str(), Some("red-1"));
        // accepted, then global cooldown rejects the next viewer
        out.clear();
        p.screen(redeem(2, "HYPE", "a", ""), S, "live", &mut out);
        assert!(names(&out).contains(&"redeem preset.fire hype".to_string()));
        out.clear();
        p.screen(redeem(3, "HYPE", "b", ""), 2 * S, "live", &mut out);
        assert_eq!(names(&out), ["emit policy.rejected", "action twitch.refund"]);
        let Effect::Emit(e) = &out[0] else { panic!() };
        assert!(e.payload.get_path("reason").unwrap().as_str().unwrap().starts_with("cooldown"));
        // role gate
        out.clear();
        p.screen(redeem(4, "VIP only", "c", ""), 3 * S, "live", &mut out);
        let Effect::Emit(e) = &out[0] else { panic!() };
        assert_eq!(e.payload.get_path("reason").unwrap().as_str(), Some("needs role `vip`"));
        // already fulfilled (skip-queue rewards) cannot be refunded
        out.clear();
        let mut r = redeem(5, "HYPE", "d", "");
        r.payload = r.payload.with("status", "fulfilled");
        p.screen(r, 4 * S, "live", &mut out);
        assert_eq!(names(&out), ["emit policy.rejected"]);
        // unmanaged rewards are just delivered
        out.clear();
        let mut r = redeem(6, "Hydrate", "d", "");
        r.payload = r.payload.with("managed", false);
        p.screen(r, 5 * S, "live", &mut out);
        assert_eq!(names(&out), ["deliver twitch.redeem"]);
    }

    #[test]
    fn per_viewer_limit_per_stream_resets_on_stream_online() {
        let one = "title = \"Once\"\ncost = 1\nmax_per_user_per_stream = 1\nfires = \"preset.hype\"";
        let mut p = policy("[policy]\nveto = { redeem = false }", &[("once", one)]);
        let mut out = Vec::new();
        p.screen(redeem(1, "Once", "a", ""), S, "live", &mut out);
        out.clear();
        p.screen(redeem(2, "Once", "a", ""), 2 * S, "live", &mut out);
        assert_eq!(names(&out)[0], "emit policy.rejected");
        out.clear();
        p.screen(ev("twitch.stream.online", Value::map(), None), 3 * S, "live", &mut out);
        out.clear();
        p.screen(redeem(3, "Once", "a", ""), 4 * S, "live", &mut out);
        assert_eq!(names(&out)[0], "redeem preset.fire hype");
    }

    #[test]
    fn approval_queue_approve_reject_expire_and_refund() {
        let appr = "title = \"Pick\"\ncost = 100\napproval = true\nfires = \"preset.hype\"\ncooldown = \"1m\"";
        let mut p = policy("[policy]\napproval_ttl = \"1m\"", &[("pick", appr)]);
        let mut out = Vec::new();
        p.screen(redeem(0x10, "Pick", "a", "hi"), S, "live", &mut out);
        assert_eq!(names(&out), ["emit policy.pending", "pending"]);
        assert_eq!(p.pending_len(), 1);
        out.clear();
        p.resolve("p10", true, "mod1", None, "live", &mut out).unwrap();
        assert_eq!(names(&out), ["emit policy.approved", "redeem preset.fire hype", "pending"]);
        let Effect::Redeem(r) = out.remove(1) else { panic!() };
        out.clear();
        p.settle(*r, Ok(()), &mut out);
        assert_eq!(names(&out), ["emit policy.accepted", "deliver twitch.redeem", "action twitch.fulfill"]);
        let Effect::Emit(e) = &out[0] else { panic!() };
        assert_eq!(e.payload.get_path("approved_by").and_then(Value::as_str), Some("mod1"));
        // second one rejected by a mod → refund and the cooldown is given back
        out.clear();
        p.screen(redeem(0x11, "Pick", "b", ""), 70 * S, "live", &mut out);
        out.clear();
        p.resolve("p11", false, "mod1", Some("not now"), "live", &mut out).unwrap();
        assert_eq!(names(&out), ["emit policy.rejected", "action twitch.refund", "pending"]);
        out.clear();
        p.screen(redeem(0x12, "Pick", "c", ""), 71 * S, "live", &mut out);
        assert_eq!(names(&out), ["emit policy.pending", "pending"], "cooldown was rolled back");
        // expiry rejects + refunds
        out.clear();
        p.tick(71 * S + 60 * S, "live", &mut out);
        assert_eq!(names(&out), ["emit policy.rejected", "action twitch.refund", "pending"]);
        assert_eq!(p.pending_len(), 0);
        assert!(p.resolve("p12", true, "mod1", None, "live", &mut out).is_err());
    }

    #[test]
    fn veto_window_holds_large_alerts_with_text() {
        let mut p = policy("[policy.veto]\nms = \"3s\"\nmin_bits = 500", &[]);
        let mut out = Vec::new();
        let cheer = |id: u64, bits: i64, msg: &str| {
            let mut e = ev("twitch.cheer", Value::map().with("bits", bits).with("message", msg).with("user", "a"), Some(viewer("a", vec![])));
            e.id = id;
            e
        };
        p.screen(cheer(1, 100, "small one"), S, "live", &mut out);
        assert_eq!(names(&out), ["deliver twitch.cheer"]);
        out.clear();
        p.screen(cheer(2, 1000, "Cheer1000"), S, "live", &mut out);
        assert_eq!(names(&out), ["deliver twitch.cheer"], "cheermotes alone are not user text");
        out.clear();
        p.screen(cheer(3, 1000, "Cheer1000 read this out"), S, "live", &mut out);
        assert_eq!(names(&out), ["emit policy.pending", "pending"]);
        // window ends → delivered, marked vetted
        out.clear();
        p.tick(S + 2_999 * MS, "live", &mut out);
        assert!(out.is_empty());
        p.tick(4 * S, "live", &mut out);
        assert_eq!(names(&out), ["emit policy.approved", "deliver twitch.cheer", "pending"]);
        let Effect::Deliver(e) = &out[1] else { panic!() };
        assert_eq!(e.payload.get_path("vetted"), Some(&Value::Bool(true)));
        // mod kills one: delivered without the text
        out.clear();
        p.screen(cheer(4, 1000, "Cheer1000 something awful"), 5 * S, "live", &mut out);
        out.clear();
        p.resolve("p4", false, "mod1", None, "live", &mut out).unwrap();
        assert_eq!(names(&out), ["emit policy.rejected", "deliver twitch.cheer", "pending"]);
        let Effect::Deliver(e) = &out[1] else { panic!() };
        assert_eq!(e.payload.get_path("message").unwrap().as_str(), Some(""));
        assert_eq!(e.payload.get_path("vetoed"), Some(&Value::Bool(true)));
        // mods are never held
        out.clear();
        let mut m = cheer(5, 1000, "mod text");
        m.actor = Some(viewer("m", vec![Role::Mod]));
        p.screen(m, 6 * S, "live", &mut out);
        assert_eq!(names(&out), ["deliver twitch.cheer"]);
    }

    #[test]
    fn chat_filter_automod_and_deletion_sync() {
        let mut p = policy("[policy]\nblocklist = [\"badword\"]\n[policy.veto]\nresub = true", &[]);
        let chat =
            |mid: &str, msg: &str| ev("twitch.chat", Value::map().with("message", msg).with("message_id", mid).with("user", "a"), Some(viewer("a", vec![])));
        let mut out = Vec::new();
        p.screen(chat("m1", "hello"), S, "live", &mut out);
        assert_eq!(names(&out), ["deliver twitch.chat"]);
        out.clear();
        p.screen(chat("m2", "b a d w o r d"), S, "live", &mut out);
        assert_eq!(names(&out), ["emit policy.filtered"]);
        out.clear();
        p.screen(ev("twitch.automod.hold", Value::map().with("message_id", "m3"), None), S, "live", &mut out);
        out.clear();
        p.screen(chat("m3", "held text"), S, "live", &mut out);
        assert_eq!(names(&out), ["emit policy.filtered"]);
        // a held resub from a user who then gets banned is dropped, not delivered
        out.clear();
        let mut resub = ev("twitch.resub", Value::map().with("message", "hi all").with("months", 3).with("user", "x"), Some(viewer("x", vec![Role::Sub])));
        resub.id = 9;
        p.screen(resub, S, "live", &mut out);
        assert_eq!(p.pending_len(), 1);
        out.clear();
        p.screen(ev("twitch.user.purge", Value::map().with("user_id", "x"), None), 2 * S, "live", &mut out);
        assert_eq!(names(&out), ["emit policy.rejected", "pending", "deliver twitch.user.purge"]);
        assert_eq!(p.pending_len(), 0);
    }

    #[test]
    fn ad_break_switches_mode_and_back() {
        let mut p = policy("[policy]\nad_break_mode = true", &[]);
        let mut out = Vec::new();
        p.screen(ev("twitch.ad_break", Value::map().with("duration", 90), None), S, "live", &mut out);
        assert_eq!(names(&out), ["mode ad_break", "deliver twitch.ad_break"]);
        out.clear();
        p.tick(90 * S, "ad_break", &mut out);
        assert!(out.is_empty());
        p.tick(91 * S, "ad_break", &mut out);
        assert_eq!(names(&out), ["mode live"]);
        // offline: no switch; manual change during the break: no switch back
        out.clear();
        p.screen(ev("twitch.ad_break", Value::map().with("duration", 30), None), 100 * S, "offline", &mut out);
        assert_eq!(names(&out), ["deliver twitch.ad_break"]);
        out.clear();
        p.screen(ev("twitch.ad_break", Value::map().with("duration", 30), None), 200 * S, "brb", &mut out);
        out.clear();
        p.tick(240 * S, "live", &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn effects_pause_outside_effect_modes() {
        let p = policy("", &[]);
        let set = Op::Set { address: "fx.x".into(), value: Value::Float(1.0) };
        let say = Op::Action { name: "bot.say".into(), args: Value::Null };
        let cue = Op::Action { name: "lights.cue".into(), args: Value::Null };
        assert!(!p.effect_paused(&set, "live"));
        assert!(p.effect_paused(&set, "brb"));
        assert!(p.effect_paused(&cue, "ad_break"));
        assert!(!p.effect_paused(&say, "brb"));
    }

    #[test]
    fn run_gated_role_cooldown_filter_approval() {
        let mut p = policy("[policy]\nblocklist = [\"nope\"]", &[]);
        let spec = GateSpec { role: Role::Vip, cooldown: CooldownSpec { global_ms: Some(30_000), per_user_ms: None }, approval: false, filter: true };
        let cmds = vec!["preset.fire hype".to_string()];
        let mut out = Vec::new();
        let err = p.run_gated("1".into(), "!hype", &spec, cmds.clone(), Some(viewer("a", vec![])), None, S, &mut out).unwrap_err();
        assert!(err.contains("vip"));
        out.clear();
        p.run_gated("2".into(), "!hype", &spec, cmds.clone(), Some(viewer("v", vec![Role::Vip])), None, S, &mut out).unwrap();
        assert_eq!(names(&out), ["emit policy.accepted", "run preset.fire hype"]);
        out.clear();
        assert!(
            p.run_gated("3".into(), "!hype", &spec, cmds.clone(), Some(viewer("w", vec![Role::Vip])), None, 2 * S, &mut out)
                .unwrap_err()
                .starts_with("cooldown")
        );
        let chat = ev("twitch.chat", Value::map().with("message", "!say n0pe"), Some(viewer("v", vec![Role::Vip])));
        let open = GateSpec { role: Role::Everyone, ..Default::default() };
        let spec_f = GateSpec { filter: true, ..open.clone() };
        assert!(p.run_gated("4".into(), "!say", &spec_f, cmds.clone(), chat.actor.clone(), Some(chat), 3 * S, &mut out).unwrap_err().contains("blocked"));
        let spec_a = GateSpec { approval: true, ..open };
        out.clear();
        p.run_gated("5".into(), "!big", &spec_a, cmds, Some(viewer("a", vec![])), None, 3 * S, &mut out).unwrap();
        assert_eq!(names(&out), ["emit policy.pending", "pending"]);
    }

    #[test]
    fn chat_actions_need_mods_for_moderation() {
        assert!(chat_action_allowed("mod.ban", Some(&viewer("a", vec![Role::Sub]))).is_err());
        assert!(chat_action_allowed("mod.ban", Some(&viewer("a", vec![Role::Mod]))).is_ok());
        assert!(chat_action_allowed("twitch.raid", None).is_err());
        assert!(chat_action_allowed("twitch.chat.send", None).is_ok());
        assert!(chat_action_allowed("queue.request", None).is_ok());
    }

    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            #[test]
            fn display_text_is_bounded_and_idempotent(s in "\\PC{0,600}", max_len in 1usize..400, marks in 0usize..4) {
                let d = display_text(&s, max_len, marks);
                let bases = d.chars().filter(|c| !is_combining_mark(*c)).count();
                prop_assert!(bases <= max_len + 1);
                prop_assert!(!d.chars().any(|c| c.is_control() || is_invisible(c)));
                prop_assert_eq!(display_text(&d, max_len + 1, marks), d.clone());
                let mut run = 0;
                for c in d.chars() {
                    if is_combining_mark(c) { run += 1; prop_assert!(run <= marks); } else { run = 0; }
                }
            }

            #[test]
            fn cooldown_never_admits_two_commits_inside_the_window(times in proptest::collection::vec(0u64..100_000, 1..60), cd in 1u64..20_000) {
                let mut t = times.clone();
                t.sort();
                let spec = CooldownSpec { global_ms: Some(cd), per_user_ms: None };
                let mut c = Cooldowns::default();
                let mut last: Option<u64> = None;
                for now in t {
                    if c.check("k", "u", &spec, now).is_ok() {
                        if let Some(l) = last { prop_assert!(now >= l + cd); }
                        c.commit("k", "u", now);
                        last = Some(now);
                    } else {
                        prop_assert!(last.is_some_and(|l| now < l + cd));
                    }
                }
            }

            #[test]
            fn blocked_words_survive_obfuscation(word in "[a-z]{4,8}", fill in "[ .\\-_]{0,1}", upper in any::<bool>()) {
                let bl = vec![fold_term(&word)];
                let mut s: String = word.chars().map(|c| c.to_string()).collect::<Vec<_>>().join(&fill);
                if upper { s = s.to_uppercase(); }
                let fullwidth: String = word.chars().map(|c| char::from_u32(c as u32 - 'a' as u32 + 0xFF41).unwrap()).collect();
                let text = format!("well {} then", s);
                prop_assert!(blocked_term(&text, &bl).is_some());
                prop_assert!(blocked_term(&fullwidth, &bl).is_some());
            }

            #[test]
            fn refund_iff_rejected_and_refundable(bits_mode in 0usize..4, status_fulfilled in any::<bool>()) {
                let mode = ["live", "brb", "ad_break", "rehearsal"][bits_mode];
                let mut p = Policy::default();
                let t: toml::Table = toml::from_str(HYPE).unwrap();
                p.rewards = vec![RewardDef::parse("hype", &t).unwrap()];
                p.cfg.veto.redeem = false;
                let mut r = redeem(1, "HYPE", "a", "");
                if status_fulfilled { r.payload = r.payload.with("status", "fulfilled"); }
                let mut out = Vec::new();
                p.screen(r, S, mode, &mut out);
                let n = names(&out);
                let accepted = n.contains(&"redeem preset.fire hype".to_string());
                let refunded = n.contains(&"action twitch.refund".to_string());
                prop_assert_eq!(accepted, mode == "live" || mode == "rehearsal");
                prop_assert_eq!(refunded, !accepted && !status_fulfilled);
                prop_assert!(!(accepted && refunded));
            }
        }
    }
}
