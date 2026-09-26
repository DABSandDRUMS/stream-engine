//! Reactions (§15.5): "when this happens → do that". The list reads every reaction as a plain
//! sentence with an on/off switch; the editor is a guided WHEN / ONLY IF / DO form (friendly
//! event, condition and step pickers) that edits the same text the engine runs. The raw text
//! (with autocomplete for event types, payload fields, addresses, presets, scenes, commands) and
//! the file live under "Details". Validation uses the engine's own parsers, "Test it" fires the
//! event through the simulator, and saving goes through `project.write` into `rules/*.toml`
//! (comments in the file are kept).

use crate::app::App;
use crate::views::live::nice;
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align, Color32, CornerRadius, FontId, Layout, Pos2, Rect, RichText, Sense, UiBuilder, Vec2};
use se_proto::command::tokenize;
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_medium, font_mono, font_semibold, mix, radius, spacing, type_scale};
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

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Group {
    Viewers,
    Show,
}

/// A friendly event the WHEN picker offers.
struct Trigger {
    pattern: &'static str,
    label: &'static str,
    /// Completes "When …".
    sentence: &'static str,
    icon: &'static str,
    group: Group,
}

const fn trig(pattern: &'static str, label: &'static str, sentence: &'static str, icon: &'static str, group: Group) -> Trigger {
    Trigger { pattern, label, sentence, icon, group }
}

const MODE_START: &str = "mode.enter.*";
const MODE_END: &str = "mode.exit.*";

const TRIGGERS: &[Trigger] = &[
    trig("twitch.follow", "Follow", "someone follows", icon::HEART, Group::Viewers),
    trig("twitch.sub", "Sub", "someone subscribes", icon::STAR, Group::Viewers),
    trig("twitch.resub", "Resub", "someone resubscribes", icon::STAR, Group::Viewers),
    trig("twitch.gift", "Gifted subs", "someone gifts subs", icon::GIFT, Group::Viewers),
    trig("twitch.cheer", "Cheer (bits)", "someone cheers", icon::SPARKLE, Group::Viewers),
    trig("twitch.raid", "Raid", "someone raids", icon::USERS, Group::Viewers),
    trig("twitch.redeem", "Channel points", "someone redeems channel points", icon::GIFT, Group::Viewers),
    trig("twitch.chat", "Chat message", "someone chats", icon::CHAT, Group::Viewers),
    trig("tip", "Tip", "someone tips", icon::HEART, Group::Viewers),
    trig("twitch.hype_train.begin", "Hype train", "a hype train starts", icon::ROCKET, Group::Viewers),
    trig("twitch.ad_break", "Ad break", "an ad break starts", icon::CLOCK, Group::Viewers),
    trig("twitch.*", "Anything on Twitch", "anything happens on Twitch", icon::TWITCH, Group::Viewers),
    trig("band.kick", "Kick drum hit", "the kick drum hits", icon::DRUM, Group::Show),
    trig("band.snare", "Snare hit", "the snare hits", icon::DRUM, Group::Show),
    trig("band.drop", "Music drop", "the music drops", icon::MUSIC, Group::Show),
    trig("beat", "Every beat", "a beat lands", icon::MUSIC, Group::Show),
    trig("deck.key", "Stream Deck button", "a Stream Deck button is pressed", icon::KEYBOARD, Group::Show),
    trig(MODE_START, "Show mode starts", "any show mode starts", icon::PLAY, Group::Show),
    trig(MODE_END, "Show mode ends", "any show mode ends", icon::STOP, Group::Show),
    trig("queue.song_started", "Song request starts", "a requested song starts", icon::QUEUE, Group::Show),
    trig("timeline.cue", "Timeline moment", "a timeline moment passes", icon::TIMELINE, Group::Show),
];

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
fn when_sentence(when: &str) -> String {
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
    Lights,
    Say,
    Sound,
    Mix,
    Mode,
    Wait,
    Marker,
    TimelinePlay,
    TimelineStop,
    Custom,
}

impl Step {
    const ALL: [Step; 14] = [
        Step::Preset,
        Step::Release,
        Step::Scene,
        Step::Preview,
        Step::Lights,
        Step::Say,
        Step::Sound,
        Step::Mix,
        Step::Mode,
        Step::Wait,
        Step::Marker,
        Step::TimelinePlay,
        Step::TimelineStop,
        Step::Custom,
    ];

    fn verb(self) -> &'static str {
        match self {
            Step::Preset => "preset.fire",
            Step::Release => "preset.release",
            Step::Scene => "scene.cut",
            Step::Preview => "scene.go",
            Step::Lights => "lights.cue",
            Step::Say => "bot.say",
            Step::Sound => "audio.play",
            Step::Mix => "mixer.snapshot.recall",
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
            Step::Preset => "Fire a quick effect",
            Step::Release => "Stop a quick effect",
            Step::Scene => "Switch to a scene",
            Step::Preview => "Put a scene up next",
            Step::Lights => "Run a light cue",
            Step::Say => "Say something in chat",
            Step::Sound => "Play a sound",
            Step::Mix => "Recall a sound mix",
            Step::Mode => "Change the show mode",
            Step::Wait => "Wait a moment",
            Step::Marker => "Add a stream marker",
            Step::TimelinePlay => "Play a timeline",
            Step::TimelineStop => "Stop a timeline",
            Step::Custom => "Custom command",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Step::Preset | Step::Release => icon::BOLT,
            Step::Scene | Step::Preview => icon::LAYERS,
            Step::Lights => icon::LIGHT,
            Step::Say => icon::CHAT,
            Step::Sound => icon::VOLUME,
            Step::Mix => icon::SLIDERS,
            Step::Mode => icon::PLAY,
            Step::Wait => icon::CLOCK,
            Step::Marker => icon::STAR,
            Step::TimelinePlay | Step::TimelineStop => icon::TIMELINE,
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

    /// What's missing when the step has no target yet (`None` = nothing required).
    fn missing(self) -> Option<&'static str> {
        Some(match self {
            Step::Preset | Step::Release => "pick a quick effect",
            Step::Scene | Step::Preview => "pick a scene",
            Step::Lights => "pick a light cue list",
            Step::Say => "type what to say",
            Step::Sound => "pick a sound",
            Step::Mix => "pick a sound mix",
            Step::Mode => "pick a show mode",
            Step::Wait => "say how long to wait",
            Step::TimelinePlay | Step::TimelineStop => "pick a timeline",
            Step::Marker | Step::Custom => return None,
        })
    }
}

