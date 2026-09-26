//! Automation → Actions: saved actions. A saved action is a named list of steps that any
//! trigger, button, pedal or chat command can run (`presets/<id>.toml`: `do = [...]`, how long
//! it lasts — `hold`, `toggle`, `until_released` — plus `confirm` and `chat`). A new project has
//! none; they're only made here, from an explicit Create.
//!
//! Left: every saved action with its color and what it does in one line. Right: the selected one
//! (or a new draft) — Try it / Stop, Save / Discard while changed, then Steps (the shared steps
//! editor), How long, Safety, Controls (knobs: live while it runs, saved by themselves), what
//! else its file does, what runs it (with "Put on a button"), and the file.
//!
//! Engine: queries `presets` (with `knobs`), `presets.catalog`, `errors`, `project.read`;
//! actions `project.write`, `preset.knob {name, target, value, save}`; commands `preset.fire` /
//! `preset.release`. Other keys in the file (`fx`, `set`, `lights`, knobs, …) are left alone:
//! `project.write` only touches the keys the editor owns.

use crate::app::{App, ViewId};
use crate::views::knobs::{self, Moved};
use crate::views::live::nice;
use crate::views::{build, quick_edit, rules};
use egui::{Align, Color32, CornerRadius, Layout, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_medium, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, Tone, icon};
use std::collections::HashMap;

/// Seconds a knob has to rest before its value is saved (a slider let go saves at once).
const SAVE_AFTER: f64 = 0.6;
/// How often the catalog (labels, what each one does, where each is used) is refreshed.
const CATALOG_EVERY: f64 = 3.0;
/// Width of the list pane.
const LIST_W: f32 = 300.0;

fn state_id() -> egui::Id {
    egui::Id::new("saved-actions-view")
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn strs(v: &Value, k: &str) -> Vec<String> {
    v.get_path(k).and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(Value::as_str).map(String::from).collect()
}

fn push_new(v: &mut Vec<String>, x: String) {
    if !v.contains(&x) {
        v.push(x);
    }
}

/// `#rrggbb` for a color.
fn hex_of(c: Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

fn file_of(id: &str) -> String {
    format!("presets/{id}.toml")
}

// ---- the draft -----------------------------------------------------------------------------------

/// How long a saved action lasts once it runs.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Length {
    /// Runs its steps and is done.
    #[default]
    Once,
    /// `hold = "<dur>"`: on for a while, then undone.
    For,
    /// `toggle = true`: on until run again.
    Toggle,
    /// `until_released = true`: on until released (a button let go).
    Held,
}

impl Length {
    const ALL: [Length; 4] = [Length::Once, Length::For, Length::Toggle, Length::Held];

    fn label(self) -> &'static str {
        match self {
            Length::Once => "Once",
            Length::For => "For a time",
            Length::Toggle => "Until pressed again",
            Length::Held => "While held",
        }
    }

    fn help(self) -> &'static str {
        match self {
            Length::Once => "Runs its steps and is done.",
            Length::For => "Stays on for a while, then undoes what its steps changed.",
            Length::Toggle => "Stays on until it runs again, then undoes what its steps changed.",
            Length::Held => "Stays on while its button is held down; letting go undoes what its steps changed.",
        }
    }
}

/// What the editor changes in a saved action's file.
#[derive(Clone, Debug, PartialEq)]
struct Draft {
    /// File id (`presets/<id>.toml`); `None` until it's created.
    id: Option<String>,
    label: String,
    /// `#rrggbb`, or empty for none.
    color: String,
    steps: Vec<String>,
    length: Length,
    /// How long with [`Length::For`] (`8s`).
    hold: String,
    /// Ask before running on air (hold to confirm; chat can never run it).
    confirm: bool,
    /// Viewers may run it from chat.
    chat: bool,
}

impl Draft {
    fn new() -> Draft {
        Draft { id: None, label: String::new(), color: String::new(), steps: Vec::new(), length: Length::Once, hold: "5s".into(), confirm: false, chat: false }
    }

    /// Read the keys the editor owns from the file's text.
    fn from_toml(id: &str, text: &str) -> Result<Draft, String> {
        let t: toml::Table = text.parse().map_err(|e: toml::de::Error| e.message().to_string())?;
        let st = |k: &str| t.get(k).and_then(toml::Value::as_str).unwrap_or("").to_string();
        let b = |k: &str| t.get(k).and_then(toml::Value::as_bool);
        let hold = match t.get("hold") {
            Some(toml::Value::String(s)) => Some(s.clone()),
            Some(toml::Value::Integer(n)) => Some(format!("{n}ms")),
            Some(toml::Value::Float(f)) => Some(format!("{}ms", f.round())),
            _ => None,
        };
        let length = if b("toggle") == Some(true) {
            Length::Toggle
        } else if b("until_released") == Some(true) {
            Length::Held
        } else if hold.is_some() {
            Length::For
        } else {
            Length::Once
        };
        let confirm = b("confirm") == Some(true);
        Ok(Draft {
            id: Some(id.to_string()),
            label: st("label"),
            color: st("color"),
            steps: t.get("do").and_then(toml::Value::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()).unwrap_or_default(),
            length,
            hold: hold.unwrap_or_else(|| "5s".into()),
            confirm,
            // the engine's default: chat may run it unless it asks first
            chat: !confirm && b("chat").unwrap_or(true),
        })
    }

    /// What has to change before it can be saved (empty = saveable).
    fn errors(&self) -> Vec<String> {
        let mut e = Vec::new();
        if self.label.trim().is_empty() {
            e.push("Give it a name.".to_string());
        }
        if self.id.is_none() && self.steps.iter().all(|c| c.trim().is_empty()) {
            e.push("Add at least one step.".into());
        }
        for (i, c) in self.steps.iter().enumerate().filter(|(_, c)| !c.trim().is_empty()) {
            if let Some(what) = rules::step_missing(c) {
                e.push(format!("Step {}: {what}.", i + 1));
            }
        }
        if self.length == Length::For && se_proto::parse_duration_ms(&self.hold).is_none_or(|ms| ms == 0) {
            e.push(format!("“{}” isn't a time. Use something like 8s or 1m.", self.hold.trim()));
        }
        e
    }

