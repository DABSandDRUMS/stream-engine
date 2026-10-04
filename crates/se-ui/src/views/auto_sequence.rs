//! Automation → Auto sequence: presets that switch the program scene by themselves, on a timer,
//! through a list of scenes (`autoseq/<id>.toml`: `label`, `order` in_order | random, `dwell`,
//! optional `transition` / `ms`, and `[[step]]` rows with the same keys per scene). One preset
//! runs at a time; the Overview's Auto sequence card turns it on and off.
//!
//! Left: every auto sequence with its scenes, order and time in one line. Right: the selected one
//! (or a new draft) — Play / Stop, Create / Save / Discard while changed, then Name, How it plays,
//! Scenes (each with its own time and transition, move up / down, remove, add) and the file.
//!
//! Engine: queries `autoseq`, `scenes`, `transitions`, `errors`, `project.read`; actions
//! `project.write {path, text}` (the whole file, `AutoSeqDef::to_toml`) / `{path, delete}`,
//! `autoseq.play {name}`, `autoseq.stop`, `autoseq.select {name}`, `autoseq.next`; state
//! `show.autoseq.{active,preset,step,next,next_at}`.

use crate::app::{App, ViewId};
use crate::views::live::nice;
use crate::views::quick_edit;
use egui::{Align, CornerRadius, Layout, RichText, Vec2};
use se_core::autoseq::{AutoSeqDef, AutoSeqStep, Order};
use se_core::config::Dur;
use se_proto::Value;
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_mono, font_semibold, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, LedState, Size, Tone, icon};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Width of the list pane.
const LIST_W: f32 = 300.0;
/// Time on each scene of a new auto sequence (the engine's default too).
const DEFAULT_DWELL_MS: u64 = 30_000;
/// Longest time on one scene the editor offers, in seconds.
const MAX_DWELL_S: f64 = 3600.0;
/// Transition length a newly set one starts at.
const DEFAULT_MS: u64 = 700;
/// A switch flipped on the Overview wins over the (lagging) engine echo for this long.
const EDIT_HOLD: Duration = Duration::from_millis(1500);

