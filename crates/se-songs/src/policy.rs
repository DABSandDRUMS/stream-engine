//! Queue policy (§13.2): UI-editable, persisted in the runtime DB. Gates run in two stages:
//! who may request ([`Policy::gate_requester`], before any API cost) and what may be queued
//! ([`Policy::gate_video`] + [`platform_check`], after lookup).

use crate::text::{self, fmt_duration};
use crate::youtube::Video;
use se_proto::{Role, Value};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Approval {
    Off,
    /// Every request from below mod waits for approval.
    All,
    /// Requests from roles below `approval_below` wait for approval.
    Role,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Policy {
    // --- access
    pub open: bool,
    pub min_role: Role,
    /// Minimum follow age in days for requesters below sub (0 = off).
    pub min_follow_days: u32,
    /// Price of a request; 0 = free. Either payment satisfies it.
    pub cost_bits: u32,
    pub cost_points: u32,
    // --- limits
    pub max_queue: u32,
    /// Max requests per user that haven't played yet.
    pub max_per_user: u32,
    pub user_cooldown_s: u32,
    pub max_duration_s: u32,
    /// Minutes before the same video may play again (0 = off).
    pub no_repeat_min: u32,
    // --- content
    pub blocked_videos: Vec<String>,
    /// Channel ids (`UC…`) or channel names.
    pub blocked_channels: Vec<String>,
    pub blocked_artists: Vec<String>,
    pub blocked_keywords: Vec<String>,
    pub explicit_filter: bool,
    /// Only videos already in the library (no API lookups at all).
    pub library_only: bool,
    // --- flow
    pub approval: Approval,
    pub approval_below: Role,
    pub paid_skip_line: bool,
    /// Distinct votes needed to skip the current song (0 = vote-skip off).
    pub voteskip_votes: u32,
    // --- actions
    pub banned_users: Vec<String>,
}

impl Default for Policy {
    fn default() -> Self {
        Policy {
            open: true,
            min_role: Role::Everyone,
            min_follow_days: 0,
            cost_bits: 0,
            cost_points: 0,
            max_queue: 50,
            max_per_user: 3,
            user_cooldown_s: 0,
            max_duration_s: 600,
            no_repeat_min: 60,
            blocked_videos: Vec::new(),
            blocked_channels: Vec::new(),
            blocked_artists: Vec::new(),
            blocked_keywords: Vec::new(),
            explicit_filter: false,
            library_only: false,
            approval: Approval::Off,
            approval_below: Role::Sub,
            paid_skip_line: true,
            voteskip_votes: 0,
            banned_users: Vec::new(),
        }
    }
}

/// Why a request was refused (chat wording lives in [`Reject::reason`]).
#[derive(Clone, Debug, PartialEq)]
pub enum Reject {
    Closed,
    Banned,
    Role(Role),
    FollowAge(u32),
    Cost { bits: u32, points: u32 },
    QueueFull(u32),
    UserLimit(u32),
    Cooldown(u32),
    Empty,
    NotFound,
    NoResults,
    NotEmbeddable,
    Unavailable,
    Live,
    TooLong(u32),
    Region,
    AgeRestricted,
    Explicit,
    Blocked,
    Duplicate(usize),
    Repeat(u32),
    Unplayable(String),
    NotInLibrary,
    SearchPaused,
    LookupsPaused,
    NoKey,
    Lookup(String),
    ModRejected(Option<String>),
}

fn role_name(r: Role) -> &'static str {
    match r {
        Role::Everyone => "everyone",
        Role::Follower => "followers",
        Role::Sub => "subs",
        Role::Vip => "VIPs",
        Role::Mod => "mods",
        Role::Owner => "the streamer",
    }
}