    /// The keys written (null removes one that no longer applies); other keys stay as they are.
    fn set_value(&self) -> Value {
        let or_null = |on: bool, v: Value| if on { v } else { Value::Null };
        let color = self.color.trim();
        Value::map()
            .with("label", self.label.trim())
            .with("color", or_null(!color.is_empty(), Value::Str(color.into())))
            .with("do", Value::List(self.steps.iter().map(|c| c.trim()).filter(|c| !c.is_empty()).map(|c| Value::Str(c.into())).collect()))
            .with("hold", or_null(self.length == Length::For, Value::Str(self.hold.trim().into())))
            .with("toggle", or_null(self.length == Length::Toggle, Value::Bool(true)))
            .with("until_released", or_null(self.length == Length::Held, Value::Bool(true)))
            .with("confirm", or_null(self.confirm, Value::Bool(true)))
            // chat may run it by default; only "no" is written (asking first already rules chat out)
            .with("chat", or_null(!self.confirm && !self.chat, Value::Bool(false)))
    }

    /// `project.write` args: a new file for a new saved action (`id` picked among `taken`), else
    /// the owned keys of the existing one.
    fn write_args(&self, taken: &[String]) -> (String, Value) {
        let id = self.id.clone().unwrap_or_else(|| quick_edit::free_id(&self.label, taken));
        let args = Value::map().with("path", file_of(&id)).with("set", self.set_value());
        (id, args)
    }
}

// ---- engine data ----------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Use {
    kind: String,
    file: String,
    file_label: String,
    name: String,
}

/// What a saved action does, in words.
#[derive(Clone, Default)]
struct About {
    screen: Vec<String>,
    lights: String,
    sound: Vec<String>,
    also: Vec<String>,
    /// One line for the list: "Color split, Confetti, Fast chase · 8 seconds".
    line: String,
}

/// `presets.catalog`, parsed once per reply.
#[derive(Clone, Default)]
struct Catalog {
    seq: u64,
    loaded: bool,
    /// name → label of each kind of thing a saved action runs
    effects: HashMap<String, String>,
    overlays: HashMap<String, String>,
    looks: HashMap<String, String>,
    cuelists: HashMap<String, String>,
    sounds: HashMap<String, String>,
    about: HashMap<String, About>,
    uses: HashMap<String, Vec<Use>>,
}

/// "1 second", "2.5 seconds".
fn secs_words(s: f64) -> String {
    let s = (s * 10.0).round() / 10.0;
    if s == 1.0 {
        "1 second".into()
    } else if s.fract() == 0.0 {
        format!("{} seconds", s as i64)
    } else {
        format!("{s} seconds")
    }
}

impl Catalog {
    fn parse(v: &Value, seq: u64) -> Catalog {
        let labels = |k: &str, key: &str| -> HashMap<String, String> {
            v.get_path(k)
                .and_then(Value::as_list)
                .unwrap_or(&[])
                .iter()
                .map(|x| {
                    let (n, l) = (s(x, key), s(x, "label"));
                    (n.to_string(), if l.is_empty() { nice(n) } else { l.to_string() })
                })
                .collect()
        };
        let mut c = Catalog {
            seq,
            loaded: true,
            effects: labels("effects", "name"),
            overlays: labels("overlays", "id"),
            looks: labels("looks", "name"),
            cuelists: labels("cuelists", "name"),
            sounds: labels("sounds", "name"),
            about: HashMap::new(),
            uses: HashMap::new(),
        };
        if let Some(m) = v.get_path("used_by").and_then(Value::as_map) {
            for (id, l) in m {
                let uses = l
                    .as_list()
                    .unwrap_or(&[])
                    .iter()
                    .map(|u| Use { kind: s(u, "kind").into(), file: s(u, "file").into(), file_label: s(u, "file_label").into(), name: s(u, "name").into() })
                    .collect();
                c.uses.insert(id.clone(), uses);
            }
        }
        if let Some(m) = v.get_path("presets").and_then(Value::as_map) {
            let about: HashMap<String, About> = m.iter().map(|(id, p)| (id.clone(), c.describe(p))).collect();
            c.about = about;
        }
        c
    }

    fn label(map: &HashMap<String, String>, name: &str) -> String {
        map.get(name).cloned().unwrap_or_else(|| nice(name))
    }

    /// The words for one saved action's summary from the catalog.
    fn describe(&self, p: &Value) -> About {
        let mut screen: Vec<String> = strs(p, "effects").iter().map(|e| Self::label(&self.effects, e)).collect();
        screen.extend(strs(p, "overlays").iter().map(|o| Self::label(&self.overlays, o)));
        let lights_name = s(p, "lights_name");
        let mut lights = match s(p, "lights") {
            "look" => Self::label(&self.looks, lights_name),
            "cuelist" => Self::label(&self.cuelists, lights_name),
            _ => String::new(),
        };
        let mut sound = Vec::new();
        let mut also = Vec::new();
        if !s(p, "sound").is_empty() {
            sound.push(format!("Plays “{}”", Self::label(&self.sounds, s(p, "sound"))));
        }
        for t in strs(p, "triggers") {
            let segs: Vec<&str> = t.split('.').collect();
            match segs.as_slice() {
                ["audio", "bus", bus, "fx", fx] => push_new(
                    &mut sound,
                    format!("{} on the {}", nice(fx), if *bus == "music" { "music".to_string() } else { format!("{} sound", nice(bus).to_lowercase()) }),
                ),
                ["audio", ..] => push_new(&mut sound, "A sound effect".into()),
                _ => push_new(&mut also, nice(segs.last().copied().unwrap_or(""))),
            }
        }
        for a in strs(p, "set") {
            if let Some(rest) = a.strip_prefix("fx.") {
                push_new(&mut screen, Self::label(&self.effects, rest.split('.').next().unwrap_or("")));
            } else if a.starts_with("lights.") {
                if lights.is_empty() {
                    lights = "Changes the lights".into();
                }
            } else if a.starts_with("audio.duck") {
                push_new(&mut sound, "Turns the music down".into());
            } else if a.starts_with("audio.") || a.starts_with("mixer.") {
                push_new(&mut sound, "Changes the sound mix".into());
            } else {
                push_new(&mut also, "Changes a setting".into());
            }
        }
        if !s(p, "mix").is_empty() {
            sound.push(format!("Recalls the sound mix “{}”", nice(s(p, "mix"))));
        }
        if !s(p, "scene").is_empty() {
            also.push(format!("Switches to the scene “{}”", nice(s(p, "scene"))));
        }
        if !s(p, "mode").is_empty() {
            also.push(format!("Switches to {} mode", nice(s(p, "mode")).to_lowercase()));
        }
        match p.get_path("steps").and_then(Value::as_i64).unwrap_or(0) {
            0 => {}
            1 => also.push("Runs one extra step".into()),
            n => also.push(format!("Runs {n} extra steps")),
        }
        let held = p.get_path("settings").and_then(Value::as_i64).unwrap_or(0) > 0;
        let short = if p.get_path("toggle").is_some_and(Value::truthy) {
            "until pressed again".to_string()
        } else if p.get_path("until_released").is_some_and(Value::truthy) {
            "while held".to_string()
        } else if let Some(ms) = p.get_path("hold_ms").and_then(Value::as_f64) {
            secs_words(ms / 1000.0)
        } else if held {
            "until stopped".to_string()
        } else {
            "once".to_string()
        };
        let mut parts: Vec<String> = screen.clone();
        if !lights.is_empty() {
            parts.push(if s(p, "lights").is_empty() { "lights".into() } else { format!("{lights} lights") });
        }
        // "Plays “Air horn”" → "plays “Air horn”" in the middle of the line
        let decap = |x: &String| {
            let mut c = x.chars();
            c.next().map(|f| f.to_lowercase().chain(c).collect::<String>()).unwrap_or_default()
        };
        parts.extend(sound.iter().map(decap));
        parts.extend(also.iter().map(decap));
        let what = if parts.is_empty() { "Nothing yet".to_string() } else { rules::capitalize(&parts.join(", ")) };
        About { screen, lights, sound, also, line: format!("{what} · {short}") }
    }
}