/// The friendly shape of a command (`Custom` when the guided editor can't show it).
fn parse_step(cmd: &str) -> (Step, Vec<String>) {
    let Ok(toks) = tokenize(cmd) else { return (Step::Custom, Vec::new()) };
    let Some((verb, rest)) = toks.split_first() else { return (Step::Custom, Vec::new()) };
    let Some(&kind) = Step::ALL.iter().find(|s| s.verb() == verb.as_str()) else { return (Step::Custom, Vec::new()) };
    if kind == Step::Say {
        return match rest {
            [one] if one.starts_with("text=") => (kind, vec![one["text=".len()..].to_string()]),
            _ if rest.iter().any(|a| a.contains('=')) => (Step::Custom, Vec::new()),
            [] => (kind, Vec::new()),
            _ => (kind, vec![rest.join(" ")]),
        };
    }
    if rest.len() > kind.max_args() || rest.iter().any(|a| a.contains('=')) {
        return (Step::Custom, Vec::new());
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
    let mut out = kind.verb().to_string();
    if kind == Step::Lights && args.first().is_none_or(|a| a.trim().is_empty()) {
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
        }
    }

    fn label(list: &[(String, String)], name: &str) -> String {
        if name.is_empty() {
            return "…".into();
        }
        // a value filled in when the reaction runs: `{scene}`, `{patch.ad_break.return_scene}`
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

fn step_phrase(cmd: &str, n: &Names) -> String {
    let (kind, args) = parse_step(cmd);
    let a0 = args.first().map(String::as_str).unwrap_or("");
    match kind {
        Step::Preset => format!("fire {}", Names::label(&n.presets, a0)),
        Step::Release => format!("stop {}", Names::label(&n.presets, a0)),
        Step::Scene => format!("switch to {}", Names::label(&n.scenes, a0)),
        Step::Preview => format!("put {} up next", Names::label(&n.scenes, a0)),
        Step::Lights => match args.get(1) {
            Some(c) => format!("run light cue {c} of {}", Names::label(&n.cuelists, a0)),
            None => format!("run the {} lights", Names::label(&n.cuelists, a0)),
        },
        Step::Say if a0.is_empty() => "say …".into(),
        Step::Say => format!("say “{}”", clip(&friendly_text(a0), 56)),
        Step::Sound => format!("play {}", Names::label(&n.sounds, a0)),
        Step::Mix => format!("recall the {} mix", Names::label(&n.mixes, a0)),
        Step::Mode => format!("switch to {}", Names::label(&n.modes, a0)),
        Step::Wait => format!("wait {}", if a0.is_empty() { "…" } else { a0 }),
        Step::Marker => "add a stream marker".into(),
        Step::TimelinePlay => format!("play the {} timeline", Names::label(&n.timelines, a0)),
        Step::TimelineStop => format!("stop the {} timeline", Names::label(&n.timelines, a0)),
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
        "trigger" => format!("fire {}", middle(arg)),
        v if v.ends_with(".trigger") => format!("fire {}", middle(v.trim_end_matches(".trigger"))),
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

/// Plain words for command lines ("Fire Hype", "Switch to Duo"), shared with the controller
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

    /// A quick effect's display name.
    pub fn preset(&self, name: &str) -> String {
        Names::label(&self.names.presets, name)
    }

    /// A scene's display name.
    pub fn scene(&self, name: &str) -> String {
        Names::label(&self.names.scenes, name)
    }

    /// (name, display name) of every quick effect / scene.
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
            when: "twitch.follow".into(),
            cond: String::new(),
            commands: vec![Step::Preset.verb().into()],
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
            e.push("Give it a name (under Details).".into());
        }
        let when = self.when.trim();
        if when.is_empty() {
            e.push("Pick what it reacts to.".into());
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
            if let Some(what) = kind.missing()
                && args.iter().all(|a| a.trim().is_empty())
            {
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
            e.push("Reactions are saved in the rules folder (Details → Saved in).".into());
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
        let set = self.set_value();
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

    pub fn delete_args(&self, file_text: Option<&str>) -> Option<Value> {
        let orig = self.orig.as_ref()?;
        let multi = file_text.and_then(|t| t.parse::<toml::Table>().ok()).is_some_and(|t| t.get("rule").is_some_and(|r| r.is_array()));
        Some(if multi {
            Value::map().with("path", self.file.clone()).with("table", "rule").with("match", Value::map().with("name", orig.clone())).with("delete", true)
        } else {
            Value::map().with("path", self.file.clone()).with("delete", true)
        })
    }

    /// A name for a new reaction from its sentence, unique among `taken`.
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

/// A reaction in the list, worded once per `rules`/`presets`/`scenes` reply.
struct Row {
    name: String,
    group: u8,
    title: String,
    subtitle: String,
    enabled: bool,
    fired_ms: Option<i64>,
    value: Value,
}

type RowKey = (u64, u64, u64);

#[derive(Default)]
pub struct RulesEditor {
    pub edit: Option<RuleDraft>,
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
        self.test_vals.clear();
        self.rows_src = None;
    }
    pub fn editing_name(&self) -> Option<&str> {
        self.edit.as_ref().and_then(|d| d.orig.as_deref())
    }
    /// Open a row of the `rules` query.
    pub fn open(&mut self, r: &Value) {
        let s = |k: &str| r.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
        let name = s("name");
        self.rows_src = None;
        self.test_vals.clear();
        self.edit = Some(RuleDraft {
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
        });
    }
}

// ---- autocomplete (raw fields) -------------------------------------------------------------------

/// Suggestions for the token under the cursor (the last whitespace-separated word).
pub fn suggest(field: &str, text: &str, app: &App) -> Vec<String> {
    let last = text.rsplit(|c: char| c.is_whitespace() || c == '(' || c == '!').next().unwrap_or("");
    let mut pool: Vec<String> = Vec::new();
    match field {
        "when" => {
            pool.extend(SIM_FOR.iter().map(|(t, _)| t.to_string()));
            pool.extend(app.m.events.iter().map(|e| e.ty.clone()));
            pool.extend(
                ["mode.enter.*", "mode.exit.*", "band.kick", "band.snare", "band.drop", "beat", "deck.key", "queue.song_started", "timeline.cue", "twitch.*"]
                    .map(String::from),
            );
        }
        "if" => {
            let when = app.build.rules.edit.as_ref().map(|d| d.when.clone()).unwrap_or_default();
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
    let t = app.t.clone();
    let kind = key.trim_start_matches("raw-").split(':').next().unwrap_or(key);
    let focused = app.build.rules.focus.as_deref() == Some(key);
    let sugg = if focused { app.build.rules.edit.as_mut().map(|d| get(d).clone()).map(|txt| suggest(kind, &txt, app)).unwrap_or_default() } else { Vec::new() };
    let Some(d) = app.build.rules.edit.as_mut() else { return };
    let r = ui.add(se_ui_kit::widgets::field(get(d)).hint_text(hint).desired_width(f32::INFINITY).font(font_mono(type_scale::BODY - 1.0)));
    if r.has_focus() {
        app.build.rules.focus = Some(key.to_string());
    }
    if !sugg.is_empty() && (r.has_focus() || focused) {
        ui.horizontal_wrapped(|ui| {
            for s in sugg {
                if widgets::button_ex(ui, &t, None, &s, Kind::Secondary, Size::Small, 0.0, true).clicked()
                    && let Some(d) = app.build.rules.edit.as_mut()
                {
                    complete(get(d), &s);
                    r.request_focus();
                }
            }
        });
    }
}

// ---- view ----------------------------------------------------------------------------------------

use widgets::chip;

fn wrapped(ui: &egui::Ui, text: &str, font: FontId, color: Color32, width: f32, rows: usize) -> Arc<egui::Galley> {
    let mut job = LayoutJob::single_section(text.to_owned(), egui::TextFormat::simple(font, color));
    job.wrap = TextWrapping { max_width: width, max_rows: rows, break_anywhere: false, overflow_character: Some('…') };
    ui.painter().layout_job(job)
}

fn list_group(when: &str) -> u8 {
    if when.starts_with("twitch.") || when == "tip" || when.starts_with("tip.") {
        0
    } else if when.starts_with("band.") || when == "beat" || when.starts_with("music.") || when.starts_with("queue.") {
        1
    } else if when.starts_with("mode.") || when.starts_with("deck.") || when.starts_with("timeline.") {
        2
    } else {
        3
    }
}

const GROUP_TITLES: [&str; 4] = ["From your viewers", "Music and drums", "Your show", "Other"];

fn rows(app: &mut App) -> Arc<Vec<Row>> {
    let key = (app.m.q_seq("rules"), app.m.q_seq("presets"), app.m.q_seq("scenes"));
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
                group: list_group(&when),
                title: when_phrase(&when, &s("if")),
                subtitle: format!("→ {}", do_phrase(&cmds, &names)),
                enabled: r.get_path("enabled").is_some_and(Value::truthy),
                fired_ms: r.get_path("last_fired_ms_ago").and_then(Value::as_i64),
                value: r.clone(),
            }
        })
        .collect();
    out.sort_by_key(|r| r.group);
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

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let avail = ui.available_size();
    if avail.x < 820.0 {
        // narrow (docked panel): the list, or the editor with a way back
        if app.build.rules.edit.is_some() {
            if widgets::button_ex(ui, &t, Some(icon::LEFT), "All reactions", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                app.build.rules.edit = None;
            }
            ui.add_space(spacing::S);
            editor(app, ui, &t);
        } else {
            list(app, ui, &t);
        }
        return;
    }
    let list_w = (avail.x * 0.34).clamp(380.0, 760.0);
    let ed_w = avail.x - list_w - spacing::L;
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(list_w, avail.y), Layout::top_down(Align::Min), |ui| list(app, ui, &t));
        ui.add_space(spacing::L - ui.spacing().item_spacing.x);
        ui.allocate_ui_with_layout(Vec2::new(ed_w, avail.y), Layout::top_down(Align::Min), |ui| editor(app, ui, &t));
    });
}

