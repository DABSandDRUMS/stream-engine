//! Inline links on settings (docs/ui-model.md). The ∿ button beside a numeric setting makes it
//! follow a signal (modulation, saved as a binding); the ⚡ badge beside an on/off setting lists
//! what turns it on and off (triggers → actions) and adds one more. [`users_of`], [`triggered`]
//! and [`modulated`] answer the same questions for badges drawn elsewhere.
//!
//! Data (kept fresh by [`keep_fresh`]): queries `bindings`, `config.bindings`, `rules`,
//! `presets`, `config.presets`, `controllers.deck`, `controllers.midi`, `bot.commands`,
//! `timelines`. Writes happen only on an explicit Create / Save / Remove: `project.write`
//! (links, events, saved actions), `deck.assign`, `midi.learn`, `bot.command.save`.

use crate::app::{App, ViewId};
use crate::views::live::nice;
use crate::views::modulate::{self, Draft};
use crate::views::rules;
use egui::{Align, Color32, CornerRadius, Id, Layout, Pos2, Response, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};
use se_core::bindings::BindingRt;
use se_core::config::{BindMode, BindingDef, Curve};
use se_proto::{Meta, Op, Value, address};
use se_ui_kit::Theme;
use se_ui_kit::motion;
use se_ui_kit::theme::{font, font_medium, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, Tone, icon};
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Modulation glyph (Nerd Font `md-sine_wave`), shared with the scene editor's layer rows.
pub const SINE: &str = "\u{f095b}";

const MOD_W: f32 = 440.0;
const TRIG_W: f32 = 440.0;
/// Seconds between refreshes of the slower sources (controllers, chat commands, timelines).
const FRESH_SECS: f32 = 4.0;
const SOURCES: [&str; 6] = ["config.bindings", "config.presets", "controllers.deck", "controllers.midi", "bot.commands", "timelines"];
/// Queries the index of "who changes what" is built from.
const INDEXED: [&str; 7] = ["rules", "config.presets", "presets", "controllers.deck", "controllers.midi", "bot.commands", "timelines"];

/// Popover drafts, the fetch clock and the index of who changes what (lives in
/// `app.build.modulate.links`).
#[derive(Default)]
pub struct LinksState {
    fetched: Option<Instant>,
    index: Mutex<Option<([u64; 7], Arc<Vec<Hit>>)>>,
    modp: Option<ModPop>,
    trig: Option<TrigPop>,
}

/// Refresh the queries the badges read (throttled; call once per frame on pages that show them).
pub fn keep_fresh(app: &mut App) {
    let st = &mut app.build.modulate.links;
    if !app.m.connected || st.fetched.is_some_and(|t| t.elapsed().as_secs_f32() < FRESH_SECS) {
        return;
    }
    st.fetched = Some(Instant::now());
    for q in SOURCES {
        app.m.query(q, Value::Null);
    }
}

/// Ask again on the next [`keep_fresh`] (after a write).
pub(crate) fn refetch(app: &mut App) {
    app.build.modulate.links.fetched = None;
    app.m.refresh_soon();
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn lines<'a>(v: &'a Value, k: &str) -> Vec<&'a str> {
    match v.get_path(k) {
        Some(Value::List(l)) => l.iter().filter_map(Value::as_str).collect(),
        Some(Value::Str(x)) => vec![x.as_str()],
        _ => Vec::new(),
    }
}

fn list<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get_path(k).and_then(Value::as_list).unwrap_or(&[])
}

// ---- who changes what ---------------------------------------------------------------------------

/// One place that changes a setting (or fires a saved action), in words.
#[derive(Clone, Debug, PartialEq)]
pub struct Use {
    pub icon: &'static str,
    /// Where: "Stream Deck · key 3", "When someone cheers", "Chat command !blur".
    pub title: String,
    /// What it does to the setting: "toggles", "turns on", "on while held".
    pub does: String,
    /// The editor that owns it.
    pub view: ViewId,
    /// For events: the row of the `rules` query to open in the editor.
    pub rule: Option<Value>,
}

/// One place that changes an address (or wildcard pattern).
#[derive(Clone, Debug)]
struct Hit {
    address: String,
    at: Use,
}

/// Query replies the index is built from.
#[derive(Default)]
struct Sources<'a> {
    rules: &'a [Value],
    /// `config.presets`: id → definition.
    presets: Option<&'a Value>,
    /// `presets`: labels.
    labels: &'a [Value],
    decks: &'a [Value],
    midis: &'a [Value],
    commands: &'a [Value],
    timelines: &'a [Value],
}