/// One row of the list.
struct Item {
    id: String,
    label: String,
    color: Option<Color32>,
    active: bool,
    remaining: Option<f64>,
    /// Stays on (for a while or until released), so it can be stopped.
    held: bool,
    knobs: Vec<Value>,
    /// Its file has a mistake (what's wrong); `listed` = the engine still knows a working
    /// version of it.
    broken: Option<String>,
    listed: bool,
}

fn items(app: &App) -> Vec<Item> {
    let mut v: Vec<Item> = app
        .m
        .q_list("presets")
        .iter()
        .filter(|p| !s(p, "name").is_empty())
        .map(|p| Item {
            id: s(p, "name").to_string(),
            label: nice(s(p, "label")),
            color: se_ui_kit::theme::hex(s(p, "color")),
            active: p.get_path("active").is_some_and(Value::truthy),
            remaining: p.get_path("remaining_ms").and_then(Value::as_f64).map(|ms| ms / 1000.0),
            held: p.get_path("held").is_some_and(Value::truthy),
            knobs: p.get_path("knobs").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default(),
            broken: None,
            listed: true,
        })
        .collect();
    for e in app.m.q_list("errors") {
        let Some(id) = s(e, "file").strip_prefix("presets/").and_then(|f| f.strip_suffix(".toml")) else { continue };
        let msg = s(e, "msg").to_string();
        match v.iter_mut().find(|i| i.id == id) {
            Some(i) => i.broken = Some(msg),
            None => v.push(Item {
                id: id.to_string(),
                label: nice(id),
                color: None,
                active: false,
                remaining: None,
                held: false,
                knobs: Vec::new(),
                broken: Some(msg),
                listed: false,
            }),
        }
    }
    v.sort_by_key(|i| i.label.to_lowercase());
    v
}

/// A config error in plain words: `knob “Speed”: `min` (12) …` → `The “Speed” knob: min (12) …`.
fn plain_error(msg: &str) -> String {
    let msg = msg.replace('`', "");
    match msg.strip_prefix("knob “").and_then(|r| r.split_once("”: ")) {
        Some((name, rest)) => format!("The “{name}” knob: {rest}"),
        None => rules::capitalize(&msg),
    }
}

// ---- state ----------------------------------------------------------------------------------------

/// A knob being turned: the value it shows until the engine reports it back.
#[derive(Clone)]
struct Edit {
    value: Value,
    at: f64,
    saved: bool,
}

#[derive(Clone, Default)]
struct State {
    selected: Option<String>,
    search: String,
    confirm_air: bool,
    cat: Catalog,
    catalog_at: f64,
    /// (saved action, knob target) → the value being set.
    edits: HashMap<(String, String), Edit>,
    /// The saved action whose knobs were last saved, and when.
    saved_at: Option<(String, f64)>,
    /// Keep a just-made selection until the engine lists it.
    hold_until: f64,
    /// What the detail pane edits: a new draft (`id: None`) or the selected one.
    draft: Option<Draft>,
    /// The draft as read from its file (Save / Discard show while `draft` differs).
    base: Option<Draft>,
    /// `project.read` reply the draft was read from, and whether the file was asked for.
    read_seq: u64,
    requested: bool,
}

impl State {
    fn dirty(&self) -> bool {
        match (&self.draft, &self.base) {
            (Some(d), Some(b)) => d != b,
            (Some(_), None) => true,
            _ => false,
        }
    }

    fn select(&mut self, id: Option<String>) {
        self.selected = id;
        self.confirm_air = false;
        self.draft = None;
        self.base = None;
        self.read_seq = 0;
        self.requested = false;
    }
}

/// Turn a knob in the engine: `save = false` only moves what's running (and what the next
/// firing uses); `save = true` also writes it into the saved action's file.
fn send_knob(app: &mut App, id: &str, target: &str, v: &Value, save: bool) {
    app.m.action("preset.knob", Value::map().with("name", id).with("target", target).with("value", v.clone()).with("save", save));
}

/// Save knobs that have rested, and forget edits once the engine shows their value.
fn settle(app: &mut App, ui: &egui::Ui, st: &mut State, list: &[Item], now: f64) {
    let mut due = Vec::new();
    for ((id, target), e) in &mut st.edits {
        if !e.saved && now - e.at >= SAVE_AFTER {
            e.saved = true;
            due.push((id.clone(), target.clone(), e.value.clone()));
        }
    }
    for (id, target, v) in due {
        send_knob(app, &id, &target, &v, true);
        st.saved_at = Some((id, now));
        app.m.refresh_soon();
    }
    if st.edits.values().any(|e| !e.saved) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
    let reported = |id: &str, target: &str| {
        list.iter().find(|i| i.id == id).and_then(|i| i.knobs.iter().find(|k| s(k, "target") == target)).and_then(|k| k.get_path("value")).cloned()
    };
    st.edits.retain(|(id, target), e| {
        let back = reported(id, target).is_some_and(|v| v.as_f64().zip(e.value.as_f64()).map_or(v == e.value, |(a, b)| (a - b).abs() < 1e-6));
        !e.saved || (!back && now - e.at < 10.0)
    });
}