fn list(app: &mut App, ui: &mut egui::Ui, t: &Theme) {
    let rows = rows(app);
    let on = rows.iter().filter(|r| r.enabled).count();
    let sub = if rows.is_empty() { String::new() } else { format!("{} reactions · {on} switched on", rows.len()) };
    let (mut new, mut first) = (false, false);
    let height = ui.available_height();
    widgets::titled(
        ui,
        t,
        "Your reactions",
        &sub,
        |ui| new = widgets::button_ex(ui, t, Some(icon::PLUS), "New reaction", Kind::Primary, Size::Medium, 0.0, true).clicked(),
        |ui| {
            ui.set_width(ui.available_width());
            if rows.is_empty() {
                if !app.m.connected {
                    widgets::empty_state(ui, t, icon::WARN, "Stream Engine isn't running", "Your reactions show up here once it's running.", None);
                } else if widgets::empty_state(
                    ui,
                    t,
                    icon::BOLT,
                    "No reactions yet",
                    "A reaction does something by itself when something happens, like firing Hype when someone cheers.",
                    None,
                ) {
                    first = true;
                }
                return;
            }
            let groups = rows.iter().map(|r| r.group).collect::<std::collections::BTreeSet<_>>().len();
            egui::ScrollArea::vertical().id_salt("rules-list").auto_shrink([false, true]).max_height((height - 90.0).max(120.0)).show(ui, |ui| {
                let mut last = None;
                for r in rows.iter() {
                    if groups > 1 && last != Some(r.group) {
                        if last.is_some() {
                            ui.add_space(spacing::S);
                        }
                        let n = rows.iter().filter(|x| x.group == r.group).count();
                        widgets::section(ui, t, "", &format!("{} ({n})", GROUP_TITLES[r.group as usize]));
                        last = Some(r.group);
                    }
                    let selected = app.build.rules.editing_name() == Some(r.name.as_str());
                    let mut en = r.enabled;
                    let recent = r.fired_ms.is_some_and(|ms| ms < 2500);
                    let tip = match r.fired_ms {
                        Some(ms) => format!("{} · last happened {}", r.name, ago(ms)),
                        None => format!("{} · hasn't happened yet", r.name),
                    };
                    let (resp, toggled) = reaction_row(ui, t, &r.title, &r.subtitle, &mut en, selected, recent);
                    if toggled {
                        app.m.text(&format!("{} '{}'", if en { "rule.enable" } else { "rule.disable" }, r.name));
                        app.m.refresh_soon();
                    } else if resp.on_hover_text(tip).clicked() {
                        app.build.rules.open(&r.value);
                    }
                    ui.add_space(2.0);
                }
            });
        },
    );
    if new || first {
        app.build.rules.new_rule();
    }
}