impl Reject {
    /// Short machine code for events/UI.
    pub fn code(&self) -> &'static str {
        match self {
            Reject::Closed => "closed",
            Reject::Banned => "banned",
            Reject::Role(_) => "role",
            Reject::FollowAge(_) => "follow_age",
            Reject::Cost { .. } => "cost",
            Reject::QueueFull(_) => "queue_full",
            Reject::UserLimit(_) => "user_limit",
            Reject::Cooldown(_) => "cooldown",
            Reject::Empty => "empty",
            Reject::NotFound => "not_found",
            Reject::NoResults => "no_results",
            Reject::NotEmbeddable => "not_embeddable",
            Reject::Unavailable => "unavailable",
            Reject::Live => "live",
            Reject::TooLong(_) => "too_long",
            Reject::Region => "region",
            Reject::AgeRestricted => "age_restricted",
            Reject::Explicit => "explicit",
            Reject::Blocked => "blocked",
            Reject::Duplicate(_) => "duplicate",
            Reject::Repeat(_) => "repeat",
            Reject::Unplayable(_) => "unplayable",
            Reject::NotInLibrary => "not_in_library",
            Reject::SearchPaused => "search_paused",
            Reject::LookupsPaused => "lookups_paused",
            Reject::NoKey => "no_key",
            Reject::Lookup(_) => "lookup_failed",
            Reject::ModRejected(_) => "mod_rejected",
        }
    }

    /// Chat wording (completes "@user ✗ …").
    pub fn reason(&self) -> String {
        match self {
            Reject::Closed => "song requests are closed right now".into(),
            Reject::Banned => "you can't request songs here".into(),
            Reject::Role(r) => format!("song requests are for {} and up", role_name(*r)),
            Reject::FollowAge(d) => format!("follow for at least {d} day{} to request songs", if *d == 1 { "" } else { "s" }),
            Reject::Cost { bits, points } => match (*bits, *points) {
                (b, 0) => format!("requests cost {b} bits — cheer with !sr"),
                (0, p) => format!("requests cost {p} channel points — use the song request reward"),
                (b, p) => format!("requests cost {b} bits (cheer with !sr) or {p} channel points (use the reward)"),
            },
            Reject::QueueFull(n) => format!("the queue is full ({n} songs)"),
            Reject::UserLimit(n) => format!("you already have {n} song{} waiting", if *n == 1 { "" } else { "s" }),
            Reject::Cooldown(s) => format!("wait {} before your next request", fmt_duration(*s)),
            Reject::Empty => "usage: !sr <YouTube link or song name>".into(),
            Reject::NotFound => "couldn't find that video".into(),
            Reject::NoResults => "no playable results for that search".into(),
            Reject::NotEmbeddable => "that video can't be played outside YouTube".into(),
            Reject::Unavailable => "that video isn't available".into(),
            Reject::Live => "live streams and premieres can't be requested".into(),
            Reject::TooLong(max) => format!("that's too long (max {})", fmt_duration(*max)),
            Reject::Region => "that video is blocked in our region".into(),
            Reject::AgeRestricted => "that video is age-restricted".into(),
            Reject::Explicit => "explicit songs aren't allowed".into(),
            Reject::Blocked => "that song isn't allowed here".into(),
            Reject::Duplicate(0) => "that's playing right now".into(),
            Reject::Duplicate(p) => format!("that's already in the queue (#{p})"),
            Reject::Repeat(m) => format!("that played recently (no repeats within {m} min)"),
            Reject::Unplayable(why) => format!("that video can't be played here ({why})"),
            Reject::NotInLibrary => "only songs from the library can be requested right now".into(),
            Reject::SearchPaused => "song search is used up for today — paste a YouTube link instead".into(),
            Reject::LookupsPaused => "YouTube lookups are used up for today — only songs already in the library work until midnight Pacific".into(),
            Reject::NoKey => "song lookup isn't set up yet".into(),
            Reject::Lookup(_) => "YouTube lookup failed, try again in a moment".into(),
            Reject::ModRejected(Some(r)) => format!("a mod declined your request ({r})"),
            Reject::ModRejected(None) => "a mod declined your request".into(),
        }
    }
}

/// Who is asking, as far as the policy is concerned.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Requester {
    pub login: String,
    pub display: String,
    pub user_id: Option<String>,
    pub role: Role,
    /// Seconds since following (`None` = unknown / not following).
    pub follow_age_s: Option<i64>,
    pub bits: u32,
    pub points: u32,
    pub redemption_id: Option<String>,
    pub reward_id: Option<String>,
    /// Issued from the UI/CLI/deck by the operator: policy limits don't apply.
    pub operator: bool,
}