/// Read the selected saved action's file into the draft (again whenever a newer reply comes
/// in and nothing is being edited).
fn load_draft(app: &mut App, st: &mut State, list: &[Item]) {
    let Some(id) = st.selected.clone() else { return };
    if !list.iter().any(|i| i.id == id && i.listed) {
        return;
    }
    let file = file_of(&id);
    let key = format!("project.read:{file}");
    if !st.requested {
        st.requested = true;
        app.m.query_as(&key, "project.read", Value::map().with("path", file));
    }
    let seq = app.m.q_seq(&key);
    if seq == 0 || seq == st.read_seq || st.dirty() && st.draft.is_some() {
        return;
    }
    let text = app.m.q(&key).and_then(|v| v.get_path("text")).and_then(Value::as_str).unwrap_or("");
    if let Ok(d) = Draft::from_toml(&id, text) {
        st.base = Some(d.clone());
        st.draft = Some(d);
    }
    st.read_seq = seq;
}

/// Open this page with `preset` selected (Lights' "Put on a pad").
pub fn open(app: &mut App, ctx: &egui::Context, preset: &str) {
    let now = ctx.input(|i| i.time);
    let mut st: State = ctx.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<State>(state_id())));
    st.select(Some(preset.to_string()));
    st.hold_until = now + 5.0;
    st.catalog_at = 0.0;
    ctx.data_mut(|d| d.insert_temp(state_id(), st));
    app.m.refresh_soon();
    app.open_view(ViewId::Actions);
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let now = ui.input(|i| i.time);
    let mut st: State = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<State>(state_id())));
    view(app, ui, &mut st, now);
    ui.data_mut(|d| d.insert_temp(state_id(), st));
}

fn view(app: &mut App, ui: &mut egui::Ui, st: &mut State, now: f64) {
    let t = app.t.clone();
    if app.m.connected && (!st.cat.loaded || now - st.catalog_at > CATALOG_EVERY) {
        st.catalog_at = now;
        app.m.query("presets.catalog", Value::Null);
    }
    let seq = app.m.q_seq("presets.catalog");
    if seq != st.cat.seq
        && let Some(v) = app.m.q("presets.catalog")
    {
        st.cat = Catalog::parse(v, seq);
    }
    let list = items(app);
    // keep the selection while a just-made one waits for the engine to list it; a new draft
    // has no selection
    let creating = st.draft.as_ref().is_some_and(|d| d.id.is_none());
    let keep = creating || now < st.hold_until || st.selected.as_ref().is_some_and(|s| list.iter().any(|i| i.id == *s));
    if !keep {
        st.select(list.first().map(|i| i.id.clone()));
    }
    settle(app, ui, st, &list, now);
    load_draft(app, st, &list);

    // the list pane gets its own copies; the detail pane edits the state
    let mut search = std::mem::take(&mut st.search);
    let lines: Vec<String> = list
        .iter()
        .map(|it| match (&it.broken, st.cat.about.get(&it.id)) {
            (Some(_), _) if !it.listed => "Its file has a mistake".to_string(),
            (_, Some(a)) => a.line.clone(),
            _ => String::new(),
        })
        .collect();
    let selected = st.selected.clone();
    let (pick, new) = widgets::split(
        ui,
        LIST_W,
        |ui| list_pane(ui, &t, &mut search, selected.as_deref(), &list, &lines),
        |ui| {
            egui::ScrollArea::vertical().id_salt("actions-detail").auto_shrink([false, false]).show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(980.0));
                detail(app, ui, &t, st, &list, now);
            });
        },
    )
    .0;
    st.search = search;
    if new {
        st.select(None);
        st.draft = Some(Draft::new());
    } else if let Some(id) = pick {
        st.select(Some(id));
    }
}

// ---- left: the list ------------------------------------------------------------------------------

/// Returns (a saved action picked, "new" clicked). `lines` = what each one does, in words.
fn list_pane(ui: &mut egui::Ui, t: &Theme, search: &mut String, selected: Option<&str>, list: &[Item], lines: &[String]) -> (Option<String>, bool) {
    let new = widgets::pane_header(ui, t, "Actions", Some(list.len()), Some("New saved action"));
    if list.len() > 8 {
        ui.add(widgets::field(search).hint_text(format!("{}  Search", icon::SEARCH)).desired_width(ui.available_width()));
        ui.add_space(spacing::S);
    }
    let q = search.trim().to_lowercase();
    let mut pick = None;
    egui::ScrollArea::vertical().id_salt("actions-list").auto_shrink([false, false]).show(ui, |ui| {
        let mut shown = 0;
        let broken = list.iter().any(|i| i.broken.is_some());
        for (group, fix) in [("Needs a fix", true), ("Saved actions", false)] {
            let mut labelled = !broken;
            for (it, line) in list.iter().zip(lines).filter(|(i, _)| i.broken.is_some() == fix) {
                if !q.is_empty() && !it.label.to_lowercase().contains(&q) && !line.to_lowercase().contains(&q) {
                    continue;
                }
                if !labelled {
                    widgets::group_label(ui, t, group);
                    labelled = true;
                }
                shown += 1;
                let (trailing, tc) = if it.active { ("Running".to_string(), t.green) } else { (String::new(), t.text_dim) };
                let sel = selected == Some(it.id.as_str());
                if action_row(ui, t, it.color, &it.label, line, &trailing, tc, sel).clicked() && !sel {
                    pick = Some(it.id.clone());
                }
            }
        }
        if list.is_empty() {
            widgets::hint(ui, t, "No saved actions yet.");
        } else if shown == 0 {
            widgets::hint(ui, t, "Nothing matches your search.");
        }
    });
    (pick, new)
}

/// Text cut to `max_w` with "…" (single line).
fn fit(ui: &egui::Ui, text: &str, fid: egui::FontId, color: Color32, max_w: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(fid, color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(10.0));
    ui.painter().layout_job(job)
}

/// A list row like the kit's, with the saved action's color in place of an icon.
#[allow(clippy::too_many_arguments)]
fn action_row(
    ui: &mut egui::Ui,
    t: &Theme,
    color: Option<Color32>,
    title: &str,
    subtitle: &str,
    trailing: &str,
    tc: Color32,
    selected: bool,
) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 54.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(radius::CONTROL), t.selection);
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(radius::CONTROL), t.surface_hi);
    }
    let sw = Rect::from_center_size(egui::pos2(rect.left() + 12.0 + 8.0, rect.center().y), Vec2::splat(16.0));
    swatch(ui, t, sw, color, 4);
    let x = rect.left() + 12.0 + 28.0;
    let mut right = rect.right() - 12.0;
    if !trailing.is_empty() {
        let g = ui.painter().layout_no_wrap(trailing.to_string(), font(type_scale::SMALL), tc);
        ui.painter().galley(egui::pos2(right - g.size().x, rect.center().y - g.size().y / 2.0), g.clone(), tc);
        right -= g.size().x + 10.0;
    }
    let tg = fit(ui, title, font_medium(type_scale::BODY), t.fg, right - x);
    let sg = fit(ui, subtitle, font(type_scale::SMALL), t.text_dim, right - x);
    let elided = tg.elided || sg.elided;
    ui.painter().galley(egui::pos2(x, rect.center().y - 9.0 - tg.size().y / 2.0), tg, t.fg);
    ui.painter().galley(egui::pos2(x, rect.center().y + 10.0 - sg.size().y / 2.0), sg, t.text_dim);
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if elided { resp.on_hover_text(format!("{title}\n{subtitle}")) } else { resp }
}