/// A reaction: on/off switch, the sentence, and what it does. Returns (row, switch flipped).
fn reaction_row(ui: &mut egui::Ui, t: &Theme, title: &str, subtitle: &str, on: &mut bool, selected: bool, recent: bool) -> (egui::Response, bool) {
    let w = ui.available_width();
    let text_x = 12.0 + 40.0 + 14.0;
    let text_w = (w - text_x - 12.0).max(40.0);
    let title_c = if *on { t.fg } else { t.text_dim };
    let tg = wrapped(ui, title, font_medium(type_scale::BODY), title_c, text_w, 2);
    let sg = wrapped(ui, subtitle, font(type_scale::SMALL + 0.5), t.text_dim, text_w, 2);
    let h = 11.0 + tg.size().y + 3.0 + sg.size().y + 11.0;
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
    let p = ui.painter().clone();
    let r = CornerRadius::same(radius::CONTROL);
    if selected {
        p.rect_filled(rect, r, mix(t.surface, t.accent, 0.14));
    } else if resp.hovered() {
        p.rect_filled(rect, r, t.surface_hi);
    }
    if recent {
        p.rect_filled(Rect::from_min_size(rect.min + Vec2::new(0.0, 8.0), Vec2::new(3.0, rect.height() - 16.0)), CornerRadius::same(2), t.green);
    }
    let top = rect.top() + 11.0;
    p.galley(Pos2::new(rect.left() + text_x, top), tg.clone(), title_c);
    p.galley(Pos2::new(rect.left() + text_x, top + tg.size().y + 3.0), sg, t.text_dim);
    let sw = Rect::from_min_size(Pos2::new(rect.left() + 12.0, rect.top() + 11.0 + (tg.size().y - 22.0) / 2.0), Vec2::new(40.0, 22.0));
    // a child ui leaves the row's layout cursor alone
    let toggled = widgets::toggle(&mut ui.new_child(UiBuilder::new().max_rect(sw)), t, on).on_hover_text("Switch on or off right now").changed();
    (resp.on_hover_cursor(egui::CursorIcon::PointingHand), toggled)
}

