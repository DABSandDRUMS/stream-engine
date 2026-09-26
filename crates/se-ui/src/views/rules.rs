//! Automation → Events (§15.5): triggers that answer stream, music and show events. A list
//! pane (grouped by where the event comes from: viewers, stream, music & drums, show, controls)
//! and the selected trigger: WHEN (event chips), ONLY IF (checks, modes, scenes, cooldowns) and
//! DO (guided steps that edit the same command lines the engine runs). The exact text and the
//! file sit under "File". Validation uses the engine's own parsers, "Test" plays the event
//! through the simulator, and saving goes through `project.write` into `rules/*.toml` (comments
//! in the file are kept). [`steps_editor`] is shared with saved actions and scenes.

use crate::app::{App, ViewId};
use crate::views::live::nice;
use egui::{Align, Color32, CornerRadius, FontId, Layout, RichText};
use se_proto::command::tokenize;
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_medium, font_mono, font_semibold, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::sync::Arc;

/// Simulator preset for an event type (se-core `sim.rs`), with default args.
pub const SIM_FOR: &[(&str, &str)] = &[
    ("twitch.cheer", "cheer"),
    ("twitch.sub", "sub"),
    ("twitch.resub", "resub"),
    ("twitch.gift", "gift_bomb"),
    ("twitch.follow", "follow"),
    ("twitch.raid", "raid"),
    ("twitch.redeem", "redeem"),
    ("twitch.chat", "chat"),
    ("tip", "tip"),
    ("twitch.ad_break", "ad_break"),
    ("twitch.hype_train.begin", "hype_train"),
];

/// Payload fields per event type (the normalized Twitch/tip contract, se-core `sim.rs`, plus the
/// band onsets and Stream Deck keys).
pub const FIELDS: &[(&str, &[&str])] = &[
    ("twitch.cheer", &["bits", "message", "user"]),
    ("twitch.sub", &["tier", "months", "is_gift", "user"]),
    ("twitch.resub", &["tier", "months", "message", "user"]),
    ("twitch.gift", &["count", "tier", "user"]),
    ("twitch.follow", &["user"]),
    ("twitch.raid", &["viewers", "from", "user"]),
    ("twitch.redeem", &["reward", "cost", "input", "user"]),
    ("twitch.chat", &["message", "message_id", "user"]),
    ("tip", &["amount", "currency", "message", "user"]),
    ("twitch.ad_break", &["duration", "automatic"]),
    ("twitch.hype_train.begin", &["level"]),
    ("band.kick", &["velocity"]),
    ("band.snare", &["velocity"]),
    ("deck.key", &["key", "page", "down"]),
];

const VERBS: &[&str] = &[
    "preset.fire",
    "preset.release",
    "scene.go",
    "scene.cut",
    "scene.take",
    "set",
    "animate",
    "trigger",
    "release",
    "toggle",
    "mode.set",
    "emit",
    "wait",
    "bot.say",
    "lights.cue",
    "lights.release",
    "audio.play",
    "audio.duck",
    "mixer.snapshot.recall",
    "obs.stream.start",
    "obs.stream.stop",
    "twitch.marker",
    "queue.skip",
    "timeline.play",
    "timeline.stop",
    "clean",
    "panic",
];

// ---- words: events -------------------------------------------------------------------------------

/// Trigger families: how the Events list and the "When" picker are grouped.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
enum Family {
    Viewers,
    Stream,
    Music,
    Show,
    Controls,
    Other,
}

impl Family {
    const PICKER: [Family; 5] = [Family::Viewers, Family::Stream, Family::Music, Family::Show, Family::Controls];

    fn title(self) -> &'static str {
        match self {
            Family::Viewers => "Viewers",
            Family::Stream => "Stream",
            Family::Music => "Music & drums",
            Family::Show => "Show",
            Family::Controls => "Controls",
            Family::Other => "Other",
        }
    }

    /// The family of an event pattern (the picker's own events first, then by name).
    fn of(when: &str) -> Family {
        if let Some(i) = trigger_index(when) {
            return TRIGGERS[i].family;
        }
        let head = when.split('.').next().unwrap_or("");
        let stream = ["twitch.ad_break", "twitch.stream", "twitch.*", "goal.", "alert.", "obs."];
        if stream.iter().any(|p| when.starts_with(p)) {
            Family::Stream
        } else if matches!(head, "twitch" | "tip" | "tiktok" | "youtube") {
            Family::Viewers
        } else if matches!(head, "band" | "beat" | "music" | "drums" | "queue" | "mic")
            || [".kick", ".snare", ".hat", ".drop", ".section"].iter().any(|s| when.ends_with(s))
        {
            Family::Music
        } else if matches!(head, "mode" | "scene" | "timeline" | "preset" | "show") {
            Family::Show
        } else if matches!(head, "deck" | "midi" | "osc" | "voice") {
            Family::Controls
        } else {
            Family::Other
        }
    }
}

/// A friendly event the WHEN picker offers.
struct Trigger {
    pattern: &'static str,
    label: &'static str,
    /// Completes "When …".
    sentence: &'static str,
    icon: &'static str,
    family: Family,
}

const fn trig(pattern: &'static str, label: &'static str, sentence: &'static str, icon: &'static str, family: Family) -> Trigger {
    Trigger { pattern, label, sentence, icon, family }
}

const MODE_START: &str = "mode.enter.*";
const MODE_END: &str = "mode.exit.*";

const TRIGGERS: &[Trigger] = &[
    trig("twitch.follow", "Follow", "someone follows", icon::HEART, Family::Viewers),
    trig("twitch.sub", "Sub", "someone subscribes", icon::STAR, Family::Viewers),
    trig("twitch.resub", "Resub", "someone resubscribes", icon::STAR, Family::Viewers),
    trig("twitch.gift", "Gifted subs", "someone gifts subs", icon::GIFT, Family::Viewers),
    trig("twitch.cheer", "Cheer (bits)", "someone cheers", icon::SPARKLE, Family::Viewers),
    trig("twitch.raid", "Raid", "someone raids", icon::USERS, Family::Viewers),
    trig("twitch.redeem", "Channel points", "someone redeems channel points", icon::GIFT, Family::Viewers),
    trig("twitch.chat", "Chat message", "someone chats", icon::CHAT, Family::Viewers),
    trig("tip", "Tip", "someone tips", icon::HEART, Family::Viewers),
    trig("twitch.hype_train.begin", "Hype train", "a hype train starts", icon::ROCKET, Family::Viewers),
    trig("twitch.ad_break", "Ad break", "an ad break starts", icon::CLOCK, Family::Stream),
    trig("goal.reached", "Goal reached", "a goal is reached", icon::TROPHY, Family::Stream),
    trig("twitch.*", "Anything on Twitch", "anything happens on Twitch", icon::TWITCH, Family::Stream),
    trig("band.kick", "Kick drum hit", "the kick drum hits", icon::DRUM, Family::Music),
    trig("band.snare", "Snare hit", "the snare hits", icon::DRUM, Family::Music),
    trig("band.drop", "Music drop", "the music drops", icon::MUSIC, Family::Music),
    trig("beat", "Every beat", "a beat lands", icon::MUSIC, Family::Music),
    trig("queue.song_started", "Song request starts", "a requested song starts", icon::QUEUE, Family::Music),
    trig(MODE_START, "Show mode starts", "any show mode starts", icon::PLAY, Family::Show),
    trig(MODE_END, "Show mode ends", "any show mode ends", icon::STOP, Family::Show),
    trig("timeline.cue", "Timeline moment", "a timeline moment passes", icon::TIMELINE, Family::Show),
    trig("deck.key", "Stream Deck button", "a Stream Deck button is pressed", icon::KEYBOARD, Family::Controls),
];

/// The events the "When" picker offers: (event pattern, label, words after "When …", icon).
pub fn event_choices() -> impl Iterator<Item = (&'static str, &'static str, &'static str, &'static str)> {
    TRIGGERS.iter().map(|t| (t.pattern, t.label, t.sentence, t.icon))
}

fn trigger_index(when: &str) -> Option<usize> {
    let key = if when.starts_with("mode.enter.") {
        MODE_START
    } else if when.starts_with("mode.exit.") {
        MODE_END
    } else {
        when
    };
    TRIGGERS.iter().position(|t| t.pattern == key)
}

/// The icon of an event pattern (the picker's, else its family's).
fn event_icon(when: &str) -> &'static str {
    match trigger_index(when) {
        Some(i) => TRIGGERS[i].icon,
        None => match Family::of(when) {
            Family::Viewers => icon::USERS,
            Family::Stream => icon::LIVE,
            Family::Music => icon::MUSIC,
            Family::Show => icon::PLAY,
            Family::Controls => icon::CONTROLLER,
            Family::Other => icon::BOLT,
        },
    }
}

/// Words kept in capitals when a name is shown (`brb` → `BRB`, `fx` → `FX`).
const ACRONYMS: [&str; 10] = ["brb", "afk", "fx", "dj", "mc", "vhs", "rgb", "pov", "obs", "hdmi"];