/// The saved action's color as a rounded square.
fn swatch(ui: &egui::Ui, t: &Theme, r: Rect, color: Option<Color32>, rounding: u8) {
    let p = ui.painter();
    match color {
        Some(c) => p.rect(r, CornerRadius::same(rounding), c, Stroke::new(1.0, t.border), StrokeKind::Inside),
        None => p.rect(r, CornerRadius::same(rounding), t.surface_hi, Stroke::new(1.0, t.border), StrokeKind::Inside),
    };
}

// ---- right: the detail ---------------------------------------------------------------------------

fn detail(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, list: &[Item], now: f64) {
    let creating = st.draft.as_ref().is_some_and(|d| d.id.is_none());
    let it = st.selected.clone().and_then(|id| list.iter().find(|i| i.id == id));
    if creating {
        header(app, ui, t, st, None, list, now);
        draft_sections(app, ui, t, st);
        return;
    }
    let Some(it) = it else {
        if st.selected.is_some() {
            widgets::empty_state(ui, t, icon::CLOCK, "Opening…", "", None);
        } else if widgets::empty_state(
            ui,
            t,
            icon::BOLT,
            "Saved actions",
            "A saved action is a named list of steps that any trigger, button or chat command can run.",
            Some("New saved action"),
        ) {
            st.select(None);
            st.draft = Some(Draft::new());
        }
        return;
    };
    header(app, ui, t, st, Some(it), list, now);
    if let Some(msg) = &it.broken {
        let body = if it.listed {
            format!("{} The last working version still runs. Fix the file, or restore an earlier version.", plain_error(msg))
        } else {
            format!("{} Fix the file, or restore an earlier version.", plain_error(msg))
        };
        if widgets::callout(ui, t, Tone::Warn, icon::WARN, "Its file has a mistake", &body, Some("Open History")) {
            app.open_view(ViewId::History);
        }
        ui.add_space(spacing::M);
    }
    if !it.listed {
        return;
    }
    if st.draft.is_some() {
        draft_sections(app, ui, t, st);
    } else {
        widgets::hint(ui, t, "Reading its file…");
    }
    let cat = std::mem::take(&mut st.cat);
    also_section(ui, t, &cat, &it.id);
    knobs_section(app, ui, t, st, it, now);
    runs_it_section(app, ui, t, &cat, &it.id, list);
    st.cat = cat;
    file_section(app, ui, t, st, &it.id, cat_uses(&st.cat, &it.id));
}

fn cat_uses(cat: &Catalog, id: &str) -> usize {
    cat.uses.get(id).map_or(0, Vec::len)
}

// ---- header: name, try it, save ------------------------------------------------------------------

fn header(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, it: Option<&Item>, list: &[Item], now: f64) {
    let on_air = crate::views::status::on_air(app);
    let dirty = st.dirty();
    let errs = st.draft.as_ref().map(Draft::errors).unwrap_or_default();
    let creating = it.is_none();
    let title = match (&st.draft, it) {
        (Some(d), _) if !d.label.trim().is_empty() => d.label.trim().to_string(),
        (_, Some(it)) => it.label.clone(),
        _ => "New saved action".to_string(),
    };
    let sub = match it {
        Some(it) => {
            let line = st.cat.about.get(&it.id).map(|a| a.line.clone()).unwrap_or_default();
            match it.remaining.filter(|r| *r > 0.0 && it.active) {
                Some(r) => format!("Running · {} left", secs_words(r.ceil())),
                None if it.active => "Running".into(),
                None => line,
            }
        }
        None => "Steps any trigger, button or chat command can run".into(),
    };
    let (mut save, mut discard, mut fire, mut stop) = (false, false, false, false);
    widgets::detail_header(ui, t, icon::BOLT, &title, &sub, |ui| {
        if dirty {
            let label = if creating { "Create" } else { "Save" };
            let tip = if errs.is_empty() { "Save it to its file" } else { "Finish the notes below first" };
            save = widgets::button_ex(ui, t, Some(icon::CHECK), label, Kind::Primary, Size::Medium, 0.0, errs.is_empty()).on_hover_text(tip).clicked();
            discard = widgets::button_ex(ui, t, None, if creating { "Cancel" } else { "Discard" }, Kind::Ghost, Size::Medium, 0.0, true).clicked();
        }
        if let Some(it) = it.filter(|i| i.listed) {
            if it.active && it.held {
                stop = widgets::button_ex(ui, t, Some(icon::STOP), "Stop", Kind::Danger, Size::Medium, 0.0, true).clicked();
            } else {
                let (lbl, kind) = if on_air { ("Try it on air", Kind::Live) } else { ("Try it", Kind::Secondary) };
                let tip = match (on_air, dirty) {
                    (_, true) => "Runs the saved version.",
                    (true, _) => "You're on air: your viewers will see it.",
                    _ => "You're off air: only you see it.",
                };
                fire = widgets::button_ex(ui, t, Some(icon::PLAY), lbl, kind, Size::Medium, 0.0, true).on_hover_text(tip).clicked();
            }
        }
    });
    if dirty && !errs.is_empty() {
        let (tone, head) = if creating { (Tone::Info, "To create it") } else { (Tone::Warn, "Almost there") };
        widgets::callout(ui, t, tone, icon::INFO, head, &errs.join("\n"), None);
        ui.add_space(spacing::M);
    }
    if let Some(it) = it {
        if stop {
            app.m.command(Op::PresetRelease { name: it.id.clone() });
        }
        if fire {
            if on_air {
                st.confirm_air = true;
            } else {
                try_it(app, st, &it.id, now);
            }
        }
        if st.confirm_air {
            ui.horizontal(|ui| {
                let w = ui.available_width() - 110.0;
                ui.allocate_ui_with_layout(Vec2::new(w, 0.0), Layout::top_down(Align::Min), |ui| {
                    if widgets::callout(
                        ui,
                        t,
                        Tone::Danger,
                        icon::LIVE,
                        "Your viewers will see this",
                        "You're on air, so it runs on stream right now.",
                        Some("Try it on air"),
                    ) {
                        st.confirm_air = false;
                        try_it(app, st, &it.id, now);
                    }
                });
                if widgets::button_ex(ui, t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                    st.confirm_air = false;
                }
            });
            ui.add_space(spacing::M);
        }
    }
    if discard {
        if creating {
            st.draft = None;
            st.select(list.first().map(|i| i.id.clone()));
        } else {
            st.draft = st.base.clone();
        }
    }
    if save && let Some(d) = st.draft.clone() {
        let taken: Vec<String> = list.iter().map(|i| i.id.clone()).collect();
        let (id, args) = d.write_args(&taken);
        app.m.action("project.write", args);
        let saved = Draft { id: Some(id.clone()), ..d };
        st.base = Some(saved.clone());
        st.draft = Some(saved);
        st.selected = Some(id.clone());
        st.hold_until = now + 5.0;
        // the file's new text arrives with the next read
        app.m.query_as(&format!("project.read:{}", file_of(&id)), "project.read", Value::map().with("path", file_of(&id)));
        app.m.toast(if creating { "Saved action created." } else { "Saved action saved." }, false);
        app.m.refresh_soon();
    }
}