fn state_id() -> egui::Id {
    egui::Id::new("auto-sequence-view")
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn file_of(id: &str) -> String {
    format!("autoseq/{id}.toml")
}

/// "45 s", "1.5 s", "2 min".
fn secs_words(ms: u64) -> String {
    if ms >= 60_000 && ms.is_multiple_of(60_000) {
        format!("{} min", ms / 60_000)
    } else if ms.is_multiple_of(1000) {
        format!("{} s", ms / 1000)
    } else {
        format!("{:.1} s", ms as f64 / 1000.0)
    }
}

/// Scenes, order, and their effective timing (not just the preset's default).
fn summary(steps: &[Value], random: bool, dwell_ms: u64) -> String {
    let scenes = steps.len();
    let n = if scenes == 1 { "1 scene".to_string() } else { format!("{scenes} scenes") };
    let time = |step: &Value| step.get_path("dwell_ms").and_then(Value::as_f64).map_or(dwell_ms, |ms| ms as u64);
    let first = steps.first().map_or(dwell_ms, time);
    let timing = if steps.iter().all(|step| time(step) == first) { format!("{} each", secs_words(first)) } else { "per-scene times".to_string() };
    format!("{n} · {} · {timing}", if random { "random" } else { "in order" })
}

/// "32s", "1:05".
fn countdown(secs: u64) -> String {
    if secs < 60 { format!("{secs}s") } else { format!("{}:{:02}", secs / 60, secs % 60) }
}

/// The engine's scenes as (id, display name).
fn scene_names(app: &App) -> Vec<(String, String)> {
    app.m
        .q_list("scenes")
        .iter()
        .filter_map(|v| {
            let n = v.get_path("name").and_then(Value::as_str).filter(|n| !n.is_empty())?;
            let l = v.get_path("label").and_then(Value::as_str).filter(|l| !l.is_empty()).unwrap_or(n);
            Some((n.to_string(), nice(l)))
        })
        .collect()
}

fn scene_label(scenes: &[(String, String)], id: &str) -> String {
    scenes.iter().find(|(n, _)| n == id).map(|(_, l)| l.clone()).unwrap_or_else(|| nice(id))
}

/// Display name of an `autoseq` query row (the label, else the id).
fn label_of(p: &Value) -> String {
    let l = s(p, "label");
    nice(if l.is_empty() { s(p, "name") } else { l })
}

/// The auto sequence the engine runs, if any.
fn running_id(app: &App) -> Option<String> {
    app.m.b("show.autoseq.active").then(|| app.m.str("show.autoseq.preset").to_string())
}

/// What the running auto sequence shows and what comes next: "Drum cams → Crowd in 32s"
/// (`None` while stopped).
fn now_line(app: &App, scenes: &[(String, String)]) -> Option<String> {
    if !app.m.b("show.autoseq.active") {
        return None;
    }
    let program = app.m.str("show.scene.program");
    let cur = scene_label(scenes, program);
    let next = app.m.str("show.autoseq.next");
    let at = app.m.get("show.autoseq.next_at").and_then(Value::as_f64).unwrap_or(0.0);
    if next.is_empty() || at <= 0.0 {
        return Some(cur);
    }
    if next == program {
        return Some(format!("{cur}, the only scene"));
    }
    let rem = ((at - se_clock::now() as f64) / 1e9).ceil();
    let next = scene_label(scenes, next);
    Some(if rem >= 1.0 { format!("{cur} → {next} in {}", countdown(rem as u64)) } else { format!("{cur} → {next}") })
}

/// Seconds editor for a time in ms; the new ms when changed.
fn secs_drag(ui: &mut egui::Ui, ms: u64, min: f64, max: f64, speed: f64) -> Option<u64> {
    let mut v = ms as f64 / 1000.0;
    ui.add(egui::DragValue::new(&mut v).range(min..=max).speed(speed).suffix(" s").max_decimals(2)).changed().then(|| (v * 1000.0).round() as u64)
}

// ---- the draft -----------------------------------------------------------------------------------

/// What the editor changes: the whole file (`AutoSeqDef`), and which file.
#[derive(Clone, Debug, PartialEq)]
struct Draft {
    /// File id (`autoseq/<id>.toml`); `None` until it's created.
    id: Option<String>,
    def: AutoSeqDef,
}

impl Draft {
    fn new() -> Draft {
        Draft {
            id: None,
            def: AutoSeqDef {
                name: String::new(),
                label: String::new(),
                order: Order::default(),
                avoid_repeat: 1,
                dwell: Dur(DEFAULT_DWELL_MS),
                transition: None,
                ms: None,
                steps: Vec::new(),
            },
        }
    }

    /// Read the file's text the way the engine does.
    fn from_toml(id: &str, text: &str) -> Result<Draft, String> {
        let t: toml::Table = text.parse().map_err(|e: toml::de::Error| e.message().to_string())?;
        AutoSeqDef::parse(id, &t).map(|def| Draft { id: Some(id.to_string()), def })
    }

    /// What has to change before it can be saved (empty = saveable).
    fn errors(&self) -> Vec<String> {
        let mut e = Vec::new();
        if self.def.label.trim().is_empty() {
            e.push("Give it a name.".to_string());
        }
        if self.def.steps.is_empty() {
            e.push("Add at least one scene.".into());
        }
        e
    }

    /// The draft as saved (a new one gets an id not among `taken`) and its `project.write` args.
    fn write_args(&self, taken: &[String]) -> (Draft, Value) {
        let id = self.id.clone().unwrap_or_else(|| quick_edit::free_id(&self.def.label, taken));
        let def = AutoSeqDef { name: id.clone(), label: self.def.label.trim().to_string(), ..self.def.clone() };
        let args = Value::map().with("path", file_of(&id)).with("text", def.to_toml());
        (Draft { id: Some(id), def }, args)
    }
}

/// One edit made in the editor.
#[derive(Clone, Debug, PartialEq)]
enum Change {
    /// Default time for scenes without their own time (ms).
    Dwell(u64),
    /// Explicit time on one scene (ms), independent of the default.
    StepDwell(usize, u64),
    /// Let a scene follow the default time again.
    ResetStepDwell(usize),
    /// Transition into one scene; `None` (like the rest) also drops its own length.
    StepTransition(usize, Option<String>),
    Up(usize),
    Down(usize),
    Remove(usize),
    /// A scene at the end (not twice).
    Add(String),
    /// Each of these scenes not in it yet, in this order.
    AddAll(Vec<String>),
}

fn add_scene(def: &mut AutoSeqDef, scene: String) {
    if !scene.is_empty() && !def.steps.iter().any(|st| st.scene == scene) {
        def.steps.push(AutoSeqStep { scene, dwell: None, transition: None, ms: None });
    }
}

fn apply(def: &mut AutoSeqDef, c: Change) {
    match c {
        Change::Dwell(ms) => {
            def.dwell = Dur(ms);
        }
        Change::StepDwell(i, ms) => {
            if let Some(st) = def.steps.get_mut(i) {
                st.dwell = Some(Dur(ms));
            }
        }
        Change::ResetStepDwell(i) => {
            if let Some(st) = def.steps.get_mut(i) {
                st.dwell = None;
            }
        }
        Change::StepTransition(i, tr) => {
            if let Some(st) = def.steps.get_mut(i) {
                if tr.is_none() {
                    st.ms = None;
                }
                st.transition = tr;
            }
        }
        Change::Up(i) => {
            if i > 0 && i < def.steps.len() {
                def.steps.swap(i - 1, i);
            }
        }
        Change::Down(i) => {
            if i + 1 < def.steps.len() {
                def.steps.swap(i, i + 1);
            }
        }
        Change::Remove(i) => {
            if i < def.steps.len() {
                def.steps.remove(i);
            }
        }
        Change::Add(scene) => add_scene(def, scene),
        Change::AddAll(scenes) => {
            for scene in scenes {
                add_scene(def, scene);
            }
        }
    }
}

// ---- engine data ----------------------------------------------------------------------------------

/// One row of the list.
struct Item {
    id: String,
    label: String,
    /// Scenes, order and time in one line.
    line: String,
    /// What the engine says is wrong with its file; `listed` = the engine loaded it.
    broken: Option<String>,
    listed: bool,
}

fn items(app: &App) -> Vec<Item> {
    let mut v: Vec<Item> = app
        .m
        .q_list("autoseq")
        .iter()
        .filter(|p| !s(p, "name").is_empty())
        .map(|p| Item {
            id: s(p, "name").to_string(),
            label: label_of(p),
            line: summary(
                p.get_path("steps").and_then(Value::as_list).unwrap_or(&[]),
                s(p, "order") == "random",
                p.get_path("dwell_ms").and_then(Value::as_f64).map_or(DEFAULT_DWELL_MS, |ms| ms as u64),
            ),
            broken: None,
            listed: true,
        })
        .collect();
    for e in app.m.q_list("errors") {
        let Some(id) = s(e, "file").strip_prefix("autoseq/").and_then(|f| f.strip_suffix(".toml")) else { continue };
        let msg = s(e, "msg").to_string();
        match v.iter_mut().find(|i| i.id == id) {
            Some(i) => i.broken = Some(i.broken.take().map_or_else(|| msg.clone(), |b| format!("{b}\n{msg}"))),
            None => v.push(Item { id: id.to_string(), label: nice(id), line: String::new(), broken: Some(msg), listed: false }),
        }
    }
    v.sort_by_key(|i| i.label.to_lowercase());
    v
}

// ---- state ----------------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct State {
    selected: Option<String>,
    /// Keep a just-made selection until the engine lists it.
    hold_until: f64,
    /// What the detail pane edits: a new draft (`id: None`) or the selected one.
    draft: Option<Draft>,
    /// The draft as read from its file (Save / Discard show while `draft` differs).
    base: Option<Draft>,
    /// Why its file couldn't be read into the editor.
    load_err: Option<String>,
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
        self.draft = None;
        self.base = None;
        self.load_err = None;
        self.read_seq = 0;
        self.requested = false;
    }
}