impl Requester {
    pub fn is_mod(&self) -> bool {
        self.operator || self.role >= Role::Mod
    }
    pub fn paid(&self) -> bool {
        self.bits > 0 || self.points > 0 || self.redemption_id.is_some()
    }
}

/// Queue facts the requester gate needs.
#[derive(Clone, Debug, Default)]
pub struct QueueFacts {
    pub length: usize,
    pub user_open: usize,
    /// Unix ms of the user's last accepted request.
    pub last_request_ms: Option<i64>,
}

/// Platform checks that apply to everyone (the player simply can't play these).
pub fn platform_check(v: &Video, region: &str) -> Result<(), Reject> {
    if v.privacy == "private" || v.upload_status != "processed" {
        return Err(Reject::Unavailable);
    }
    if v.live != "none" {
        return Err(Reject::Live);
    }
    if !v.embeddable {
        return Err(Reject::NotEmbeddable);
    }
    if v.age_restricted {
        return Err(Reject::AgeRestricted);
    }
    if !region.is_empty() {
        let r = region.to_ascii_uppercase();
        if v.region_blocked.iter().any(|b| b.eq_ignore_ascii_case(&r))
            || v.region_allowed.as_ref().is_some_and(|a| !a.iter().any(|x| x.eq_ignore_ascii_case(&r)))
        {
            return Err(Reject::Region);
        }
    }
    if v.duration_s == 0 {
        return Err(Reject::Live);
    }
    Ok(())
}

const EXPLICIT_MARKERS: &[&str] = &["explicit", "explicit version", "uncensored", "dirty version", "parental advisory", "nsfw"];
const CLEAN_MARKERS: &[&str] = &["clean", "clean version", "radio edit", "censored"];

/// Heuristic explicit detection: rating boards (MPAA R/NC-17, TV-MA) or explicit markers in
/// the title/tags without a clean-version marker. YouTube exposes no explicit-lyrics flag.
pub fn looks_explicit(v: &Video) -> bool {
    if v.mature {
        return true;
    }
    let title = text::fold(&v.title);
    let tags: Vec<String> = v.tags.iter().map(|t| text::fold(t)).collect();
    let has = |set: &[&str]| set.iter().any(|m| text::contains_phrase(&title, m) || tags.iter().any(|t| t == m));
    has(EXPLICIT_MARKERS) && !CLEAN_MARKERS.iter().any(|m| text::contains_phrase(&title, m))
}

impl Policy {
    /// Stage 1: may this person request at all right now? Runs before any API cost.
    pub fn gate_requester(&self, r: &Requester, q: &QueueFacts, now_ms: i64) -> Result<(), Reject> {
        if r.operator {
            return Ok(());
        }
        if self.banned_users.iter().any(|b| b.eq_ignore_ascii_case(&r.login)) {
            return Err(Reject::Banned);
        }
        if r.role >= Role::Mod {
            return Ok(());
        }
        if !self.open {
            return Err(Reject::Closed);
        }
        if r.role < self.min_role {
            return Err(Reject::Role(self.min_role));
        }
        if self.min_follow_days > 0 && r.role < Role::Sub && r.follow_age_s.is_none_or(|a| a < self.min_follow_days as i64 * 86_400) {
            return Err(Reject::FollowAge(self.min_follow_days));
        }
        if (self.cost_bits > 0 || self.cost_points > 0) && !self.payment_ok(r) {
            return Err(Reject::Cost { bits: self.cost_bits, points: self.cost_points });
        }
        if self.max_queue > 0 && q.length >= self.max_queue as usize {
            return Err(Reject::QueueFull(self.max_queue));
        }
        if self.max_per_user > 0 && q.user_open >= self.max_per_user as usize {
            return Err(Reject::UserLimit(self.max_per_user));
        }
        if self.user_cooldown_s > 0
            && let Some(last) = q.last_request_ms
        {
            let wait_ms = self.user_cooldown_s as i64 * 1000 - (now_ms - last);
            if wait_ms > 0 {
                return Err(Reject::Cooldown(((wait_ms + 999) / 1000) as u32));
            }
        }
        Ok(())
    }