fn try_it(app: &mut App, st: &mut State, id: &str, now: f64) {
    // knobs still resting go to the file now; the engine already runs with their values
    for ((eid, target), e) in &mut st.edits {
        if eid == id && !e.saved {
            e.saved = true;
            e.at = now;
            send_knob(app, eid, target, &e.value, true);
        }
    }
    app.m.command(Op::PresetFire { name: id.to_string(), payload: Value::Null });
    app.m.refresh_soon();
}

// ---- the draft's sections ------------------------------------------------------------------------

fn draft_sections(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State) {
    let Some(mut d) = st.draft.clone() else { return };
    let salt = format!("action-{}-", d.id.as_deref().unwrap_or("new"));
    widgets::inspector_section(
        ui,
        t,
        "action-name",
        "Name",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Name", |ui| {
                ui.add(widgets::field(&mut d.label).hint_text("Hype, BRB, Drum solo…").desired_width(280.0));
            });
            widgets::prop_row(ui, t, "Color", |ui| {
                let mut on = !d.color.is_empty();
                if widgets::toggle(ui, t, &mut on).on_hover_text("A color for its button and pad").changed() {
                    d.color = if on { hex_of(t.accent) } else { String::new() };
                }
                if let Some(mut c) = se_ui_kit::theme::hex(&d.color) {
                    if egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::Opaque).changed() {
                        d.color = hex_of(c);
                    }
                } else {
                    widgets::hint(ui, t, "No color");
                }
            });
        },
    );
    widgets::inspector_section(
        ui,
        t,
        "action-steps",
        "Steps",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, t, "Top to bottom, in order.");
            ui.add_space(spacing::S);
            rules::steps_editor(app, ui, &salt, &mut d.steps, "");
        },
    );
    widgets::inspector_section(
        ui,
        t,
        "action-length",
        "How long",
        true,
        |_| {},
        |ui| {
            let labels: Vec<&str> = Length::ALL.iter().map(|l| l.label()).collect();
            let mut i = Length::ALL.iter().position(|l| *l == d.length).unwrap_or(0);
            if widgets::segmented(ui, t, &mut i, &labels) {
                d.length = Length::ALL[i];
            }
            ui.add_space(spacing::XS);
            widgets::hint(ui, t, d.length.help());
            if d.length == Length::For {
                widgets::prop_row(ui, t, "For", |ui| {
                    ui.add(widgets::field(&mut d.hold).hint_text("8s").desired_width(90.0));
                    widgets::hint(ui, t, "like 8s or 1m");
                });
            }
        },
    );
    widgets::inspector_section(
        ui,
        t,
        "action-safety",
        "Safety",
        true,
        |_| {},
        |ui| {
            widgets::toggle_row(ui, t, "Ask before running on air", "Its button has to be held to run it, and chat can never run it.", &mut d.confirm);
            ui.add_space(spacing::S);
            ui.add_enabled_ui(!d.confirm, |ui| {
                let mut chat = d.chat && !d.confirm;
                if widgets::toggle_row(ui, t, "Viewers may run it from chat", "Only through a chat command or reward that runs it.", &mut chat).changed() {
                    d.chat = chat;
                }
            });
        },
    );
    if st.draft.as_ref() != Some(&d) {
        st.draft = Some(d);
    }
}

// ---- what else its file does ---------------------------------------------------------------------

/// Parts of a saved action made in its file (effects, lights, sound, …), which the steps
/// editor doesn't show.
fn also_section(ui: &mut egui::Ui, t: &Theme, cat: &Catalog, id: &str) {
    let Some(a) = cat.about.get(id) else { return };
    let also: Vec<&String> = a.also.iter().filter(|x| !x.starts_with("Runs ")).collect();
    if a.screen.is_empty() && a.lights.is_empty() && a.sound.is_empty() && also.is_empty() {
        return;
    }
    widgets::inspector_section(
        ui,
        t,
        "action-also",
        "Also from its file",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, t, "These run with its steps. Change them in its file.");
            ui.add_space(spacing::XS);
            if !a.screen.is_empty() {
                widgets::prop_row(ui, t, "On screen", |ui| ui.label(RichText::new(a.screen.join(", ")).color(t.fg)));
            }
            if !a.lights.is_empty() {
                widgets::prop_row(ui, t, "Lights", |ui| ui.label(RichText::new(&a.lights).color(t.fg)));
            }
            if !a.sound.is_empty() {
                widgets::prop_row(ui, t, "Sound", |ui| ui.label(RichText::new(a.sound.join(", ")).color(t.fg)));
            }
            if !also.is_empty() {
                let words: Vec<&str> = also.iter().map(|s| s.as_str()).collect();
                widgets::prop_row(ui, t, "Also", |ui| ui.label(RichText::new(words.join(", ")).color(t.fg)));
            }
        },
    );
}

// ---- controls (knobs) ----------------------------------------------------------------------------