// ---- editor --------------------------------------------------------------------------------------

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
            if let Some(def) = cfg.as_list().and_then(|l| l.iter().find(|r| r.get_path("name").and_then(Value::as_str) == Some(name.as_str())))
                && let Some(d) = app.build.rules.edit.as_mut()
            {
                let dur = |v: Option<&Value>| {
                    v.map(|v| v.as_str().map(String::from).unwrap_or_else(|| format!("{v}ms"))).filter(|s| s != "nullms").unwrap_or_default()
                };
                d.cooldown_global = dur(def.get_path("cooldown.global").filter(|v| !v.is_null()));
                d.cooldown_actor = dur(def.get_path("cooldown.per_actor").filter(|v| !v.is_null()));
                let join = |k: &str| def.get_path(k).and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str()).collect::<Vec<_>>().join(", ");
                d.modes = join("modes");
                d.scenes = join("scenes");
                d.filled = true;
            }
            app.m.query_as(&format!("project.read:{file}"), "project.read", Value::map().with("path", file));
        }
    }
}

/// Lists behind the step pickers (cue lists, sounds, mixes, timelines), refreshed slowly.
fn fetch_lists(app: &mut App, now: f64) {
    if !app.m.connected || now - app.build.rules.lists_at < 5.0 {
        return;
    }
    app.build.rules.lists_at = now;
    for q in ["lights.cuelists", "audio.mix", "mixer.snapshots", "timelines"] {
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

fn editor(app: &mut App, ui: &mut egui::Ui, t: &Theme) {
    if app.build.rules.edit.is_none() {
        let mut new = false;
        widgets::panel(ui, t, |ui| {
            ui.set_width(ui.available_width());
            new = widgets::empty_state(
                ui,
                t,
                icon::BOLT,
                "Pick a reaction to change it",
                "Or make a new one with New reaction: choose what happens, then what Stream Engine does.",
                None,
            );
        });
        if new {
            app.build.rules.new_rule();
        }
        return;
    }
    fill(app);
    fetch_lists(app, ui.input(|i| i.time));
    sync_rows(app);
    let names = Names::read(app);
    egui::ScrollArea::vertical().id_salt("rule-editor").auto_shrink([false, false]).show(ui, |ui| {
        // wide screens: WHEN / ONLY IF beside DO / limits
        let full = ui.available_width();
        let two = full >= 1500.0;
        let col = ((full - spacing::L) / 2.0).floor();
        ui.set_max_width(if two { full } else { full.min(1080.0) });
        header(app, ui, t, &names);
        ui.add_space(spacing::L);
        if two {
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(Vec2::new(col, 0.0), Layout::top_down(Align::Min), |ui| {
                    when_card(app, ui, t, &names);
                    ui.add_space(spacing::L);
                    if_card(app, ui, t, &names);
                });
                ui.add_space(spacing::L - ui.spacing().item_spacing.x);
                ui.allocate_ui_with_layout(Vec2::new(col, 0.0), Layout::top_down(Align::Min), |ui| {
                    do_card(app, ui, t, &names);
                    ui.add_space(spacing::L);
                    limits_card(app, ui, t, &names);
                    ui.add_space(spacing::M);
                    footer(app, ui, t);
                });
            });
        } else {
            when_card(app, ui, t, &names);
            ui.add_space(spacing::L);
            if_card(app, ui, t, &names);
            ui.add_space(spacing::L);
            do_card(app, ui, t, &names);
            ui.add_space(spacing::L);
            limits_card(app, ui, t, &names);
            ui.add_space(spacing::M);
            footer(app, ui, t);
        }
        ui.add_space(spacing::XL);
    });
}

fn header(app: &mut App, ui: &mut egui::Ui, t: &Theme, names: &Names) {
    sync_test_args(app);
    let Some(d) = app.build.rules.edit.clone() else { return };
    let errs = d.errors();
    let (mut save, mut test, mut close) = (false, false, false);
    widgets::panel(ui, t, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            if let Some(e) = app.build.rules.edit.as_mut() {
                widgets::toggle(ui, t, &mut e.enabled).on_hover_text("Off keeps it saved, but it won't happen.");
                ui.label(RichText::new(if e.enabled { "On" } else { "Off" }).color(t.text_dim));
                ui.add_space(spacing::S);
            }
            let title = if d.orig.is_none() { "New reaction" } else { "Reaction" };
            ui.label(RichText::new(title).font(font_semibold(type_scale::BODY)).color(t.text_dim));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let tip = if errs.is_empty() { "Save this reaction" } else { "Finish the notes below first" };
                close = widgets::icon_button(ui, t, icon::CROSS, "Close").clicked();
                save = widgets::button_ex(ui, t, Some(icon::CHECK), "Save", Kind::Primary, Size::Medium, 0.0, errs.is_empty()).on_hover_text(tip).clicked();
                test = widgets::button_ex(ui, t, Some(icon::PLAY), "Test it", Kind::Secondary, Size::Medium, 0.0, !d.when.trim().is_empty())
                    .on_hover_text("Plays the event as if it really happened. Save first to test your changes.")
                    .clicked();
            });
        });
        ui.add_space(spacing::S);
        ui.add(egui::Label::new(RichText::new(when_phrase(&d.when, &d.cond)).font(font_semibold(type_scale::HEADING)).color(t.fg)).wrap());
        ui.add(egui::Label::new(RichText::new(format!("→ {}", do_phrase(&d.commands, names))).size(type_scale::LARGE).color(t.text_dim)).wrap());
        if !errs.is_empty() {
            ui.add_space(spacing::M);
            widgets::callout(ui, t, widgets::Tone::Warn, icon::WARN, "Almost there", &errs.join("\n"), None);
        }
    });
    if save {
        let mut d = d.clone();
        if d.name.trim().is_empty() {
            let taken: Vec<String> = app.m.q_list("rules").iter().filter_map(|r| r.get_path("name").and_then(Value::as_str).map(String::from)).collect();
            d.name = d.auto_name(&taken);
        }
        let text = file_text(app, &d.file);
        app.m.action("project.write", d.write_args(text.as_deref()));
        if let Some(e) = app.build.rules.edit.as_mut() {
            e.name = d.name.clone();
            e.orig = Some(d.name.trim().to_string());
        }
        app.m.toast("Reaction saved.", false);
        app.m.refresh_soon();
        app.m.query_as(&format!("project.read:{}", d.file), "project.read", Value::map().with("path", d.file.clone()));
    }
    if test {
        app.m.text(&d.test_command());
    }
    if close {
        app.build.rules.edit = None;
    }
}