/// A name as the streamer reads it everywhere: `ad_break` → `Ad break`, `SUB BIG` → `Sub big`,
/// `brb` → `BRB`, `kit` → `Kit`. Mixed-case labels keep their own casing.
pub fn nice_name(s: &str) -> String {
    let s = s.trim();
    let base = if s.chars().any(char::is_alphabetic) && !s.chars().any(char::is_lowercase) { s.to_lowercase() } else { s.to_string() };
    nice(&base)
        .split(' ')
        .map(|w| {
            let core: String = w.chars().filter(|c| c.is_alphanumeric()).collect();
            if ACRONYMS.contains(&core.to_lowercase().as_str()) { w.to_uppercase() } else { w.to_string() }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

fn pretty(s: &str) -> String {
    nice_name(s)
}

/// A user label for sentences: the label when there is one, else the name.
fn display_label(name: &str, label: &str) -> String {
    if label.trim().is_empty() { nice_name(name) } else { nice_name(label) }
}

/// Chat templates as words: `Thanks {user} for {bits} bits` → `Thanks (their name) for (bits) bits`.
pub fn friendly_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(open) = rest.find('{') {
        let Some(close) = rest[open..].find('}').map(|c| open + c) else { break };
        out.push_str(&rest[..open]);
        // already in brackets ("(… {x} …)"): don't add another pair
        let inside = out.matches('(').count() > out.matches(')').count();
        let words = placeholder_words(&rest[open + 1..close]);
        if inside {
            out.push_str(&words);
        } else {
            out.push('(');
            out.push_str(&words);
            out.push(')');
        }
        rest = &rest[close + 1..];
    }
    out.push_str(rest);
    out
}

fn placeholder_words(p: &str) -> String {
    if let Some(choices) = p.strip_prefix("random:") {
        let c: Vec<&str> = choices.split('|').map(str::trim).filter(|x| !x.is_empty()).collect();
        return match c.as_slice() {
            [] => "something random".into(),
            [one] => (*one).to_string(),
            [init @ .., last] => format!("{} or {last}", init.join(", ")),
        };
    }
    let known = match p {
        "user" | "name" | "actor" => "their name",
        "touser" => "who they mention",
        "args" | "input" => "what they typed",
        "count" => "the count",
        "uptime" => "time on air",
        "song" => "current song",
        "bits" | "viewers" => "how many",
        "from" | "from_login" => "raider",
        "months" => "how many months",
        "tier" => "tier",
        "amount" => "amount",
        "currency" => "currency",
        "reward" => "reward",
        "message" => "their message",
        "goals.subs.current" => "subs so far",
        "goals.subs.target" => "sub goal",
        "queue.now.user" => "who asked for it",
        "queue.length" => "how many",
        "queue.url" => "song list link",
        _ => "",
    };
    if !known.is_empty() {
        return known.into();
    }
    if let Ok(n) = p.parse::<u32>() {
        return format!("word {n}");
    }
    let words: Vec<&str> = p.split(['.', '_']).filter(|w| !w.is_empty()).collect();
    let tail = if words.len() > 2 { &words[words.len() - 2..] } else { &words[..] };
    tail.join(" ").to_lowercase()
}

/// "while you're live", "during BRB" (show modes inside sentences).
fn mode_while(m: &str) -> String {
    match m {
        "live" => "while you're live".into(),
        "offline" => "while you're off air".into(),
        m => format!("during {}", pretty(m)),
    }
}

/// Completes "When …" for an event pattern.
pub fn when_sentence(when: &str) -> String {
    let when = when.trim();
    if when.is_empty() {
        return "…".into();
    }
    if let Some(m) = when.strip_prefix("mode.enter.") {
        return match m {
            "*" | "" => "any show mode starts".into(),
            "live" => "you go live".into(),
            "offline" => "you go off air".into(),
            m => format!("you switch to {}", pretty(m)),
        };
    }
    if let Some(m) = when.strip_prefix("mode.exit.") {
        return match m {
            "*" | "" => "any show mode ends".into(),
            "live" => "you stop being live".into(),
            m => format!("you leave {}", pretty(m)),
        };
    }
    match TRIGGERS.iter().find(|x| x.pattern == when) {
        Some(x) => x.sentence.into(),
        None => format!("“{}” happens", nice(&when.replace(['.', '*'], " ").split_whitespace().collect::<Vec<_>>().join(" "))),
    }
}

// ---- words: conditions ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum FieldKind {
    Num,
    /// 0–1 hit strength, shown as words ("hits hard") and a slider.
    Strength,
    Text,
    Bool,
    Mode,
}

/// Event field → (label, kind, noun used in sentences like "1000+ bits").
const FIELD_INFO: &[(&str, &str, FieldKind, &str)] = &[
    ("bits", "Bits", FieldKind::Num, "bits"),
    ("viewers", "Raid size", FieldKind::Num, "viewers"),
    ("tier", "Sub tier", FieldKind::Num, ""),
    ("months", "Months", FieldKind::Num, "months"),
    ("count", "Number of gifts", FieldKind::Num, "gifts"),
    ("amount", "Tip amount", FieldKind::Num, ""),
    ("cost", "Points spent", FieldKind::Num, "points"),
    ("velocity", "How hard", FieldKind::Strength, ""),
    ("level", "Hype level", FieldKind::Num, ""),
    ("duration", "Ad length (seconds)", FieldKind::Num, ""),
    ("key", "Button number", FieldKind::Num, ""),
    ("reward", "Reward", FieldKind::Text, ""),
    ("message", "Message", FieldKind::Text, ""),
    ("input", "Viewer's text", FieldKind::Text, ""),
    ("user", "Viewer", FieldKind::Text, ""),
    ("from", "Raider", FieldKind::Text, ""),
    ("currency", "Currency", FieldKind::Text, ""),
    ("page", "Deck page", FieldKind::Text, ""),
    ("is_gift", "It's a gift", FieldKind::Bool, ""),
    ("automatic", "Automatic ad", FieldKind::Bool, ""),
    ("down", "Pressed down", FieldKind::Bool, ""),
    ("mode", "Show mode", FieldKind::Mode, ""),
];

fn field_info(f: &str) -> (String, Option<FieldKind>, &'static str) {
    match FIELD_INFO.iter().find(|(n, ..)| *n == f) {
        Some((_, label, kind, noun)) => (label.to_string(), Some(*kind), noun),
        None => (nice(f), None, ""),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CondOp {
    Ge,
    Gt,
    Le,
    Lt,
    Eq,
    Ne,
    Yes,
    No,
}

impl CondOp {
    fn sym(self) -> &'static str {
        match self {
            CondOp::Ge => ">=",
            CondOp::Gt => ">",
            CondOp::Le => "<=",
            CondOp::Lt => "<",
            CondOp::Eq | CondOp::Yes => "==",
            CondOp::Ne | CondOp::No => "!=",
        }
    }
}

const NUM_OPS: [(CondOp, &str); 6] =
    [(CondOp::Ge, "at least"), (CondOp::Gt, "more than"), (CondOp::Le, "at most"), (CondOp::Lt, "less than"), (CondOp::Eq, "exactly"), (CondOp::Ne, "not")];
const TEXT_OPS: [(CondOp, &str); 2] = [(CondOp::Eq, "is"), (CondOp::Ne, "is not")];

/// One simple "only if" check: `event.<field> <op> <value>` or `mode == '<mode>'`.
#[derive(Clone, Debug, PartialEq)]
struct Cond {
    field: String,
    kind: FieldKind,
    op: CondOp,
    value: String,
}

impl Cond {
    fn new(field: &str) -> Cond {
        let kind = if field == "mode" { FieldKind::Mode } else { field_info(field).1.unwrap_or(FieldKind::Num) };
        let op = match kind {
            FieldKind::Num | FieldKind::Strength => CondOp::Ge,
            FieldKind::Bool => CondOp::Yes,
            _ => CondOp::Eq,
        };
        let value = if kind == FieldKind::Strength { "0.5".to_string() } else { String::new() };
        Cond { field: field.into(), kind, op, value }
    }

    /// Expression text, `None` while the row is unfinished.
    fn text(&self) -> Option<String> {
        let lhs = if self.field == "mode" { "mode".to_string() } else { format!("event.{}", self.field) };
        match self.op {
            CondOp::Yes => Some(lhs),
            CondOp::No => Some(format!("!{lhs}")),
            op => {
                let v = self.value.trim();
                if v.is_empty() {
                    return None;
                }
                let rhs = if matches!(self.kind, FieldKind::Num | FieldKind::Strength) && v.parse::<f64>().is_ok() {
                    v.to_string()
                } else if v.contains('\'') {
                    format!("\"{v}\"")
                } else {
                    format!("'{v}'")
                };
                Some(format!("{lhs} {} {rhs}", op.sym()))
            }
        }
    }

    /// Words for sentences ("1000+ bits", "tier 2+", "not a gift", "while Live").
    fn phrase(&self) -> String {
        let (label, _, noun) = field_info(&self.field);
        let low = label.to_lowercase();
        let v = self.value.trim();
        match self.kind {
            FieldKind::Strength => {
                let x: f64 = v.parse().unwrap_or(0.0);
                match self.op {
                    CondOp::Ge | CondOp::Gt if x >= 0.75 => "very hard".into(),
                    CondOp::Ge | CondOp::Gt if x >= 0.35 => "hard".into(),
                    CondOp::Ge | CondOp::Gt => "at all".into(),
                    CondOp::Le | CondOp::Lt if x <= 0.35 => "very softly".into(),
                    CondOp::Le | CondOp::Lt => "softly".into(),
                    _ => format!("at {:.0}% strength", x * 100.0),
                }
            }
            FieldKind::Mode if self.op == CondOp::Ne => format!("except {}", mode_while(v)),
            FieldKind::Mode => mode_while(v),
            FieldKind::Bool => {
                let (yes, no) = match self.field.as_str() {
                    "is_gift" => ("a gift".to_string(), "not a gift".to_string()),
                    "automatic" => ("automatic".into(), "not automatic".into()),
                    "down" => ("pressed".into(), "released".into()),
                    _ => (low.clone(), format!("not {low}")),
                };
                if self.op == CondOp::No { no } else { yes }
            }
            FieldKind::Text if self.op == CondOp::Ne => format!("{low} not “{v}”"),
            FieldKind::Text => format!("{low} “{v}”"),
            FieldKind::Num if self.field == "tier" => match self.op {
                CondOp::Ge => format!("tier {v}+"),
                CondOp::Gt => format!("tier above {v}"),
                CondOp::Le => format!("tier {v} or lower"),
                CondOp::Lt => format!("tier below {v}"),
                CondOp::Ne => format!("not tier {v}"),
                _ => format!("tier {v}"),
            },
            FieldKind::Num if !noun.is_empty() => match self.op {
                CondOp::Ge => format!("{v}+ {noun}"),
                CondOp::Gt => format!("over {v} {noun}"),
                CondOp::Le => format!("at most {v} {noun}"),
                CondOp::Lt => format!("under {v} {noun}"),
                CondOp::Ne => format!("not {v} {noun}"),
                _ => format!("exactly {v} {noun}"),
            },
            FieldKind::Num => {
                let word = NUM_OPS.iter().find(|(o, _)| *o == self.op).map_or("of", |(_, w)| *w);
                format!("{low} {word} {v}")
            }
        }
    }
}

fn lhs_field(s: &str) -> Option<String> {
    let s = s.trim();
    if s == "mode" {
        return Some("mode".into());
    }
    let f = s.strip_prefix("event.")?;
    (!f.is_empty() && f.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then(|| f.to_string())
}

fn unquote(s: &str) -> Option<String> {
    let s = s.trim();
    ['\'', '"'].into_iter().find_map(|q| {
        let inner = s.strip_prefix(q)?.strip_suffix(q)?;
        (!inner.contains(q)).then(|| inner.to_string())
    })
}

fn parse_cond(p: &str) -> Option<Cond> {
    if let Some(rest) = p.strip_prefix('!') {
        let f = lhs_field(rest)?;
        return (f != "mode").then(|| Cond { field: f, kind: FieldKind::Bool, op: CondOp::No, value: String::new() });
    }
    for (sym, op) in [(">=", CondOp::Ge), ("<=", CondOp::Le), ("==", CondOp::Eq), ("!=", CondOp::Ne), (">", CondOp::Gt), ("<", CondOp::Lt)] {
        let Some((l, r)) = p.split_once(sym) else { continue };
        let field = lhs_field(l)?;
        let r = r.trim();
        if let Some(text) = unquote(r) {
            let kind = if field == "mode" { FieldKind::Mode } else { FieldKind::Text };
            return matches!(op, CondOp::Eq | CondOp::Ne).then_some(Cond { field, kind, op, value: text });
        }
        if (r == "true" || r == "false") && field != "mode" && matches!(op, CondOp::Eq | CondOp::Ne) {
            let yes = (r == "true") == (op == CondOp::Eq);
            return Some(Cond { field, kind: FieldKind::Bool, op: if yes { CondOp::Yes } else { CondOp::No }, value: String::new() });
        }
        let kind = if field_info(&field).1 == Some(FieldKind::Strength) { FieldKind::Strength } else { FieldKind::Num };
        return (field != "mode" && r.parse::<f64>().is_ok()).then(|| Cond { field, kind, op, value: r.into() });
    }
    let f = lhs_field(p)?;
    (f != "mode").then(|| Cond { field: f, kind: FieldKind::Bool, op: CondOp::Yes, value: String::new() })
}

/// The simple checks of an "only if" expression; `None` when it needs the raw editor.
fn parse_conds(src: &str) -> Option<Vec<Cond>> {
    let src = src.trim();
    if src.is_empty() {
        return Some(Vec::new());
    }
    if src.contains("||") || src.contains('(') {
        return None;
    }
    src.split("&&").map(|p| parse_cond(p.trim())).collect()
}

fn conds_text(rows: &[Cond]) -> String {
    rows.iter().filter_map(Cond::text).collect::<Vec<_>>().join(" && ")
}

/// Event fields offered for `when` (deduplicated, technical ids left out).
fn fields_for(when: &str) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = Vec::new();
    if when.trim().is_empty() {
        return out;
    }
    for (ty, fields) in FIELDS {
        if se_proto::address::matches(when, ty) {
            for f in fields.iter().copied().filter(|f| *f != "message_id") {
                if !out.contains(&f) {
                    out.push(f);
                }
            }
        }
    }
    out
}

/// "When someone cheers with 1000+ bits, while Live".
fn when_phrase(when: &str, cond: &str) -> String {
    let mut s = format!("When {}", when_sentence(when));
    match parse_conds(cond) {
        Some(rows) => {
            let done: Vec<&Cond> = rows.iter().filter(|c| c.text().is_some()).collect();
            for c in done.iter().filter(|c| c.kind == FieldKind::Strength) {
                s.push(' ');
                s.push_str(&c.phrase());
            }
            let ev: Vec<String> = done.iter().filter(|c| !matches!(c.kind, FieldKind::Mode | FieldKind::Strength)).map(|c| c.phrase()).collect();
            let md: Vec<String> = done.iter().filter(|c| c.kind == FieldKind::Mode).map(|c| c.phrase()).collect();
            if !ev.is_empty() {
                s.push_str(" with ");
                s.push_str(&ev.join(", "));
            }
            if !md.is_empty() {
                s.push_str(if ev.is_empty() { " " } else { ", " });
                s.push_str(&md.join(", "));
            }
        }
        None => s.push_str(", if a custom check passes"),
    }
    s
}

// ---- words: steps --------------------------------------------------------------------------------

/// What one DO step does, as the guided editor offers it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Step {
    Preset,
    Release,
    Scene,
    Preview,
    LayerFx,
    LayerVisible,
    Setting,
    Animation,
    Notify,
    Say,
    Sound,
    Mix,
    Lights,
    Look,
    Mode,
    TimelinePlay,
    TimelineStop,
    Marker,
    Wait,
    Custom,
}

/// Placeholder address of a layer step nothing has been picked for yet (empty segments).
const BLANK_FX: &str = "scene..node..fx..enabled";
const BLANK_LAYER: &str = "scene..node..visible";

impl Step {
    /// The step picker, in groups (a line between groups).
    const MENU: [&'static [Step]; 6] = [
        &[Step::Preset, Step::Release],
        &[Step::Scene, Step::Preview, Step::LayerFx, Step::LayerVisible, Step::Setting, Step::Animation],
        &[Step::Notify, Step::Say, Step::Sound, Step::Mix],
        &[Step::Lights, Step::Look],
        &[Step::Mode, Step::TimelinePlay, Step::TimelineStop, Step::Marker, Step::Wait],
        &[Step::Custom],
    ];

    /// The step a command's first word stands for, when that alone decides it.
    fn from_verb(verb: &str) -> Option<Step> {
        Some(match verb {
            "preset.fire" => Step::Preset,
            "preset.release" => Step::Release,
            "scene.cut" => Step::Scene,
            "scene.go" => Step::Preview,
            "bot.say" => Step::Say,
            "audio.play" => Step::Sound,
            "mixer.snapshot.recall" => Step::Mix,
            // a bare `lights.cue` is a cue list; `look=` is read before this
            "lights.cue" => Step::Lights,
            "mode.set" => Step::Mode,
            "wait" => Step::Wait,
            "twitch.marker" => Step::Marker,
            "timeline.play" => Step::TimelinePlay,
            "timeline.stop" => Step::TimelineStop,
            _ => return None,
        })
    }

    fn verb(self) -> &'static str {
        match self {
            Step::Preset => "preset.fire",
            Step::Release => "preset.release",
            Step::Scene => "scene.cut",
            Step::Preview => "scene.go",
            Step::LayerFx | Step::LayerVisible | Step::Setting => "set",
            Step::Animation => "trigger",
            Step::Notify => "emit",
            Step::Say => "bot.say",
            Step::Sound => "audio.play",
            Step::Mix => "mixer.snapshot.recall",
            Step::Lights | Step::Look => "lights.cue",
            Step::Mode => "mode.set",
            Step::Wait => "wait",
            Step::Marker => "twitch.marker",
            Step::TimelinePlay => "timeline.play",
            Step::TimelineStop => "timeline.stop",
            Step::Custom => "",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Step::Preset => "Run a saved action",
            Step::Release => "Stop a saved action",
            Step::Scene => "Switch to a scene",
            Step::Preview => "Put a scene up next",
            Step::LayerFx => "Turn a layer effect on/off",
            Step::LayerVisible => "Show or hide a layer",
            Step::Setting => "Change a setting",
            Step::Animation => "Play a source's animation",
            Step::Notify => "Show a notification",
            Step::Say => "Say something in chat",
            Step::Sound => "Play a sound",
            Step::Mix => "Recall a sound mix",
            Step::Lights => "Run a light cue",
            Step::Look => "Turn on a light look",
            Step::Mode => "Change the show mode",
            Step::TimelinePlay => "Play a timeline",
            Step::TimelineStop => "Stop a timeline",
            Step::Marker => "Add a stream marker",
            Step::Wait => "Wait a moment",
            Step::Custom => "Custom command",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Step::Preset | Step::Release => icon::BOLT,
            Step::Scene | Step::Preview => icon::SCENE,
            Step::LayerFx => icon::WAND,
            Step::LayerVisible => icon::EYE,
            Step::Setting => icon::SLIDERS,
            Step::Animation => icon::SPARKLE,
            Step::Notify => icon::ALERT,
            Step::Say => icon::CHAT,
            Step::Sound => icon::VOLUME,
            Step::Mix => icon::MIX,
            Step::Lights | Step::Look => icon::LIGHT,
            Step::Mode => icon::PLAY,
            Step::TimelinePlay | Step::TimelineStop => icon::TIMELINE,
            Step::Marker => icon::STAR,
            Step::Wait => icon::CLOCK,
            Step::Custom => icon::CONSOLE,
        }
    }

    fn max_args(self) -> usize {
        match self {
            Step::Lights => 2,
            Step::Custom => 0,
            Step::Say => usize::MAX,
            _ => 1,
        }
    }

    /// What's still missing before the step can run (`None` = ready to go).
    fn missing(self, args: &[String]) -> Option<&'static str> {
        let a0 = args.first().map(|a| a.trim()).unwrap_or("");
        let empty = args.iter().all(|a| a.trim().is_empty());
        Some(match self {
            Step::Preset | Step::Release if empty => "pick a saved action",
            Step::Scene | Step::Preview if empty => "pick a scene",
            Step::LayerFx if !layer_picked(a0, true) => "pick a layer effect",
            Step::LayerVisible if !layer_picked(a0, false) => "pick a layer",
            Step::Setting if a0.is_empty() => "pick a setting",
            Step::Setting if args.get(1).is_none_or(|v| v.trim().is_empty()) => "say what to set it to",
            Step::Animation if empty => "pick a source",
            Step::Notify if empty => "pick a notification",
            Step::Say if empty => "type what to say",
            Step::Sound if empty => "pick a sound",
            Step::Mix if empty => "pick a sound mix",
            Step::Lights if empty => "pick a light cue list",
            Step::Look if empty => "pick a light look",
            Step::Mode if empty => "pick a show mode",
            Step::TimelinePlay | Step::TimelineStop if empty => "pick a timeline",
            Step::Wait if empty => "say how long to wait",
            _ => return None,
        })
    }

    /// The command a fresh step of this kind starts as.
    fn blank(self) -> String {
        match self {
            Step::Look => "lights.cue look=".into(),
            Step::Wait => "wait 2s".into(),
            Step::LayerFx => format!("set {BLANK_FX} true"),
            Step::LayerVisible => format!("set {BLANK_LAYER} false"),
            k => k.verb().into(),
        }
    }
}

/// (scene, layer, effect) of a layer address: `scene.<s>.node.<id>.visible` (no effect) or
/// `scene.<s>.node.<id>.fx.<name>.enabled`. Parts not picked yet are empty.
fn layer_parts(addr: &str) -> Option<(&str, &str, Option<&str>)> {
    let (scene, rest) = addr.strip_prefix("scene.")?.split_once(".node.")?;
    if scene.contains('.') {
        return None;
    }
    if let Some(node) = rest.strip_suffix(".visible") {
        return (!node.contains('.')).then_some((scene, node, None));
    }
    let (node, fx) = rest.strip_suffix(".enabled")?.split_once(".fx.")?;
    (!node.contains('.')).then_some((scene, node, Some(fx)))
}