impl<'a> Sources<'a> {
    fn of(app: &'a App) -> Sources<'a> {
        Sources {
            rules: app.m.q_list("rules"),
            presets: app.m.q("config.presets"),
            labels: app.m.q_list("presets"),
            decks: app.m.q_list("controllers.deck"),
            midis: app.m.q_list("controllers.midi"),
            commands: app.m.q_list("bot.commands"),
            timelines: app.m.q_list("timelines"),
        }
    }
}

/// `1.5` → "1.5", `2.0` → "2".
fn num(f: f64) -> String {
    let t = format!("{f:.3}");
    let t = t.trim_end_matches('0').trim_end_matches('.');
    if t == "-0" { "0".into() } else { t.to_string() }
}

fn value_words(v: &Value) -> String {
    match v {
        Value::Bool(true) => "on".into(),
        Value::Bool(false) => "off".into(),
        Value::Int(i) => i.to_string(),
        Value::Float(f) => num(*f),
        Value::Str(x) => format!("\u{201c}{x}\u{201d}"),
        _ => "a new value".into(),
    }
}

fn set_words(v: &Value) -> String {
    match v {
        Value::Bool(true) => "turns on".into(),
        Value::Bool(false) => "turns off".into(),
        Value::Null => "changes it".into(),
        v => format!("sets to {}", value_words(v)),
    }
}

/// A saved action's `set` entry: held while it runs.
fn while_words(v: &Value) -> String {
    match v {
        Value::Bool(true) => "on while it runs".into(),
        Value::Bool(false) => "off while it runs".into(),
        v => format!("set to {} while it runs", value_words(v)),
    }
}

/// The address one command line changes, and how, in words.
fn line_hit(line: &str) -> Option<(String, String)> {
    Some(match Op::parse(line.trim()).ok()? {
        Op::Set { address, value } | Op::SetBase { address, value } => (address, set_words(&value)),
        Op::Animate { address, to, .. } => (address, format!("animates to {}", value_words(&to))),
        Op::Release { address } => (address, "puts it back".into()),
        Op::Trigger { address, .. } => (address, "fires it".into()),
        Op::Action { name, args } if name == "toggle" => (args.get_path("address")?.as_str()?.to_string(), "toggles".into()),
        _ => return None,
    })
}

/// `fs_a` → "Footswitch A", `enc.3` → "Knob 3", `push.3` → "Knob 3 (press)".
fn control_words(n: &str) -> String {
    let parts: Vec<&str> = n.split(['.', '_']).filter(|p| !p.is_empty()).collect();
    match parts.as_slice() {
        ["fs", x] => format!("Footswitch {}", x.to_uppercase()),
        ["enc", x] => format!("Knob {x}"),
        ["push", x] => format!("Knob {x} (press)"),
        _ => nice(&parts.join(" ")),
    }
}

/// `72.4` seconds → "1:12".
fn time_words(secs: f64) -> String {
    let s = secs.max(0.0).round() as i64;
    format!("{}:{:02}", s / 60, s % 60)
}

/// Every place that changes an address.
fn scan(src: &Sources) -> Vec<Hit> {
    let mut out: Vec<Hit> = Vec::new();
    let mut push = |address: String, icon: &'static str, title: &str, does: String, view: ViewId, rule: Option<&Value>| {
        out.push(Hit { address, at: Use { icon, title: title.to_string(), does, view, rule: rule.cloned() } });
    };
    for r in src.rules {
        let title = format!("When {}", rules::when_sentence(s(r, "when")));
        for l in lines(r, "do") {
            if let Some((target, does)) = line_hit(l) {
                push(target, icon::BOLT, &title, does, ViewId::Reactions, Some(r));
            }
        }
    }
    if let Some(Value::Map(presets)) = src.presets {
        for (id, p) in presets {
            let label = [s(p, "label"), src.labels.iter().find(|l| s(l, "name") == id).map(|l| s(l, "label")).unwrap_or("")]
                .into_iter()
                .find(|l| !l.is_empty())
                .map(rules::nice_name)
                .unwrap_or_else(|| rules::nice_name(id));
            let title = format!("Saved action · {label}");
            for l in lines(p, "do") {
                if let Some((target, does)) = line_hit(l) {
                    push(target, icon::PLAY, &title, does, ViewId::Actions, None);
                }
            }
            for l in lines(p, "on_release") {
                if let Some((target, does)) = line_hit(l) {
                    push(target, icon::PLAY, &title, format!("{does} when it stops"), ViewId::Actions, None);
                }
            }
            if let Some(Value::Map(set)) = p.get_path("set") {
                for (a, v) in set {
                    push(a.clone(), icon::PLAY, &title, while_words(v), ViewId::Actions, None);
                }
            }
        }
    }
    for d in src.decks {
        let name = if src.decks.len() > 1 { format!("Stream Deck {}", nice(s(d, "id"))) } else { "Stream Deck".to_string() };
        let pages = list(d, "pages");
        for p in pages {
            let page = if pages.len() > 1 {
                let l = s(p, "label");
                format!(" · {}", rules::nice_name(if l.is_empty() { s(p, "page") } else { l }))
            } else {
                String::new()
            };
            for k in list(p, "keys") {
                let title = format!("{name}{page} · key {}", k.get_path("key").and_then(Value::as_i64).unwrap_or(0) + 1);
                match s(k, "kind") {
                    "toggle" => push(s(k, "address").into(), icon::CONTROLLER, &title, "toggles".into(), ViewId::Controllers, None),
                    "momentary" => push(s(k, "address").into(), icon::CONTROLLER, &title, "on while held".into(), ViewId::Controllers, None),
                    "action" => {
                        for l in s(k, "action").split(';') {
                            if let Some((target, does)) = line_hit(l) {
                                push(target, icon::CONTROLLER, &title, does, ViewId::Controllers, None);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    for d in src.midis {
        let dev = if s(d, "client").is_empty() { nice(s(d, "id")) } else { s(d, "client").to_string() };
        for m in list(d, "maps") {
            let title = format!("{dev} · {}", control_words(s(m, "control")));
            let a = s(m, "action");
            let fixed = [("hold ", "on while held"), ("adjust ", "turns it up and down")];
            if let Some((addr, does)) = fixed.iter().find_map(|(p, w)| a.strip_prefix(p).map(|x| (x, *w))) {
                push(addr.trim().into(), icon::CONTROLLER, &title, does.into(), ViewId::Controllers, None);
                continue;
            }
            // `+ inc / − dec` for encoders, else the press commands
            let cmds: Vec<&str> = match a.strip_prefix("+ ") {
                Some(rest) => rest.split(" / ").flat_map(|x| x.trim_start_matches("− ").split(';')).collect(),
                None => a.split(';').collect(),
            };
            for l in cmds {
                if let Some((target, does)) = line_hit(l) {
                    push(target, icon::CONTROLLER, &title, does, ViewId::Controllers, None);
                }
            }
        }
    }
    for c in src.commands {
        let title = format!("Chat command {}", s(c, "name"));
        for l in lines(c, "do") {
            if let Some((target, does)) = line_hit(l) {
                push(target, icon::BOT, &title, does, ViewId::Chatbot, None);
            }
        }
    }
    for tl in src.timelines {
        let name = if s(tl, "label").is_empty() { rules::nice_name(s(tl, "name")) } else { rules::nice_name(s(tl, "label")) };
        for tr in list(tl, "tracks") {
            match s(tr, "type") {
                "cues" => {
                    for cue in list(tr, "cues") {
                        let at = cue.get_path("at").and_then(Value::as_f64).unwrap_or(0.0);
                        let when = if s(cue, "label").is_empty() { time_words(at) } else { s(cue, "label").to_string() };
                        let title = format!("Timeline {name} · {when}");
                        for l in lines(cue, "do") {
                            if let Some((target, does)) = line_hit(l) {
                                push(target, icon::TIMELINE, &title, does, ViewId::Timeline, None);
                            }
                        }
                    }
                }
                "regions" => {
                    for rg in list(tr, "regions") {
                        let start = rg.get_path("start").and_then(Value::as_f64).unwrap_or(0.0);
                        let when = if s(rg, "label").is_empty() { time_words(start) } else { s(rg, "label").to_string() };
                        let title = format!("Timeline {name} · {when}");
                        let Some(action) = rg.get_path("action").filter(|a| s(a, "kind") == "commands") else { continue };
                        for (k, suffix) in [("on", ""), ("off", " when it ends")] {
                            for l in lines(action, k) {
                                if let Some((target, does)) = line_hit(l) {
                                    push(target, icon::TIMELINE, &title, format!("{does}{suffix}"), ViewId::Timeline, None);
                                }
                            }
                        }
                    }
                }
                "automation" => {
                    let a = s(tr, "lane.address");
                    if !a.is_empty() {
                        push(a.into(), icon::TIMELINE, &format!("Timeline {name}"), "moves it over time".into(), ViewId::Timeline, None);
                    }
                }
                _ => {}
            }
        }
    }
    out
}

fn index(app: &App) -> Arc<Vec<Hit>> {
    let key = INDEXED.map(|q| app.m.q_seq(q));
    let mut g = app.build.modulate.links.index.lock().unwrap_or_else(|p| p.into_inner());
    if let Some((k, hits)) = g.as_ref()
        && *k == key
    {
        return hits.clone();
    }
    let hits = Arc::new(scan(&Sources::of(app)));
    *g = Some((key, hits.clone()));
    hits
}

fn users_in(hits: &[Hit], addr: &str) -> Vec<Use> {
    let mut out: Vec<Use> = Vec::new();
    for h in hits.iter().filter(|h| address::matches(&h.address, addr)) {
        if !out.contains(&h.at) {
            out.push(h.at.clone());
        }
    }
    out
}

/// Everything that changes `address`: events, saved actions, buttons and pedals, chat commands,
/// timeline cues (wildcard patterns included).
pub fn users_of(app: &App, address: &str) -> Vec<Use> {
    users_in(&index(app), address)
}

/// True when `pattern` (an address or wildcard pattern) can name an address under `prefix`
/// (`scene.duo.node.cam.`).
fn under(pattern: &str, prefix: &str) -> bool {
    if pattern.starts_with(prefix) {
        return true;
    }
    if !pattern.contains('*') {
        return false;
    }
    let want: Vec<&str> = prefix.trim_end_matches('.').split('.').collect();
    let have: Vec<&str> = pattern.split('.').collect();
    for (i, w) in want.iter().enumerate() {
        match have.get(i) {
            Some(&"**") => return true,
            Some(h) if address::matches(h, w) => {}
            _ => return false,
        }
    }
    have.len() > want.len()
}

/// Does any trigger change an address under `prefix` (`scene.<s>.node.<id>.`)?
pub fn triggered(app: &App, prefix: &str) -> bool {
    index(app).iter().any(|h| under(&h.address, prefix))
}

/// Does any link move an address under `prefix` (`scene.<s>.node.<id>.`)?
pub fn modulated(app: &App, prefix: &str) -> bool {
    app.m.q_list("bindings").iter().any(|b| under(s(b, "target"), prefix))
}

/// Rows of the `bindings` query that move `address` (exactly or by pattern).
fn bindings_for(app: &App, addr: &str) -> Vec<Value> {
    app.m.q_list("bindings").iter().filter(|b| address::matches(s(b, "target"), addr)).cloned().collect()
}

/// The full definition of a `bindings` row (from `config.bindings`).
pub(crate) fn config_def(app: &App, row: &Value) -> Option<BindingDef> {
    let (n, target) = (s(row, "name"), s(row, "target"));
    let v = app.m.q("config.bindings")?.as_list()?.iter().find(|b| s(b, "name") == n && (target.is_empty() || s(b, "target") == target))?;
    serde_json::from_value(serde_json::Value::from(v)).ok()
}

pub(crate) fn file_text(app: &App, file: &str) -> Option<String> {
    app.m.q(&format!("project.read:{file}")).and_then(|v| v.get_path("text")).and_then(Value::as_str).map(String::from)
}

pub(crate) fn read_file(app: &mut App, file: &str) {
    app.m.query_as(&format!("project.read:{file}"), "project.read", Value::map().with("path", file));
}

// ---- words: settings and signals --------------------------------------------------------------

/// Setting words: `offset_x` → "Offset X", `enabled` → "On", `gain` → "Volume".
fn param(rest: &[&str]) -> String {
    let words: Vec<&str> = rest
        .iter()
        .map(|w| match *w {
            "enabled" => "on",
            "gain" => "volume",
            "rect" => "position",
            "radius" => "corners",
            "z" => "stacking",
            w => w,
        })
        .collect();
    nice(&words.join(" ")).split(' ').map(|w| if w.chars().count() == 1 { w.to_uppercase() } else { w.to_string() }).collect::<Vec<_>>().join(" ")
}

fn canvas_words(c: &str) -> String {
    match c {
        "wide" => "Main".into(),
        "tall" => "Vertical".into(),
        c => nice(c),
    }
}

/// An address as parts a person reads: `scene.duo.node.cam.offset_x` → ["Duo", "Cam", "Offset X"].
/// `scene` names a scene by id (its label), when known.
pub fn address_parts(addr: &str, scene: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let seg: Vec<&str> = addr.split('.').collect();
    let scene_name = |s: &str| if s.contains('*') { "Every scene".to_string() } else { scene(s).unwrap_or_else(|| rules::nice_name(s)) };
    let layer = |id: &str| if id.contains('*') { "Every layer".to_string() } else { nice(id) };
    let effect = crate::views::scene_edit::effect_name;
    let mut out = match seg.as_slice() {
        ["scene", sc, "node", id, "fx", "patch", p, rest @ ..] => vec![scene_name(sc), layer(id), effect(&format!("patch.{p}")), param(rest)],
        ["scene", sc, "node", id, "fx", fx, rest @ ..] => vec![scene_name(sc), layer(id), effect(fx), param(rest)],
        ["scene", sc, "node", id, p @ ("rect" | "crop" | "radius" | "z"), c] => {
            vec![scene_name(sc), layer(id), format!("{} ({})", param(&[*p]), canvas_words(c))]
        }
        ["scene", sc, "node", id, rest @ ..] => vec![scene_name(sc), layer(id), param(rest)],
        ["scene", sc, "fx", "patch", p, rest @ ..] => vec![scene_name(sc), effect(&format!("patch.{p}")), param(rest)],
        ["scene", sc, "fx", fx, rest @ ..] => vec![scene_name(sc), effect(fx), param(rest)],
        ["scene", sc, rest @ ..] => vec![scene_name(sc), param(rest)],
        ["fx", fx, rest @ ..] => vec!["Whole screen".into(), effect(fx), param(rest)],
        ["patch", id, rest @ ..] => vec![effect(&format!("patch.{id}")), param(rest)],
        ["audio", "bus", b, rest @ ..] => vec!["Sound".into(), nice(b), param(rest)],
        ["audio", rest @ ..] => vec!["Sound".into(), param(rest)],
        ["lights", rest @ ..] => vec!["Lights".into(), param(rest)],
        _ => vec![param(&seg)],
    };
    out.retain(|p| !p.is_empty());
    out
}

/// Scene labels from the `scenes` query.
fn scene_label(app: &App) -> impl Fn(&str) -> Option<String> + '_ {
    |id: &str| {
        app.m.q_list("scenes").iter().find(|r| s(r, "name") == id).map(|r| {
            let l = s(r, "label");
            rules::nice_name(if l.is_empty() { id } else { l })
        })
    }
}

/// "Duo › Cam › Offset X".
pub fn address_label(app: &App, addr: &str) -> String {
    address_parts(addr, &scene_label(app)).join(" › ")
}

/// The last two parts ("Cam › Offset X"), for narrow lists.
pub fn address_short(app: &App, addr: &str) -> String {
    let p = address_parts(addr, &scene_label(app));
    p[p.len().saturating_sub(2)..].join(" › ")
}

/// Which list group a link's target belongs to (index into [`AREAS`]).
pub const AREAS: [&str; 6] = ["Layers", "Effects", "Sources", "Lights", "Sound", "Other"];

pub fn area(target: &str) -> usize {
    let first = target.split('.').next().unwrap_or("");
    if target.contains(".fx.") || first == "fx" {
        1
    } else if first == "scene" && target.contains(".node.") {
        0
    } else if matches!(first, "patch" | "source" | "sources" | "video_in" | "web") {
        2
    } else if first == "lights" {
        3
    } else if matches!(first, "audio" | "mixer" | "tts") {
        4
    } else {
        5
    }
}

/// Parts of an analysed bus: (signal suffix, chip label).
const BAND: [(&str, &str); 7] =
    [("level", "Level"), ("bass", "Bass"), ("mid", "Mids"), ("high", "Highs"), ("kick", "Kick"), ("snare", "Snare"), ("hat", "Hi-hat")];
const BEAT: [(&str, &str); 3] = [("beat.phase", "Beat position"), ("lfo.beat", "Every beat"), ("lfo.bar", "Every bar")];
const SLOW: [(&str, &str); 4] = [("lfo.slow", "Slow wave"), ("lfo.mid", "Medium wave"), ("lfo.fast", "Fast wave"), ("lfo.random", "Random drift")];
const CHANNEL: [(&str, &str); 2] = [("twitch.viewers", "Viewers"), ("twitch.chat_rate", "Chat speed")];
/// First segments that are never an analysed bus.
const NOT_BUSES: [&str; 11] = ["lfo", "beat", "twitch", "time", "audio", "midi", "osc", "drums", "signal", "mixer", "controllers"];

/// A signal as words: `band.kick` → "Band kick", `lfo.slow` → "Slow wave".
pub fn signal_label(name: &str) -> String {
    if let Some((_, l)) = BEAT.iter().chain(&SLOW).chain(&CHANNEL).find(|(n, _)| *n == name) {
        return l.to_string();
    }
    let seg: Vec<&str> = name.split('.').collect();
    match seg.as_slice() {
        ["beat", "bpm"] => "Tempo".into(),
        ["midi", dev, rest @ ..] => format!("{} {}", nice(dev), control_words(&rest.join("."))),
        [bus, part] if BAND.iter().any(|(p, _)| p == part) => {
            let w = BAND.iter().find(|(p, _)| p == part).map(|(_, w)| w.to_lowercase()).unwrap_or_default();
            format!("{} {w}", nice(bus))
        }
        _ => nice(&seg.join(" ")),
    }
}

/// [`signal_label`] for the middle of a sentence: "follows band kick", "follows X-TOUCH Fader 1".
pub fn signal_words(name: &str) -> String {
    let l = signal_label(name);
    let first = l.split(' ').next().unwrap_or("");
    // acronyms and brands ("X-TOUCH", "VHS") keep their capitals
    if first.chars().filter(|c| c.is_alphabetic()).count() > 1 && !first.chars().any(char::is_lowercase) {
        return l;
    }
    let mut c = l.chars();
    c.next().map(|f| f.to_lowercase().chain(c).collect()).unwrap_or_default()
}

/// A group of signals: short name (popover tabs), title, rows of (row label, [(signal, chip label)]).
struct Group {
    short: &'static str,
    title: &'static str,
    rows: Vec<(String, Vec<(String, String)>)>,
}

const CONTROLS: &str = "Controls";

/// Signals to offer. The Controls group is there whenever a MIDI device is (so a control can
/// be learned), even before it reports any knobs or faders.
fn signal_groups(app: &App) -> Vec<Group> {
    let mut buses: Vec<&str> = app.m.signals.keys().filter_map(|k| k.strip_suffix(".kick")).filter(|b| !b.contains('.') && !NOT_BUSES.contains(b)).collect();
    buses.sort_unstable();
    buses.dedup();
    let row = |items: &[(&str, &str)]| vec![(String::new(), items.iter().map(|(n, l)| (n.to_string(), l.to_string())).collect())];
    let mut out = Vec::new();
    if !buses.is_empty() {
        let rows = buses.iter().map(|b| (nice(b), BAND.iter().map(|(p, l)| (format!("{b}.{p}"), l.to_string())).collect())).collect();
        out.push(Group { short: "Music", title: "Music & drums", rows });
    }
    out.push(Group { short: "Beat", title: "Beat", rows: row(&BEAT[..]) });
    out.push(Group { short: "Motion", title: "Slow motion", rows: row(&SLOW[..]) });
    let midis = app.m.q_list("controllers.midi");
    if !midis.is_empty() {
        let rows = midis
            .iter()
            .filter_map(|d| {
                let id = s(d, "id");
                let items: Vec<(String, String)> = list(d, "controls")
                    .iter()
                    .filter(|c| !c.get_path("button").is_some_and(Value::truthy) && !c.get_path("relative").is_some_and(Value::truthy))
                    .map(|c| (format!("midi.{id}.{}", s(c, "name")), control_words(s(c, "name"))))
                    .collect();
                (!items.is_empty()).then(|| (if s(d, "client").is_empty() { nice(id) } else { s(d, "client").to_string() }, items))
            })
            .collect();
        out.push(Group { short: CONTROLS, title: CONTROLS, rows });
    }
    out.push(Group { short: "Channel", title: "Channel", rows: row(&CHANNEL[..]) });
    out
}

/// Signal chips, grouped. `compact` (popovers): one group at a time behind tabs; otherwise every
/// group under its heading. `learn`: the setting a "Move a control…" MIDI learn should drive.
/// Returns true when the signal changed.
pub(crate) fn signal_picker(app: &mut App, ui: &mut Ui, t: &Theme, signal: &mut String, learn: Option<&str>, compact: bool) -> bool {
    let groups = signal_groups(app);
    let has = |g: &Group, sig: &str| g.rows.iter().any(|(_, items)| items.iter().any(|(n, _)| n == sig));
    let mut changed = false;
    let mut chips = |ui: &mut Ui, label: &str, items: &[(String, String)], signal: &mut String| {
        if compact && !label.is_empty() {
            ui.add_space(spacing::XS);
            ui.label(RichText::new(label).size(type_scale::SMALL).color(t.text_dim));
        }
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing = Vec2::splat(spacing::XS + 2.0);
            if !compact && !label.is_empty() {
                ui.allocate_ui_with_layout(Vec2::new(88.0, 28.0), Layout::left_to_right(Align::Center), |ui| {
                    ui.set_min_width(88.0);
                    ui.add(egui::Label::new(RichText::new(label).size(type_scale::SMALL + 0.5).color(t.text_dim)).truncate());
                });
            }
            for (n, l) in items {
                if widgets::chip(ui, t, "", l, *signal == *n).on_hover_text(n.as_str()).clicked() && *signal != *n {
                    *signal = n.clone();
                    changed = true;
                }
            }
        });
    };
    if !signal.is_empty() && !groups.iter().any(|g| has(g, signal.as_str())) {
        widgets::group_label(ui, t, "Now");
        chips(ui, "", &[(signal.clone(), signal_label(signal))], signal);
    }
    let shown: Vec<usize> = if compact {
        let id = ui.id().with("links-signal-group");
        let current = groups.iter().position(|g| has(g, signal.as_str())).unwrap_or(0);
        let mut i = ui.data(|d| d.get_temp::<usize>(id)).unwrap_or(current).min(groups.len() - 1);
        let tabs: Vec<&str> = groups.iter().map(|g| g.short).collect();
        if widgets::segmented(ui, t, &mut i, &tabs) {
            ui.data_mut(|d| d.insert_temp(id, i));
        }
        ui.add_space(spacing::XS);
        vec![i]
    } else {
        (0..groups.len()).collect()
    };
    for gi in shown {
        let g = &groups[gi];
        if !compact {
            widgets::group_label(ui, t, g.title);
        }
        for (label, items) in &g.rows {
            chips(ui, label, items, signal);
        }
        if g.rows.is_empty() {
            widgets::hint(ui, t, "Your MIDI controller hasn't reported any knobs or faders yet.");
        }
        let Some(target) = learn.filter(|a| !a.is_empty() && g.title == CONTROLS) else { continue };
        ui.add_space(spacing::XS);
        if app.m.b("controllers.learn.active") && app.m.str("controllers.learn.target") == target {
            if widgets::callout(
                ui,
                t,
                Tone::Warn,
                icon::HAND,
                "Move a knob or fader now",
                "The first control you move will drive this setting.",
                Some("Cancel"),
            ) {
                app.m.action("midi.learn.cancel", Value::Null);
            }
        } else if widgets::chip(ui, t, icon::HAND, "Move a control…", false).on_hover_text("Link the next knob or fader you move").clicked() {
            app.m.action("midi.learn", Value::map().with("target", target));
            refetch(app);
        }
    }
    changed
}

/// Curve choice as chips. Returns true when changed.
pub(crate) fn curve_chips(ui: &mut Ui, t: &Theme, c: &mut Curve) -> bool {
    let mut changed = false;
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(spacing::XS + 2.0);
        for k in [Curve::Linear, Curve::Exp, Curve::Log, Curve::Smoothstep, Curve::FaderTaper] {
            if widgets::chip(ui, t, "", modulate::curve_words(k), *c == k).clicked() && *c != k {
                *c = k;
                changed = true;
            }
        }
    });
    changed
}

/// From/to drag values within `range` (unbounded when the range is empty).
pub(crate) fn range_values(ui: &mut Ui, t: &Theme, r: &mut [f64; 2], range: (f64, f64)) {
    let (lo, hi) = (range.0.min(range.1), range.0.max(range.1));
    let bounded = hi > lo;
    let speed = if bounded { ((hi - lo) / 400.0).max(0.0005) } else { 0.01 };
    for (i, v) in r.iter_mut().enumerate() {
        if i == 1 {
            ui.label(RichText::new("to").color(t.text_faint));
        }
        let mut dv = egui::DragValue::new(v).speed(speed).max_decimals(3);
        if bounded {
            dv = dv.range(lo..=hi);
        }
        ui.add(dv);
    }
}

/// Rise / fall times.
pub(crate) fn smoothing(ui: &mut Ui, t: &Theme, d: &mut Draft) {
    ui.add(egui::DragValue::new(&mut d.attack_ms).range(0.0..=5000.0).speed(1.0).prefix("rise ").suffix(" ms"));
    ui.label(RichText::new("·").color(t.text_faint));
    ui.add(egui::DragValue::new(&mut d.release_ms).range(0.0..=10000.0).speed(2.0).prefix("fall ").suffix(" ms"));
}

/// The signal (faint) and what the setting does with it, live.
pub(crate) fn live_scope(app: &App, ui: &mut Ui, t: &Theme, d: &Draft, h: f32) {
    let hist: Vec<f32> = app.m.signals.get(&d.signal).map(|h| h.iter().copied().collect()).unwrap_or_default();
    let shaped = d.simulate(&hist, 1.0 / 30.0);
    let r = d.range.map(|[a, b]| [a.min(b) as f32, a.max(b) as f32]).unwrap_or([0.0, 1.0]);
    se_ui_kit::curve::dual_scope(ui, t, Vec2::new(ui.available_width(), h), &hist, &shaped, r);
}

// ---- the ∿ and ⚡ buttons -------------------------------------------------------------------------

fn popup_id(r: &Response) -> Id {
    r.id.with("links-popover")
}

/// Small ghost icon button with an optional count, tinted by state; highlighted while its
/// popover is open.
fn glyph_button(ui: &mut Ui, t: &Theme, glyph: &str, count: &str, color: Color32, tip: &str) -> Response {
    let g = ui.painter().layout_no_wrap(glyph.to_string(), font(type_scale::BODY), color);
    let c = (!count.is_empty()).then(|| ui.painter().layout_no_wrap(count.to_string(), font_medium(type_scale::SMALL), color));
    let content = g.size().x + c.as_ref().map_or(0.0, |c| c.size().x + 4.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new((content + 14.0).max(28.0), 28.0), Sense::click());
    let open = egui::Popup::is_id_open(ui.ctx(), popup_id(&resp));
    if ui.is_rect_visible(rect) {
        let k = motion::t(ui.ctx(), resp.id.with("hover"), resp.hovered() || open, motion::FAST);
        let p = ui.painter();
        if k > 0.0 {
            p.rect(rect, CornerRadius::same(radius::CONTROL), t.surface_hi.gamma_multiply(k), Stroke::new(1.0, t.border.gamma_multiply(k)), StrokeKind::Inside);
        }
        let mut x = rect.center().x - content / 2.0;
        let gy = g.size().y;
        let gx = g.size().x;
        p.galley(Pos2::new(x, rect.center().y - gy / 2.0), g, color);
        x += gx + 4.0;
        if let Some(c) = c {
            let cy = c.size().y;
            p.galley(Pos2::new(x, rect.center().y - cy / 2.0), c, color);
        }
    }
    resp.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, tip));
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if open { resp } else { resp.on_hover_text(tip) }
}