fn knobs_section(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, it: &Item, now: f64) {
    if it.knobs.is_empty() {
        return;
    }
    let value_of = |st: &State, k: &Value| -> Value {
        st.edits.get(&(it.id.clone(), s(k, "target").to_string())).map(|e| e.value.clone()).unwrap_or_else(|| k.get_path("value").cloned().unwrap_or_default())
    };
    let resettable = it
        .knobs
        .iter()
        .any(|k| knobs::default_of(k).is_some_and(|d| d.as_f64().zip(value_of(st, k).as_f64()).map_or(*d != value_of(st, k), |(a, b)| (a - b).abs() > 1e-9)));
    let mut reset = false;
    let saving = st.edits.iter().any(|((id, _), e)| *id == it.id && !e.saved);
    let saved = st.saved_at.as_ref().is_some_and(|(id, s)| *id == it.id && now - s < 2.5);
    widgets::inspector_section(
        ui,
        t,
        "action-controls",
        "Controls",
        true,
        |ui| {
            reset = widgets::button_ex(ui, t, Some(icon::UNDO), "Reset", Kind::Ghost, Size::Small, 0.0, resettable)
                .on_hover_text("Put every control back where it started.")
                .clicked();
            if saving {
                widgets::badge(ui, t, "Saving…", t.text_dim);
            } else if saved {
                widgets::badge(ui, t, "Saved", t.green);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            }
        },
        |ui| {
            widgets::hint(
                ui,
                t,
                if it.active { "It's running: you see and hear changes right away." } else { "Changes save by themselves and apply every time it runs." },
            );
            ui.add_space(spacing::S);
            let gap = spacing::M;
            let w = ui.available_width();
            let cols = (((w + gap) / (360.0 + gap)).floor() as usize).clamp(1, it.knobs.len());
            let cw = ((w - gap * (cols as f32 - 1.0)) / cols as f32).floor();
            for row in it.knobs.chunks(cols) {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for k in row {
                        let target = s(k, "target").to_string();
                        let mut v = value_of(st, k);
                        let moved = ui
                            .allocate_ui_with_layout(Vec2::new(cw, 0.0), Layout::top_down(Align::Min), |ui| {
                                ui.set_width(cw);
                                egui::Frame::new()
                                    .fill(t.surface_hi)
                                    .corner_radius(CornerRadius::same(radius::CONTROL))
                                    .inner_margin(egui::Margin::symmetric(14, 12))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        knobs::control(ui, t, ("saved-action", &it.id, &target), k, &mut v)
                                    })
                                    .inner
                            })
                            .inner;
                        if moved != Moved::No {
                            let done = moved == Moved::Done;
                            send_knob(app, &it.id, &target, &v, done);
                            if done {
                                st.saved_at = Some((it.id.clone(), now));
                                app.m.refresh_soon();
                            }
                            st.edits.insert((it.id.clone(), target), Edit { value: v, at: now, saved: done });
                        }
                    }
                });
                ui.add_space(gap);
            }
        },
    );
    if reset {
        for k in &it.knobs {
            if let Some(d) = knobs::default_of(k) {
                let target = s(k, "target").to_string();
                send_knob(app, &it.id, &target, d, true);
                st.edits.insert((it.id.clone(), target), Edit { value: d.clone(), at: now, saved: true });
            }
        }
        st.saved_at = Some((it.id.clone(), now));
        app.m.refresh_soon();
    }
}

// ---- what runs it --------------------------------------------------------------------------------

fn runs_it_section(app: &mut App, ui: &mut egui::Ui, t: &Theme, cat: &Catalog, id: &str, list: &[Item]) {
    let uses = cat.uses.get(id).map(Vec::as_slice).unwrap_or(&[]);
    let (mut open, mut button) = (None, false);
    widgets::inspector_section(
        ui,
        t,
        "action-runs-it",
        "Triggered by",
        true,
        |ui| {
            button = widgets::button_ex(ui, t, Some(icon::CONTROLLER), "Put on a button", Kind::Secondary, Size::Small, 0.0, true)
                .on_hover_text("Buttons & pedals: give it a Stream Deck key, a pad or a pedal.")
                .clicked();
        },
        |ui| {
            if uses.is_empty() {
                widgets::hint(ui, t, "Nothing runs it yet: put it on a button, or use “Run a saved action” in a trigger or chat command.");
            }
            for u in uses {
                let (ic, words) = use_words(u, list);
                let view = build::view_for_kind(&u.kind);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(ic).color(t.text_dim));
                    ui.add(egui::Label::new(RichText::new(&words).color(t.fg)).truncate());
                    if let Some(v) = view
                        && widgets::icon_button(ui, t, icon::RIGHT, "Open").clicked()
                    {
                        open = Some(v);
                    }
                });
            }
        },
    );
    if let Some(v) = open.or(button.then_some(ViewId::Controllers)) {
        app.open_view(v);
    }
}

/// Plain words for one place that runs a saved action.
fn use_words(u: &Use, list: &[Item]) -> (&'static str, String) {
    let stem = u.file.rsplit('/').next().unwrap_or("").trim_end_matches(".toml");
    let file = if u.file_label.is_empty() { nice(stem) } else { u.file_label.clone() };
    let name = if u.name.is_empty() { file.clone() } else { u.name.clone() };
    match u.kind.as_str() {
        "rules" => (icon::BOLT, format!("Trigger “{name}”")),
        "commands" => (icon::BOT, format!("Chat command “{name}”")),
        "controllers" => (icon::CONTROLLER, if u.name.is_empty() { file } else { format!("{file}: {name}") }),
        "timelines" => (icon::TIMELINE, format!("Timeline “{file}”")),
        "rewards" => (icon::TWITCH, format!("Channel points reward “{name}”")),
        "presets" => (icon::BOLT, format!("Saved action “{}”", list.iter().find(|i| i.id == stem).map(|i| i.label.clone()).unwrap_or_else(|| nice(stem)))),
        "bindings" => (icon::WAVE, format!("Modulation “{}” (only while it runs)", nice(&name))),
        "alerts" => (icon::ALERT, format!("Notification “{name}”")),
        "scenes" => (icon::SCENE, format!("Scene “{name}”")),
        "layouts" => (icon::KEYBOARD, "A keyboard shortcut".into()),
        k => (icon::LINK, format!("{} “{name}”", nice(k))),
    }
}

// ---- file ----------------------------------------------------------------------------------------