/// A layer step's address has everything picked (`fx`: it names an effect too).
fn layer_picked(addr: &str, fx: bool) -> bool {
    layer_parts(addr).is_some_and(|(s, n, f)| !s.is_empty() && !n.is_empty() && f.map_or(!fx, |f| fx && !f.is_empty()))
}

fn layer_addr(scene: &str, node: &str, fx: Option<&str>) -> String {
    match fx {
        Some(f) => format!("scene.{scene}.node.{node}.fx.{f}.enabled"),
        None => format!("scene.{scene}.node.{node}.visible"),
    }
}

/// The friendly shape of a command (`Custom` when the guided editor can't show it).
fn parse_step(cmd: &str) -> (Step, Vec<String>) {
    let custom = || (Step::Custom, Vec::new());
    let Ok(toks) = tokenize(cmd) else { return custom() };
    let Some((verb, rest)) = toks.split_first() else { return custom() };
    // `lights.cue look=<name>`, `patch.<id>.trigger` / `trigger patch.<id>`
    if verb == "lights.cue"
        && let [one] = rest
        && let Some(look) = one.strip_prefix("look=")
    {
        return (Step::Look, vec![look.to_string()]);
    }
    if let Some(id) = verb.strip_prefix("patch.").and_then(|v| v.strip_suffix(".trigger")) {
        return if rest.is_empty() && !id.is_empty() { (Step::Animation, vec![id.to_string()]) } else { custom() };
    }
    let layer = |a: &str| layer_parts(a).map(|(_, _, fx)| if fx.is_some() { Step::LayerFx } else { Step::LayerVisible });
    match verb.as_str() {
        "trigger" => {
            return match rest {
                [] => (Step::Animation, Vec::new()),
                [a] if a.starts_with("patch.") && !a.contains('=') => (Step::Animation, vec![a["patch.".len()..].to_string()]),
                _ => custom(),
            };
        }
        "toggle" => {
            return match rest {
                [a] if !a.contains('=') => layer(a).map_or_else(custom, |k| (k, vec![a.clone(), "toggle".into()])),
                _ => custom(),
            };
        }
        "set" => {
            return match rest {
                [a, v] if !a.contains('=') => match (layer(a), v.as_str()) {
                    (Some(k), "true" | "false") => (k, vec![a.clone(), if v == "true" { "on" } else { "off" }.into()]),
                    _ => (Step::Setting, vec![a.clone(), v.clone()]),
                },
                [a] if !a.contains('=') => (Step::Setting, vec![a.clone()]),
                [] => (Step::Setting, Vec::new()),
                _ => custom(),
            };
        }
        "animate" => {
            return match rest {
                // a duration still being typed stays here; the engine's parser flags it
                [a, v, d] if !a.contains('=') => (Step::Setting, vec![a.clone(), v.clone(), d.clone()]),
                _ => custom(),
            };
        }
        "emit" => {
            return match rest.split_first() {
                None => (Step::Notify, Vec::new()),
                Some((ty, kv)) if !ty.contains('=') && kv.iter().all(|t| t.contains('=')) => (Step::Notify, rest.to_vec()),
                _ => custom(),
            };
        }
        _ => {}
    }
    let Some(kind) = Step::from_verb(verb) else { return custom() };
    if kind == Step::Say {
        return match rest {
            [one] if one.starts_with("text=") => (kind, vec![one["text=".len()..].to_string()]),
            _ if rest.iter().any(|a| a.contains('=')) => custom(),
            [] => (kind, Vec::new()),
            _ => (kind, vec![rest.join(" ")]),
        };
    }
    if rest.len() > kind.max_args() || rest.iter().any(|a| a.contains('=')) {
        return custom();
    }
    (kind, rest.to_vec())
}

/// One command token, quoted when it needs to be (`'` and `"` both inside: split quoting).
fn quote_arg(a: &str) -> String {
    if !a.is_empty() && a.chars().all(|c| c.is_ascii_alphanumeric() || "_-.:/#".contains(c)) {
        a.to_string()
    } else if !a.contains('\'') {
        format!("'{a}'")
    } else if !a.contains('"') {
        format!("\"{a}\"")
    } else {
        format!("'{}'", a.split('\'').collect::<Vec<_>>().join("'\"'\"'"))
    }
}

fn build_step(kind: Step, args: &[&str]) -> String {
    let arg = |i: usize| args.get(i).map(|a| a.trim()).unwrap_or("");
    let a0 = arg(0);
    match kind {
        Step::Animation if a0.is_empty() => return "trigger".into(),
        Step::Animation => return format!("patch.{a0}.trigger"),
        Step::Look => return format!("lights.cue look={}", if a0.is_empty() { String::new() } else { quote_arg(a0) }),
        Step::LayerFx | Step::LayerVisible => {
            let addr = match a0 {
                "" if kind == Step::LayerFx => BLANK_FX,
                "" => BLANK_LAYER,
                a => a,
            };
            return match arg(1) {
                "toggle" => format!("toggle {addr}"),
                "off" => format!("set {addr} false"),
                _ => format!("set {addr} true"),
            };
        }
        Step::Setting => {
            return match (a0, arg(1), arg(2)) {
                ("", ..) => "set".into(),
                (a, "", _) => format!("set {a}"),
                (a, v, "") => format!("set {a} {}", quote_arg(v)),
                (a, v, d) => format!("animate {a} {} {d}", quote_arg(v)),
            };
        }
        Step::Notify => {
            let mut out = "emit".to_string();
            if !a0.is_empty() {
                out.push(' ');
                out.push_str(a0);
                for (k, v) in args[1..].iter().filter_map(|kv| kv.split_once('=')).filter(|(_, v)| !v.trim().is_empty()) {
                    out.push_str(&format!(" {k}={}", quote_arg(v.trim())));
                }
            }
            return out;
        }
        _ => {}
    }
    let mut out = kind.verb().to_string();
    if kind == Step::Lights && a0.is_empty() {
        return out;
    }
    for a in args.iter().filter(|a| !a.trim().is_empty()) {
        out.push(' ');
        if kind == Step::Say && a.contains('=') {
            out.push_str("text=");
        }
        out.push_str(&quote_arg(a));
    }
    out
}

/// Names and labels the pickers offer (from the engine's queries).
#[derive(Default)]
struct Names {
    presets: Vec<(String, String)>,
    scenes: Vec<(String, String)>,
    cuelists: Vec<(String, String)>,
    modes: Vec<(String, String)>,
    sounds: Vec<(String, String)>,
    mixes: Vec<(String, String)>,
    timelines: Vec<(String, String)>,
    /// Sources with an animation that can be played (triggered).
    animations: Vec<(String, String)>,
    looks: Vec<(String, String)>,
    /// Configured notifications: (the event that shows it, its name).
    alerts: Vec<(String, String)>,
}

fn pairs(l: &[Value]) -> Vec<(String, String)> {
    l.iter()
        .filter_map(|v| {
            let n = v.get_path("name").and_then(Value::as_str)?.to_string();
            let label = display_label(&n, v.get_path("label").and_then(Value::as_str).unwrap_or(""));
            Some((n, label))
        })
        .collect()
}

fn plain(l: &[Value], f: fn(&str) -> String) -> Vec<(String, String)> {
    l.iter().filter_map(Value::as_str).map(|s| (s.to_string(), f(s))).collect()
}

/// The notifications a step can show (`alerts.config`): one per event (the first alert for an
/// event is the one that shows), leaving out alerts that answer a pattern of events.
fn alert_choices(cfg: Option<&Value>) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    for a in cfg.and_then(|c| c.get_path("alerts")).and_then(Value::as_list).unwrap_or(&[]) {
        let when = a.get_path("when").and_then(Value::as_str).unwrap_or("").trim();
        if when.is_empty() || when.contains('*') || out.iter().any(|(w, _)| w == when) {
            continue;
        }
        let label = match (a.get_path("name").and_then(Value::as_str).unwrap_or("").trim(), trigger_index(when)) {
            ("", Some(i)) => TRIGGERS[i].label.to_string(),
            ("", None) => nice_name(when),
            (n, _) => nice_name(n),
        };
        out.push((when.to_string(), label));
    }
    out
}

impl Names {
    fn read(app: &App) -> Names {
        let list = |q: &str, k: &str| app.m.q(q).and_then(|v| v.get_path(k)).and_then(Value::as_list).unwrap_or(&[]);
        Names {
            presets: pairs(app.m.q_list("presets")),
            scenes: pairs(app.m.q_list("scenes")),
            cuelists: pairs(app.m.q_list("lights.cuelists")),
            modes: plain(app.m.q_list("modes"), pretty),
            sounds: plain(list("audio.mix", "sounds"), nice_name),
            mixes: pairs(list("mixer.snapshots", "snapshots")),
            timelines: pairs(app.m.q_list("timelines")),
            animations: app
                .m
                .q_list("patches")
                .iter()
                .filter(|p| p.get_path("trigger").is_some_and(Value::truthy))
                .filter_map(|p| {
                    let id = p.get_path("id").and_then(Value::as_str)?.to_string();
                    let label = p.get_path("label").and_then(Value::as_str).filter(|l| !l.is_empty() && *l != id).map_or_else(|| nice_name(&id), nice_name);
                    Some((id, label))
                })
                .collect(),
            looks: pairs(app.m.q_list("lights.palettes")),
            alerts: alert_choices(app.m.q("alerts.config")),
        }
    }

    fn label(list: &[(String, String)], name: &str) -> String {
        if name.is_empty() {
            return "…".into();
        }
        // a value filled in when the trigger fires: `{scene}`, `{patch.ad_break.return_scene}`
        if let Some(inner) = name.strip_prefix('{').and_then(|n| n.strip_suffix('}')) {
            return match inner {
                "scene" => "the scene on air".into(),
                s if s.ends_with("scene") => "the scene from before".into(),
                _ => friendly_text(name),
            };
        }
        list.iter().find(|(n, _)| n == name).map_or_else(|| nice_name(name), |(_, l)| l.clone())
    }
}

/// Every layer and its effect switches, from the engine's state (`scene.<s>.node.<id>.visible`,
/// `….fx.<name>.enabled`). Read only while a layer step is on screen.
#[derive(Default)]
struct Layers {
    /// (scene, layer, its effects)
    items: Vec<(String, String, Vec<String>)>,
}

impl Layers {
    fn read(app: &App) -> Layers {
        let mut items: Vec<(String, String, Vec<String>)> = Vec::new();
        // sorted addresses: one layer's settings sit next to each other
        for (a, _) in app.m.under("scene") {
            let Some((s, n, fx)) = layer_parts(a) else { continue };
            if !items.last().is_some_and(|(x, y, _)| x == s && y == n) {
                items.push((s.to_string(), n.to_string(), Vec::new()));
            }
            if let (Some(f), Some(last)) = (fx, items.last_mut()) {
                last.2.push(f.to_string());
            }
        }
        Layers { items }
    }

    /// Scenes that have a layer (one with an effect, when `fx`).
    fn scenes(&self, fx: bool) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for (s, _, f) in &self.items {
            if (!fx || !f.is_empty()) && !out.contains(s) {
                out.push(s.clone());
            }
        }
        out
    }

    fn layers(&self, scene: &str, fx: bool) -> Vec<(String, String)> {
        self.items.iter().filter(|(s, _, f)| s == scene && (!fx || !f.is_empty())).map(|(_, n, _)| (n.clone(), nice_name(n))).collect()
    }

    fn effects(&self, scene: &str, layer: &str) -> Vec<(String, String)> {
        self.items.iter().filter(|(s, n, _)| s == scene && n == layer).flat_map(|(_, _, f)| f.iter().map(|x| (x.clone(), fx_name(x)))).collect()
    }
}

/// An effect's name for people: `rgb_split` → "RGB split", `patch.dream` → "Dream".
fn fx_name(fx: &str) -> String {
    nice_name(fx.strip_prefix("patch.").unwrap_or(fx))
}

/// Words for a setting address: `audio.bus.music.gain` → "Music volume", `fx.vhs.amount` → "Vhs".
pub fn setting_name(addr: &str) -> String {
    let parts: Vec<&str> = addr.split('.').collect();
    match parts.as_slice() {
        ["audio", "bus", "sfx", "gain"] => "Sound effects volume".into(),
        ["audio", "bus", bus, "gain"] => format!("{} volume", nice_name(bus)),
        ["lights", "master"] => "Lights master".into(),
        ["lights", "cuelist", list, "master"] => format!("{} lights level", nice_name(list)),
        ["fx", name, "amount"] => nice_name(name),
        ["fx", name, what] => format!("{} {}", nice_name(name), what.replace('_', " ")),
        [.., a, b] => format!("{} {}", nice_name(a), b.replace('_', " ")),
        _ => nice_name(addr),
    }
}