/// Read the selected auto sequence's file into the draft (again whenever a newer reply comes in
/// and nothing is being edited).
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
    match Draft::from_toml(&id, text) {
        Ok(d) => {
            st.base = Some(d.clone());
            st.draft = Some(d);
            st.load_err = None;
        }
        Err(e) => st.load_err = Some(e),
    }
    st.read_seq = seq;
}

/// Open this tab: `Some(id)` with that auto sequence selected, `None` with a new one to make.
pub fn open(app: &mut App, ctx: &egui::Context, id: Option<&str>) {
    let now = ctx.input(|i| i.time);
    let mut st: State = ctx.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<State>(state_id())));
    match id {
        Some(id) if st.selected.as_deref() != Some(id) => {
            st.select(Some(id.to_string()));
            st.hold_until = now + 5.0;
        }
        Some(_) => {}
        None if st.draft.as_ref().is_some_and(|d| d.id.is_none()) => {}
        None => {
            st.select(None);
            st.draft = Some(Draft::new());
        }
    }
    ctx.data_mut(|d| d.insert_temp(state_id(), st));
    app.m.refresh_soon();
    app.open_view(ViewId::AutoSequence);
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let now = ui.input(|i| i.time);
    let mut st: State = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<State>(state_id())));
    view(app, ui, &mut st, now);
    ui.data_mut(|d| d.insert_temp(state_id(), st));
}

fn view(app: &mut App, ui: &mut egui::Ui, st: &mut State, now: f64) {
    let t = app.t.clone();
    let list = items(app);
    // keep the selection while a just-made one waits for the engine to list it; a new draft has
    // no selection
    let creating = st.draft.as_ref().is_some_and(|d| d.id.is_none());
    let keep = creating || now < st.hold_until || st.selected.as_ref().is_some_and(|s| list.iter().any(|i| i.id == *s));
    if !keep {
        st.select(list.first().map(|i| i.id.clone()));
    }
    load_draft(app, st, &list);
    let selected = st.selected.clone();
    let running = running_id(app);
    let (pick, new) = widgets::split(
        ui,
        LIST_W,
        |ui| list_pane(ui, &t, selected.as_deref(), &list, running.as_deref()),
        |ui| {
            egui::ScrollArea::vertical().id_salt("autoseq-detail").auto_shrink([false, false]).show(ui, |ui| {
                ui.set_max_width(ui.available_width().min(980.0));
                detail(app, ui, &t, st, &list, now);
            });
        },
    )
    .0;
    if new {
        st.select(None);
        st.draft = Some(Draft::new());
    } else if let Some(id) = pick {
        st.select(Some(id));
    }
}

// ---- left: the list ------------------------------------------------------------------------------