    fn payment_ok(&self, r: &Requester) -> bool {
        (self.cost_bits > 0 && r.bits >= self.cost_bits)
            || (self.cost_points > 0 && (r.points >= self.cost_points || r.redemption_id.is_some()))
            // a price in one currency only: the other currency can't satisfy it
            || (self.cost_bits == 0 && self.cost_points == 0)
    }

    /// Stage 2: may this video be queued? (`queued_at` = position if already queued/playing.)
    pub fn gate_video(&self, v: &Video, r: &Requester, queued_at: Option<usize>, last_played_ms: Option<i64>, now_ms: i64) -> Result<(), Reject> {
        if let Some(p) = queued_at {
            return Err(Reject::Duplicate(p));
        }
        if r.operator {
            return Ok(());
        }
        if self.blocked_videos.iter().any(|b| b.trim() == v.id) {
            return Err(Reject::Blocked);
        }
        let channel = text::fold_strict(&v.channel);
        let channel_core = channel.trim_end_matches(" topic").trim_end_matches(" vevo").trim_end_matches("vevo").trim().to_string();
        if self.blocked_channels.iter().any(|b| {
            let b = b.trim();
            b == v.channel_id || (!b.is_empty() && text::fold_strict(b) == channel)
        }) {
            return Err(Reject::Blocked);
        }
        let title = text::fold_strict(&v.title);
        if self
            .blocked_artists
            .iter()
            .map(|a| text::fold_strict(a))
            .any(|a| !a.is_empty() && (a == channel_core || a == channel || text::contains_phrase(&title, &a)))
        {
            return Err(Reject::Blocked);
        }
        let tags: Vec<String> = v.tags.iter().map(|t| text::fold_strict(t)).collect();
        if self
            .blocked_keywords
            .iter()
            .map(|k| text::fold_strict(k))
            .any(|k| !k.is_empty() && (text::contains_phrase(&title, &k) || tags.iter().any(|t| text::contains_phrase(t, &k))))
        {
            return Err(Reject::Blocked);
        }
        if self.explicit_filter && looks_explicit(v) {
            return Err(Reject::Explicit);
        }
        if self.max_duration_s > 0 && v.duration_s > self.max_duration_s && r.role < Role::Mod {
            return Err(Reject::TooLong(self.max_duration_s));
        }
        if self.no_repeat_min > 0
            && let Some(t) = last_played_ms
            && now_ms - t < self.no_repeat_min as i64 * 60_000
        {
            return Err(Reject::Repeat(self.no_repeat_min));
        }
        Ok(())
    }

    pub fn needs_approval(&self, r: &Requester) -> bool {
        if r.is_mod() {
            return false;
        }
        match self.approval {
            Approval::Off => false,
            Approval::All => true,
            Approval::Role => r.role < self.approval_below,
        }
    }

    pub fn skips_line(&self, r: &Requester) -> bool {
        self.paid_skip_line && r.paid()
    }

    /// Apply a partial update from the UI/chat (`{field: value, …}`). Unknown fields and bad
    /// values are errors; nothing is applied unless every field validates.
    pub fn patch(&self, changes: &Value) -> Result<(Policy, Vec<String>), String> {
        let Some(map) = changes.as_map() else { return Err("expected {field: value, …}".into()) };
        let mut js = serde_json::to_value(self).map_err(|e| e.to_string())?;
        let obj = js.as_object_mut().ok_or("policy is not an object")?;
        let mut changed = Vec::new();
        for (k, v) in map {
            let f = FIELDS.iter().find(|f| f.key == k).ok_or_else(|| format!("unknown policy field `{k}`"))?;
            let new = f.coerce(v)?;
            if obj.get(k.as_str()) != Some(&new) {
                changed.push(k.clone());
            }
            obj.insert(k.clone(), new);
        }
        let p: Policy = serde_json::from_value(js).map_err(|e| e.to_string())?;
        Ok((p, changed))
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).map(Value::from).unwrap_or(Value::Null)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bool,
    Int { min: i64, max: i64 },
    Role,
    Approval,
    List { lowercase: bool },
}

/// Field schema for the policy editor.
pub struct Field {
    pub key: &'static str,
    pub group: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    pub unit: &'static str,
}