/// [`setting_name`] for the middle of a sentence ("change music volume", "change RGB split").
pub fn setting_words(addr: &str) -> String {
    let n = setting_name(addr);
    let first = n.split(' ').next().unwrap_or("");
    if first.len() > 1 && !first.chars().any(char::is_lowercase) {
        return n;
    }
    let mut c = n.chars();
    c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default()
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// "Blur on Cam face in Duo", "Cam face in Duo" (the parts of a layer address in words).
fn layer_words(addr: &str, n: &Names) -> (String, String) {
    let (s, l, fx) = layer_parts(addr).unwrap_or(("", "", None));
    let dots = |x: &str, f: fn(&str) -> String| if x.is_empty() { "…".to_string() } else { f(x) };
    let layer = format!("{} in {}", dots(l, nice_name), Names::label(&n.scenes, s));
    (fx.map(|f| dots(f, fx_name)).unwrap_or_default(), layer)
}

/// A value as words: `true` → "on", `false` → "off".
fn value_words(v: &str) -> String {
    match v {
        "true" => "on".into(),
        "false" => "off".into(),
        "" => "…".into(),
        v => clip(v, 24),
    }
}

fn step_phrase(cmd: &str, n: &Names) -> String {
    let (kind, args) = parse_step(cmd);
    let arg = |i: usize| args.get(i).map(String::as_str).unwrap_or("");
    let a0 = arg(0);
    match kind {
        Step::Preset => format!("run {}", Names::label(&n.presets, a0)),
        Step::Release => format!("stop {}", Names::label(&n.presets, a0)),
        Step::Scene => format!("switch to {}", Names::label(&n.scenes, a0)),
        Step::Preview => format!("put {} up next", Names::label(&n.scenes, a0)),
        Step::LayerFx => {
            let (fx, layer) = layer_words(a0, n);
            match arg(1) {
                "toggle" => format!("switch {fx} on or off on {layer}"),
                "off" => format!("turn off {fx} on {layer}"),
                _ => format!("turn on {fx} on {layer}"),
            }
        }
        Step::LayerVisible => {
            let (_, layer) = layer_words(a0, n);
            match arg(1) {
                "toggle" => format!("show or hide {layer}"),
                "off" => format!("hide {layer}"),
                _ => format!("show {layer}"),
            }
        }
        Step::Setting if a0.is_empty() => "change a setting".into(),
        Step::Setting if !arg(2).is_empty() => format!("fade {} to {} over {}", setting_words(a0), value_words(arg(1)), arg(2)),
        Step::Setting => format!("set {} to {}", setting_words(a0), value_words(arg(1))),
        Step::Animation => format!("play {}", Names::label(&n.animations, a0)),
        Step::Notify if a0.is_empty() => "show a notification".into(),
        Step::Notify => match n.alerts.iter().find(|(w, _)| w == a0) {
            Some((_, l)) => format!("show the {l} notification"),
            None => format!("send the “{}” event", nice_name(&a0.replace('.', " "))),
        },
        Step::Say if a0.is_empty() => "say …".into(),
        Step::Say => format!("say “{}”", clip(&friendly_text(a0), 56)),
        Step::Sound => format!("play {}", Names::label(&n.sounds, a0)),
        Step::Mix => format!("recall the {} mix", Names::label(&n.mixes, a0)),
        Step::Lights => match args.get(1) {
            Some(c) => format!("run light cue {c} of {}", Names::label(&n.cuelists, a0)),
            None => format!("run the {} lights", Names::label(&n.cuelists, a0)),
        },
        Step::Look => format!("turn on the {} light look", Names::label(&n.looks, a0)),
        Step::Mode => format!("switch to {}", Names::label(&n.modes, a0)),
        Step::TimelinePlay => format!("play the {} timeline", Names::label(&n.timelines, a0)),
        Step::TimelineStop => format!("stop the {} timeline", Names::label(&n.timelines, a0)),
        Step::Marker => "add a stream marker".into(),
        Step::Wait => format!("wait {}", if a0.is_empty() { "…" } else { a0 }),
        Step::Custom => custom_phrase(cmd),
    }
}

fn custom_phrase(cmd: &str) -> String {
    let toks = tokenize(cmd).unwrap_or_default();
    let verb = toks.first().map(String::as_str).unwrap_or("");
    let arg = toks.get(1).map(String::as_str).unwrap_or("");
    let middle = |a: &str| nice_name(a.split('.').nth(1).unwrap_or(a));
    match verb {
        "" => "…".into(),
        "trigger" => format!("play {}", middle(arg)),
        v if v.ends_with(".trigger") => format!("play {}", middle(v.trim_end_matches(".trigger"))),
        "set" | "animate" | "adjust" => format!("change {}", setting_words(arg)),
        "toggle" => format!("switch {} on/off", setting_words(arg)),
        "twitch.shoutout" => "give a shoutout".into(),
        "scene.take" => "switch to the scene that's up next".into(),
        "scene.next" => "put the next scene up next".into(),
        "scene.prev" => "put the previous scene up next".into(),
        "clean" => "clear all effects".into(),
        "panic" => "stop everything (panic)".into(),
        "obs.stream.start" => "start streaming".into(),
        "obs.stream.stop" => "stop streaming".into(),
        "queue.skip" => "skip the song".into(),
        "audio.duck" => "lower the music".into(),
        "audio.stop" => "stop sounds".into(),
        "lights.release" => "release the lights".into(),
        "session.marker" => "add a marker to the recording".into(),
        "voice.ptt" => "talk to Stream Engine (hold)".into(),
        "voice.confirm" => "say yes to a voice command".into(),
        "voice.cancel" => "say no to a voice command".into(),
        "deck.page" => format!("show the {} deck page", nice(arg)),
        "emit" | "fire" => "send a custom event".into(),
        _ => "run a custom command".into(),
    }
}

/// Plain words for command lines ("Run Hype", "Switch to Duo"), shared with the controller
/// views. Build once per frame and reuse for every row.
pub struct Words {
    names: Names,
}

impl Words {
    pub fn new(app: &App) -> Words {
        Words { names: Names::read(app) }
    }

    /// One command line (`;`-separated lists too).
    pub fn command(&self, cmd: &str) -> String {
        let cmds: Vec<&str> = cmd.split(';').map(str::trim).filter(|c| !c.is_empty()).collect();
        match cmds.as_slice() {
            ["scene.next", "scene.take"] => return "Switch to the next scene".into(),
            ["scene.prev", "scene.take"] => return "Switch to the previous scene".into(),
            _ => {}
        }
        let parts: Vec<String> = cmds.iter().map(|c| step_phrase(c, &self.names)).collect();
        capitalize(&parts.join(", then "))
    }

    /// A saved action's display name.
    pub fn preset(&self, name: &str) -> String {
        Names::label(&self.names.presets, name)
    }

    /// A scene's display name.
    pub fn scene(&self, name: &str) -> String {
        Names::label(&self.names.scenes, name)
    }

    /// (name, display name) of every saved action / scene.
    pub fn presets(&self) -> &[(String, String)] {
        &self.names.presets
    }
    pub fn scenes(&self) -> &[(String, String)] {
        &self.names.scenes
    }
}

pub fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().chain(c).collect(),
        None => String::new(),
    }
}

fn do_phrase(cmds: &[String], n: &Names) -> String {
    let parts: Vec<String> = cmds.iter().filter(|c| !c.trim().is_empty()).map(|c| step_phrase(c, n)).collect();
    if parts.is_empty() { "Does nothing yet".into() } else { capitalize(&parts.join(", ")) }
}

// ---- the draft -----------------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub struct RuleDraft {
    /// Name of the rule in its file (None = new rule).
    pub orig: Option<String>,
    pub file: String,
    pub name: String,
    pub when: String,
    pub cond: String,
    pub commands: Vec<String>,
    pub cooldown_global: String,
    pub cooldown_actor: String,
    pub modes: String,
    pub scenes: String,
    pub enabled: bool,
    pub test_args: String,
    filled: bool,
}

impl RuleDraft {
    fn new() -> RuleDraft {
        RuleDraft {
            orig: None,
            file: "rules/ui.toml".into(),
            name: String::new(),
            when: String::new(),
            cond: String::new(),
            commands: Vec::new(),
            cooldown_global: String::new(),
            cooldown_actor: String::new(),
            modes: String::new(),
            scenes: String::new(),
            enabled: true,
            test_args: String::new(),
            filled: true,
        }
    }

    /// What has to change before it can be saved (empty = saveable), using the engine's parsers.
    pub fn errors(&self) -> Vec<String> {
        let mut e = Vec::new();
        if self.orig.is_some() && self.name.trim().is_empty() {
            e.push("Give it a name (under File).".into());
        }
        let when = self.when.trim();
        if when.is_empty() {
            e.push("Pick what it answers to.".into());
        } else if !se_proto::address::is_valid(when, true) {
            e.push(format!("“{when}” isn't an event name Stream Engine can watch for."));
        }
        if !self.cond.trim().is_empty()
            && let Err(err) = se_expr::Expr::parse(self.cond.trim())
        {
            e.push(format!("The “only if” check has a mistake: {err}"));
        }
        if self.commands.iter().all(|c| c.trim().is_empty()) {
            e.push("Add at least one thing for it to do.".into());
        }
        for (i, c) in self.commands.iter().enumerate() {
            if c.trim().is_empty() {
                continue;
            }
            let (kind, args) = parse_step(c);
            if let Some(what) = kind.missing(&args) {
                e.push(format!("Step {}: {what}.", i + 1));
            } else if let Err(err) = Op::parse(c) {
                e.push(format!("Step {} has a mistake: {err}", i + 1));
            }
        }
        for d in [&self.cooldown_global, &self.cooldown_actor] {
            if !d.trim().is_empty() && se_proto::parse_duration_ms(d).is_none() {
                e.push(format!("“{}” isn't a time. Use something like 10s or 5m.", d.trim()));
            }
        }
        if !self.file.starts_with("rules/") || !self.file.ends_with(".toml") {
            e.push("Triggers are saved in the rules folder (File → Saved in).".into());
        }
        e
    }

    fn list(s: &str) -> Value {
        Value::List(s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(|x| Value::Str(x.into())).collect())
    }

    /// Keys written for this rule (null removes options that were cleared).
    pub fn set_value(&self) -> Value {
        let cd = {
            let mut m = Value::map();
            if !self.cooldown_global.trim().is_empty() {
                m = m.with("global", self.cooldown_global.trim());
            }
            if !self.cooldown_actor.trim().is_empty() {
                m = m.with("per_actor", self.cooldown_actor.trim());
            }
            if m.as_map().is_some_and(|m| m.is_empty()) { Value::Null } else { m }
        };
        let modes = Self::list(&self.modes);
        let scenes = Self::list(&self.scenes);
        let empty = |v: &Value| v.as_list().is_some_and(|l| l.is_empty());
        Value::map()
            .with("name", self.name.trim())
            .with("when", self.when.trim())
            .with("if", if self.cond.trim().is_empty() { Value::Null } else { Value::Str(self.cond.trim().into()) })
            .with("do", Value::List(self.commands.iter().map(|c| c.trim()).filter(|c| !c.is_empty()).map(|c| Value::Str(c.into())).collect()))
            .with("cooldown", cd)
            .with("modes", if empty(&modes) { Value::Null } else { modes })
            .with("scenes", if empty(&scenes) { Value::Null } else { scenes })
            .with("enabled", if self.enabled { Value::Null } else { Value::Bool(false) })
    }

    /// `project.write` args. `file_text` decides between `[[rule]]` entries and a single-rule file.
    pub fn write_args(&self, file_text: Option<&str>) -> Value {
        self.entry_args(file_text, self.set_value())
    }

    /// `project.write` args that only switch a saved trigger on or off.
    pub fn enabled_args(&self, file_text: Option<&str>) -> Value {
        self.entry_args(file_text, Value::map().with("enabled", if self.enabled { Value::Null } else { Value::Bool(false) }))
    }

    fn entry_args(&self, file_text: Option<&str>, set: Value) -> Value {
        let parsed = file_text.and_then(|t| t.parse::<toml::Table>().ok());
        let multi = parsed.as_ref().is_some_and(|t| t.get("rule").is_some_and(|r| r.is_array()));
        match &self.orig {
            None => Value::map().with("path", self.file.clone()).with("table", "rule").with("append", true).with("set", set),
            Some(orig) if multi => {
                let explicit = parsed
                    .as_ref()
                    .and_then(|t| t.get("rule"))
                    .and_then(|r| r.as_array())
                    .is_some_and(|a| a.iter().any(|e| e.get("name").and_then(|n| n.as_str()) == Some(orig.as_str())));
                let args = Value::map().with("path", self.file.clone()).with("table", "rule").with("set", set);
                match (explicit, orig.rsplit_once('#').and_then(|(_, n)| n.parse::<i64>().ok())) {
                    (false, Some(n)) => args.with("index", n - 1),
                    _ => args.with("match", Value::map().with("name", orig.clone())),
                }
            }
            Some(_) => Value::map().with("path", self.file.clone()).with("set", set),
        }
    }

    /// Same saved form (what Save would write), ignoring the "try it" values.
    fn same_as(&self, other: &RuleDraft) -> bool {
        self.file == other.file && self.set_value() == other.set_value()
    }

    pub fn delete_args(&self, file_text: Option<&str>) -> Option<Value> {
        let orig = self.orig.as_ref()?;
        let multi = file_text.and_then(|t| t.parse::<toml::Table>().ok()).is_some_and(|t| t.get("rule").is_some_and(|r| r.is_array()));
        Some(if multi {
            Value::map().with("path", self.file.clone()).with("table", "rule").with("match", Value::map().with("name", orig.clone())).with("delete", true)
        } else {
            Value::map().with("path", self.file.clone()).with("delete", true)
        })
    }

    /// A name for a new trigger from its sentence, unique among `taken`.
    fn auto_name(&self, taken: &[String]) -> String {
        let base = clip(when_phrase(&self.when, &self.cond).trim_start_matches("When "), 60);
        let mut name = base.clone();
        let mut n = 2;
        while taken.iter().any(|t| t == &name) {
            name = format!("{base} {n}");
            n += 1;
        }
        name
    }

    /// The simulator command that fires this rule's event.
    pub fn test_command(&self) -> String {
        let when = self.when.trim();
        let args = self.test_args.trim();
        match SIM_FOR.iter().find(|(ty, _)| se_proto::address::matches(when, ty)) {
            Some((_, sim)) => format!("sim.{sim} {args}").trim().to_string(),
            None => {
                let ty = when.replace("**", "test").replace('*', "test");
                format!("emit {ty} {args}").trim().to_string()
            }
        }
    }
}

/// A trigger in the list, worded once per `rules`/`presets`/`scenes` reply.
struct Row {
    name: String,
    family: Family,
    icon: &'static str,
    title: String,
    subtitle: String,
    enabled: bool,
    fired_ms: Option<i64>,
    value: Value,
}

type RowKey = (u64, u64, u64, u64);

#[derive(Default)]
pub struct RulesEditor {
    pub edit: Option<RuleDraft>,
    /// The draft as last saved (or opened): Save/Discard show while `edit` differs.
    base: Option<RuleDraft>,
    /// "Something else…" picked for the event: the event name is typed.
    other_event: bool,
    /// Which text field autocompletes (`when`, `if`, `do:<i>`, `raw-…`).
    focus: Option<String>,
    /// The guided "only if" rows and the expression they were read from.
    rows: Vec<Cond>,
    rows_src: Option<String>,
    custom_cond: bool,
    lists_at: f64,
    cache: Option<(RowKey, Arc<Vec<Row>>)>,
    /// "Try it with" values: (event field, text).
    test_vals: Vec<(String, String)>,
}

impl RulesEditor {
    pub fn new_rule(&mut self) {
        self.edit = Some(RuleDraft::new());
        self.base = None;
        self.other_event = false;
        self.test_vals.clear();
        self.rows_src = None;
    }
    pub fn editing_name(&self) -> Option<&str> {
        self.edit.as_ref().and_then(|d| d.orig.as_deref())
    }
    /// Unsaved changes (a new trigger is unsaved until it's created).
    fn dirty(&self) -> bool {
        match (&self.edit, &self.base) {
            (Some(e), Some(b)) => !e.same_as(b),
            (Some(_), None) => true,
            _ => false,
        }
    }
    /// Open a row of the `rules` query.
    pub fn open(&mut self, r: &Value) {
        let s = |k: &str| r.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
        let name = s("name");
        self.rows_src = None;
        self.test_vals.clear();
        let d = RuleDraft {
            orig: Some(name.clone()),
            file: s("file"),
            name,
            when: s("when"),
            cond: s("if"),
            commands: r.get_path("do").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            cooldown_global: String::new(),
            cooldown_actor: String::new(),
            modes: String::new(),
            scenes: String::new(),
            enabled: r.get_path("enabled").is_none_or(Value::truthy),
            test_args: String::new(),
            filled: false,
        };
        self.other_event = !d.when.is_empty() && trigger_index(&d.when).is_none();
        self.base = Some(d.clone());
        self.edit = Some(d);
    }
}

// ---- autocomplete (raw fields) -------------------------------------------------------------------