/// Returns (an auto sequence picked, "new" clicked).
fn list_pane(ui: &mut egui::Ui, t: &Theme, selected: Option<&str>, list: &[Item], running: Option<&str>) -> (Option<String>, bool) {
    let new = widgets::pane_header(ui, t, "Auto sequences", Some(list.len()), Some("New auto sequence"));
    let mut pick = None;
    egui::ScrollArea::vertical().id_salt("autoseq-list").auto_shrink([false, false]).show(ui, |ui| {
        for it in list {
            let sel = selected == Some(it.id.as_str());
            let (ic, sub) = match &it.broken {
                Some(_) if !it.listed => (icon::WARN, "Its file has a mistake"),
                Some(_) => (icon::WARN, it.line.as_str()),
                None => (icon::SHUFFLE, it.line.as_str()),
            };
            let trailing = if running == Some(it.id.as_str()) { "Running" } else { "" };
            if widgets::list_row(ui, t, ic, &it.label, sub, trailing, sel).clicked() && !sel {
                pick = Some(it.id.clone());
            }
        }
        if list.is_empty() {
            widgets::hint(ui, t, "No auto sequences yet.");
        }
    });
    (pick, new)
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
            icon::SHUFFLE,
            "Auto sequence",
            "Pick a few scenes and the show switches between them by itself, in order or at random, on a timer you set. Turn it on and off on the Overview.",
            Some("New auto sequence"),
        ) {
            st.select(None);
            st.draft = Some(Draft::new());
        }
        return;
    };
    header(app, ui, t, st, Some(it), list, now);
    if let Some(msg) = &it.broken {
        if it.listed {
            widgets::callout(ui, t, Tone::Warn, icon::WARN, "Needs a look", msg, None);
        } else if widgets::callout(
            ui,
            t,
            Tone::Warn,
            icon::WARN,
            "Its file has a mistake",
            &format!("{msg}\nFix the file, or restore an earlier version."),
            Some("Open History"),
        ) {
            app.open_view(ViewId::History);
        }
        ui.add_space(spacing::M);
    }
    if it.listed {
        if let Some(e) = &st.load_err {
            widgets::callout(ui, t, Tone::Warn, icon::WARN, "Its file can't be read", e, None);
            ui.add_space(spacing::M);
        } else if st.draft.is_some() {
            draft_sections(app, ui, t, st);
        } else {
            widgets::hint(ui, t, "Reading its file…");
        }
    }
    file_section(app, ui, t, st, &it.id);
}

// ---- header: name, play, save --------------------------------------------------------------------