fn popover_title(ui: &mut Ui, t: &Theme, title: &str, sub: &str) {
    ui.label(RichText::new(title).font(font_semibold(type_scale::BODY + 1.0)).color(t.fg));
    if !sub.is_empty() {
        widgets::hint(ui, t, sub);
    }
    ui.add_space(spacing::S);
}

/// ∿ button beside a numeric setting at `address`: tinted when a link moves it; opens a popover
/// to link it to a signal (or change / remove the link). `range` bounds the from/to values.
pub fn modulate_button(app: &mut App, ui: &mut Ui, address: &str, label: &str, range: (f64, f64)) -> Response {
    keep_fresh(app);
    let t = app.t.clone();
    let linked = bindings_for(app, address);
    let (color, tip) = match linked.first() {
        Some(b) if b.get_path("enabled").is_none_or(Value::truthy) => (t.modulated(), format!("Follows {}", signal_words(s(b, "signal")))),
        Some(b) => (mix(t.modulated(), t.text_faint, 0.5), format!("Follows {} (off)", signal_words(s(b, "signal")))),
        None => (t.text_faint, "Follow a signal".to_string()),
    };
    let resp = glyph_button(ui, &t, SINE, "", color, &tip);
    popover(&resp, MOD_W, |ui| mod_popover(app, ui, &t, address, label, range));
    resp
}