/// "Try it with" values → simulator args (`bits=1500 user=Alex`).
fn sync_test_args(app: &mut App) {
    let ed = &mut app.build.rules;
    let Some(d) = ed.edit.as_mut() else { return };
    let args: Vec<String> = ed.test_vals.iter().filter(|(_, v)| !v.trim().is_empty()).map(|(k, v)| format!("{k}={}", quote_arg(v.trim()))).collect();
    d.test_args = args.join(" ");
}

fn when_card(app: &mut App, ui: &mut egui::Ui, t: &Theme, names: &Names) {
    let when = app.build.rules.edit.as_ref().map(|d| d.when.clone()).unwrap_or_default();
    let cur = trigger_index(&when);
    let mut pick: Option<String> = None;
    widgets::titled(
        ui,
        t,
        "When this happens",
        "",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            for (group, title) in [(Group::Viewers, "From your viewers"), (Group::Show, "From your show")] {
                widgets::section(ui, t, "", title);
                ui.horizontal_wrapped(|ui| {
                    for (i, tr) in TRIGGERS.iter().enumerate().filter(|(_, x)| x.group == group) {
                        let on = cur == Some(i);
                        if chip(ui, t, tr.icon, tr.label, on).clicked() && !on {
                            pick = Some(tr.pattern.to_string());
                        }
                    }
                    if group == Group::Show {
                        let on = cur.is_none();
                        if chip(ui, t, icon::EDIT, "Something else…", on).clicked() && !on {
                            pick = Some(String::new());
                        }
                    }
                });
                ui.add_space(spacing::S);
            }
            let mode = when.strip_prefix("mode.enter.").map(|m| ("mode.enter.", m)).or_else(|| when.strip_prefix("mode.exit.").map(|m| ("mode.exit.", m)));
            if let Some((prefix, m)) = mode {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Which mode").color(t.text_dim));
                    let text = if m == "*" || m.is_empty() { "Any mode".to_string() } else { Names::label(&names.modes, m) };
                    egui::ComboBox::from_id_salt("rule-when-mode").width(200.0).selected_text(text).show_ui(ui, |ui| {
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
            if cur.is_none() {
                ui.label(RichText::new("Event name").color(t.text_dim));
                field(app, ui, "when", "Start typing, then pick a suggestion", |d| &mut d.when);
                widgets::hint(ui, t, "Suggestions appear as you type. Recent events are included.");
            }
        },
    );
    if let (Some(p), Some(d)) = (pick, app.build.rules.edit.as_mut()) {
        d.when = p;
    }
}

fn if_card(app: &mut App, ui: &mut egui::Ui, t: &Theme, names: &Names) {
    let when = app.build.rules.edit.as_ref().map(|d| d.when.clone()).unwrap_or_default();
    let fields = fields_for(&when);
    let custom = app.build.rules.custom_cond;
    let mut rows = app.build.rules.rows.clone();
    let mut changed = false;
    let mut clear = false;
    widgets::titled(
        ui,
        t,
        "Only if",
        "Optional. Leave it empty to react every time.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if custom {
                widgets::hint(ui, t, "This reaction uses a custom check. Change it here, or clear it to use the simple choices.");
                field(app, ui, "if", "event.bits >= 1000 && mode == 'live'", |d| &mut d.cond);
                clear = widgets::button_ex(ui, t, Some(icon::BROOM), "Clear the check", Kind::Secondary, Size::Small, 0.0, true).clicked();
                return;
            }
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
            changed |= ui.add(se_ui_kit::widgets::field(&mut c.value).hint_text("1000").desired_width(90.0)).changed();
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
            changed |= ui.add(se_ui_kit::widgets::field(&mut c.value).hint_text("Type the text").desired_width(200.0)).changed();
        }
    }
    changed
}

fn do_card(app: &mut App, ui: &mut egui::Ui, t: &Theme, names: &Names) {
    let cmds = app.build.rules.edit.as_ref().map(|d| d.commands.clone()).unwrap_or_default();
    let mut edit: Option<(usize, String)> = None;
    let mut remove = None;
    let mut up = None;
    let mut add = false;
    widgets::titled(
        ui,
        t,
        "Do this",
        "Top to bottom, in order.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if cmds.is_empty() {
                widgets::hint(ui, t, "Nothing yet. Add the first thing it should do.");
                ui.add_space(spacing::S);
            }
            for (i, cmd) in cmds.iter().enumerate() {
                step_row(app, ui, t, names, i, cmd, &mut edit, &mut remove, &mut up);
                ui.add_space(spacing::XS);
            }
            ui.add_space(spacing::XS);
            add = widgets::button_ex(ui, t, Some(icon::PLUS), "Add a step", Kind::Secondary, Size::Small, 0.0, true).clicked();
        },
    );
    let Some(d) = app.build.rules.edit.as_mut() else { return };
    if let Some((i, c)) = edit
        && let Some(slot) = d.commands.get_mut(i)
    {
        *slot = c;
    }
    if let Some(i) = up
        && i > 0
        && i < d.commands.len()
    {
        d.commands.swap(i - 1, i);
    }
    if let Some(i) = remove
        && i < d.commands.len()
    {
        d.commands.remove(i);
    }
    if add {
        d.commands.push(Step::Preset.verb().into());
    }
}