fn header(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, it: Option<&Item>, list: &[Item], now: f64) {
    let dirty = st.dirty();
    let errs = st.draft.as_ref().map(Draft::errors).unwrap_or_default();
    let creating = it.is_none();
    let running = it.is_some_and(|it| running_id(app).as_deref() == Some(it.id.as_str()));
    let title = match (&st.draft, it) {
        (Some(d), _) if !d.def.label.trim().is_empty() => d.def.label.trim().to_string(),
        (_, Some(it)) => it.label.clone(),
        _ => "New auto sequence".to_string(),
    };
    let sub = match it {
        Some(_) if running => {
            ui.ctx().request_repaint_after(Duration::from_millis(250));
            match now_line(app, &scene_names(app)).filter(|l| !l.is_empty()) {
                Some(l) => format!("Running · {l}"),
                None => "Running".into(),
            }
        }
        Some(it) => it.line.clone(),
        None => "Scenes that change by themselves, on a timer".into(),
    };
    let (mut save, mut discard, mut play, mut stop) = (false, false, false, false);
    widgets::detail_header(ui, t, icon::SHUFFLE, &title, &sub, |ui| {
        if dirty {
            let label = if creating { "Create" } else { "Save" };
            let tip = if errs.is_empty() { "Save it to its file" } else { "Finish the notes below first" };
            save = widgets::button_ex(ui, t, Some(icon::CHECK), label, Kind::Primary, Size::Medium, 0.0, errs.is_empty()).on_hover_text(tip).clicked();
            discard = widgets::button_ex(ui, t, None, if creating { "Cancel" } else { "Discard" }, Kind::Ghost, Size::Medium, 0.0, true).clicked();
        }
        if it.is_some_and(|i| i.listed) {
            if running {
                stop = widgets::button_ex(ui, t, Some(icon::STOP), "Stop", Kind::Danger, Size::Medium, 0.0, true)
                    .on_hover_text("Stop switching scenes. What's on air stays on air.")
                    .clicked();
            } else {
                let tip = if dirty { "Plays the saved version." } else { "Start switching between these scenes now." };
                play = widgets::button_ex(ui, t, Some(icon::PLAY), "Play", Kind::Secondary, Size::Medium, 0.0, true).on_hover_text(tip).clicked();
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
            app.m.action("autoseq.stop", Value::map());
        }
        if play {
            app.m.action("autoseq.play", Value::map().with("name", it.id.as_str()));
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
        let (saved, args) = d.write_args(&taken);
        let id = saved.id.clone().unwrap_or_default();
        app.m.action("project.write", args);
        st.base = Some(saved.clone());
        st.draft = Some(saved);
        st.selected = Some(id.clone());
        st.load_err = None;
        st.hold_until = now + 5.0;
        // We already have the authored definition. An immediate file read can race the
        // asynchronous write and replace it with old times; reselecting reads the file.
        st.read_seq = app.m.q_seq(&format!("project.read:{}", file_of(&id)));
        st.requested = true;
        app.m.toast(if creating { "Auto sequence created." } else { "Auto sequence saved." }, false);
        app.m.refresh_soon();
    }
}

// ---- the draft's sections ------------------------------------------------------------------------

/// Where the running auto sequence is: the step on air and the scene that comes next.
struct Live {
    step: Option<usize>,
    next: String,
}

fn draft_sections(app: &App, ui: &mut egui::Ui, t: &Theme, st: &mut State) {
    let Some(mut d) = st.draft.clone() else { return };
    let salt = format!("autoseq-{}", d.id.as_deref().unwrap_or("new"));
    let scenes = scene_names(app);
    let transitions: Vec<String> = app.m.q_list("transitions").iter().filter_map(Value::as_str).map(String::from).collect();
    let live = d.id.as_ref().filter(|id| running_id(app).as_ref() == Some(*id)).map(|_| Live {
        step: app.m.get("show.autoseq.step").and_then(Value::as_i64).and_then(|i| usize::try_from(i).ok()),
        next: app.m.str("show.autoseq.next").to_string(),
    });
    let mut changes: Vec<Change> = Vec::new();
    widgets::inspector_section(
        ui,
        t,
        "autoseq-name",
        "Name",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Name", |ui| {
                ui.add(widgets::field(&mut d.def.label).hint_text("Drum cycle, Chill cams…").desired_width(280.0));
            });
        },
    );
    widgets::inspector_section(
        ui,
        t,
        "autoseq-plays",
        "How it plays",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Order", |ui| {
                let mut i = usize::from(d.def.order == Order::Random);
                if widgets::segmented(ui, t, &mut i, &["In order", "Random"]) {
                    d.def.order = if i == 1 { Order::Random } else { Order::InOrder };
                }
            });
            let help = match d.def.order {
                Order::InOrder => "Top to bottom, then from the top again.",
                Order::Random => "Any other scene each time, never the same one twice in a row.",
            };
            widgets::hint(ui, t, help);
            ui.add_space(spacing::XS);
            widgets::prop_row(ui, t, "Default time", |ui| {
                if let Some(ms) = secs_drag(ui, d.def.dwell.ms(), 1.0, MAX_DWELL_S, 0.5) {
                    changes.push(Change::Dwell(ms));
                }
                widgets::hint(ui, t, "used unless a scene has its own time below");
            });
            widgets::prop_row(ui, t, "Transition", |ui| {
                let text = d.def.transition.as_deref().map_or_else(|| "Automatic".to_string(), nice);
                egui::ComboBox::from_id_salt((salt.as_str(), "transition")).width(220.0).selected_text(text).show_ui(ui, |ui| {
                    if ui.selectable_label(d.def.transition.is_none(), "Automatic").on_hover_text("Each scene's own transition settings").clicked() {
                        d.def.transition = None;
                    }
                    for n in &transitions {
                        if ui.selectable_label(d.def.transition.as_deref() == Some(n.as_str()), nice(n)).clicked() {
                            d.def.transition = Some(n.clone());
                        }
                    }
                });
            });
            widgets::prop_row(ui, t, "Switch length", |ui| {
                let mut on = d.def.ms.is_some();
                if widgets::toggle(ui, t, &mut on).on_hover_text("Set how long each switch takes").changed() {
                    d.def.ms = on.then_some(Dur(DEFAULT_MS));
                }
                match d.def.ms {
                    Some(ms) => {
                        if let Some(v) = secs_drag(ui, ms.ms(), 0.0, 10.0, 0.05) {
                            d.def.ms = Some(Dur(v));
                        }
                    }
                    None => {
                        widgets::hint(ui, t, "Automatic");
                    }
                }
            });
        },
    );
    widgets::inspector_section(
        ui,
        t,
        "autoseq-scenes",
        "Scenes",
        true,
        |_| {},
        |ui| steps_list(ui, t, &salt, &d.def, &scenes, &transitions, live.as_ref(), &mut changes),
    );
    for c in changes {
        apply(&mut d.def, c);
    }
    if st.draft.as_ref() != Some(&d) {
        st.draft = Some(d);
    }
}