/// Suggestions for the token under the cursor (the last whitespace-separated word).
pub fn suggest(field: &str, text: &str, app: &App) -> Vec<String> {
    let last = text.rsplit(|c: char| c.is_whitespace() || c == '(' || c == '!').next().unwrap_or("");
    let mut pool: Vec<String> = Vec::new();
    match field {
        // a setting: any live address containing what's typed (`blur` finds every blur switch)
        "addr" => {
            let l = text.trim().to_lowercase();
            if l.is_empty() {
                return Vec::new();
            }
            return app.m.state.keys().filter(|a| a.to_lowercase().contains(&l) && a.as_str() != text.trim()).take(8).cloned().collect();
        }
        "when" => {
            pool.extend(SIM_FOR.iter().map(|(t, _)| t.to_string()));
            pool.extend(app.m.events.iter().map(|e| e.ty.clone()));
            pool.extend(
                ["mode.enter.*", "mode.exit.*", "band.kick", "band.snare", "band.drop", "beat", "deck.key", "queue.song_started", "timeline.cue", "twitch.*"]
                    .map(String::from),
            );
        }
        "if" | "cond" => {
            // `cond`: a condition outside a trigger (no event to read fields from)
            let when = if field == "if" { app.build.rules.edit.as_ref().map(|d| d.when.clone()).unwrap_or_default() } else { String::new() };
            for (ty, fields) in FIELDS {
                if se_proto::address::matches(&when, ty) {
                    pool.extend(fields.iter().map(|f| format!("event.{f}")));
                }
            }
            for e in app.m.events.iter().rev().take(200).filter(|e| se_proto::address::matches(&when, &e.ty)) {
                if let Value::Map(m) = &e.payload {
                    pool.extend(m.keys().map(|k| format!("event.{k}")));
                }
            }
            pool.extend(["mode", "scene", "event.user", "&&", "||", "in"].map(String::from));
            pool.extend(app.m.state.keys().take(4000).cloned());
        }
        _ => {
            let words = text.split_whitespace().count() + usize::from(text.ends_with(' '));
            if words <= 1 {
                pool.extend(VERBS.iter().map(|v| v.to_string()));
                pool.extend(app.m.q_list("presets").iter().filter_map(|p| p.get_path("name").and_then(Value::as_str)).map(|n| format!("preset.fire {n}")));
            } else {
                let verb = text.split_whitespace().next().unwrap_or("");
                match verb {
                    "preset.fire" | "preset.release" => {
                        pool.extend(app.m.q_list("presets").iter().filter_map(|p| p.get_path("name").and_then(Value::as_str).map(String::from)))
                    }
                    "scene.go" | "scene.cut" => {
                        pool.extend(app.m.q_list("scenes").iter().filter_map(|p| p.get_path("name").and_then(Value::as_str).map(String::from)))
                    }
                    "mode.set" => pool.extend(app.m.q_list("modes").iter().filter_map(|v| v.as_str().map(String::from))),
                    "scene.take" => pool.extend(app.m.q_list("transitions").iter().filter_map(|v| v.as_str().map(String::from))),
                    _ => pool.extend(app.m.state.keys().take(4000).cloned()),
                }
            }
        }
    }
    pool.sort();
    pool.dedup();
    let l = last.to_lowercase();
    pool.into_iter().filter(|p| !l.is_empty() && p.to_lowercase().starts_with(&l) && *p != last).take(8).collect()
}

fn complete(text: &mut String, choice: &str) {
    let cut = text.rfind(|c: char| c.is_whitespace() || c == '(' || c == '!').map(|i| i + 1).unwrap_or(0);
    text.truncate(cut);
    if choice.contains(' ') && cut > 0 {
        // a full command suggestion replaces the whole line
        text.clear();
    }
    text.push_str(choice);
}

/// Raw text field with autocomplete. `key` is `when` / `if` / `do:<i>`, optionally `raw-` prefixed.
fn field(app: &mut App, ui: &mut egui::Ui, key: &str, hint: &str, get: impl Fn(&mut RuleDraft) -> &mut String) {
    let Some(mut text) = app.build.rules.edit.as_mut().map(|d| get(d).clone()) else { return };
    if text_field(app, ui, key, hint, &mut text)
        && let Some(d) = app.build.rules.edit.as_mut()
    {
        *get(d) = text;
    }
}

/// A monospace text field with suggestion buttons for the token being typed. `key` names the
/// field (its kind before the first `:`: `when`, `if`, `cond` for any condition, anything else
/// for commands) and keeps its focus apart from other fields. Returns true when `text` changed.
pub fn text_field(app: &mut App, ui: &mut egui::Ui, key: &str, hint: &str, text: &mut String) -> bool {
    let t = app.t.clone();
    let kind = key.trim_start_matches("raw-").split(':').next().unwrap_or(key);
    let focused = app.build.rules.focus.as_deref() == Some(key);
    let sugg = if focused { suggest(kind, text, app) } else { Vec::new() };
    let r = ui.add(se_ui_kit::widgets::field(text).hint_text(hint).desired_width(f32::INFINITY).font(font_mono(type_scale::BODY - 1.0)));
    let mut changed = r.changed();
    if r.has_focus() {
        app.build.rules.focus = Some(key.to_string());
    }
    if !sugg.is_empty() && (r.has_focus() || focused) {
        ui.horizontal_wrapped(|ui| {
            for s in sugg {
                if widgets::button_ex(ui, &t, None, &s, Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    complete(text, &s);
                    changed = true;
                    r.request_focus();
                }
            }
        });
    }
    changed
}

// ---- view ----------------------------------------------------------------------------------------

use widgets::chip;

/// Width of the Events list pane.
const LIST_W: f32 = 300.0;

/// `text` cut with "…" so it fits `max_w` in `font`.
fn fit_text(ui: &egui::Ui, text: &str, font: &FontId, max_w: f32) -> String {
    let width = |s: &str| ui.painter().layout_no_wrap(s.to_string(), font.clone(), Color32::WHITE).size().x;
    if width(text) <= max_w {
        return text.to_string();
    }
    let chars: Vec<char> = text.chars().collect();
    let (mut lo, mut hi) = (0, chars.len());
    while lo < hi {
        let mid = (lo + hi).div_ceil(2);
        let s: String = chars[..mid].iter().chain(['…'].iter()).collect();
        if width(&s) <= max_w { lo = mid } else { hi = mid - 1 }
    }
    format!("{}…", chars[..lo].iter().collect::<String>().trim_end())
}

/// "Run Hype" / "Run Hype +2" (the first step, and how many more).
fn first_action(cmds: &[String], n: &Names) -> String {
    let steps: Vec<&String> = cmds.iter().filter(|c| !c.trim().is_empty()).collect();
    match steps.as_slice() {
        [] => "Does nothing yet".into(),
        [one] => capitalize(&step_phrase(one, n)),
        [first, rest @ ..] => format!("{} +{}", capitalize(&step_phrase(first, n)), rest.len()),
    }
}

fn rows(app: &mut App) -> Arc<Vec<Row>> {
    let key = (app.m.q_seq("rules"), app.m.q_seq("presets"), app.m.q_seq("scenes"), app.m.q_seq("alerts.config"));
    if let Some((k, r)) = &app.build.rules.cache
        && *k == key
    {
        return r.clone();
    }
    let names = Names::read(app);
    let mut out: Vec<Row> = app
        .m
        .q_list("rules")
        .iter()
        .map(|r| {
            let s = |k: &str| r.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
            let when = s("when");
            let cmds: Vec<String> = r.get_path("do").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str().map(String::from)).collect();
            Row {
                name: s("name"),
                family: Family::of(&when),
                icon: event_icon(&when),
                title: when_phrase(&when, &s("if")),
                subtitle: format!("→ {}", first_action(&cmds, &names)),
                enabled: r.get_path("enabled").is_some_and(Value::truthy),
                fired_ms: r.get_path("last_fired_ms_ago").and_then(Value::as_i64),
                value: r.clone(),
            }
        })
        .collect();
    out.sort_by_key(|r| r.family);
    let out = Arc::new(out);
    app.build.rules.cache = Some((key, out.clone()));
    out
}

fn ago(ms: i64) -> String {
    let s = ms / 1000;
    if s < 60 {
        "just now".into()
    } else if s < 3600 {
        format!("{} min ago", s / 60)
    } else if s < 86_400 {
        format!("{} h ago", s / 3600)
    } else {
        format!("{} days ago", s / 86_400)
    }
}

/// What the list pane asks for.
enum ListPick {
    New,
    Open(usize),
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let rows = rows(app);
    let selected = app.build.rules.edit.as_ref().map(|d| d.orig.clone().unwrap_or_default());
    let pick = if ui.available_width() < 820.0 {
        // narrow (docked panel): the list, or the trigger with a way back
        if app.build.rules.edit.is_some() {
            if widgets::button_ex(ui, &t, Some(icon::LEFT), "All triggers", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                app.build.rules.edit = None;
            }
            ui.add_space(spacing::S);
            detail(app, ui, &t);
            None
        } else {
            list_pane(ui, &t, &rows, None)
        }
    } else {
        widgets::split(ui, LIST_W, |ui| list_pane(ui, &t, &rows, selected.as_deref()), |ui| detail(app, ui, &t)).0
    };
    match pick {
        Some(ListPick::New) => app.build.rules.new_rule(),
        Some(ListPick::Open(i)) => {
            if let Some(r) = rows.get(i) {
                app.build.rules.open(&r.value);
            }
        }
        None => {}
    }
}

fn list_pane(ui: &mut egui::Ui, t: &Theme, rows: &[Row], selected: Option<&str>) -> Option<ListPick> {
    let mut pick = widgets::pane_header(ui, t, "Events", Some(rows.len()), Some("New trigger")).then_some(ListPick::New);
    egui::ScrollArea::vertical().id_salt("rules-list").auto_shrink([false, false]).show(ui, |ui| {
        if rows.is_empty() {
            widgets::hint(ui, t, "No triggers yet.");
        }
        let mut last = None;
        for (i, r) in rows.iter().enumerate() {
            if last != Some(r.family) {
                widgets::group_label(ui, t, r.family.title());
                last = Some(r.family);
            }
            let sel = selected == Some(r.name.as_str());
            let tip = match r.fired_ms {
                Some(ms) => format!("{}\nLast ran {}", r.title, ago(ms)),
                None => format!("{}\nHasn't run yet", r.title),
            };
            if event_row(ui, t, r, sel).on_hover_text(tip).clicked() && !sel {
                pick = Some(ListPick::Open(i));
            }
        }
    });
    pick
}

/// A trigger in the list: its event's icon, "When …", "→ first step", and an on/off dot.
fn event_row(ui: &mut egui::Ui, t: &Theme, r: &Row, selected: bool) -> egui::Response {
    // icon column + padding + the dot on the right
    let text_w = ui.available_width() - 12.0 - 28.0 - 12.0 - 18.0;
    let title = fit_text(ui, &r.title, &font_medium(type_scale::BODY), text_w);
    let subtitle = fit_text(ui, &r.subtitle, &font(type_scale::SMALL), text_w);
    let resp = widgets::list_row(ui, t, r.icon, &title, &subtitle, "", selected);
    let c = egui::pos2(resp.rect.right() - 16.0, resp.rect.center().y);
    let recent = r.fired_ms.is_some_and(|ms| ms < 2500);
    if r.enabled {
        ui.painter().circle_filled(c, 4.0, if recent { t.accent } else { t.green });
    } else {
        ui.painter().circle_stroke(c, 3.5, egui::Stroke::new(1.2, t.text_faint));
    }
    resp
}

// ---- detail --------------------------------------------------------------------------------------

/// Fill cooldown/modes/scenes from the full definitions and fetch the file for write mode.
fn fill(app: &mut App) {
    let Some(d) = app.build.rules.edit.as_ref() else { return };
    if d.filled {
        return;
    }
    let name = d.orig.clone().unwrap_or_default();
    let file = d.file.clone();
    match app.m.q("config.rules").cloned() {
        None => app.m.query("config.rules", Value::Null),
        Some(cfg) => {
            if let Some(def) = cfg.as_list().and_then(|l| l.iter().find(|r| r.get_path("name").and_then(Value::as_str) == Some(name.as_str()))) {
                let dur = |v: Option<&Value>| {
                    v.map(|v| v.as_str().map(String::from).unwrap_or_else(|| format!("{v}ms"))).filter(|s| s != "nullms").unwrap_or_default()
                };
                let global = dur(def.get_path("cooldown.global").filter(|v| !v.is_null()));
                let actor = dur(def.get_path("cooldown.per_actor").filter(|v| !v.is_null()));
                let join = |k: &str| def.get_path(k).and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ");
                let (modes, scenes) = (join("modes"), join("scenes"));
                // the saved form learns them too, so filling in isn't an unsaved change
                let ed = &mut app.build.rules;
                for d in [ed.edit.as_mut(), ed.base.as_mut()].into_iter().flatten().filter(|d| d.orig.as_deref() == Some(name.as_str())) {
                    d.cooldown_global = global.clone();
                    d.cooldown_actor = actor.clone();
                    d.modes = modes.clone();
                    d.scenes = scenes.clone();
                    d.filled = true;
                }
            }
            app.m.query_as(&format!("project.read:{file}"), "project.read", Value::map().with("path", file));
        }
    }
}

/// Lists behind the step pickers (saved actions, modes, cue lists, looks, animations, sounds,
/// mixes, timelines, notifications), refreshed slowly.
fn fetch_lists(app: &mut App, now: f64) {
    if !app.m.connected || now - app.build.rules.lists_at < 5.0 {
        return;
    }
    app.build.rules.lists_at = now;
    for q in ["presets", "modes", "lights.cuelists", "lights.palettes", "patches", "audio.mix", "mixer.snapshots", "timelines", "alerts.config"] {
        app.m.query(q, Value::Null);
    }
}

/// Re-read the guided "only if" rows when the expression changed underneath them.
fn sync_rows(app: &mut App) {
    let ed = &mut app.build.rules;
    let Some(d) = ed.edit.as_ref() else { return };
    if ed.rows_src.as_deref() == Some(d.cond.as_str()) {
        return;
    }
    match parse_conds(&d.cond) {
        Some(r) => {
            ed.rows = r;
            ed.custom_cond = false;
        }
        None => {
            ed.rows.clear();
            ed.custom_cond = true;
        }
    }
    ed.rows_src = Some(d.cond.clone());
}

fn file_text(app: &App, file: &str) -> Option<String> {
    app.m.q(&format!("project.read:{file}")).and_then(|v| v.get_path("text")).and_then(Value::as_str).map(String::from)
}

fn detail(app: &mut App, ui: &mut egui::Ui, t: &Theme) {
    if app.build.rules.edit.is_none() {
        if !app.m.connected {
            widgets::empty_state(ui, t, icon::WARN, "Stream Engine isn't running", "Your triggers show up here once it's running.", None);
        } else if widgets::empty_state(
            ui,
            t,
            icon::BOLT,
            "Pick a trigger",
            "A trigger does something by itself when something happens on stream, in the music or in your show.",
            Some("New trigger"),
        ) {
            app.build.rules.new_rule();
        }
        return;
    }
    fill(app);
    sync_rows(app);
    sync_test_args(app);
    let names = Names::read(app);
    header(app, ui, t, &names);
    egui::ScrollArea::vertical().id_salt("rule-editor").auto_shrink([false, false]).show(ui, |ui| {
        ui.set_max_width(ui.available_width().min(980.0));
        when_section(app, ui, t, &names);
        if_section(app, ui, t, &names);
        do_section(app, ui, t);
        try_section(app, ui, t);
        file_section(app, ui, t);
        ui.add_space(spacing::XL);
    });
}