/// A popover under `resp` (a click outside closes it); tall content scrolls.
fn popover(resp: &Response, width: f32, body: impl FnOnce(&mut Ui)) {
    egui::Popup::from_toggle_button_response(resp).id(popup_id(resp)).close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside).width(width).show(|ui| {
        ui.set_width(width);
        // the popup's area starts at egui's default size: ask for the height the content needs
        let h = (ui.ctx().content_rect().height() - 160.0).clamp(240.0, 640.0);
        egui::ScrollArea::vertical().max_height(h).min_scrolled_height(h).auto_shrink([false, true]).show(ui, body);
    });
}

/// The ∿ popover's draft (one at a time).
struct ModPop {
    address: String,
    /// The link being changed (its name) and its definition as loaded.
    name: Option<String>,
    orig: Option<Draft>,
    draft: Option<Draft>,
    /// Which of several links on this setting.
    which: usize,
    /// A new link was written; waiting for it to show up.
    saved_at: Option<Instant>,
}

impl ModPop {
    fn new(address: &str) -> ModPop {
        ModPop { address: address.to_string(), name: None, orig: None, draft: None, which: 0, saved_at: None }
    }
}

/// A new link's starting point: from where the setting is now, a tenth of its range away.
fn new_draft(app: &App, addr: &str, range: (f64, f64)) -> Draft {
    let mut d = Draft::new(addr, &Meta::default());
    d.mode = BindMode::Replace;
    let (lo, hi) = (range.0.min(range.1), range.0.max(range.1));
    // where it is now; unknown: 0 when the range allows it (offsets), else the low end
    let now = app.m.get(addr).and_then(Value::as_f64).unwrap_or(if lo <= 0.0 && hi >= 0.0 { 0.0 } else { lo });
    let now = if hi > lo { now.clamp(lo, hi) } else { now };
    let step = if hi > lo { (hi - lo) * 0.1 } else { 1.0 };
    let to = if hi <= lo || now + step <= hi { now + step } else { (now - step).max(lo) };
    d.range = Some([now, to]);
    d
}