const ROLES: &[&str] = &["everyone", "follower", "sub", "vip", "mod"];
const APPROVALS: &[&str] = &["off", "all", "role"];

pub const FIELDS: &[Field] = &[
    Field { key: "open", group: "Access", label: "Requests open", kind: Kind::Bool, unit: "" },
    Field { key: "min_role", group: "Access", label: "Minimum role", kind: Kind::Role, unit: "" },
    Field { key: "min_follow_days", group: "Access", label: "Min follow age (below sub)", kind: Kind::Int { min: 0, max: 3650 }, unit: "days" },
    Field { key: "cost_bits", group: "Access", label: "Cost (bits)", kind: Kind::Int { min: 0, max: 1_000_000 }, unit: "bits" },
    Field { key: "cost_points", group: "Access", label: "Cost (channel points)", kind: Kind::Int { min: 0, max: 10_000_000 }, unit: "points" },
    Field { key: "max_queue", group: "Limits", label: "Max queue length", kind: Kind::Int { min: 0, max: 1000 }, unit: "songs" },
    Field { key: "max_per_user", group: "Limits", label: "Max waiting per user", kind: Kind::Int { min: 0, max: 100 }, unit: "songs" },
    Field { key: "user_cooldown_s", group: "Limits", label: "Per-user cooldown", kind: Kind::Int { min: 0, max: 86_400 }, unit: "s" },
    Field { key: "max_duration_s", group: "Limits", label: "Max duration", kind: Kind::Int { min: 0, max: 86_400 }, unit: "s" },
    Field { key: "no_repeat_min", group: "Limits", label: "No-repeat window", kind: Kind::Int { min: 0, max: 10_080 }, unit: "min" },
    Field { key: "blocked_videos", group: "Content", label: "Blocked videos (ids)", kind: Kind::List { lowercase: false }, unit: "" },
    Field { key: "blocked_channels", group: "Content", label: "Blocked channels (id or name)", kind: Kind::List { lowercase: false }, unit: "" },
    Field { key: "blocked_artists", group: "Content", label: "Blocked artists", kind: Kind::List { lowercase: true }, unit: "" },
    Field { key: "blocked_keywords", group: "Content", label: "Blocked keywords", kind: Kind::List { lowercase: true }, unit: "" },
    Field { key: "explicit_filter", group: "Content", label: "Explicit filter", kind: Kind::Bool, unit: "" },
    Field { key: "library_only", group: "Content", label: "Library only", kind: Kind::Bool, unit: "" },
    Field { key: "approval", group: "Flow", label: "Mod approval", kind: Kind::Approval, unit: "" },
    Field { key: "approval_below", group: "Flow", label: "Approve requests below", kind: Kind::Role, unit: "" },
    Field { key: "paid_skip_line", group: "Flow", label: "Paid requests skip the line", kind: Kind::Bool, unit: "" },
    Field { key: "voteskip_votes", group: "Flow", label: "Vote-skip votes (0 = off)", kind: Kind::Int { min: 0, max: 1000 }, unit: "votes" },
    Field { key: "banned_users", group: "Actions", label: "Banned from requests", kind: Kind::List { lowercase: true }, unit: "" },
];