fn header(app: &mut App, ui: &mut egui::Ui, t: &Theme, names: &Names) {
    let Some(d) = app.build.rules.edit.clone() else { return };
    let dirty = app.build.rules.dirty();
    let is_new = d.orig.is_none();
    let errs = d.errors();
    let (mut save, mut discard, mut test) = (false, false, false);
    let mut on = d.enabled;
    let mut flipped = false;
    let title = if d.when.trim().is_empty() { "New trigger".to_string() } else { when_phrase(&d.when, &d.cond) };
    widgets::detail_header(ui, t, event_icon(&d.when), &title, &format!("→ {}", do_phrase(&d.commands, names)), |ui| {
        if dirty {
            let (label, tip) = if is_new { ("Create", "Save this trigger") } else { ("Save", "Save your changes") };
            let tip = if errs.is_empty() { tip } else { "Finish the notes below first" };
            save = widgets::button_ex(ui, t, Some(icon::CHECK), label, Kind::Primary, Size::Medium, 0.0, errs.is_empty()).on_hover_text(tip).clicked();
            discard = widgets::button_ex(ui, t, None, if is_new { "Cancel" } else { "Discard" }, Kind::Ghost, Size::Medium, 0.0, true).clicked();
        }
        test = widgets::button_ex(ui, t, Some(icon::PLAY), "Test", Kind::Secondary, Size::Medium, 0.0, !d.when.trim().is_empty() && !is_new)
            .on_hover_text(if is_new { "Create it first, then test it." } else { "Plays the event as if it really happened (the saved version)." })
            .clicked();
        ui.add_space(spacing::S);
        flipped = widgets::toggle(ui, t, &mut on).on_hover_text(if on { "On: it runs when this happens." } else { "Off: kept, but it won't run." }).changed();
    });
    if dirty && !errs.is_empty() {
        let (tone, title) = if is_new { (widgets::Tone::Info, "To create it") } else { (widgets::Tone::Warn, "Almost there") };
        widgets::callout(ui, t, tone, icon::INFO, title, &errs.join("\n"), None);
        ui.add_space(spacing::M);
    }
    let ed = &mut app.build.rules;
    if flipped {
        for x in [ed.edit.as_mut(), ed.base.as_mut()].into_iter().flatten() {
            x.enabled = on;
        }
        // a saved trigger switches right away, and stays that way
        if let Some(e) = ed.edit.clone().filter(|e| e.orig.is_some()) {
            let text = file_text(app, &e.file);
            app.m.text(&format!("{} '{}'", if on { "rule.enable" } else { "rule.disable" }, e.name.trim()));
            app.m.action("project.write", e.enabled_args(text.as_deref()));
            app.m.refresh_soon();
        }
    }
    if save {
        let mut d = app.build.rules.edit.clone().unwrap_or_else(|| d.clone());
        if d.name.trim().is_empty() {
            let taken: Vec<String> = app.m.q_list("rules").iter().filter_map(|r| r.get_path("name").and_then(Value::as_str).map(String::from)).collect();
            d.name = d.auto_name(&taken);
        }
        let text = file_text(app, &d.file);
        app.m.action("project.write", d.write_args(text.as_deref()));
        d.orig = Some(d.name.trim().to_string());
        app.build.rules.base = Some(d.clone());
        app.build.rules.edit = Some(d.clone());
        app.m.toast(if is_new { "Trigger created." } else { "Trigger saved." }, false);
        app.m.refresh_soon();
        app.m.query_as(&format!("project.read:{}", d.file), "project.read", Value::map().with("path", d.file.clone()));
    }
    if discard {
        let ed = &mut app.build.rules;
        ed.edit = ed.base.clone();
        ed.rows_src = None;
        ed.other_event = ed.edit.as_ref().is_some_and(|d| !d.when.is_empty() && trigger_index(&d.when).is_none());
    }
    if test {
        app.m.text(&d.test_command());
    }
}

/// "Try it with" values → simulator args (`bits=1500 user=Alex`).
fn sync_test_args(app: &mut App) {
    let ed = &mut app.build.rules;
    let Some(d) = ed.edit.as_mut() else { return };
    let args: Vec<String> = ed.test_vals.iter().filter(|(_, v)| !v.trim().is_empty()).map(|(k, v)| format!("{k}={}", quote_arg(v.trim()))).collect();
    d.test_args = args.join(" ");
}

fn when_section(app: &mut App, ui: &mut egui::Ui, t: &Theme, names: &Names) {
    let when = app.build.rules.edit.as_ref().map(|d| d.when.clone()).unwrap_or_default();
    let other = app.build.rules.other_event;
    let cur = if other { None } else { trigger_index(&when) };
    let (mut pick, mut pick_other) = (None::<String>, false);
    widgets::inspector_section(
        ui,
        t,
        "rule-when",
        "When",
        true,
        |_| {},
        |ui| {
            for fam in Family::PICKER {
                widgets::group_label(ui, t, fam.title());
                ui.horizontal_wrapped(|ui| {
                    for (i, tr) in TRIGGERS.iter().enumerate().filter(|(_, x)| x.family == fam) {
                        let on = cur == Some(i);
                        if chip(ui, t, tr.icon, tr.label, on).clicked() && !on {
                            pick = Some(tr.pattern.to_string());
                        }
                    }
                });
            }
            ui.add_space(spacing::S);
            if chip(ui, t, icon::EDIT, "Something else…", other).clicked() && !other {
                pick_other = true;
            }
            let mode = when.strip_prefix("mode.enter.").map(|m| ("mode.enter.", m)).or_else(|| when.strip_prefix("mode.exit.").map(|m| ("mode.exit.", m)));
            if let Some((prefix, m)) = mode.filter(|_| !other) {
                ui.add_space(spacing::S);
                widgets::prop_row(ui, t, "Which mode", |ui| {
                    let text = if m == "*" || m.is_empty() { "Any mode".to_string() } else { Names::label(&names.modes, m) };
                    egui::ComboBox::from_id_salt("rule-when-mode").width(220.0).selected_text(text).show_ui(ui, |ui| {
                        if ui.selectable_label(m == "*", "Any mode").clicked() {
                            pick = Some(format!("{prefix}*"));
                        }
                        for (n, l) in &names.modes {
                            if ui.selectable_label(m == n, l).clicked() {
                                pick = Some(format!("{prefix}{n}"));
                            }
                        }
                    });
                });
            }
            if other {
                ui.add_space(spacing::S);
                widgets::prop_row(ui, t, "Event name", |ui| {
                    ui.vertical(|ui| {
                        field(app, ui, "when", "Start typing, then pick a suggestion", |d| &mut d.when);
                        widgets::hint(ui, t, "Suggestions include events that happened recently.");
                    });
                });
            }
        },
    );
    let ed = &mut app.build.rules;
    if let (Some(p), Some(d)) = (pick, ed.edit.as_mut()) {
        d.when = p;
        ed.other_event = false;
    }
    if pick_other {
        ed.other_event = true;
        if let Some(d) = ed.edit.as_mut().filter(|d| trigger_index(&d.when).is_some()) {
            d.when.clear();
        }
    }
}

fn if_section(app: &mut App, ui: &mut egui::Ui, t: &Theme, names: &Names) {
    let when = app.build.rules.edit.as_ref().map(|d| d.when.clone()).unwrap_or_default();
    let fields = fields_for(&when);
    let custom = app.build.rules.custom_cond;
    let mut rows = app.build.rules.rows.clone();
    let (mut changed, mut clear) = (false, false);
    widgets::inspector_section(
        ui,
        t,
        "rule-if",
        "Only if",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, t, "Optional. Leave it all empty to run every time.");
            ui.add_space(spacing::S);
            if custom {
                widgets::hint(ui, t, "This trigger uses a custom check. Change it here, or clear it to use the simple choices.");
                field(app, ui, "if", "event.bits >= 1000 && mode == 'live'", |d| &mut d.cond);
                clear = widgets::button_ex(ui, t, Some(icon::BROOM), "Clear the check", Kind::Secondary, Size::Small, 0.0, true).clicked();
            } else {
                let mut remove = None;
                for (i, c) in rows.iter_mut().enumerate() {
                    ui.horizontal(|ui| {
                        changed |= cond_row(ui, i, c, &fields, names);
                        if widgets::icon_button(ui, t, icon::CROSS, "Remove this check").clicked() {
                            remove = Some(i);
                        }
                    });
                }
                if let Some(i) = remove {
                    rows.remove(i);
                    changed = true;
                }
                let label = if rows.is_empty() { "Add a check" } else { "Add another check" };
                if widgets::button_ex(ui, t, Some(icon::PLUS), label, Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    rows.push(Cond::new(fields.iter().copied().find(|f| field_info(f).1 == Some(FieldKind::Num)).unwrap_or("mode")));
                    changed = true;
                }
            }
            ui.add_space(spacing::M);
            let Some(d) = app.build.rules.edit.as_mut() else { return };
            widgets::prop_row(ui, t, "Don't repeat for", |ui| {
                ui.add(widgets::field(&mut d.cooldown_global).hint_text("10s").desired_width(80.0));
                widgets::hint(ui, t, "Same viewer waits");
                ui.add(widgets::field(&mut d.cooldown_actor).hint_text("5m").desired_width(80.0));
            });
            widgets::prop_row(ui, t, "Show modes", |ui| limit_chips(ui, t, &mut d.modes, &names.modes, "Any mode"));
            widgets::prop_row(ui, t, "Scenes", |ui| limit_chips(ui, t, &mut d.scenes, &names.scenes, "Any scene"));
        },
    );
    let ed = &mut app.build.rules;
    if clear && let Some(d) = ed.edit.as_mut() {
        d.cond.clear();
    }
    if changed && let Some(d) = ed.edit.as_mut() {
        d.cond = conds_text(&rows);
        ed.rows_src = Some(d.cond.clone());
        ed.rows = rows;
    }
}

/// One "only if" row: what to check, how, and the value. Returns true when edited.
fn cond_row(ui: &mut egui::Ui, i: usize, c: &mut Cond, fields: &[&str], names: &Names) -> bool {
    let mut changed = false;
    let (label, ..) = field_info(&c.field);
    egui::ComboBox::from_id_salt(("rule-cond-field", i)).width(180.0).selected_text(label).show_ui(ui, |ui| {
        for f in fields.iter().copied().chain(["mode"]) {
            if ui.selectable_label(c.field == f, field_info(f).0).clicked() && c.field != f {
                *c = Cond::new(f);
                changed = true;
            }
        }
    });
    let ops: &[(CondOp, &str)] = match c.kind {
        FieldKind::Num => &NUM_OPS,
        FieldKind::Strength => &[(CondOp::Ge, "at least"), (CondOp::Le, "at most")],
        FieldKind::Text | FieldKind::Mode => &TEXT_OPS,
        FieldKind::Bool => &[(CondOp::Yes, "yes"), (CondOp::No, "no")],
    };
    let cur = ops.iter().find(|(o, _)| *o == c.op).map_or("…", |(_, l)| *l);
    egui::ComboBox::from_id_salt(("rule-cond-op", i)).width(110.0).selected_text(cur).show_ui(ui, |ui| {
        for (o, l) in ops {
            if ui.selectable_label(c.op == *o, *l).clicked() && c.op != *o {
                c.op = *o;
                changed = true;
            }
        }
    });
    match c.kind {
        FieldKind::Bool => {}
        FieldKind::Mode => {
            let text = if c.value.is_empty() { "Pick a mode".to_string() } else { Names::label(&names.modes, &c.value) };
            egui::ComboBox::from_id_salt(("rule-cond-mode", i)).width(160.0).selected_text(text).show_ui(ui, |ui| {
                for (n, l) in &names.modes {
                    if ui.selectable_label(&c.value == n, l).clicked() {
                        c.value = n.clone();
                        changed = true;
                    }
                }
            });
        }
        FieldKind::Num => {
            changed |= ui.add(widgets::field(&mut c.value).hint_text("1000").desired_width(90.0)).changed();
        }
        FieldKind::Strength => {
            let mut v = c.value.trim().parse::<f32>().unwrap_or(0.5).clamp(0.0, 1.0);
            ui.spacing_mut().slider_width = 160.0;
            if ui.add(egui::Slider::new(&mut v, 0.0..=1.0).show_value(false)).changed() {
                c.value = format!("{v:.2}");
                changed = true;
            }
            ui.label(format!("{:.0}%", v * 100.0));
        }
        FieldKind::Text => {
            changed |= ui.add(widgets::field(&mut c.value).hint_text("Type the text").desired_width(200.0)).changed();
        }
    }
    changed
}

fn do_section(app: &mut App, ui: &mut egui::Ui, t: &Theme) {
    let Some((mut cmds, when)) = app.build.rules.edit.as_ref().map(|d| (d.commands.clone(), d.when.clone())) else { return };
    let mut changed = false;
    widgets::inspector_section(
        ui,
        t,
        "rule-do",
        "Do",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, t, "Top to bottom, in order.");
            ui.add_space(spacing::S);
            changed = steps_editor(app, ui, "", &mut cmds, &when);
        },
    );
    if changed && let Some(d) = app.build.rules.edit.as_mut() {
        d.commands = cmds;
    }
}

/// Pretend values for testing (the event's numbers and names) and a Test button.
fn try_section(app: &mut App, ui: &mut egui::Ui, t: &Theme) {
    let Some(d) = app.build.rules.edit.clone() else { return };
    if d.orig.is_none() {
        return;
    }
    let mut test = false;
    widgets::inspector_section(
        ui,
        t,
        "rule-try",
        "Test with pretend values",
        false,
        |_| {},
        |ui| {
            let fields: Vec<&str> = fields_for(&d.when)
                .into_iter()
                .filter(|f| matches!(field_info(f).1, Some(FieldKind::Num | FieldKind::Strength | FieldKind::Text)) && !matches!(*f, "page" | "currency"))
                .take(3)
                .collect();
            let vals = &mut app.build.rules.test_vals;
            vals.retain(|(k, _)| fields.contains(&k.as_str()));
            for f in &fields {
                if !vals.iter().any(|(k, _)| k == f) {
                    vals.push((f.to_string(), String::new()));
                }
            }
            if vals.is_empty() {
                widgets::hint(ui, t, "This event has nothing to fill in.");
            }
            for (k, v) in vals.iter_mut() {
                let sample = match k.as_str() {
                    "bits" => "1500",
                    "viewers" => "25",
                    "tier" => "2",
                    "months" => "6",
                    "count" => "5",
                    "amount" => "5",
                    "cost" => "500",
                    "velocity" => "0.8",
                    "user" | "from" => "Alex",
                    "reward" => "Hydrate",
                    _ => "",
                };
                widgets::prop_row(ui, t, &field_info(k).0, |ui| {
                    ui.add(widgets::field(v).hint_text(sample).desired_width(if field_info(k).1 == Some(FieldKind::Text) { 200.0 } else { 100.0 }));
                });
            }
            ui.add_space(spacing::S);
            test = widgets::button_ex(ui, t, Some(icon::PLAY), "Test with these", Kind::Secondary, Size::Small, 0.0, !d.when.trim().is_empty())
                .on_hover_text("Plays the event as if it really happened, with these values. Nothing is sent to Twitch.")
                .clicked();
        },
    );
    if test {
        sync_test_args(app);
        if let Some(d) = app.build.rules.edit.as_ref() {
            app.m.text(&d.test_command());
        }
    }
}