/// The scenes it switches between, each with its own time and transition, and the add row.
#[allow(clippy::too_many_arguments)]
fn steps_list(
    ui: &mut egui::Ui,
    t: &Theme,
    salt: &str,
    def: &AutoSeqDef,
    scenes: &[(String, String)],
    transitions: &[String],
    live: Option<&Live>,
    changes: &mut Vec<Change>,
) {
    let n = def.steps.len();
    widgets::hint(ui, t, "Set how long each scene stays on air. Individual times are saved with the scene.");
    ui.add_space(spacing::XS);
    for (i, step) in def.steps.iter().enumerate() {
        ui.push_id((salt, &step.scene), |ui| {
            egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(radius::CONTROL)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{}", i + 1)).font(font_semibold(type_scale::BODY)).color(t.text_dim));
                    let known = scenes.iter().any(|(s, _)| *s == step.scene);
                    let label_width = (ui.available_width() - 90.0).max(40.0);
                    ui.allocate_ui_with_layout(Vec2::new(label_width, 24.0), Layout::left_to_right(Align::Center), |ui| {
                        if let Some(l) = live {
                            if l.step == Some(i) {
                                widgets::led(ui, t, LedState::Active).on_hover_text("On air now");
                            } else if l.next == step.scene {
                                widgets::led(ui, t, LedState::Armed).on_hover_text("Comes next");
                            }
                        }
                        if !known {
                            ui.label(RichText::new(icon::WARN).color(t.yellow)).on_hover_text("This scene doesn't exist any more, so it's skipped.");
                        }
                        let label = scene_label(scenes, &step.scene);
                        ui.add(egui::Label::new(RichText::new(label).font(font_semibold(type_scale::BODY)).color(t.fg)).truncate());
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::icon_button(ui, t, icon::CROSS, "Remove this scene").clicked() {
                            changes.push(Change::Remove(i));
                        }
                        if i + 1 < n && widgets::icon_button(ui, t, icon::DOWN, "Move down").clicked() {
                            changes.push(Change::Down(i));
                        }
                        if i > 0 && widgets::icon_button(ui, t, icon::UP, "Move up").clicked() {
                            changes.push(Change::Up(i));
                        }
                    });
                });
                widgets::prop_row(ui, t, "Time on scene", |ui| {
                    let ms = step.dwell.unwrap_or(def.dwell).ms();
                    if let Some(v) = secs_drag(ui, ms, 1.0, MAX_DWELL_S, 0.5) {
                        changes.push(Change::StepDwell(i, v));
                    }
                    if step.dwell.is_some() {
                        if widgets::button_ex(ui, t, Some(icon::UNDO), "Use default", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                            changes.push(Change::ResetStepDwell(i));
                        }
                    } else {
                        widgets::hint(ui, t, "uses default");
                    }
                });
                widgets::prop_row(ui, t, "Transition", |ui| {
                    let text = step.transition.as_deref().map_or_else(|| "Like the rest".to_string(), nice);
                    egui::ComboBox::from_id_salt("step-transition").width(150.0).selected_text(text).show_ui(ui, |ui| {
                        if ui.selectable_label(step.transition.is_none(), "Like the rest").clicked() && step.transition.is_some() {
                            changes.push(Change::StepTransition(i, None));
                        }
                        for tr in transitions {
                            let sel = step.transition.as_deref() == Some(tr.as_str());
                            if ui.selectable_label(sel, nice(tr)).clicked() && !sel {
                                changes.push(Change::StepTransition(i, Some(tr.clone())));
                            }
                        }
                    });
                    if let Some(ms) = step.ms {
                        widgets::hint(ui, t, &secs_words(ms.ms())).on_hover_text("How long the switch into this scene takes (set in its file)");
                    }
                });
            });
        });
        ui.add_space(spacing::XS);
    }
    if n == 0 {
        widgets::hint(ui, t, "Nothing yet. Add the scenes it should switch between.");
    }
    ui.add_space(spacing::XS);
    let addable: Vec<&(String, String)> = scenes.iter().filter(|(s, _)| !def.steps.iter().any(|st| st.scene == *s)).collect();
    ui.horizontal(|ui| {
        if addable.is_empty() {
            widgets::hint(ui, t, if scenes.is_empty() { "Make a scene first." } else { "Every scene is in it." });
            return;
        }
        egui::ComboBox::from_id_salt((salt, "add-scene"))
            .width(240.0)
            .selected_text(RichText::new(format!("{}  Add a scene", icon::PLUS)).color(t.fg))
            .show_ui(ui, |ui| {
                for (name, label) in &addable {
                    if ui.selectable_label(false, label).clicked() {
                        changes.push(Change::Add(name.clone()));
                    }
                }
            });
        if addable.len() > 1 && widgets::button_ex(ui, t, Some(icon::PLUS), "Add all scenes", Kind::Ghost, Size::Small, 0.0, true).clicked() {
            changes.push(Change::AddAll(addable.iter().map(|(name, _)| name.clone()).collect()));
        }
    });
}

// ---- file ----------------------------------------------------------------------------------------

fn file_section(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, id: &str) {
    let file = file_of(id);
    let mut delete = false;
    widgets::inspector_section(
        ui,
        t,
        "autoseq-file",
        "File",
        false,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Saved in", |ui| {
                ui.label(RichText::new(&file).font(font_mono(type_scale::SMALL + 0.5)).color(t.fg));
                if widgets::button_ex(ui, t, Some(icon::CONSOLE), "Open the file", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                    app.open_in_editor(&file, None);
                }
            });
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                delete = widgets::hold_button(ui, t, "Delete auto sequence", t.bright_red, 0.6);
                widgets::hint(ui, t, "Press and hold to delete. If it's running, it stops.");
            });
        },
    );
    if delete {
        app.m.action("project.write", Value::map().with("path", file).with("delete", true));
        app.m.toast("Auto sequence deleted.", false);
        st.select(None);
        app.m.refresh_soon();
    }
}

// ---- Overview card -------------------------------------------------------------------------------

/// The Overview card's switch and pick, shown until the engine reports them back.
#[derive(Clone, Default)]
struct Card {
    edits: HashMap<&'static str, (Value, Instant)>,
}