fn mod_popover(app: &mut App, ui: &mut Ui, t: &Theme, addr: &str, label: &str, range: (f64, f64)) {
    let mut pop = match app.build.modulate.links.modp.take() {
        Some(p) if p.address == addr => p,
        _ => ModPop::new(addr),
    };
    popover_title(ui, t, &format!("Follow a signal \u{2014} {label}"), "");
    let linked = bindings_for(app, addr);
    if linked.is_empty() {
        if pop.name.take().is_some() {
            pop.draft = None;
            pop.orig = None;
        }
        if pop.saved_at.is_some_and(|at| at.elapsed().as_secs_f32() < 3.0) {
            widgets::hint(ui, t, "Saving…");
        } else {
            pop.saved_at = None;
            new_link(app, ui, t, &mut pop, addr, label, range);
        }
    } else {
        pop.saved_at = None;
        linked_link(app, ui, t, &mut pop, &linked, label, range);
    }
    app.build.modulate.links.modp = Some(pop);
}

fn new_link(app: &mut App, ui: &mut Ui, t: &Theme, pop: &mut ModPop, addr: &str, label: &str, range: (f64, f64)) {
    if pop.draft.is_none() {
        pop.draft = Some(new_draft(app, addr, range));
    }
    let Some(d) = pop.draft.as_mut() else { return };
    widgets::hint(ui, t, "Pick what it should move with.");
    signal_picker(app, ui, t, &mut d.signal, Some(addr), true);
    ui.add_space(spacing::S);
    if let Some(r) = d.range.as_mut() {
        widgets::prop_row(ui, t, "From", |ui| range_values(ui, t, r, range));
    }
    widgets::prop_row(ui, t, "Response", |ui| curve_chips(ui, t, &mut d.curve));
    widgets::prop_row(ui, t, "Smoothing", |ui| smoothing(ui, t, d));
    if !d.signal.is_empty() {
        ui.add_space(spacing::XS);
        live_scope(app, ui, t, d, 56.0);
    }
    ui.add_space(spacing::S);
    let ok = !d.signal.is_empty() && BindingRt::new(d.def()).is_ok();
    if widgets::button_ex(ui, t, Some(icon::CHECK), "Create", Kind::Primary, Size::Medium, 0.0, ok).clicked() {
        let mut d = d.clone();
        let tail = |a: &str| a.rsplit('.').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("_");
        let taken: Vec<String> = app.m.q_list("bindings").iter().map(|b| s(b, "file").to_string()).collect();
        d.name = modulate::unique_name(&format!("{}_{}", tail(addr), d.signal.rsplit('.').next().unwrap_or("")), &taken);
        app.m.action("project.write", d.write_args(None));
        app.m.toast(format!("{label} now follows {}.", signal_words(&d.signal)), false);
        refetch(app);
        pop.draft = None;
        pop.saved_at = Some(Instant::now());
    }
}

fn linked_link(app: &mut App, ui: &mut Ui, t: &Theme, pop: &mut ModPop, linked: &[Value], label: &str, range: (f64, f64)) {
    if linked.len() > 1 {
        widgets::hint(ui, t, &format!("{} links move this setting.", linked.len()));
        ui.horizontal_wrapped(|ui| {
            for (i, b) in linked.iter().enumerate() {
                if widgets::chip(ui, t, SINE, &signal_label(s(b, "signal")), pop.which == i).clicked() {
                    pop.which = i;
                }
            }
        });
        ui.add_space(spacing::XS);
    }
    let row = &linked[pop.which.min(linked.len() - 1)];
    let name = s(row, "name").to_string();
    let file = Some(s(row, "file")).filter(|f| !f.is_empty()).map(String::from);
    if pop.name.as_deref() != Some(name.as_str()) || pop.draft.is_none() {
        let Some(def) = config_def(app, row) else {
            widgets::hint(ui, t, "Loading…");
            return;
        };
        let d = Draft::from_def(&def, file.clone());
        pop.orig = Some(d.clone());
        pop.draft = Some(d);
        pop.name = Some(name);
        if let Some(f) = &file {
            read_file(app, f);
        }
    }
    let Some(d) = pop.draft.as_mut() else { return };
    let text = file.as_deref().and_then(|f| file_text(app, f));
    widgets::prop_row(ui, t, "Follows", |ui| {
        ui.label(RichText::new(signal_label(&d.signal)).font(font_medium(type_scale::BODY)).color(t.modulated()));
    });
    widgets::details(ui, t, ("links-change-signal", &d.target), "Change signal", |ui| {
        signal_picker(app, ui, t, &mut d.signal, None, true);
    });
    match d.range.as_mut() {
        Some(r) => {
            widgets::prop_row(ui, t, "From", |ui| range_values(ui, t, r, range));
        }
        None => {
            widgets::prop_row(ui, t, "Strength", |ui| ui.add(egui::DragValue::new(&mut d.gain).speed(0.01).range(-20.0..=20.0).prefix("× ")));
        }
    }
    widgets::prop_row(ui, t, "Response", |ui| curve_chips(ui, t, &mut d.curve));
    widgets::prop_row(ui, t, "Smoothing", |ui| smoothing(ui, t, d));
    widgets::prop_row(ui, t, "On", |ui| widgets::toggle(ui, t, &mut d.enabled));
    ui.add_space(spacing::XS);
    live_scope(app, ui, t, d, 56.0);
    if d.target.contains('*') {
        ui.add_space(spacing::XS);
        widgets::hint(ui, t, &format!("This link also moves other settings ({}).", address_label(app, &d.target)));
    }
    ui.add_space(spacing::S);
    let dirty = pop.orig.as_ref() != Some(&*d);
    let loaded = file.is_none() || text.is_some();
    let valid = !d.signal.is_empty() && BindingRt::new(d.def()).is_ok();
    let (mut save, mut more, mut remove) = (false, false, false);
    ui.horizontal(|ui| {
        save = widgets::button_ex(ui, t, Some(icon::CHECK), "Save", Kind::Primary, Size::Small, 0.0, dirty && loaded && valid).clicked();
        more = widgets::button_ex(ui, t, None, "All settings", Kind::Ghost, Size::Small, 0.0, true).on_hover_text("Open it in Modulation").clicked();
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.add_enabled_ui(loaded, |ui| remove = widgets::hold_button(ui, t, "Remove", t.bright_red, 0.6));
        });
    });
    let d = d.clone();
    if save {
        app.m.action("project.write", d.write_args(text.as_deref()));
        app.m.toast(format!("Saved the link on {label}."), false);
        pop.orig = Some(d.clone());
        if let Some(f) = &file {
            read_file(app, f);
        }
        refetch(app);
    }
    if remove && let Some(args) = d.delete_args(text.as_deref()) {
        app.m.action("project.write", args);
        app.m.toast(format!("{label} no longer follows {}.", signal_words(&d.signal)), false);
        pop.name = None;
        pop.draft = None;
        pop.orig = None;
        pop.saved_at = Some(Instant::now());
        refetch(app);
    }
    if more {
        app.build.modulate.edit_existing(row);
        app.open_view(ViewId::Modulation);
        egui::Popup::close_all(ui.ctx());
    }
}