/// Name, file, the exact text the engine runs, and delete.
fn file_section(app: &mut App, ui: &mut egui::Ui, t: &Theme) {
    let Some(d) = app.build.rules.edit.clone() else { return };
    let mut delete = false;
    widgets::inspector_section(
        ui,
        t,
        "rule-file",
        "File",
        false,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Name", |ui| {
                if let Some(e) = app.build.rules.edit.as_mut() {
                    ui.add(widgets::field(&mut e.name).hint_text("Made from the sentence when you save").desired_width(320.0));
                }
            });
            widgets::prop_row(ui, t, "Saved in", |ui| {
                if let Some(e) = app.build.rules.edit.as_mut() {
                    if e.orig.is_none() {
                        ui.add(widgets::field(&mut e.file).font(font_mono(type_scale::SMALL + 0.5)).desired_width(240.0));
                    } else {
                        ui.label(RichText::new(&e.file).font(font_mono(type_scale::SMALL + 0.5)).color(t.fg));
                    }
                }
                if widgets::button_ex(ui, t, Some(icon::CONSOLE), "Open the file", Kind::Ghost, Size::Small, 0.0, d.orig.is_some()).clicked() {
                    app.open_in_editor(&d.file, None);
                }
            });
            widgets::hint(ui, t, "Buttons and chat commands use the name to switch it on or off. Below: the exact text Stream Engine runs.");
            ui.add_space(spacing::S);
            ui.label(RichText::new("When").color(t.text_dim));
            field(app, ui, "raw-when", "twitch.cheer · mode.enter.brb · band.drop", |d| &mut d.when);
            ui.label(RichText::new("Only if").color(t.text_dim));
            field(app, ui, "raw-if", "event.bits >= 1000 && mode == 'live'", |d| &mut d.cond);
            ui.label(RichText::new("Do (one command per line, in order; wait 2s pauses)").color(t.text_dim));
            let n = app.build.rules.edit.as_ref().map_or(0, |d| d.commands.len());
            for i in 0..n {
                field(app, ui, &format!("raw-do:{i}"), "preset.fire hype · bot.say 'thanks {user}!'", move |d| &mut d.commands[i]);
            }
            if d.orig.is_some() {
                ui.add_space(spacing::M);
                ui.horizontal(|ui| {
                    delete = widgets::hold_button(ui, t, "Delete trigger", t.bright_red, 0.6);
                    widgets::hint(ui, t, "Press and hold to delete.");
                });
            }
        },
    );
    if delete {
        let text = file_text(app, &d.file);
        if let Some(args) = d.delete_args(text.as_deref()) {
            app.m.action("project.write", args);
            app.m.toast(format!("Deleted “{}”.", d.name.trim()), false);
        }
        app.build.rules.edit = None;
        app.build.rules.base = None;
        app.m.refresh_soon();
    }
}

// ---- steps ---------------------------------------------------------------------------------------

/// The guided step rows plus "Add a step", editing command lines in place: a trigger's "Do",
/// a saved action's steps, a scene's "When this scene comes on". `salt` keeps the widgets of
/// different lists apart; `when` is the event the steps answer (for the chat message "Insert"
/// chips; empty = none). Returns true when `cmds` changed.
pub fn steps_editor(app: &mut App, ui: &mut egui::Ui, salt: &str, cmds: &mut Vec<String>, when: &str) -> bool {
    let t = app.t.clone();
    fetch_lists(app, ui.input(|i| i.time));
    let names = Names::read(app);
    let layers = if cmds.iter().any(|c| matches!(parse_step(c).0, Step::LayerFx | Step::LayerVisible)) { Layers::read(app) } else { Layers::default() };
    let (mut edit, mut remove, mut up) = (None, None, None);
    for (i, cmd) in cmds.iter().enumerate() {
        step_row(app, ui, &t, (&names, &layers), salt, i, cmd, when, &mut edit, &mut remove, &mut up);
        ui.add_space(spacing::XS);
    }
    if cmds.is_empty() {
        widgets::hint(ui, &t, "Nothing yet. Add the first thing it should do.");
    }
    ui.add_space(spacing::XS);
    let mut add = None;
    egui::ComboBox::from_id_salt((salt, "rule-step-add"))
        .width(240.0)
        .selected_text(RichText::new(format!("{}  Add a step", icon::PLUS)).color(t.fg))
        .show_ui(ui, |ui| step_menu(ui, None, &mut add));
    let mut changed = false;
    if let Some((i, c)) = edit
        && let Some(slot) = cmds.get_mut(i)
        && *slot != c
    {
        *slot = c;
        changed = true;
    }
    if let Some(i) = up
        && i > 0
        && i < cmds.len()
    {
        cmds.swap(i - 1, i);
        changed = true;
    }
    if let Some(i) = remove
        && i < cmds.len()
    {
        cmds.remove(i);
        changed = true;
    }
    if let Some(k) = add {
        cmds.push(k.blank());
        changed = true;
    }
    changed
}

/// The step kinds, grouped, as selectable rows.
fn step_menu(ui: &mut egui::Ui, cur: Option<Step>, pick: &mut Option<Step>) {
    for (g, group) in Step::MENU.iter().enumerate() {
        if g > 0 {
            ui.separator();
        }
        for &s in *group {
            if ui.selectable_label(cur == Some(s), format!("{}  {}", s.icon(), s.label())).clicked() && cur != Some(s) {
                *pick = Some(s);
            }
        }
    }
}

/// What a step still needs before it can run, in words ("pick a saved action"); `None` = ready.
pub fn step_missing(cmd: &str) -> Option<String> {
    if cmd.trim().is_empty() {
        return Some("say what it should do".into());
    }
    let (kind, args) = parse_step(cmd);
    if let Some(what) = kind.missing(&args) {
        return Some(what.into());
    }
    Op::parse(cmd).err().map(|e| format!("fix a mistake: {e}"))
}

/// A name picker over `opts`; a text box when the engine hasn't listed any. Returns a new pick.
pub fn pick_name(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    cur: &str,
    opts: &[(String, String)],
    placeholder: &str,
    width: f32,
) -> Option<String> {
    if opts.is_empty() {
        let mut s = cur.to_string();
        return ui.add(widgets::field(&mut s).hint_text(placeholder).desired_width(width)).changed().then_some(s);
    }
    let text = if cur.is_empty() { placeholder.to_string() } else { Names::label(opts, cur) };
    let mut out = None;
    egui::ComboBox::from_id_salt(id).width(width).selected_text(text).show_ui(ui, |ui| {
        for (name, label) in opts {
            if ui.selectable_label(name == cur, label).clicked() {
                out = Some(name.clone());
            }
        }
    });
    out
}

/// Steps whose pickers always go on their own line under the step's kind.
fn wide(kind: Step) -> bool {
    matches!(kind, Step::LayerFx | Step::LayerVisible | Step::Setting | Step::Notify | Step::Custom)
}

#[allow(clippy::too_many_arguments)]
fn step_row(
    app: &mut App,
    ui: &mut egui::Ui,
    t: &Theme,
    (names, layers): (&Names, &Layers),
    salt: &str,
    i: usize,
    cmd: &str,
    when: &str,
    edit: &mut Option<(usize, String)>,
    remove: &mut Option<usize>,
    up: &mut Option<usize>,
) {
    let (kind, args) = parse_step(cmd);
    let a0 = args.first().cloned().unwrap_or_default();
    egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(radius::CONTROL)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        // narrow places (a side panel): what to pick goes on its own line
        let own_line = wide(kind) || ui.available_width() < 560.0;
        let mut new_cmd = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{}", i + 1)).font(font_semibold(type_scale::BODY)).color(t.text_dim));
            ui.label(RichText::new(kind.icon()).color(t.text_dim));
            let mut k = None;
            let kind_w = if own_line { (ui.available_width() - 70.0).clamp(120.0, 260.0) } else { 220.0 };
            egui::ComboBox::from_id_salt((salt, "rule-step-kind", i))
                .width(kind_w)
                .selected_text(kind.label())
                .show_ui(ui, |ui| step_menu(ui, Some(kind), &mut k));
            if let Some(k) = k {
                *edit = Some((i, if k == Step::Custom { cmd.to_string() } else { k.blank() }));
            }
            if !own_line {
                let w = (ui.available_width() - 90.0).clamp(140.0, 360.0);
                new_cmd = step_args(ui, t, names, (salt, i), kind, &args, w);
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::icon_button(ui, t, icon::CROSS, "Remove this step").clicked() {
                    *remove = Some(i);
                }
                if i > 0 && widgets::icon_button(ui, t, icon::UP, "Move up").clicked() {
                    *up = Some(i);
                }
            });
        });
        if own_line {
            ui.add_space(spacing::XS);
            let id = (salt, i);
            match kind {
                Step::LayerFx | Step::LayerVisible => new_cmd = layer_args(ui, t, names, layers, id, kind, &args),
                Step::Setting => new_cmd = setting_args(app, ui, t, &format!("addr:{salt}{i}"), &args),
                Step::Notify => {
                    let (c, open) = notify_args(ui, t, names, id, &args, when);
                    new_cmd = c;
                    if open {
                        app.open_view(ViewId::Alerts);
                    }
                }
                Step::Custom => {
                    let mut s = cmd.to_string();
                    if text_field(app, ui, &format!("{salt}do:{i}"), "e.g. preset.fire hype · bot.say 'thanks {user}!'", &mut s) {
                        new_cmd = Some(s);
                    }
                    if !cmd.trim().is_empty() {
                        widgets::hint(ui, t, &format!("Does: {}", custom_phrase(cmd)));
                    }
                }
                _ => {
                    ui.horizontal(|ui| {
                        let w = ui.available_width();
                        new_cmd = step_args(ui, t, names, id, kind, &args, w);
                    });
                }
            }
        }
        if let Some(c) = new_cmd {
            *edit = Some((i, c));
        }
        if kind == Step::Say {
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Insert").color(t.text_dim));
                let extra =
                    fields_for(when).into_iter().filter(|f| !matches!(*f, "user" | "message" | "input" | "is_gift" | "automatic" | "down" | "key" | "page"));
                // "their name" only means something when a viewer did something
                let user = (!when.trim().is_empty()).then_some("user");
                for f in user.into_iter().chain(extra) {
                    let code = format!("{{{f}}}");
                    let label = if f == "user" { "Their name".to_string() } else { field_info(f).0 };
                    if widgets::chip(ui, t, "", &label, false).on_hover_text(format!("Becomes {} when it happens", placeholder_words(f))).clicked() {
                        let text = if a0.is_empty() { code } else { format!("{a0} {code}") };
                        *edit = Some((i, build_step(kind, &[&text])));
                    }
                }
            });
        }
    });
}

/// The pickers of one simple step (what to run, which scene, …). Returns the new command line.
fn step_args(ui: &mut egui::Ui, t: &Theme, names: &Names, id: (&str, usize), kind: Step, args: &[String], w: f32) -> Option<String> {
    let a0 = args.first().cloned().unwrap_or_default();
    let pid = (id.0, "rule-step-arg", id.1);
    let new_arg = match kind {
        Step::Preset | Step::Release => pick_name(ui, pid, &a0, &names.presets, "Pick a saved action", w),
        Step::Animation => pick_name(ui, pid, &a0, &names.animations, "Pick a source", w),
        Step::Scene | Step::Preview => pick_name(ui, pid, &a0, &names.scenes, "Pick a scene", w),
        Step::Look => pick_name(ui, pid, &a0, &names.looks, "Pick a look", w),
        Step::Sound => pick_name(ui, pid, &a0, &names.sounds, "Pick a sound", w),
        Step::Mix => pick_name(ui, pid, &a0, &names.mixes, "Pick a mix", w),
        Step::Mode => pick_name(ui, pid, &a0, &names.modes, "Pick a mode", w),
        Step::TimelinePlay | Step::TimelineStop => pick_name(ui, pid, &a0, &names.timelines, "Pick a timeline", w),
        Step::Lights => {
            let cue = args.get(1).cloned().unwrap_or_default();
            let mut out = None;
            if let Some(l) = pick_name(ui, pid, &a0, &names.cuelists, "Pick a cue list", (w - 100.0).max(100.0)) {
                out = Some(build_step(kind, &[&l, &cue]));
            }
            let mut c = cue.clone();
            if ui.add(widgets::field(&mut c).hint_text("cue (optional)").desired_width(90.0)).changed() {
                out = Some(build_step(kind, &[&a0, &c]));
            }
            return out;
        }
        Step::Say => {
            let mut s = a0.clone();
            ui.add(widgets::field(&mut s).hint_text("Thanks for the support!").desired_width(w)).changed().then_some(s)
        }
        Step::Wait => {
            let mut s = a0.clone();
            let r = ui.add(widgets::field(&mut s).hint_text("2s").desired_width(70.0)).changed().then_some(s);
            widgets::hint(ui, t, "like 2s or 1m");
            r
        }
        Step::Marker => {
            let mut s = a0.clone();
            ui.add(widgets::field(&mut s).hint_text("What happened (optional)").desired_width(w)).changed().then_some(s)
        }
        Step::LayerFx | Step::LayerVisible | Step::Setting | Step::Notify | Step::Custom => None,
    };
    new_arg.map(|v| build_step(kind, &[&v]))
}

/// Scene → layer (→ effect) pickers and On / Off / Switch for a layer step.
fn layer_args(ui: &mut egui::Ui, t: &Theme, names: &Names, layers: &Layers, id: (&str, usize), kind: Step, args: &[String]) -> Option<String> {
    let fx = kind == Step::LayerFx;
    let a0 = args.first().map(String::as_str).unwrap_or("");
    let mode = args.get(1).map(String::as_str).unwrap_or("on");
    let (s, l, f) = layer_parts(a0).unwrap_or(("", "", None));
    let scenes: Vec<(String, String)> = layers
        .scenes(fx)
        .into_iter()
        .map(|s| {
            let label = Names::label(&names.scenes, &s);
            (s, label)
        })
        .collect();
    if scenes.is_empty() && s.is_empty() {
        widgets::hint(ui, t, if fx { "No layer has an effect yet. Add one to a layer in Scenes." } else { "No scene has a layer yet." });
        return None;
    }
    let blank_fx = fx.then_some("");
    let mut addr = None;
    let mut new_mode = None;
    ui.horizontal_wrapped(|ui| {
        let w = ((ui.available_width() - 230.0) / if fx { 3.0 } else { 2.0 }).clamp(110.0, 220.0);
        if let Some(ns) = pick_name(ui, (id.0, "layer-scene", id.1), s, &scenes, "Scene", w) {
            addr = Some(layer_addr(&ns, "", blank_fx));
        }
        if !s.is_empty() && let Some(nl) = pick_name(ui, (id.0, "layer-layer", id.1), l, &layers.layers(s, fx), "Layer", w) {
            addr = Some(layer_addr(s, &nl, blank_fx));
        }
        if fx && !l.is_empty() && let Some(nf) = pick_name(ui, (id.0, "layer-fx", id.1), f.unwrap_or(""), &layers.effects(s, l), "Effect", w) {
            addr = Some(layer_addr(s, l, Some(&nf)));
        }
        let labels = if fx { ["On", "Off", "Switch"] } else { ["Show", "Hide", "Switch"] };
        let mut m = match mode {
            "off" => 1,
            "toggle" => 2,
            _ => 0,
        };
        if widgets::segmented(ui, t, &mut m, &labels) {
            new_mode = Some(["on", "off", "toggle"][m]);
        }
    });
    if mode == "toggle" {
        widgets::hint(ui, t, "Switch flips it each time: on when it's off, off when it's on.");
    }
    match (addr, new_mode) {
        (None, None) => None,
        (addr, m) => Some(build_step(kind, &[addr.as_deref().unwrap_or(a0), m.unwrap_or(mode)])),
    }
}

/// Setting (searchable address), value, and an optional fade for a "Change a setting" step.
fn setting_args(app: &mut App, ui: &mut egui::Ui, t: &Theme, key: &str, args: &[String]) -> Option<String> {
    let arg = |i: usize| args.get(i).cloned().unwrap_or_default();
    let (mut addr, mut v, mut d) = (arg(0), arg(1), arg(2));
    let mut changed = text_field(app, ui, key, "Search settings, e.g. blur or music volume", &mut addr);
    ui.horizontal(|ui| {
        ui.label(RichText::new("To").color(t.text_dim));
        changed |= ui.add(widgets::field(&mut v).hint_text("0.5 · true · #ff0000").desired_width(140.0)).changed();
        ui.label(RichText::new("Fade over").color(t.text_dim));
        changed |= ui.add(widgets::field(&mut d).hint_text("no fade").desired_width(90.0)).changed();
        if !addr.trim().is_empty() {
            widgets::hint(ui, t, &setting_name(addr.trim()));
        }
    });
    changed.then(|| build_step(Step::Setting, &[&addr, &v, &d]))
}