/// Compact auto sequence control for the Overview, one row: on / off, which one, what's on and
/// what comes next with a countdown, Skip and Edit. Without any it's a one-line invitation.
pub fn overview_strip(app: &mut App, ui: &mut egui::Ui) {
    let id = ui.make_persistent_id("autoseq_overview");
    let now = Instant::now();
    let mut card: Card = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<Card>(id)));
    card.edits.retain(|_, (_, at)| now.saturating_duration_since(*at) < EDIT_HOLD);
    let t = app.t.clone();
    let scenes = scene_names(app);
    // (id, name, summary)
    let presets: Vec<(String, String, String)> = items(app).into_iter().filter(|i| i.listed).map(|i| (i.id, i.label, i.line)).collect();
    let engine_on = app.m.b("show.autoseq.active");
    let active = card.edits.get("active").map_or(engine_on, |(v, _)| v.truthy());
    let picked = card.edits.get("preset").and_then(|(v, _)| v.as_str()).unwrap_or(app.m.str("show.autoseq.preset")).to_string();
    let chosen = presets.iter().find(|(n, ..)| *n == picked).or(presets.first()).cloned();
    let status = match &chosen {
        _ if active && !engine_on => Some("Starting…".to_string()),
        _ if active => now_line(app, &scenes),
        Some((_, _, line)) => Some(line.clone()),
        None => None,
    };
    if engine_on {
        ui.ctx().request_repaint_after(Duration::from_millis(250));
    }
    let mut out: Vec<(&str, Value)> = Vec::new();
    let mut open_edit: Option<Option<String>> = None;
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = spacing::S;
            ui.label(RichText::new(icon::SHUFFLE).size(type_scale::BODY + 1.0).color(t.text_dim));
            ui.label(RichText::new("Auto sequence").font(font_semibold(type_scale::BODY)).color(t.fg));
            ui.add_space(spacing::S);
            let Some((cid, clabel, _)) = &chosen else {
                widgets::hint(ui, &t, "Let the show switch between your scenes by itself, in order or at random.");
                if widgets::button_ex(ui, &t, Some(icon::PLUS), "Make one", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    open_edit = Some(None);
                }
                return;
            };
            let mut on = active;
            let tip = if active { "Stop switching scenes. What's on air stays on air." } else { "Start switching scenes by themselves" };
            if widgets::toggle(ui, &t, &mut on).on_hover_text(tip).changed() {
                card.edits.insert("active", (Value::Bool(on), now));
                if on {
                    out.push(("autoseq.play", Value::map().with("name", cid.as_str())));
                } else {
                    out.push(("autoseq.stop", Value::map()));
                }
            }
            egui::ComboBox::from_id_salt("autoseq-overview-preset").width(180.0).selected_text(clabel.as_str()).show_ui(ui, |ui| {
                for (n, l, _) in &presets {
                    if ui.selectable_label(n == cid, l).clicked() && n != cid {
                        card.edits.insert("preset", (Value::Str(n.clone()), now));
                        out.push(("autoseq.select", Value::map().with("name", n.as_str())));
                    }
                }
            });
            if let Some(text) = &status {
                let w = (ui.available_width() - 170.0).max(0.0);
                ui.allocate_ui_with_layout(Vec2::new(w, 24.0), Layout::left_to_right(Align::Center), |ui| {
                    let c = if active { t.fg } else { t.text_dim };
                    ui.add(egui::Label::new(RichText::new(text).color(c)).truncate());
                });
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::button_ex(ui, &t, Some(icon::EDIT), "Edit", Kind::Ghost, Size::Small, 0.0, true).on_hover_text("Open it under Automation").clicked()
                {
                    open_edit = Some(Some(cid.clone()));
                }
                if active
                    && widgets::button_ex(ui, &t, Some(icon::RIGHT), "Skip", Kind::Secondary, Size::Small, 0.0, engine_on)
                        .on_hover_text("Switch to the next scene now")
                        .clicked()
                {
                    out.push(("autoseq.next", Value::map()));
                }
            });
        });
    });
    ui.data_mut(|d| d.insert_temp(id, card));
    for (name, args) in out {
        app.m.action(name, args);
    }
    if let Some(sel) = open_edit {
        let ctx = ui.ctx().clone();
        open(app, &ctx, sel.as_deref());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scenes(d: &AutoSeqDef) -> Vec<&str> {
        d.steps.iter().map(|st| st.scene.as_str()).collect()
    }

    /// What the engine loads from the `project.write` args the editor sends.
    fn engine_reads(id: &str, args: &Value) -> AutoSeqDef {
        assert_eq!(args.get_path("path").and_then(Value::as_str), Some(file_of(id).as_str()));
        let text = args.get_path("text").and_then(Value::as_str).expect("the whole file");
        let t: toml::Table = text.parse().expect("valid TOML");
        AutoSeqDef::parse(id, &t).expect("a valid auto sequence")
    }

    #[test]
    fn scene_times_stay_independent_until_explicitly_reset() {
        let mut d = Draft::new().def;
        apply(&mut d, Change::AddAll(vec!["cams".into(), "crowd".into(), "stage".into()]));
        apply(&mut d, Change::StepDwell(0, DEFAULT_DWELL_MS));
        apply(&mut d, Change::StepDwell(1, 60_000));
        apply(&mut d, Change::Dwell(60_000));
        apply(&mut d, Change::Dwell(90_000));
        assert_eq!((d.step_dwell(0), d.step_dwell(1), d.step_dwell(2)), (Dur(30_000), Dur(60_000), Dur(90_000)));
        apply(&mut d, Change::Down(0));
        assert_eq!((d.steps[0].scene.as_str(), d.step_dwell(0)), ("crowd", Dur(60_000)));
        assert_eq!((d.steps[1].scene.as_str(), d.step_dwell(1)), ("cams", Dur(30_000)));
        apply(&mut d, Change::ResetStepDwell(1));
        apply(&mut d, Change::Dwell(45_000));
        assert_eq!((d.step_dwell(0), d.step_dwell(1), d.step_dwell(2)), (Dur(60_000), Dur(45_000), Dur(45_000)));
    }

    #[test]
    fn scenes_are_added_once_and_move_within_the_list() {
        let mut d = Draft::new().def;
        apply(&mut d, Change::Add("crowd".into()));
        apply(&mut d, Change::AddAll(vec!["cams".into(), "crowd".into(), "stage".into()]));
        apply(&mut d, Change::Add("stage".into()));
        apply(&mut d, Change::Add(String::new()));
        assert_eq!(scenes(&d), ["crowd", "cams", "stage"]);
        apply(&mut d, Change::Up(0));
        apply(&mut d, Change::Down(2));
        assert_eq!(scenes(&d), ["crowd", "cams", "stage"], "the ends stay put");
        apply(&mut d, Change::Down(0));
        assert_eq!(scenes(&d), ["cams", "crowd", "stage"]);
        apply(&mut d, Change::Up(2));
        assert_eq!(scenes(&d), ["cams", "stage", "crowd"]);
        apply(&mut d, Change::Remove(3));
        apply(&mut d, Change::Remove(0));
        assert_eq!(scenes(&d), ["stage", "crowd"]);
    }

    #[test]
    fn like_the_rest_drops_a_scenes_own_transition_and_length() {
        let mut d = Draft::new().def;
        d.steps.push(AutoSeqStep { scene: "cams".into(), dwell: None, transition: Some("morph".into()), ms: Some(Dur(1200)) });
        apply(&mut d, Change::StepTransition(0, Some("fade".into())));
        assert_eq!((d.steps[0].transition.as_deref(), d.steps[0].ms), (Some("fade"), Some(Dur(1200))), "another transition keeps its length");
        apply(&mut d, Change::StepTransition(0, None));
        assert_eq!((d.steps[0].transition.as_deref(), d.steps[0].ms), (None, None));
    }

    #[test]
    fn what_the_editor_makes_is_what_the_engine_plays() {
        let mut d = Draft::new();
        d.def.label = " Drum cycle ".into();
        d.def.order = Order::Random;
        d.def.transition = Some("fade".into());
        d.def.ms = Some(Dur(800));
        apply(&mut d.def, Change::Dwell(45_000));
        apply(&mut d.def, Change::AddAll(vec!["drum_cams".into(), "crowd".into(), "stage".into()]));
        apply(&mut d.def, Change::StepDwell(0, 60_000));
        apply(&mut d.def, Change::StepDwell(2, 45_000));
        apply(&mut d.def, Change::StepTransition(2, Some("morph".into())));
        apply(&mut d.def, Change::Down(0));
        assert!(d.errors().is_empty(), "{:?}", d.errors());
        let (saved, args) = d.write_args(&["drum_cycle".to_string()]);
        assert_eq!(saved.id.as_deref(), Some("drum_cycle_2"), "never overwrites another auto sequence");
        let def = engine_reads("drum_cycle_2", &args);
        assert_eq!(def, saved.def);
        assert_eq!(
            (def.label.as_str(), def.order, def.dwell, def.transition.as_deref(), def.ms),
            ("Drum cycle", Order::Random, Dur(45_000), Some("fade"), Some(Dur(800)))
        );
        assert_eq!(scenes(&def), ["crowd", "drum_cams", "stage"]);
        assert_eq!((def.step_dwell(0), def.step_dwell(1)), (Dur(45_000), Dur(60_000)));
        assert_eq!(def.steps[2].dwell, Some(Dur(45_000)), "a scene's own time persists even when it equals the default");
        assert_eq!(def.steps[2].transition.as_deref(), Some("morph"));
        // and it opens again as the draft that was saved (so it isn't shown as changed)
        let text = args.get_path("text").and_then(Value::as_str).unwrap();
        assert_eq!(Draft::from_toml("drum_cycle_2", text).unwrap(), saved);
    }

    #[test]
    fn a_draft_is_checked_before_saving() {
        let mut d = Draft::new();
        assert_eq!(d.errors().len(), 2, "a new one needs a name and a scene");
        d.def.label = "Chill".into();
        assert_eq!(d.errors().len(), 1);
        apply(&mut d.def, Change::Add("cams".into()));
        assert!(d.errors().is_empty());
    }
}