impl Field {
    fn coerce(&self, v: &Value) -> Result<serde_json::Value, String> {
        let bad = || format!("bad value for `{}`: {v}", self.key);
        Ok(match self.kind {
            Kind::Bool => match v {
                Value::Bool(b) => serde_json::Value::Bool(*b),
                Value::Int(i) => serde_json::Value::Bool(*i != 0),
                Value::Str(s) if matches!(s.as_str(), "true" | "on" | "yes" | "1") => serde_json::Value::Bool(true),
                Value::Str(s) if matches!(s.as_str(), "false" | "off" | "no" | "0") => serde_json::Value::Bool(false),
                _ => return Err(bad()),
            },
            Kind::Int { min, max } => {
                let n = match v {
                    Value::Str(s) => {
                        let s = s.trim();
                        s.parse::<i64>().ok().or_else(|| se_proto::parse_duration_ms(s).filter(|_| self.unit == "s").map(|ms| (ms / 1000) as i64))
                    }
                    other => other.as_f64().filter(|f| f.is_finite()).map(|f| f.round() as i64),
                }
                .ok_or_else(bad)?;
                serde_json::Value::from(n.clamp(min, max))
            }
            Kind::Role => {
                let s = v.as_str().ok_or_else(bad)?;
                let r = Role::parse(s.trim()).filter(|r| *r != Role::Owner).ok_or_else(|| format!("`{}` must be one of {}", self.key, ROLES.join(", ")))?;
                serde_json::to_value(r).map_err(|e| e.to_string())?
            }
            Kind::Approval => {
                let s = v.as_str().map(str::trim).ok_or_else(bad)?;
                if !APPROVALS.contains(&s) {
                    return Err(format!("`{}` must be one of {}", self.key, APPROVALS.join(", ")));
                }
                serde_json::Value::String(s.into())
            }
            Kind::List { lowercase } => {
                let items: Vec<String> = match v {
                    Value::List(l) => l.iter().map(|x| x.to_string()).collect(),
                    Value::Str(s) => s.split([',', '\n']).map(String::from).collect(),
                    Value::Null => Vec::new(),
                    _ => return Err(bad()),
                };
                let mut out: Vec<String> = Vec::new();
                for i in items {
                    let mut i = i.trim().trim_start_matches('@').to_string();
                    if lowercase {
                        i = i.to_lowercase();
                    }
                    if !i.is_empty() && !out.contains(&i) {
                        out.push(i);
                    }
                }
                serde_json::Value::from(out)
            }
        })
    }

    pub fn to_value(&self) -> Value {
        let mut v = Value::map().with("key", self.key).with("group", self.group).with("label", self.label).with("unit", self.unit);
        v = match self.kind {
            Kind::Bool => v.with("type", "bool"),
            Kind::Int { min, max } => v.with("type", "int").with("range", Value::List(vec![Value::Int(min), Value::Int(max)])),
            Kind::Role => v.with("type", "enum").with("options", ROLES.iter().map(|s| Value::from(*s)).collect::<Vec<_>>()),
            Kind::Approval => v.with("type", "enum").with("options", APPROVALS.iter().map(|s| Value::from(*s)).collect::<Vec<_>>()),
            Kind::List { .. } => v.with("type", "list"),
        };
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::youtube::parse_videos;

    fn fixture(name: &str) -> Vec<Video> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/youtube").join(name);
        parse_videos(&serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()).unwrap()
    }

    fn video(file: &str, id: &str) -> Video {
        fixture(file).into_iter().find(|v| v.id == id).unwrap()
    }

    fn viewer(role: Role) -> Requester {
        Requester { login: "viewer".into(), display: "Viewer".into(), role, follow_age_s: Some(86_400 * 30), ..Default::default() }
    }

    const NOW: i64 = 1_800_000_000_000;

    #[test]
    fn access_gates_in_order() {
        let mut p = Policy::default();
        let q = QueueFacts::default();
        assert_eq!(p.gate_requester(&viewer(Role::Everyone), &q, NOW), Ok(()));
        p.open = false;
        assert_eq!(p.gate_requester(&viewer(Role::Vip), &q, NOW), Err(Reject::Closed));
        assert_eq!(p.gate_requester(&viewer(Role::Mod), &q, NOW), Ok(()), "mods bypass closed");
        p.open = true;
        p.min_role = Role::Sub;
        assert_eq!(p.gate_requester(&viewer(Role::Follower), &q, NOW), Err(Reject::Role(Role::Sub)));
        assert_eq!(p.gate_requester(&viewer(Role::Sub), &q, NOW), Ok(()));
        p.min_role = Role::Everyone;
        p.min_follow_days = 60;
        assert_eq!(p.gate_requester(&viewer(Role::Follower), &q, NOW), Err(Reject::FollowAge(60)));
        assert_eq!(p.gate_requester(&Requester { follow_age_s: None, ..viewer(Role::Everyone) }, &q, NOW), Err(Reject::FollowAge(60)));
        assert_eq!(p.gate_requester(&viewer(Role::Sub), &q, NOW), Ok(()), "subs are exempt from follow age");
        p.min_follow_days = 0;
        p.banned_users = vec!["viewer".into()];
        assert_eq!(p.gate_requester(&viewer(Role::Mod), &q, NOW), Err(Reject::Banned), "a ban applies to mods too");
        assert_eq!(p.gate_requester(&Requester { operator: true, ..viewer(Role::Everyone) }, &q, NOW), Ok(()));
    }