/// Which notification to show, and the name it shows. Returns (new command, open Notifications).
fn notify_args(ui: &mut egui::Ui, t: &Theme, names: &Names, id: (&str, usize), args: &[String], when: &str) -> (Option<String>, bool) {
    let a0 = args.first().map(String::as_str).unwrap_or("");
    if names.alerts.is_empty() && a0.is_empty() {
        let open = widgets::callout(
            ui,
            t,
            widgets::Tone::Info,
            icon::ALERT,
            "No notifications set up yet",
            "Set one up in Notifications, then pick it here.",
            Some("Open Notifications"),
        );
        return (None, open);
    }
    let user = args[1.min(args.len())..].iter().find_map(|kv| kv.strip_prefix("user=")).unwrap_or("").to_string();
    let others: Vec<&String> = args.iter().skip(1).filter(|kv| !kv.starts_with("user=")).collect();
    let rebuild = |ty: &str, user: &str| {
        let u = format!("user={user}");
        let mut parts: Vec<&str> = vec![ty];
        if !user.trim().is_empty() {
            parts.push(&u);
        }
        parts.extend(others.iter().map(|s| s.as_str()));
        build_step(Step::Notify, &parts)
    };
    let mut out = None;
    ui.horizontal_wrapped(|ui| {
        if let Some(ty) = pick_name(ui, (id.0, "notify", id.1), a0, &names.alerts, "Pick a notification", 240.0) {
            // a viewer's own name when the trigger answers something a viewer did
            let u = if user.is_empty() && fields_for(when).contains(&"user") { "{user}".to_string() } else { user.clone() };
            out = Some(rebuild(&ty, &u));
        }
        ui.label(RichText::new("Name shown").color(t.text_dim));
        let mut u = user.clone();
        if ui.add(widgets::field(&mut u).hint_text("{user}").desired_width(160.0)).changed() {
            out = Some(rebuild(a0, &u));
        }
    });
    (out, false)
}

/// Toggle `item` in a comma-separated list.
fn toggle_csv(csv: &mut String, item: &str) {
    let mut items: Vec<String> = csv.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect();
    match items.iter().position(|x| x == item) {
        Some(i) => {
            items.remove(i);
        }
        None => items.push(item.to_string()),
    }
    *csv = items.join(", ");
}

/// Chips for a limit list; `none` says what an empty pick means.
fn limit_chips(ui: &mut egui::Ui, t: &Theme, csv: &mut String, known: &[(String, String)], none: &str) {
    let chosen: Vec<String> = csv.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect();
    let mut flip = None;
    ui.horizontal_wrapped(|ui| {
        for (n, l) in known {
            if chip(ui, t, "", l, chosen.contains(n)).clicked() {
                flip = Some(n.clone());
            }
        }
        for n in chosen.iter().filter(|n| !known.iter().any(|(k, _)| k == *n)) {
            if chip(ui, t, "", &nice_name(n), true).clicked() {
                flip = Some(n.clone());
            }
        }
        if chosen.is_empty() {
            widgets::hint(ui, t, none);
        }
    });
    if let Some(n) = flip {
        toggle_csv(csv, &n);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draft() -> RuleDraft {
        RuleDraft {
            orig: Some("big cheer".into()),
            file: "rules/cheers.toml".into(),
            name: "big cheer".into(),
            when: "twitch.cheer".into(),
            cond: "event.bits >= 1000".into(),
            commands: vec!["preset.fire hype".into()],
            ..RuleDraft::new()
        }
    }

    #[test]
    fn values_filled_in_later_read_as_words() {
        let scenes = vec![("duo".to_string(), "Duo".to_string())];
        assert_eq!(Names::label(&scenes, "duo"), "Duo");
        assert_eq!(Names::label(&scenes, "{scene}"), "the scene on air");
        assert_eq!(Names::label(&scenes, "{patch.ad_break.return_scene}"), "the scene from before");
        assert!(!Names::label(&scenes, "{user}").contains('{'));
    }

    #[test]
    fn validation_uses_engine_parsers() {
        assert!(draft().errors().is_empty(), "{:?}", draft().errors());
        let mut d = draft();
        d.cond = "event.bits >=".into();
        d.commands = vec!["$(boom)".into()];
        d.cooldown_global = "soon".into();
        let e = d.errors();
        assert_eq!(e.len(), 3, "{e:?}");
        d = draft();
        d.file = "../x.toml".into();
        assert_eq!(d.errors().len(), 1);
    }

    #[test]
    fn unfinished_steps_block_saving() {
        let mut d = draft();
        d.commands = vec!["preset.fire".into(), "bot.say".into(), "twitch.marker".into()];
        assert_eq!(d.errors().len(), 2, "a picker left empty is unfinished; a marker needs no text");
        d.commands.clear();
        assert_eq!(d.errors().len(), 1, "a reaction needs something to do");
    }

    #[test]
    fn write_args_pick_entry_mode_from_the_file() {
        let multi = "[[rule]]\nname = \"big cheer\"\nwhen = \"twitch.cheer\"\n";
        let a = draft().write_args(Some(multi));
        assert_eq!(a.get_path("table").and_then(Value::as_str), Some("rule"));
        assert_eq!(a.get_path("match.name").and_then(Value::as_str), Some("big cheer"));
        assert_eq!(a.get_path("set.if").and_then(Value::as_str), Some("event.bits >= 1000"));
        assert!(a.get_path("set.cooldown").is_some_and(Value::is_null), "cleared cooldown is removed");
        let mut auto = draft();
        auto.orig = Some("cheers#2".into());
        let a = auto.write_args(Some("[[rule]]\nwhen='a'\n[[rule]]\nwhen='b'\n"));
        assert_eq!(a.get_path("index").and_then(Value::as_i64), Some(1));
        let single = draft().write_args(Some("name = 'big cheer'\nwhen = 'twitch.cheer'\n"));
        assert!(single.get_path("table").is_none());
        let mut new = draft();
        new.orig = None;
        let a = new.write_args(None);
        assert!(a.get_path("append").is_some_and(Value::truthy));
    }

    #[test]
    fn test_command_uses_the_simulator_when_it_can() {
        let mut d = draft();
        d.test_args = "bits=1500".into();
        assert_eq!(d.test_command(), "sim.cheer bits=1500");
        d.when = "twitch.*".into();
        assert!(d.test_command().starts_with("sim."));
        d.when = "band.drop".into();
        d.test_args.clear();
        assert_eq!(d.test_command(), "emit band.drop");
        d.when = "mode.enter.*".into();
        assert_eq!(d.test_command(), "emit mode.enter.test");
    }

    #[test]
    fn completion_replaces_the_last_token() {
        let mut s = String::from("event.bits >= 10 && ev");
        complete(&mut s, "event.user");
        assert_eq!(s, "event.bits >= 10 && event.user");
        let mut c = String::from("pre");
        complete(&mut c, "preset.fire hype");
        assert_eq!(c, "preset.fire hype");
    }

    #[test]
    fn chat_templates_read_as_words() {
        assert_eq!(friendly_text("Thanks {user} for {bits} bits"), "Thanks (their name) for (how many) bits");
        assert_eq!(friendly_text("{random:a|b|c}!"), "(a, b or c)!");
        assert_eq!(friendly_text("Goal {goals.subs.current}/{goals.subs.target}"), "Goal (subs so far)/(sub goal)");
        assert_eq!(friendly_text("no braces {"), "no braces {");
        assert_eq!(friendly_text("Song queue ({queue.length} waiting)"), "Song queue (how many waiting)");
        assert_eq!(friendly_text("{song} (asked by {queue.now.user})"), "(current song) (asked by who asked for it)");
        assert_eq!(nice_name("SUB BIG"), "Sub big");
        assert_eq!(nice_name("brb"), "BRB");
        assert_eq!(nice_name("Kit (HDMI 1)"), "Kit (HDMI 1)");
    }

    #[test]
    fn simple_checks_round_trip() {
        let src = "event.tier >= 2 && mode == 'live' && !event.is_gift";
        let rows = parse_conds(src).expect("simple checks");
        assert_eq!(rows.len(), 3);
        assert_eq!(conds_text(&rows), src, "unchanged rows write back the same text");
        assert!(parse_conds("event.bits > 1 || mode == 'x'").is_none(), "or-checks need the raw editor");
        assert!(parse_conds("(event.bits > 1)").is_none());
        assert!(parse_conds("mode > 3").is_none(), "modes compare by name");
        let mut half = rows.clone();
        half.push(Cond::new("bits"));
        assert_eq!(conds_text(&half), src, "unfinished rows are left out");
    }

    #[test]
    fn steps_round_trip_through_the_guided_editor() {
        for cmd in [
            "preset.fire hype",
            "scene.cut brb",
            "lights.cue main 2",
            "lights.cue look=warm",
            "patch.terminal_boot.trigger",
            "wait 2s",
            "bot.say 'HYPE! Thanks {user} for {bits} bits'",
            "set scene.duo.node.cam.fx.blur.enabled true",
            "set scene.duo.node.cam.fx.blur.enabled false",
            "toggle scene.duo.node.cam.fx.patch.dream.enabled",
            "set scene.duo.node.cam.visible false",
            "toggle scene.duo.node.cam.visible",
            "set audio.bus.music.gain 0.5",
            "animate fx.vhs.amount 1 2s",
            "emit twitch.follow user='{user}'",
        ] {
            let (k, args) = parse_step(cmd);
            assert_ne!(k, Step::Custom, "{cmd}");
            let refs: Vec<&str> = args.iter().map(String::as_str).collect();
            assert_eq!(parse_step(&build_step(k, &refs)), (k, args.clone()), "{cmd}");
        }
        assert_eq!(parse_step("scene.cut brb fade").0, Step::Custom, "a transition needs the raw editor");
        assert_eq!(parse_step("twitch.shoutout user={from_login}").0, Step::Custom);
        let tricky = "it's \"loud\" = fun";
        let built = build_step(Step::Say, &[tricky]);
        assert!(Op::parse(&built).is_ok(), "{built}");
        assert_eq!(parse_step(&built), (Step::Say, vec![tricky.to_string()]));
    }

    #[test]
    fn layer_setting_and_notification_steps_read_as_what_they_do() {
        let fx = parse_step("toggle scene.duo.node.cam.fx.blur.enabled");
        assert_eq!(fx, (Step::LayerFx, vec!["scene.duo.node.cam.fx.blur.enabled".to_string(), "toggle".to_string()]));
        assert_eq!(parse_step("set scene.duo.node.cam.visible true").0, Step::LayerVisible);
        // anything but on/off on a layer switch is a plain setting; other toggles need the raw editor
        assert_eq!(parse_step("set scene.duo.node.cam.visible 1").0, Step::Setting);
        assert_eq!(parse_step("toggle audio.duck.active").0, Step::Custom);
        assert_eq!(parse_step("animate fx.vhs.amount 1 2s out_cubic").0, Step::Custom);
        assert_eq!(parse_step("emit twitch.follow oops").0, Step::Custom);
        // the engine reads every guided form
        for (k, args) in [
            (Step::LayerFx, vec!["scene.duo.node.cam.fx.blur.enabled", "toggle"]),
            (Step::LayerVisible, vec!["scene.duo.node.cam.visible", "off"]),
            (Step::Setting, vec!["fx.vhs.amount", "0.4", "500ms"]),
            (Step::Setting, vec!["show.title", "Hello there"]),
            (Step::Notify, vec!["twitch.cheer", "user=Alex B", "bits=100"]),
        ] {
            let built = build_step(k, &args);
            assert!(Op::parse(&built).is_ok(), "{built}");
            assert_eq!(parse_step(&built), (k, args.iter().map(|a| a.to_string()).collect()), "{built}");
        }
        let fx = build_step(Step::LayerFx, &["scene.duo.node.cam.fx.blur.enabled", "toggle"]);
        assert_eq!(fx, "toggle scene.duo.node.cam.fx.blur.enabled");
        assert!(matches!(Op::parse(&fx), Ok(Op::Action { name, .. }) if name == "toggle"));
        // a layer step is unfinished until scene, layer (and effect) are picked
        assert!(step_missing(&Step::LayerFx.blank()).is_some());
        assert!(step_missing("set scene.duo.node..fx..enabled true").is_some());
        assert!(step_missing("set scene.duo.node.cam.fx..enabled true").is_some());
        assert_eq!(step_missing("set scene.duo.node.cam.fx.blur.enabled true"), None);
        assert!(step_missing(&Step::LayerVisible.blank()).is_some());
        assert!(step_missing("set fx.vhs.amount").is_some(), "a setting needs a value");
        assert!(step_missing(&Step::Notify.blank()).is_some());
        // words
        let n = Names {
            scenes: vec![("duo".into(), "Duo".into())],
            alerts: alert_choices(Some(&Value::map().with("alerts", Value::List(vec![Value::map().with("name", "follow").with("when", "twitch.follow")])))),
            ..Default::default()
        };
        assert_eq!(step_phrase("set scene.duo.node.cam_face.fx.rgb_split.enabled true", &n), "turn on RGB split on Cam face in Duo");
        assert_eq!(step_phrase("set scene.duo.node.cam_face.visible false", &n), "hide Cam face in Duo");
        assert_eq!(step_phrase("emit twitch.follow user=Alex", &n), "show the Follow notification");
        assert_eq!(step_phrase("set audio.bus.music.gain 0.5", &n), "set music volume to 0.5");
    }

    #[test]
    fn animation_and_look_steps_read_as_what_they_do() {
        // both ways of playing a source's animation read as one; the guided editor writes the file's way
        assert_eq!(parse_step("trigger patch.confetti"), (Step::Animation, vec!["confetti".to_string()]));
        assert_eq!(build_step(Step::Animation, &["confetti"]), "patch.confetti.trigger");
        assert!(Op::parse(&build_step(Step::Animation, &["confetti"])).is_ok());
        // triggers with settings, or of something that isn't a source, need the raw editor
        assert_eq!(parse_step("patch.confetti.trigger count=50").0, Step::Custom);
        assert_eq!(parse_step("trigger fx.glitch").0, Step::Custom);
        // a look is not a cue list
        assert_eq!(parse_step("lights.cue main").0, Step::Lights);
        assert_eq!(parse_step("lights.cue look=warm").0, Step::Look);
        assert!(Op::parse(&build_step(Step::Look, &["warm"])).is_ok());
        // fresh steps are unfinished until something is picked
        for k in [Step::Animation, Step::Look] {
            assert_eq!(parse_step(&k.blank()), (k, if k == Step::Look { vec![String::new()] } else { Vec::new() }));
            assert!(step_missing(&k.blank()).is_some(), "{k:?}");
        }
        assert_eq!(step_missing("patch.terminal_boot.trigger"), None);
    }

    #[test]
    fn events_group_by_where_they_come_from() {
        assert_eq!(Family::of("twitch.cheer"), Family::Viewers);
        assert_eq!(Family::of("twitch.ad_break"), Family::Stream);
        assert_eq!(Family::of("band.kick"), Family::Music);
        assert_eq!(Family::of("music.drop"), Family::Music);
        assert_eq!(Family::of("mode.enter.live"), Family::Show);
        assert_eq!(Family::of("midi.nano.pad1"), Family::Controls);
        assert_eq!(Family::of("custom.thing"), Family::Other);
    }
}