/// ⚡ badge beside an on/off setting at `address`: how many things turn it on or off; opens a
/// popover listing them (click one to open its editor) with "Add a trigger".
pub fn trigger_badge(app: &mut App, ui: &mut Ui, address: &str, label: &str) -> Response {
    keep_fresh(app);
    let t = app.t.clone();
    let uses = users_of(app, address);
    let (color, count, tip) = match uses.as_slice() {
        [] => (t.text_faint, String::new(), "Add a trigger".to_string()),
        [u] => (t.fg, "1".to_string(), format!("{} \u{2014} {}", u.title, u.does)),
        many => (t.fg, many.len().to_string(), format!("{} things turn this on or off", many.len())),
    };
    let resp = glyph_button(ui, &t, icon::BOLT, &count, color, &tip);
    popover(&resp, TRIG_W, |ui| trig_popover(app, ui, &t, address, label, &uses));
    resp
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TKind {
    Button,
    Chat,
    Event,
    Saved,
}

const TKINDS: [(TKind, &str, &str); 4] = [
    (TKind::Button, icon::CONTROLLER, "Button or pedal"),
    (TKind::Chat, icon::BOT, "Chat command"),
    (TKind::Event, icon::BOLT, "Stream event"),
    (TKind::Saved, icon::PLAY, "Saved action"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Behave {
    On,
    Off,
    Toggle,
}

impl Behave {
    fn words(self) -> &'static str {
        match self {
            Behave::On => "Turn on",
            Behave::Off => "Turn off",
            Behave::Toggle => "Toggle",
        }
    }
    /// The command line it runs.
    fn line(self, addr: &str) -> String {
        match self {
            Behave::On => format!("set {addr} true"),
            Behave::Off => format!("set {addr} false"),
            Behave::Toggle => format!("toggle {addr}"),
        }
    }
    /// Choices for a kind of trigger (a stream event flipping a switch back and forth isn't
    /// something anyone means).
    fn options(kind: TKind) -> &'static [Behave] {
        match kind {
            TKind::Event => &[Behave::On, Behave::Off],
            _ => &[Behave::On, Behave::Off, Behave::Toggle],
        }
    }
}

/// The ⚡ popover's "Add a trigger" draft.
struct TrigPop {
    address: String,
    kind: Option<TKind>,
    behave: Behave,
    /// Button or pedal: index into decks, or `decks.len()` for MIDI learn.
    device: usize,
    page: String,
    key: Option<i64>,
    chat: String,
    event: String,
    preset: String,
}

impl TrigPop {
    fn new(address: &str) -> TrigPop {
        TrigPop {
            address: address.to_string(),
            kind: None,
            behave: Behave::Toggle,
            device: 0,
            page: String::new(),
            key: None,
            chat: String::new(),
            event: String::new(),
            preset: String::new(),
        }
    }
}

fn open_use(app: &mut App, ui: &Ui, u: &Use) {
    if let Some(r) = &u.rule {
        app.build.rules.open(r);
    }
    app.open_view(u.view);
    egui::Popup::close_all(ui.ctx());
}

fn trig_popover(app: &mut App, ui: &mut Ui, t: &Theme, addr: &str, label: &str, uses: &[Use]) {
    let mut pop = match app.build.modulate.links.trig.take() {
        Some(p) if p.address == addr => p,
        _ => TrigPop::new(addr),
    };
    popover_title(ui, t, label, "What turns this on and off.");
    if uses.is_empty() {
        widgets::hint(ui, t, "Nothing turns it on or off yet.");
    }
    let mut open = None;
    for u in uses {
        if widgets::list_row(ui, t, u.icon, &u.title, "", &u.does, false).on_hover_text("Open it").clicked() {
            open = Some(u.clone());
        }
    }
    if let Some(u) = open {
        open_use(app, ui, &u);
    }
    widgets::group_label(ui, t, "Add a trigger");
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(spacing::XS + 2.0);
        for (k, ic, words) in TKINDS {
            if widgets::chip(ui, t, ic, words, pop.kind == Some(k)).clicked() && pop.kind != Some(k) {
                pop.kind = Some(k);
                let opts = Behave::options(k);
                pop.behave = if opts.contains(&Behave::Toggle) { Behave::Toggle } else { Behave::On };
            }
        }
    });
    if let Some(kind) = pop.kind {
        ui.add_space(spacing::S);
        add_trigger(app, ui, t, &mut pop, kind, addr, label);
    }
    app.build.modulate.links.trig = Some(pop);
}

/// Behavior choice (Turn on / Turn off / Toggle).
fn behave_row(ui: &mut Ui, t: &Theme, pop: &mut TrigPop, kind: TKind) {
    let opts = Behave::options(kind);
    let words: Vec<&str> = opts.iter().map(|b| b.words()).collect();
    let mut i = opts.iter().position(|b| *b == pop.behave).unwrap_or(0);
    widgets::prop_row(ui, t, "It should", |ui| {
        if widgets::segmented(ui, t, &mut i, &words) {
            pop.behave = opts[i];
        }
    });
}

fn add_trigger(app: &mut App, ui: &mut Ui, t: &Theme, pop: &mut TrigPop, kind: TKind, addr: &str, label: &str) {
    let opts = Behave::options(kind);
    if !opts.contains(&pop.behave) {
        pop.behave = opts[0];
    }
    match kind {
        TKind::Button => add_button(app, ui, t, pop, addr, label),
        TKind::Chat => add_chat(app, ui, t, pop, addr, label),
        TKind::Event => add_event(app, ui, t, pop, addr, label),
        TKind::Saved => add_saved(app, ui, t, pop, addr, label),
    }
}