/// A name picker over `opts`; a text box when the engine hasn't listed any. Returns a new pick.
fn pick_name(
    ui: &mut egui::Ui,
    id: impl std::hash::Hash + std::fmt::Debug,
    cur: &str,
    opts: &[(String, String)],
    placeholder: &str,
    width: f32,
) -> Option<String> {
    if opts.is_empty() {
        let mut s = cur.to_string();
        return ui.add(se_ui_kit::widgets::field(&mut s).hint_text(placeholder).desired_width(width)).changed().then_some(s);
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

#[allow(clippy::too_many_arguments)]
fn step_row(
    app: &mut App,
    ui: &mut egui::Ui,
    t: &Theme,
    names: &Names,
    i: usize,
    cmd: &str,
    edit: &mut Option<(usize, String)>,
    remove: &mut Option<usize>,
    up: &mut Option<usize>,
) {
    let (kind, args) = parse_step(cmd);
    let a0 = args.first().cloned().unwrap_or_default();
    egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(radius::CONTROL)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("{}", i + 1)).font(font_semibold(type_scale::BODY)).color(t.text_dim));
            ui.label(RichText::new(kind.icon()).color(t.accent));
            let mut k = kind;
            egui::ComboBox::from_id_salt(("rule-step-kind", i)).width(210.0).selected_text(kind.label()).show_ui(ui, |ui| {
                for s in Step::ALL {
                    ui.selectable_value(&mut k, s, s.label());
                }
            });
            if k != kind {
                let next = match k {
                    Step::Custom => cmd.to_string(),
                    Step::Wait => "wait 2s".into(),
                    k => k.verb().to_string(),
                };
                *edit = Some((i, next));
            }
            let w = (ui.available_width() - 90.0).clamp(140.0, 360.0);
            let new_arg = match kind {
                Step::Preset | Step::Release => pick_name(ui, ("rule-step-arg", i), &a0, &names.presets, "Pick an effect", w),
                Step::Scene | Step::Preview => pick_name(ui, ("rule-step-arg", i), &a0, &names.scenes, "Pick a scene", w),
                Step::Sound => pick_name(ui, ("rule-step-arg", i), &a0, &names.sounds, "Pick a sound", w),
                Step::Mix => pick_name(ui, ("rule-step-arg", i), &a0, &names.mixes, "Pick a mix", w),
                Step::Mode => pick_name(ui, ("rule-step-arg", i), &a0, &names.modes, "Pick a mode", w),
                Step::TimelinePlay | Step::TimelineStop => pick_name(ui, ("rule-step-arg", i), &a0, &names.timelines, "Pick a timeline", w),
                Step::Lights => {
                    let cue = args.get(1).cloned().unwrap_or_default();
                    if let Some(l) = pick_name(ui, ("rule-step-arg", i), &a0, &names.cuelists, "Pick a cue list", w * 0.65) {
                        *edit = Some((i, build_step(kind, &[&l, &cue])));
                    }
                    let mut c = cue.clone();
                    if ui.add(se_ui_kit::widgets::field(&mut c).hint_text("cue (optional)").desired_width(90.0)).changed() {
                        *edit = Some((i, build_step(kind, &[&a0, &c])));
                    }
                    None
                }
                Step::Say => {
                    let mut s = a0.clone();
                    ui.add(se_ui_kit::widgets::field(&mut s).hint_text("Thanks for the support!").desired_width(w)).changed().then_some(s)
                }
                Step::Wait => {
                    let mut s = a0.clone();
                    let r = ui.add(se_ui_kit::widgets::field(&mut s).hint_text("2s").desired_width(70.0)).changed().then_some(s);
                    widgets::hint(ui, t, "like 2s or 1m");
                    r
                }
                Step::Marker => {
                    let mut s = a0.clone();
                    ui.add(se_ui_kit::widgets::field(&mut s).hint_text("What happened (optional)").desired_width(w)).changed().then_some(s)
                }
                Step::Custom => None,
            };
            if let Some(v) = new_arg {
                *edit = Some((i, build_step(kind, &[&v])));
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
        if kind == Step::Custom {
            field(app, ui, &format!("do:{i}"), "e.g. preset.fire hype · bot.say 'thanks {user}!'", move |d| &mut d.commands[i]);
            if !cmd.trim().is_empty() {
                widgets::hint(ui, t, &format!("Does: {}", custom_phrase(cmd)));
            }
        } else if kind == Step::Say {
            let when = app.build.rules.edit.as_ref().map(|d| d.when.clone()).unwrap_or_default();
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Insert").color(t.text_dim));
                let extra =
                    fields_for(&when).into_iter().filter(|f| !matches!(*f, "user" | "message" | "input" | "is_gift" | "automatic" | "down" | "key" | "page"));
                for f in std::iter::once("user").chain(extra) {
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

fn limit_chips(ui: &mut egui::Ui, t: &Theme, csv: &mut String, known: &[(String, String)]) {
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
    });
    if let Some(n) = flip {
        toggle_csv(csv, &n);
    }
}

fn limits_card(app: &mut App, ui: &mut egui::Ui, t: &Theme, names: &Names) {
    widgets::titled(
        ui,
        t,
        "Limits",
        "Optional. How often and when it may happen.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let Some(d) = app.build.rules.edit.as_mut() else { return };
            ui.horizontal(|ui| {
                ui.label("Don't repeat for");
                ui.add(se_ui_kit::widgets::field(&mut d.cooldown_global).hint_text("10s").desired_width(70.0));
                ui.add_space(spacing::L);
                ui.label("The same viewer waits");
                ui.add(se_ui_kit::widgets::field(&mut d.cooldown_actor).hint_text("5m").desired_width(70.0));
            });
            widgets::hint(ui, t, "Times like 10s, 5m or 1h. Leave empty for no limit.");
            ui.add_space(spacing::M);
            widgets::section(ui, t, "", "Only during these show modes");
            limit_chips(ui, t, &mut d.modes, &names.modes);
            widgets::hint(ui, t, if d.modes.trim().is_empty() { "None picked: any mode." } else { "Click again to remove." });
            ui.add_space(spacing::M);
            widgets::section(ui, t, "", "Only on these scenes");
            limit_chips(ui, t, &mut d.scenes, &names.scenes);
            widgets::hint(ui, t, if d.scenes.trim().is_empty() { "None picked: any scene." } else { "Click again to remove." });
        },
    );
}

fn footer(app: &mut App, ui: &mut egui::Ui, t: &Theme) {
    let Some(d) = app.build.rules.edit.clone() else { return };
    widgets::panel(ui, t, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new("Try it").font(font_semibold(type_scale::BODY)).color(t.fg));
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
                    "user" => "Alex",
                    "from" => "Alex",
                    "reward" => "Hydrate",
                    _ => "",
                };
                ui.label(RichText::new(field_info(k).0).color(t.text_dim));
                ui.add(se_ui_kit::widgets::field(v).hint_text(sample).desired_width(if field_info(k).1 == Some(FieldKind::Text) { 140.0 } else { 80.0 }));
            }
            if widgets::button_ex(ui, t, Some(icon::PLAY), "Test it", Kind::Secondary, Size::Small, 0.0, !d.when.trim().is_empty())
                .on_hover_text("Plays the event as if it really happened, with these pretend values. Save first to test your changes.")
                .clicked()
            {
                sync_test_args(app);
                if let Some(d) = app.build.rules.edit.as_ref() {
                    app.m.text(&d.test_command());
                }
            }
        });
        ui.add_space(spacing::S);
        widgets::details(ui, t, "rule-details", "Details", |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Name").color(t.text_dim));
                if let Some(e) = app.build.rules.edit.as_mut() {
                    ui.add(se_ui_kit::widgets::field(&mut e.name).hint_text("Made from the sentence when you save").desired_width(320.0));
                }
            });
            widgets::hint(ui, t, "Buttons and chat commands use this name to switch the reaction on or off.");
            ui.horizontal(|ui| {
                ui.label(RichText::new("Saved in").color(t.text_dim));
                if let Some(e) = app.build.rules.edit.as_mut() {
                    if e.orig.is_none() {
                        ui.add(se_ui_kit::widgets::field(&mut e.file).font(font_mono(type_scale::SMALL + 0.5)).desired_width(240.0));
                    } else {
                        ui.label(RichText::new(&e.file).font(font_mono(type_scale::SMALL + 0.5)).color(t.fg));
                    }
                }
                if widgets::button_ex(ui, t, Some(icon::CONSOLE), "Open the file", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                    app.open_in_editor(&d.file, None);
                }
            });
            widgets::hint(ui, t, "The exact text Stream Engine runs. Suggestions appear as you type.");
            ui.label(RichText::new("When").color(t.text_dim));
            field(app, ui, "raw-when", "twitch.cheer · mode.enter.brb · band.drop", |d| &mut d.when);
            ui.label(RichText::new("Only if").color(t.text_dim));
            field(app, ui, "raw-if", "event.bits >= 1000 && mode == 'live'", |d| &mut d.cond);
            ui.label(RichText::new("Do (one command per line, in order; wait 2s pauses)").color(t.text_dim));
            let n = app.build.rules.edit.as_ref().map_or(0, |d| d.commands.len());
            for i in 0..n {
                field(app, ui, &format!("raw-do:{i}"), "preset.fire hype · bot.say 'thanks {user}!'", move |d| &mut d.commands[i]);
            }
        });
        if d.orig.is_some() {
            ui.add_space(spacing::M);
            ui.horizontal(|ui| {
                if widgets::hold_button(ui, t, "Delete reaction", t.bright_red, 0.6) {
                    let text = file_text(app, &d.file);
                    if let Some(args) = d.delete_args(text.as_deref()) {
                        app.m.action("project.write", args);
                        app.m.toast(format!("Deleted “{}”.", d.name.trim()), false);
                    }
                    app.build.rules.edit = None;
                    app.m.refresh_soon();
                }
                widgets::hint(ui, t, "Press and hold to delete.");
            });
        }
    });
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
        for cmd in ["preset.fire hype", "scene.cut brb", "lights.cue main 2", "wait 2s", "bot.say 'HYPE! Thanks {user} for {bits} bits'"] {
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
}