    #[test]
    fn cost_needs_matching_payment() {
        let p = Policy { cost_points: 500, ..Default::default() };
        let q = QueueFacts::default();
        let free = viewer(Role::Everyone);
        assert_eq!(p.gate_requester(&free, &q, NOW), Err(Reject::Cost { bits: 0, points: 500 }));
        let redeemed = Requester { redemption_id: Some("r".into()), ..free.clone() };
        assert_eq!(p.gate_requester(&redeemed, &q, NOW), Ok(()));
        let cheer = Requester { bits: 1000, ..free.clone() };
        assert_eq!(p.gate_requester(&cheer, &q, NOW), Err(Reject::Cost { bits: 0, points: 500 }), "bits can't pay a points-only price");
        let both = Policy { cost_bits: 100, cost_points: 500, ..Default::default() };
        assert_eq!(both.gate_requester(&Requester { bits: 100, ..free.clone() }, &q, NOW), Ok(()));
        assert_eq!(both.gate_requester(&Requester { bits: 99, ..free }, &q, NOW), Err(Reject::Cost { bits: 100, points: 500 }));
    }

    #[test]
    fn limits_and_cooldown() {
        let p = Policy { max_queue: 2, max_per_user: 1, user_cooldown_s: 120, ..Default::default() };
        let v = viewer(Role::Everyone);
        assert_eq!(p.gate_requester(&v, &QueueFacts { length: 2, ..Default::default() }, NOW), Err(Reject::QueueFull(2)));
        assert_eq!(p.gate_requester(&v, &QueueFacts { user_open: 1, ..Default::default() }, NOW), Err(Reject::UserLimit(1)));
        assert_eq!(p.gate_requester(&v, &QueueFacts { last_request_ms: Some(NOW - 30_500), ..Default::default() }, NOW), Err(Reject::Cooldown(90)));
        assert_eq!(p.gate_requester(&v, &QueueFacts { last_request_ms: Some(NOW - 120_000), ..Default::default() }, NOW), Ok(()));
    }

    #[test]
    fn platform_checks_use_recorded_metadata() {
        let region = "US";
        assert_eq!(platform_check(&video("videos_ok.json", "dQw4w9WgXcQ"), region), Ok(()));
        assert_eq!(platform_check(&video("videos_restricted.json", "NoEmbed0001"), region), Err(Reject::NotEmbeddable));
        assert_eq!(platform_check(&video("videos_restricted.json", "AgeGate0001"), region), Err(Reject::AgeRestricted));
        assert_eq!(platform_check(&video("videos_restricted.json", "RegionBlk01"), region), Err(Reject::Region));
        assert_eq!(platform_check(&video("videos_restricted.json", "RegionBlk01"), "GB"), Ok(()));
        assert_eq!(platform_check(&video("videos_restricted.json", "RegionAlw01"), region), Err(Reject::Region));
        assert_eq!(platform_check(&video("videos_restricted.json", "RegionAlw01"), "jp"), Ok(()));
        assert_eq!(platform_check(&video("videos_restricted.json", "LiveNow0001"), region), Err(Reject::Live));
        assert_eq!(platform_check(&video("videos_restricted.json", "Private0001"), region), Err(Reject::Unavailable));
    }