fn create_button(ui: &mut Ui, t: &Theme, ok: bool) -> bool {
    ui.add_space(spacing::S);
    widgets::button_ex(ui, t, Some(icon::CHECK), "Create", Kind::Primary, Size::Medium, 0.0, ok).clicked()
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

fn add_button(app: &mut App, ui: &mut Ui, t: &Theme, pop: &mut TrigPop, addr: &str, label: &str) {
    let decks = app.m.q_list("controllers.deck").to_vec();
    let midi = !app.m.q_list("controllers.midi").is_empty();
    if decks.is_empty() && !midi {
        if widgets::callout(ui, t, Tone::Info, icon::CONTROLLER, "No Stream Deck or MIDI controller yet", "Connect one, then come back here.", Some("Devices"))
        {
            app.open_view(ViewId::Devices);
            egui::Popup::close_all(ui.ctx());
        }
        return;
    }
    // which device
    let n = decks.len() + usize::from(midi);
    pop.device = pop.device.min(n - 1);
    if n > 1 {
        ui.horizontal_wrapped(|ui| {
            for (i, d) in decks.iter().enumerate() {
                let name = if decks.len() > 1 { format!("Stream Deck {}", nice(s(d, "id"))) } else { "Stream Deck".into() };
                if widgets::chip(ui, t, icon::KEYBOARD, &name, pop.device == i).clicked() {
                    pop.device = i;
                    pop.key = None;
                }
            }
            if midi && widgets::chip(ui, t, icon::CONTROLLER, "MIDI pedal or button", pop.device == decks.len()).clicked() {
                pop.device = decks.len();
            }
        });
        ui.add_space(spacing::XS);
    }
    behave_row(ui, t, pop, TKind::Button);
    if pop.device == decks.len() {
        // MIDI: learn writes the mapping when the pedal is pressed
        let target = match pop.behave {
            Behave::Toggle => addr.to_string(),
            b => format!("do:{}", b.line(addr)),
        };
        let learning = app.m.b("controllers.learn.active");
        if learning && app.m.str("controllers.learn.target") == target {
            if widgets::callout(ui, t, Tone::Warn, icon::HAND, "Press a pedal or button now", "The first one you press gets this job.", Some("Cancel")) {
                app.m.action("midi.learn.cancel", Value::Null);
            }
            return;
        }
        widgets::hint(ui, t, "Press Learn, then the pedal or MIDI button you want to use.");
        ui.add_space(spacing::S);
        if widgets::button_ex(ui, t, Some(icon::HAND), "Learn", Kind::Primary, Size::Medium, 0.0, !learning).clicked() {
            app.m.action("midi.learn", Value::map().with("target", target));
            refetch(app);
        }
        return;
    }
    let deck = &decks[pop.device];
    let pages = list(deck, "pages");
    if pages.is_empty() {
        widgets::hint(ui, t, "This Stream Deck has no pages yet. Add one in Buttons & pedals.");
        return;
    }
    if !pages.iter().any(|p| s(p, "page") == pop.page) {
        pop.page = s(deck, "page").to_string();
        if !pages.iter().any(|p| s(p, "page") == pop.page) {
            pop.page = s(&pages[0], "page").to_string();
        }
        pop.key = None;
    }
    if pages.len() > 1 {
        widgets::prop_row(ui, t, "Page", |ui| {
            ui.horizontal_wrapped(|ui| {
                for p in pages {
                    let l = if s(p, "label").is_empty() { s(p, "page") } else { s(p, "label") };
                    if widgets::chip(ui, t, "", &rules::nice_name(l), s(p, "page") == pop.page).clicked() {
                        pop.page = s(p, "page").to_string();
                        pop.key = None;
                    }
                }
            });
        });
    }
    let page = pages.iter().find(|p| s(p, "page") == pop.page).unwrap_or(&pages[0]);
    let used: Vec<(i64, String)> = list(page, "keys")
        .iter()
        .filter(|k| s(k, "kind") != "empty")
        .map(|k| (k.get_path("key").and_then(Value::as_i64).unwrap_or(-1), s(k, "label").to_string()))
        .collect();
    let count = deck.get_path("keys").and_then(Value::as_i64).unwrap_or(15).max(1);
    let cols = deck.get_path("cols").and_then(Value::as_i64).unwrap_or(5).max(1);
    widgets::prop_row(ui, t, "Key", |ui| key_grid(ui, t, cols, count, &used, &mut pop.key));
    if create_button(ui, t, pop.key.is_some())
        && let Some(k) = pop.key
    {
        let mut args = Value::map().with("deck", s(deck, "id")).with("page", pop.page.clone()).with("key", k).with("label", clip(label, 14));
        args = match pop.behave {
            Behave::Toggle => args.with("toggle", addr),
            b => args.with("do", Value::List(vec![Value::Str(b.line(addr))])),
        };
        app.m.action("deck.assign", args);
        app.m.toast(format!("Key {} now does: {} {}.", k + 1, pop.behave.words().to_lowercase(), label), false);
        pop.kind = None;
        pop.key = None;
        refetch(app);
    }
}

/// A grid of deck keys: used ones are shown and can't be picked.
fn key_grid(ui: &mut Ui, t: &Theme, cols: i64, count: i64, used: &[(i64, String)], sel: &mut Option<i64>) {
    const CELL: f32 = 34.0;
    const GAP: f32 = 4.0;
    let rows = (count + cols - 1) / cols;
    let size = Vec2::new(cols as f32 * (CELL + GAP) - GAP, rows as f32 * (CELL + GAP) - GAP);
    let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
    for k in 0..count {
        let (r, c) = (k / cols, k % cols);
        let cell = egui::Rect::from_min_size(rect.min + Vec2::new(c as f32 * (CELL + GAP), r as f32 * (CELL + GAP)), Vec2::splat(CELL));
        let taken = used.iter().find(|(n, _)| *n == k);
        let resp = ui.interact(cell, ui.id().with(("links-key", k)), if taken.is_some() { Sense::hover() } else { Sense::click() });
        let selected = *sel == Some(k);
        let (fill, stroke) = if selected {
            (mix(t.surface, t.accent, 0.12), t.accent)
        } else if taken.is_some() {
            (t.surface_hi, t.border)
        } else if resp.hovered() {
            (t.surface, t.text_faint)
        } else {
            (t.surface, t.border)
        };
        let p = ui.painter();
        p.rect(cell, CornerRadius::same(radius::CONTROL), fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
        let (text, color) = match taken {
            Some((_, l)) if !l.is_empty() => (clip(l, 4), t.text_faint),
            Some(_) => ("•".to_string(), t.text_faint),
            None => ((k + 1).to_string(), if selected { t.fg } else { t.text_dim }),
        };
        p.text(cell.center(), egui::Align2::CENTER_CENTER, text, font(type_scale::SMALL - 1.0), color);
        match taken {
            Some((_, l)) => {
                resp.on_hover_text(if l.is_empty() { format!("Key {} is in use", k + 1) } else { format!("Key {} is in use ({l})", k + 1) });
            }
            None => {
                if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    *sel = Some(k);
                }
            }
        }
    }
}

fn add_chat(app: &mut App, ui: &mut Ui, t: &Theme, pop: &mut TrigPop, addr: &str, label: &str) {
    if pop.chat.is_empty() {
        pop.chat = format!("!{}", modulate::sanitize(label).replace('_', ""));
    }
    widgets::prop_row(ui, t, "Command", |ui| ui.add(widgets::field(&mut pop.chat).desired_width(180.0)));
    behave_row(ui, t, pop, TKind::Chat);
    let name = pop.chat.trim().to_string();
    let name = if name.starts_with('!') { name } else { format!("!{name}") };
    let taken = app.m.q_list("bot.commands").iter().any(|c| s(c, "name") == name || lines(c, "aliases").contains(&name.as_str()));
    let valid = name.len() > 1 && !name.chars().any(char::is_whitespace);
    if taken {
        widgets::hint(ui, t, &format!("{name} already exists. Pick another word."));
    } else {
        widgets::hint(ui, t, "Anyone in chat can type it. Changes from chat wear off after a short while.");
    }
    if create_button(ui, t, valid && !taken) {
        let fields = Value::map().with("name", name.clone()).with("do", Value::List(vec![Value::Str(pop.behave.line(addr))]));
        app.m.action("bot.command.save", Value::map().with("fields", fields));
        app.m.toast(format!("{name} now does: {} {label}.", pop.behave.words().to_lowercase()), false);
        pop.kind = None;
        pop.chat.clear();
        refetch(app);
    }
}

fn add_event(app: &mut App, ui: &mut Ui, t: &Theme, pop: &mut TrigPop, addr: &str, label: &str) {
    let events: Vec<(&str, &str, &str, &str)> = rules::event_choices().filter(|(p, ..)| !p.contains('*')).collect();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(spacing::XS + 2.0);
        for (p, l, _, ic) in &events {
            if widgets::chip(ui, t, ic, l, pop.event == *p).clicked() {
                pop.event = p.to_string();
            }
        }
    });
    ui.add_space(spacing::XS);
    behave_row(ui, t, pop, TKind::Event);
    let chosen = events.iter().find(|(p, ..)| *p == pop.event);
    if let Some((_, _, sentence, _)) = chosen {
        widgets::hint(ui, t, &format!("When {sentence}, {} {label}.", pop.behave.words().to_lowercase()));
    }
    if create_button(ui, t, chosen.is_some())
        && let Some((p, l, ..)) = chosen
    {
        let taken: Vec<&str> = app.m.q_list("rules").iter().map(|r| s(r, "name")).collect();
        let base = format!("{l}: {} {label}", pop.behave.words().to_lowercase());
        let mut name = base.clone();
        let mut n = 2;
        while taken.contains(&name.as_str()) {
            name = format!("{base} {n}");
            n += 1;
        }
        let set = Value::map().with("name", name).with("when", *p).with("do", Value::List(vec![Value::Str(pop.behave.line(addr))]));
        app.m.action("project.write", Value::map().with("path", "rules/ui.toml").with("table", "rule").with("append", true).with("set", set));
        app.m.toast(format!("Added: when {}, {} {label}.", rules::when_sentence(p), pop.behave.words().to_lowercase()), false);
        pop.kind = None;
        pop.event.clear();
        refetch(app);
    }
}