fn file_section(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, id: &str, uses: usize) {
    let file = file_of(id);
    let mut delete = false;
    widgets::inspector_section(
        ui,
        t,
        "action-file",
        "File",
        false,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Saved in", |ui| {
                ui.label(RichText::new(&file).font(se_ui_kit::theme::font_mono(type_scale::SMALL + 0.5)).color(t.fg));
                if widgets::button_ex(ui, t, Some(icon::CONSOLE), "Open the file", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                    app.open_in_editor(&file, None);
                }
            });
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                delete = widgets::hold_button(ui, t, "Delete saved action", t.bright_red, 0.6);
                let note = if uses > 0 { "Press and hold to delete. What runs it will stop working." } else { "Press and hold to delete." };
                widgets::hint(ui, t, note);
            });
        },
    );
    if delete {
        app.m.action("project.write", Value::map().with("path", file).with("delete", true));
        app.m.toast("Saved action deleted.", false);
        st.select(None);
        app.m.refresh_soon();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What `project.write` does with `set`: keys set, `null` keys removed, the rest (and the
    /// comments) left as they were.
    fn write(src: &str, set: &Value) -> String {
        let mut doc: toml_edit::DocumentMut = src.parse().unwrap();
        for (k, v) in set.as_map().unwrap() {
            match se_proto::value::to_toml(v) {
                None => {
                    doc.remove(k);
                }
                Some(tv) => {
                    let mut nv: toml_edit::Value = tv.to_string().parse().unwrap();
                    match doc.get_mut(k) {
                        Some(toml_edit::Item::Value(old)) => {
                            *nv.decor_mut() = old.decor().clone();
                            *old = nv;
                        }
                        _ => {
                            doc.insert(k, toml_edit::Item::Value(nv));
                        }
                    }
                }
            }
        }
        doc.to_string()
    }

    fn engine_reads(text: &str) -> se_core::config::PresetDef {
        let t: toml::Table = toml::from_str(text).expect("valid TOML");
        let def: se_core::config::PresetDef = toml::Value::Table(t).try_into().expect("a valid saved action");
        for c in def.commands.iter() {
            Op::parse(c).expect("its steps parse");
        }
        def
    }

    #[test]
    fn a_new_saved_action_writes_a_file_the_engine_runs() {
        let d = Draft {
            label: "Hide cam".into(),
            color: "#ff8800".into(),
            steps: vec!["set scene.duo.node.cam.visible false".into(), "  ".into()],
            length: Length::Held,
            ..Draft::new()
        };
        assert!(d.errors().is_empty(), "{:?}", d.errors());
        let (id, args) = d.write_args(&["hide_cam".to_string()]);
        assert_eq!(id, "hide_cam_2", "never overwrites another saved action");
        assert_eq!(args.get_path("path").and_then(Value::as_str), Some("presets/hide_cam_2.toml"));
        let text = write("", args.get_path("set").unwrap());
        let def = engine_reads(&text);
        assert_eq!(def.label.as_deref(), Some("Hide cam"));
        assert_eq!(def.commands, ["set scene.duo.node.cam.visible false"]);
        assert!(def.until_released && !def.toggle && def.hold.is_none());
        assert_eq!(def.chat, Some(false), "viewers can't run a new one unless allowed");
        // and it reads back as the same draft
        assert_eq!(Draft::from_toml("hide_cam_2", &text).unwrap(), Draft { id: Some("hide_cam_2".into()), steps: def.commands.clone(), ..d });
    }

    #[test]
    fn editing_keeps_what_the_editor_does_not_own() {
        let src = "# made by hand\nlabel = \"Hype\"\nhold = \"8s\"\nfx = [{ name = \"rgb_split\" }]\nset = { \"fx.rgb_split.amount\" = 0.8 }\n[[knob]]\nlabel = \"Split\"\ntarget = \"fx.rgb_split.amount\"\nmin = 0\nmax = 1\n";
        let mut d = Draft::from_toml("hype", src).unwrap();
        assert_eq!((d.length, d.hold.as_str(), d.chat), (Length::For, "8s", true));
        d.length = Length::Toggle;
        d.confirm = true;
        d.steps.push("bot.say HYPE".into());
        assert!(d.errors().is_empty(), "{:?}", d.errors());
        let (id, args) = d.write_args(&[]);
        assert_eq!(id, "hype");
        let text = write(src, args.get_path("set").unwrap());
        assert!(text.starts_with("# made by hand"));
        let def = engine_reads(&text);
        assert!(def.toggle && def.confirm && def.hold.is_none(), "{text}");
        assert_eq!(def.fx.len(), 1);
        assert_eq!(def.set.len(), 1);
        assert_eq!(def.knobs.len(), 1);
        assert_eq!(def.chat, None, "asking first already keeps chat out");
        let back = Draft::from_toml("hype", &text).unwrap();
        assert_eq!((back.label, back.steps, back.length, back.confirm, back.chat), (d.label, d.steps, Length::Toggle, true, false));
    }

    #[test]
    fn a_draft_is_checked_before_saving() {
        let mut d = Draft::new();
        assert_eq!(d.errors().len(), 2, "a new one needs a name and a step");
        d.label = "Drop".into();
        d.steps = vec!["preset.fire".into()];
        d.length = Length::For;
        d.hold = "soon".into();
        assert_eq!(d.errors().len(), 2, "{:?}", d.errors());
        d.steps = vec!["scene.cut drop".into()];
        d.hold = "4s".into();
        assert!(d.errors().is_empty());
    }

    #[test]
    fn what_it_does_reads_as_words() {
        let cat = Catalog {
            effects: HashMap::from([("rgb_split".into(), "Color split".into()), ("vhs".into(), "VHS tape".into())]),
            overlays: HashMap::from([("confetti".into(), "Confetti".into())]),
            cuelists: HashMap::from([("chase_fast".into(), "Fast chase".into())]),
            sounds: HashMap::from([("airhorn".into(), "Air horn".into())]),
            ..Default::default()
        };
        let hype = Value::map()
            .with("effects", Value::List(vec!["rgb_split".into()]))
            .with("overlays", Value::List(vec!["confetti".into()]))
            .with("lights", "cuelist")
            .with("lights_name", "chase_fast")
            .with("sound", "airhorn")
            .with("hold_ms", 8000)
            .with("settings", 1)
            .with("set", Value::List(vec!["fx.rgb_split.amount".into()]));
        let a = cat.describe(&hype);
        assert_eq!(a.screen, ["Color split", "Confetti"], "an effect in both `fx` and `set` shows once");
        assert_eq!(a.lights, "Fast chase");
        assert_eq!(a.sound, ["Plays “Air horn”"]);
        assert!(a.line.ends_with(" · 8 seconds"), "{}", a.line);
        let duck = Value::map().with("toggle", true).with("settings", 1).with("set", Value::List(vec!["audio.duck.active".into()]));
        let a = cat.describe(&duck);
        assert_eq!(a.sound, ["Turns the music down"]);
        assert!(a.line.ends_with(" · until pressed again"), "{}", a.line);
        let stutter = Value::map().with("triggers", Value::List(vec!["audio.bus.music.fx.stutter".into()])).with("hold_ms", 2000);
        assert_eq!(cat.describe(&stutter).sound, ["Stutter on the music"]);
    }
}