    #[test]
    fn content_rules() {
        let v = viewer(Role::Everyone);
        let rick = video("videos_ok.json", "dQw4w9WgXcQ");
        let mut p = Policy::default();
        assert_eq!(p.gate_video(&rick, &v, None, None, NOW), Ok(()));
        assert_eq!(p.gate_video(&rick, &v, Some(3), None, NOW), Err(Reject::Duplicate(3)));
        p.blocked_videos = vec!["dQw4w9WgXcQ".into()];
        assert_eq!(p.gate_video(&rick, &v, None, None, NOW), Err(Reject::Blocked));
        p.blocked_videos.clear();
        p.blocked_channels = vec!["UCuAXFkgsw1L7xaCfnd5JJOw".into()];
        assert_eq!(p.gate_video(&rick, &v, None, None, NOW), Err(Reject::Blocked));
        p.blocked_channels = vec!["RICK ASTLEY".into()];
        assert_eq!(p.gate_video(&rick, &v, None, None, NOW), Err(Reject::Blocked));
        p.blocked_channels.clear();
        p.blocked_artists = vec!["Rіck Astley".into()]; // Cyrillic і in the blocklist entry
        assert_eq!(p.gate_video(&rick, &v, None, None, NOW), Err(Reject::Blocked));
        p.blocked_artists.clear();
        p.blocked_keywords = vec!["rick rolled".into()]; // matches a tag
        assert_eq!(p.gate_video(&rick, &v, None, None, NOW), Err(Reject::Blocked));
        p.blocked_keywords = vec!["ass".into()];
        assert_eq!(p.gate_video(&rick, &v, None, None, NOW), Ok(()), "keyword must match whole words");
        p.blocked_keywords.clear();
        p.no_repeat_min = 60;
        assert_eq!(p.gate_video(&rick, &v, None, Some(NOW - 59 * 60_000), NOW), Err(Reject::Repeat(60)));
        assert_eq!(p.gate_video(&rick, &v, None, Some(NOW - 61 * 60_000), NOW), Ok(()));
        let long = video("videos_policy.json", "LongSong001");
        assert_eq!(p.gate_video(&long, &v, None, None, NOW), Err(Reject::TooLong(600)));
        assert_eq!(p.gate_video(&long, &viewer(Role::Mod), None, None, NOW), Ok(()));
        p.explicit_filter = true;
        assert_eq!(p.gate_video(&video("videos_policy.json", "Explicit001"), &v, None, None, NOW), Err(Reject::Explicit));
        assert_eq!(p.gate_video(&video("videos_policy.json", "CleanVer001"), &v, None, None, NOW), Ok(()));
        assert_eq!(p.gate_video(&video("videos_restricted.json", "MatureR0001"), &v, None, None, NOW), Err(Reject::Explicit));
    }

    #[test]
    fn approval_modes() {
        let mut p = Policy { approval: Approval::All, ..Default::default() };
        assert!(p.needs_approval(&viewer(Role::Vip)));
        assert!(!p.needs_approval(&viewer(Role::Mod)));
        p.approval = Approval::Role;
        p.approval_below = Role::Sub;
        assert!(p.needs_approval(&viewer(Role::Follower)));
        assert!(!p.needs_approval(&viewer(Role::Sub)));
        p.approval = Approval::Off;
        assert!(!p.needs_approval(&viewer(Role::Everyone)));
        assert!(p.skips_line(&Requester { points: 1, ..viewer(Role::Everyone) }));
    }

    #[test]
    fn patch_validates_everything_or_nothing() {
        let p = Policy::default();
        let (n, changed) = p
            .patch(
                &Value::map()
                    .with("max_queue", 20)
                    .with("min_role", "vip")
                    .with("blocked_keywords", "Foo, bar ,foo")
                    .with("user_cooldown_s", "2m")
                    .with("approval", "role"),
            )
            .unwrap();
        assert_eq!(n.max_queue, 20);
        assert_eq!(n.min_role, Role::Vip);
        assert_eq!(n.blocked_keywords, vec!["foo".to_string(), "bar".to_string()]);
        assert_eq!(n.user_cooldown_s, 120);
        assert_eq!(n.approval, Approval::Role);
        assert_eq!(changed.len(), 5);
        assert!(p.patch(&Value::map().with("max_queue", 5).with("nope", 1)).is_err());
        assert!(p.patch(&Value::map().with("min_role", "owner")).is_err());
        assert!(p.patch(&Value::map().with("approval", "sometimes")).is_err());
        assert_eq!(p.patch(&Value::map().with("max_queue", -5)).unwrap().0.max_queue, 0);
        // round-trips through JSON (DB persistence)
        let back: Policy = serde_json::from_value(serde_json::to_value(&n).unwrap()).unwrap();
        assert_eq!(back, n);
        assert_eq!(FIELDS.len(), serde_json::to_value(Policy::default()).unwrap().as_object().unwrap().len());
    }
}