fn add_saved(app: &mut App, ui: &mut Ui, t: &Theme, pop: &mut TrigPop, addr: &str, label: &str) {
    let saved: Vec<(String, String)> = app
        .m
        .q_list("presets")
        .iter()
        .map(|p| {
            let l = s(p, "label");
            (s(p, "name").to_string(), rules::nice_name(if l.is_empty() { s(p, "name") } else { l }))
        })
        .collect();
    if saved.is_empty() {
        if widgets::callout(ui, t, Tone::Info, icon::PLAY, "No saved actions yet", "Make one in Automation → Actions, then add this to it.", Some("Actions"))
        {
            app.open_view(ViewId::Actions);
            egui::Popup::close_all(ui.ctx());
        }
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(spacing::XS + 2.0);
        for (id, l) in &saved {
            if widgets::chip(ui, t, icon::PLAY, l, pop.preset == *id).clicked() {
                pop.preset = id.clone();
            }
        }
    });
    ui.add_space(spacing::XS);
    behave_row(ui, t, pop, TKind::Saved);
    let def = app.m.q("config.presets").and_then(Value::as_map).and_then(|m| m.get(&pop.preset)).cloned();
    if let Some((_, l)) = saved.iter().find(|(id, _)| *id == pop.preset) {
        widgets::hint(ui, t, &format!("Firing {l} will also {} {label}.", pop.behave.words().to_lowercase()));
    }
    if create_button(ui, t, def.is_some())
        && let Some(def) = def
    {
        let mut cmds: Vec<Value> = lines(&def, "do").into_iter().map(|l| Value::Str(l.to_string())).collect();
        cmds.push(Value::Str(pop.behave.line(addr)));
        let path = format!("presets/{}.toml", pop.preset);
        app.m.action("project.write", Value::map().with("path", path).with("set", Value::map().with("do", Value::List(cmds))));
        app.m.toast(format!("Added to the saved action: {} {label}.", pop.behave.words().to_lowercase()), false);
        pop.kind = None;
        refetch(app);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strs(l: &[&str]) -> Value {
        Value::List(l.iter().map(|x| Value::Str(x.to_string())).collect())
    }

    #[test]
    fn users_of_reads_every_command_form() {
        let cam = "scene.duo.node.cam.fx.blur.enabled";
        let rules = vec![
            Value::map().with("name", "follow").with("when", "twitch.follow").with("do", strs(&[&format!("set {cam} true"), "preset.fire hype"])),
            Value::map().with("name", "any").with("when", "twitch.raid").with("do", strs(&["set scene.*.node.cam.fx.blur.enabled false"])),
        ];
        let presets = Value::map().with(
            "hype",
            Value::map()
                .with("label", "HYPE")
                .with("do", strs(&[&format!("toggle {cam}")]))
                .with("on_release", strs(&[&format!("animate {cam} 0 1s")]))
                .with("set", Value::map().with(cam, true)),
        );
        let key = |n: i64, kind: &str| Value::map().with("key", n).with("kind", kind);
        let decks = vec![Value::map().with("id", "main").with(
            "pages",
            Value::List(vec![Value::map().with("page", "home").with(
                "keys",
                Value::List(vec![
                    key(2, "toggle").with("address", cam),
                    key(3, "momentary").with("address", cam),
                    key(4, "action").with("action", format!("set {cam} true; preset.release hype")),
                    key(5, "preset").with("preset", "hype"),
                ]),
            )]),
        )];
        let midis = vec![Value::map().with("id", "xtouch").with("client", "X-TOUCH MINI").with(
            "maps",
            Value::List(vec![
                Value::map().with("control", "fs_a").with("action", format!("toggle {cam}")),
                Value::map().with("control", "enc.1").with("action", format!("adjust {cam}")),
            ]),
        )];
        let commands = vec![Value::map().with("name", "!blur").with("do", strs(&[&format!("set {cam} false")]))];
        let src = Sources { rules: &rules, presets: Some(&presets), decks: &decks, midis: &midis, commands: &commands, ..Default::default() };
        let hits = scan(&src);
        let got: Vec<(String, String)> = users_in(&hits, cam).into_iter().map(|u| (u.title, u.does)).collect();
        let want = [
            ("When someone follows", "turns on"),
            ("When someone raids", "turns off"),
            ("Saved action · Hype", "toggles"),
            ("Saved action · Hype", "animates to 0 when it stops"),
            ("Saved action · Hype", "on while it runs"),
            ("Stream Deck · key 3", "toggles"),
            ("Stream Deck · key 4", "on while held"),
            ("Stream Deck · key 5", "turns on"),
            ("X-TOUCH MINI · Footswitch A", "toggles"),
            ("X-TOUCH MINI · Knob 1", "turns it up and down"),
            ("Chat command !blur", "turns off"),
        ];
        assert_eq!(got, want.map(|(a, b)| (a.to_string(), b.to_string())));
        // the rule row travels with the use so the Events editor can open it
        assert_eq!(users_in(&hits, cam)[0].rule.as_ref().map(|r| s(r, "name")), Some("follow"));
        // other settings are left alone
        assert!(users_in(&hits, "scene.duo.node.cam.visible").is_empty());
    }

    #[test]
    fn prefixes_match_addresses_and_patterns_under_them() {
        let p = "scene.duo.node.cam.";
        assert!(under("scene.duo.node.cam.opacity", p));
        assert!(under("scene.*.node.cam.fx.blur.enabled", p));
        assert!(under("scene.duo.node.cam_*.scale", "scene.duo.node.cam_kit."));
        assert!(!under("scene.duo.node.cam_*.scale", p), "`cam_*` needs the underscore");
        assert!(under("scene.**", p));
        assert!(!under("scene.duo.node.cam2.opacity", p));
        assert!(!under("scene.duo.node.*", p), "names the layer itself, nothing under it");
        assert!(!under("scene.solo.node.cam.opacity", p));
    }

    #[test]
    fn addresses_and_signals_read_as_words() {
        let scene = |id: &str| (id == "duo").then(|| "Duo cam".to_string());
        let w = |a: &str| address_parts(a, &scene).join(" › ");
        assert_eq!(w("scene.duo.node.cam_kit.offset_x"), "Duo cam › Cam kit › Offset X");
        assert_eq!(w("scene.duo.node.cam.fx.rgb_split.enabled"), "Duo cam › Cam › Color split › On");
        assert_eq!(w("scene.duo.node.cam.fx.patch.aurora.speed"), "Duo cam › Cam › Aurora › Speed");
        assert_eq!(w("scene.*.node.cam.crop.tall"), "Every scene › Cam › Crop (Vertical)");
        assert_eq!(w("scene.solo.node.logo.rect.wide"), "Solo › Logo › Position (Main)");
        assert_eq!(w("fx.vhs.amount"), "Whole screen › VHS tape › Amount");
        assert_eq!(w("audio.bus.music.gain"), "Sound › Music › Volume");
        assert_eq!(w("lights.master"), "Lights › Master");
        assert_eq!(signal_label("band.kick"), "Band kick");
        assert_eq!(signal_label("music.hat"), "Music hi-hat");
        assert_eq!(signal_label("lfo.slow"), "Slow wave");
        assert_eq!(signal_label("midi.xtouch.fs_a"), "X-TOUCH Footswitch A");
        assert_eq!(signal_words("band.kick"), "band kick");
        assert_eq!(signal_words("midi.xtouch.fader_1"), "X-TOUCH Fader 1", "a brand keeps its capitals mid-sentence");
        assert_eq!(area("scene.duo.node.cam.offset_x"), 0);
        assert_eq!(area("scene.duo.node.cam.fx.blur.amount"), 1);
        assert_eq!(area("audio.bus.music.gain"), 4);
    }
}
