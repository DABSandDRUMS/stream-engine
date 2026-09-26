//! Automation → Modulation (docs/ui-model.md): every link from a signal to a setting, grouped by
//! what it moves, and a roomy editor — the same controls as the ∿ popover beside any setting
//! (links.rs) plus every shaping option, a live transfer curve and the shaped output over the
//! signal's recent history (computed with the core's own shaping code). Links are saved to
//! `bindings/<name>.toml` (or edited in place in the file they live in, e.g. a learned fader in
//! `controllers/*.toml`) through `project.write`, only on Create / Save / Remove.

use crate::app::App;
use crate::views::links;
use crate::views::live::nice;
use egui::{RichText, Ui, Vec2};
use se_core::bindings::{BindingRt, apply_curve};
use se_core::config::{BindMode, BindingDef, Curve, Dur};
use se_proto::{Meta, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_mono, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};

#[derive(Clone, Debug, PartialEq)]
pub struct Draft {
    pub name: String,
    pub target: String,
    pub signal: String,
    pub gain: f64,
    pub offset: f64,
    pub gate: Option<f64>,
    pub attack_ms: f64,
    pub release_ms: f64,
    pub curve: Curve,
    pub invert: bool,
    pub range: Option<[f64; 2]>,
    /// Adaptive gain window in seconds (`None` = off).
    pub auto_normalize: Option<f64>,
    pub mode: BindMode,
    pub scope: String,
    pub enabled: bool,
    /// Editing an existing binding: its name and file.
    pub existing: Option<(String, Option<String>)>,
}

impl Draft {
    pub fn new(target: &str, meta: &Meta) -> Draft {
        let base = target.rsplit('.').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("_");
        Draft {
            name: sanitize(&format!("{base}_mod")),
            target: target.to_string(),
            signal: String::new(),
            gain: 1.0,
            offset: 0.0,
            gate: None,
            attack_ms: 10.0,
            release_ms: 150.0,
            curve: Curve::Linear,
            invert: false,
            range: meta.range,
            auto_normalize: None,
            mode: BindMode::Add,
            scope: String::new(),
            enabled: true,
            existing: None,
        }
    }

    /// A draft of an existing binding (its full definition) that lives in `file`.
    pub fn from_def(def: &BindingDef, file: Option<String>) -> Draft {
        Draft {
            name: def.name.clone(),
            target: def.target.clone(),
            signal: def.signal.clone(),
            gain: def.gain,
            offset: def.offset,
            gate: def.gate,
            attack_ms: def.attack.map(|a| a.ms() as f64).unwrap_or(0.0),
            release_ms: def.release.map(|a| a.ms() as f64).unwrap_or(0.0),
            curve: def.curve,
            invert: def.invert,
            range: def.range,
            auto_normalize: match &def.auto_normalize {
                Some(Value::Bool(true)) => Some(10.0),
                Some(Value::Str(s)) => se_proto::parse_duration_ms(s).map(|ms| ms as f64 / 1000.0),
                Some(v) => v.as_f64(),
                None => None,
            },
            mode: def.mode,
            scope: def.scope.clone().unwrap_or_default(),
            enabled: def.enabled,
            existing: Some((def.name.clone(), file)),
        }
    }

    /// The core binding definition this draft describes.
    pub fn def(&self) -> BindingDef {
        BindingDef {
            name: self.name.clone(),
            target: self.target.clone(),
            signal: self.signal.clone(),
            gain: self.gain,
            offset: self.offset,
            gate: self.gate,
            attack: (self.attack_ms > 0.0).then(|| Dur(self.attack_ms.round() as u64)),
            release: (self.release_ms > 0.0).then(|| Dur(self.release_ms.round() as u64)),
            curve: self.curve,
            invert: self.invert,
            range: self.range,
            auto_normalize: self.auto_normalize.map(|s| Value::Str(format!("{}s", s.round() as i64))),
            mode: self.mode,
            scope: (!self.scope.trim().is_empty()).then(|| self.scope.trim().to_string()),
            enabled: self.enabled,
            ..Default::default()
        }
    }

    /// Static transfer function (no smoothing, no adaptive gain): input 0–1 → output.
    pub fn transfer(&self, x: f64) -> f64 {
        let mut v = if self.invert { 1.0 - x } else { x };
        if let Some(g) = self.gate
            && v < g
        {
            v = 0.0;
        }
        v = apply_curve(self.curve, v * self.gain + self.offset);
        match self.range {
            Some([lo, hi]) => lo + v * (hi - lo),
            None => v,
        }
    }

    /// Shaped output over a recorded signal history (30 Hz), exactly as the core shapes it.
    pub fn simulate(&self, history: &[f32], dt: f64) -> Vec<f32> {
        let Ok(mut rt) = BindingRt::new(self.def()) else { return Vec::new() };
        history.iter().map(|x| rt.shape(*x as f64, dt) as f32).collect()
    }

    /// Keys written for this binding (null removes options that were cleared).
    fn set_value(&self) -> Value {
        let d = self.def();
        Value::map()
            .with("target", d.target.clone())
            .with("signal", d.signal.clone())
            .with("gain", d.gain)
            .with("offset", d.offset)
            .with("curve", curve_name(d.curve))
            .with("mode", mode_name(d.mode))
            .with("invert", d.invert)
            .with("enabled", d.enabled)
            .with("gate", d.gate.map(Value::Float).unwrap_or_default())
            .with("attack", ms_value(self.attack_ms))
            .with("release", ms_value(self.release_ms))
            .with("range", d.range.map(|[a, b]| Value::List(vec![Value::Float(a), Value::Float(b)])).unwrap_or_default())
            .with("auto_normalize", d.auto_normalize.clone().unwrap_or_default())
            .with("scope", d.scope.clone().map(Value::Str).unwrap_or_default())
    }

    /// `project.write` args naming this existing binding: its file, plus the `[[binding]]`
    /// entry when the file holds several (by explicit name, else by position `<file>#<n>`).
    fn entry_args(&self, file_text: Option<&str>) -> Option<Value> {
        let Some((name, Some(path))) = &self.existing else { return None };
        let args = Value::map().with("path", path.clone());
        let parsed = file_text.and_then(|t| t.parse::<toml::Table>().ok());
        let Some(entries) = parsed.as_ref().and_then(|t| t.get("binding")).and_then(|b| b.as_array()) else { return Some(args) };
        let args = args.with("table", "binding");
        let explicit = entries.iter().any(|e| e.get("name").and_then(|n| n.as_str()) == Some(name.as_str()));
        Some(match (explicit, name.rsplit_once('#').and_then(|(_, n)| n.parse::<i64>().ok())) {
            (false, Some(n)) => args.with("index", n - 1),
            _ => args.with("match", Value::map().with("name", name.clone())),
        })
    }

    /// `project.write` arguments: a new binding gets its own file; an existing one is edited in
    /// place. `file_text` is the current text of the existing binding's file.
    pub fn write_args(&self, file_text: Option<&str>) -> Value {
        match self.entry_args(file_text) {
            Some(args) => args.with("set", self.set_value()),
            None => Value::map().with("path", format!("bindings/{}.toml", sanitize(&self.name))).with("set", self.set_value().with("name", Value::Null)),
        }
    }

    /// `project.write` arguments that remove this existing binding (its entry, or its file when
    /// it is the only thing in it).
    pub fn delete_args(&self, file_text: Option<&str>) -> Option<Value> {
        Some(self.entry_args(file_text)?.with("delete", true))
    }
}

/// `"10ms"`, or null (removes the key) for 0.
fn ms_value(ms: f64) -> Value {
    if ms > 0.0 { Value::Str(format!("{}ms", ms.round() as i64)) } else { Value::Null }
}

pub fn curve_name(c: Curve) -> &'static str {
    match c {
        Curve::Linear => "linear",
        Curve::Exp => "exp",
        Curve::Log => "log",
        Curve::Smoothstep => "smoothstep",
        Curve::FaderTaper => "fader_taper",
    }
}

fn mode_name(m: BindMode) -> &'static str {
    match m {
        BindMode::Add => "add",
        BindMode::Multiply => "multiply",
        BindMode::Replace => "replace",
    }
}

/// File-stem-safe binding name.
pub fn sanitize(s: &str) -> String {
    let out: String = s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c.to_ascii_lowercase() } else { '_' }).collect();
    let out = out.trim_matches('_').to_string();
    if out.is_empty() { "binding".into() } else { out }
}

/// A file-stem-safe name for a new link that no file in `taken` (`bindings/<x>.toml`) uses.
pub fn unique_name(base: &str, taken: &[String]) -> String {
    let base = sanitize(base);
    let used = |n: &str| taken.iter().any(|f| f == &format!("bindings/{n}.toml"));
    let mut name = base.clone();
    let mut i = 2;
    while used(&name) {
        name = format!("{base}_{i}");
        i += 1;
    }
    name
}

/// Plain words for a curve (the file keeps [`curve_name`]).
pub(crate) fn curve_words(c: Curve) -> &'static str {
    match c {
        Curve::Linear => "Straight",
        Curve::Exp => "Slow start",
        Curve::Log => "Fast start",
        Curve::Smoothstep => "Soft S-curve",
        Curve::FaderTaper => "Like a fader",
    }
}

/// Display name of a link: `bass_zoom#1` → "Bass zoom", `bass_zoom#2` → "Bass zoom (2)".
fn link_name(name: &str) -> String {
    match name.rsplit_once('#') {
        Some((base, "1")) => nice(base),
        Some((base, n)) => format!("{} ({n})", nice(base)),
        None => nice(name),
    }
}

#[derive(Default)]
pub struct ModulateState {
    /// The link open in the Modulation editor.
    pub draft: Option<Draft>,
    pub signal_filter: String,
    /// Editing an existing link whose full definition (`config.bindings`) hasn't arrived yet;
    /// editing waits for it so a save never writes placeholder values over the file's.
    loading: bool,
    /// The open link's file text was asked for.
    file_asked: bool,
    /// The ∿/⚡ popovers and the index of who changes what.
    pub(crate) links: links::LinksState,
}

impl ModulateState {
    /// A new link on `target` (from the Inspector's ∿ or the page's "+").
    pub fn start(&mut self, target: &str, meta: &Meta, _current: &Value) {
        self.draft = Some(Draft::new(target, meta));
        self.signal_filter.clear();
        self.loading = false;
        self.file_asked = false;
    }

    /// Edit an existing link (a row of the `bindings` query); its full definition is filled in
    /// from `config.bindings` when it arrives.
    pub fn edit_existing(&mut self, b: &Value) {
        let s = |k: &str| b.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
        let mut d = Draft::new(&s("target"), &Meta::default());
        d.name = s("name");
        d.signal = s("signal");
        d.enabled = b.get_path("enabled").is_none_or(Value::truthy);
        let file = Some(s("file")).filter(|f| !f.is_empty());
        d.existing = Some((d.name.clone(), file));
        self.draft = Some(d);
        self.signal_filter.clear();
        self.loading = true;
        self.file_asked = false;
    }
}

/// Fill a just-opened link from `config.bindings` and fetch its file (to edit it in place).
fn load_existing(app: &mut App) {
    let Some((name, file)) = app.build.modulate.draft.as_ref().and_then(|d| d.existing.clone()) else { return };
    if let Some(f) = &file
        && !app.build.modulate.file_asked
    {
        app.build.modulate.file_asked = true;
        links::read_file(app, f);
    }
    if !app.build.modulate.loading {
        return;
    }
    let target = app.build.modulate.draft.as_ref().map(|d| d.target.clone()).unwrap_or_default();
    let row = Value::map().with("name", name).with("target", target);
    if let Some(def) = links::config_def(app, &row) {
        app.build.modulate.draft = Some(Draft::from_def(&def, file));
        app.build.modulate.loading = false;
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn clip(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n.saturating_sub(1)).collect::<String>()) }
}

/// A link in the list, worded once per frame.
struct Row {
    value: Value,
    name: String,
    area: usize,
    title: String,
    sub: String,
    full: String,
    trailing: &'static str,
}

fn rows(app: &App) -> Vec<Row> {
    let mut out: Vec<Row> = app
        .m
        .q_list("bindings")
        .iter()
        .map(|b| {
            let (target, signal) = (s(b, "target"), links::signal_words(s(b, "signal")));
            let trailing = if !b.get_path("enabled").is_none_or(Value::truthy) {
                "Off"
            } else if !b.get_path("active").is_some_and(Value::truthy) {
                "Waiting"
            } else {
                ""
            };
            Row {
                value: b.clone(),
                name: s(b, "name").to_string(),
                area: links::area(target),
                title: clip(&links::address_short(app, target), 30),
                sub: clip(&format!("follows {signal}"), 38),
                full: format!("{} follows {signal}", links::address_label(app, target)),
                trailing,
            }
        })
        .collect();
    out.sort_by(|a, b| (a.area, &a.title).cmp(&(b.area, &b.title)));
    out
}

/// The Modulation page: links (grouped by what they move) · the open link.
pub fn ui(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    links::keep_fresh(app);
    load_existing(app);
    let rows = rows(app);
    let sel = app.build.modulate.draft.as_ref().and_then(|d| d.existing.as_ref()).map(|(n, _)| n.clone());
    let any = !rows.is_empty();
    let ((pick, create), start) = widgets::split(ui, 290.0, |ui| list_pane(ui, &t, &rows, sel.as_deref()), |ui| detail(app, ui, &t, any));
    if create || start {
        app.build.modulate.start("", &Meta::default(), &Value::Null);
    }
    if let Some(i) = pick {
        app.build.modulate.edit_existing(&rows[i].value);
    }
}

/// Returns (picked row, "+" clicked).
fn list_pane(ui: &mut Ui, t: &Theme, rows: &[Row], sel: Option<&str>) -> (Option<usize>, bool) {
    let create = widgets::pane_header(ui, t, "Links", Some(rows.len()), Some("New link"));
    let mut pick = None;
    egui::ScrollArea::vertical().id_salt("modulation-list").auto_shrink([false, false]).show(ui, |ui| {
        let mut area = usize::MAX;
        for (i, r) in rows.iter().enumerate() {
            if r.area != area {
                area = r.area;
                widgets::group_label(ui, t, links::AREAS[area]);
            }
            if widgets::list_row(ui, t, links::SINE, &r.title, &r.sub, r.trailing, sel == Some(r.name.as_str())).on_hover_text(&r.full).clicked() {
                pick = Some(i);
            }
        }
    });
    (pick, create)
}

/// The open link, or what a link is. Returns true when the empty state's "New link" is clicked.
fn detail(app: &mut App, ui: &mut Ui, t: &Theme, any: bool) -> bool {
    if app.build.modulate.draft.is_none() {
        let (title, body) = if any {
            ("Pick a link", "A link makes a setting follow a signal. Pick one on the left to change it, or make a new one.")
        } else {
            ("Nothing follows a signal yet", "A link makes a setting follow a signal: a layer that bounces with the kick, blur that swells with the bass.")
        };
        return widgets::empty_state(ui, t, links::SINE, title, body, Some("New link"));
    }
    egui::ScrollArea::vertical().id_salt("modulation-detail").auto_shrink([false, false]).show(ui, |ui| editor(app, ui, t));
    false
}

fn editor(app: &mut App, ui: &mut Ui, t: &Theme) {
    let Some(mut d) = app.build.modulate.draft.clone() else { return };
    let loading = app.build.modulate.loading;
    let is_new = d.existing.is_none();
    let file = d.existing.as_ref().and_then(|(_, f)| f.clone());
    let text = file.as_deref().and_then(|f| links::file_text(app, f));
    let loaded = file.is_none() || text.is_some();
    let err = BindingRt::new(d.def()).err();
    let target = d.target.trim().to_string();
    let ready = !loading && loaded && !d.signal.is_empty() && !target.is_empty() && !d.name.trim().is_empty() && err.is_none();
    let title = if target.is_empty() { "New link".to_string() } else { links::address_label(app, &target) };
    let sub = if d.signal.is_empty() { "Pick a signal for it to follow".to_string() } else { format!("Follows {}", links::signal_words(&d.signal)) };
    let (mut save, mut close) = (false, false);
    widgets::detail_header(ui, t, links::SINE, &title, &sub, |ui| {
        save = widgets::button_ex(ui, t, Some(icon::CHECK), if is_new { "Create" } else { "Save" }, Kind::Primary, Size::Medium, 0.0, ready).clicked();
        close = widgets::button_ex(ui, t, None, "Close", Kind::Ghost, Size::Medium, 0.0, true).clicked();
    });
    if let Some(e) = &err {
        ui.label(RichText::new(format!("{} {e}", icon::WARN)).size(type_scale::SMALL + 0.5).color(t.bright_red));
        ui.add_space(spacing::S);
    }
    if loading {
        widgets::hint(ui, t, "Loading this link…");
    }
    ui.add_enabled_ui(!loading, |ui| sections(app, ui, t, &mut d, is_new));
    let mut remove = false;
    if !is_new {
        ui.add_space(spacing::M);
        ui.add_enabled_ui(loaded, |ui| remove = widgets::hold_button(ui, t, "Remove link", t.bright_red, 0.6));
    }
    if save {
        if is_new {
            let taken: Vec<String> = app.m.q_list("bindings").iter().map(|b| s(b, "file").to_string()).collect();
            let stem = unique_name(&d.name, &taken);
            d.name = stem.clone();
            app.m.action("project.write", d.write_args(None));
            // from now on it is the link in bindings/<stem>.toml (named after its file)
            d.name = format!("{stem}#1");
            d.existing = Some((d.name.clone(), Some(format!("bindings/{stem}.toml"))));
            app.build.modulate.file_asked = false;
        } else {
            app.m.action("project.write", d.write_args(text.as_deref()));
            if let Some(f) = &file {
                links::read_file(app, f);
            }
        }
        app.m.toast(format!("Saved the link {}.", link_name(&d.name)), false);
        links::refetch(app);
    }
    if remove && let Some(args) = d.delete_args(text.as_deref()) {
        app.m.action("project.write", args);
        app.m.toast(format!("Removed the link {}.", link_name(&d.name)), false);
        links::refetch(app);
        app.build.modulate.draft = None;
        return;
    }
    app.build.modulate.draft = if close { None } else { Some(d) };
}

fn sections(app: &mut App, ui: &mut Ui, t: &Theme, d: &mut Draft, is_new: bool) {
    widgets::inspector_section(ui, t, "mod-preview", "What it does", true, |_| {}, |ui| preview(app, ui, t, d));
    widgets::inspector_section(
        ui,
        t,
        "mod-setting",
        "Setting",
        true,
        |_| {},
        |ui| {
            if is_new {
                widgets::prop_row(ui, t, "Setting", |ui| {
                    // a new link's name follows the setting until you type your own
                    let auto = Draft::new(&d.target, &Meta::default()).name;
                    let r = ui.add(
                        widgets::field(&mut d.target)
                            .font(font_mono(type_scale::SMALL + 0.5))
                            .hint_text("scene.main.node.cam.scale")
                            .desired_width(ui.available_width()),
                    );
                    if r.changed() && d.name == auto {
                        d.name = Draft::new(&d.target, &Meta::default()).name;
                    }
                });
                widgets::prop_row(ui, t, "Name", |ui| ui.add(widgets::field(&mut d.name).desired_width(240.0)));
            } else {
                widgets::prop_row(ui, t, "Setting", |ui| {
                    ui.label(RichText::new(links::address_label(app, &d.target)).color(t.modulated())).on_hover_text(d.target.as_str());
                });
            }
        },
    );
    widgets::inspector_section(
        ui,
        t,
        "mod-signal",
        "Signal",
        true,
        |_| {},
        |ui| {
            let target = d.target.trim().to_string();
            let learn = (is_new && se_proto::address::is_valid(&target, false)).then_some(target.as_str());
            links::signal_picker(app, ui, t, &mut d.signal, learn, false);
            ui.add_space(spacing::XS);
            widgets::details(ui, t, "mod-all-signals", "All signals", |ui| all_signals(app, ui, t, &mut d.signal));
        },
    );
    let bounds = app.m.meta.get(d.target.trim()).and_then(|m| m.range).map(|[a, b]| (a, b)).unwrap_or((0.0, 0.0));
    widgets::inspector_section(
        ui,
        t,
        "mod-response",
        "Response",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "From / to", |ui| {
                let mut on = d.range.is_some();
                if widgets::toggle(ui, t, &mut on).on_hover_text("Map the signal onto these values").changed() {
                    d.range = on.then_some(if bounds.1 > bounds.0 { [bounds.0, bounds.1] } else { [0.0, 1.0] });
                }
                if let Some(r) = d.range.as_mut() {
                    links::range_values(ui, t, r, bounds);
                }
            });
            widgets::prop_row(ui, t, "Strength", |ui| ui.add(egui::DragValue::new(&mut d.gain).speed(0.01).range(-20.0..=20.0).prefix("× ")));
            widgets::prop_row(ui, t, "Shift", |ui| ui.add(egui::DragValue::new(&mut d.offset).speed(0.01).range(-10.0..=10.0)));
            widgets::prop_row(ui, t, "Upside down", |ui| widgets::toggle(ui, t, &mut d.invert));
            widgets::prop_row(ui, t, "Ignore below", |ui| {
                let mut on = d.gate.is_some();
                if widgets::toggle(ui, t, &mut on).changed() {
                    d.gate = on.then_some(0.1);
                }
                if let Some(g) = d.gate.as_mut() {
                    ui.add(egui::Slider::new(g, 0.0..=1.0).max_decimals(3));
                }
            });
            widgets::prop_row(ui, t, "Curve", |ui| links::curve_chips(ui, t, &mut d.curve));
            widgets::prop_row(ui, t, "Smoothing", |ui| links::smoothing(ui, t, d));
            widgets::prop_row(ui, t, "Auto level", |ui| {
                let mut on = d.auto_normalize.is_some();
                if widgets::toggle(ui, t, &mut on).on_hover_text("Same feel on quiet and loud songs").changed() {
                    d.auto_normalize = on.then_some(10.0);
                }
                if let Some(w) = d.auto_normalize.as_mut() {
                    ui.add(egui::DragValue::new(w).range(1.0..=120.0).suffix(" s window"));
                }
            });
        },
    );
    widgets::inspector_section(
        ui,
        t,
        "mod-more",
        "More",
        false,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "On", |ui| widgets::toggle(ui, t, &mut d.enabled));
            widgets::prop_row(ui, t, "Combine", |ui| {
                let modes = [BindMode::Add, BindMode::Multiply, BindMode::Replace];
                let mut i = modes.iter().position(|m| *m == d.mode).unwrap_or(0);
                if widgets::segmented(ui, t, &mut i, &["Add", "Multiply", "Replace"]) {
                    d.mode = modes[i];
                }
            });
            widgets::prop_row(ui, t, "Only when", |ui| {
                ui.add(widgets::field(&mut d.scope).hint_text("always · scene.duo · mode.live").desired_width(ui.available_width()));
            });
            let dest = match &d.existing {
                Some((_, Some(f))) => f.clone(),
                _ => format!("bindings/{}.toml", sanitize(&d.name)),
            };
            widgets::prop_row(ui, t, "Saved in", |ui| ui.label(RichText::new(dest).font(font_mono(type_scale::SMALL + 0.5)).color(t.text_dim)));
        },
    );
}

/// Any signal the engine reports, searchable (custom signals, OSC, meters…).
fn all_signals(app: &mut App, ui: &mut Ui, t: &Theme, signal: &mut String) {
    let st = &mut app.build.modulate;
    ui.add(widgets::field(&mut st.signal_filter).hint_text("Search signals").desired_width(ui.available_width()));
    let f = st.signal_filter.to_lowercase();
    let mut names: Vec<&String> = app.m.signals.keys().filter(|n| f.is_empty() || n.to_lowercase().contains(&f)).collect();
    names.sort();
    egui::ScrollArea::vertical().id_salt("mod-all-signals").max_height(240.0).auto_shrink([false, true]).show(ui, |ui| {
        for n in names.into_iter().take(60) {
            if widgets::list_row(ui, t, "", &links::signal_label(n), "", n, *signal == *n).clicked() {
                *signal = n.clone();
            }
        }
    });
}

/// Transfer curve (signal in → setting out) and the live signal with what the setting does.
fn preview(app: &App, ui: &mut Ui, t: &Theme, d: &Draft) {
    let hist: Vec<f32> = app.m.signals.get(&d.signal).map(|h| h.iter().copied().collect()).unwrap_or_default();
    let shaped = d.simulate(&hist, 1.0 / 30.0);
    let out_range = d.range.map(|[a, b]| [a.min(b) as f32, a.max(b) as f32]).unwrap_or([0.0, 1.0]);
    widgets::hint(ui, t, "Left: signal in → setting out. Right: the signal (faint) and what the setting does with it.");
    ui.add_space(spacing::S);
    ui.horizontal_top(|ui| {
        let f = |x: f64| d.transfer(x);
        se_ui_kit::curve::transfer_curve(ui, t, Vec2::new(170.0, 140.0), &f, d.range.unwrap_or([0.0, 1.0]), hist.last().map(|v| *v as f64));
        ui.vertical(|ui| {
            se_ui_kit::curve::dual_scope(ui, t, Vec2::new((ui.available_width() - 8.0).max(160.0), 140.0), &hist, &shaped, out_range);
            let now = match (hist.last(), shaped.last()) {
                _ if d.signal.is_empty() => "Pick a signal to see it move.".to_string(),
                (Some(v), Some(o)) => format!("{} is {v:.3} → the setting gets {o:.3}", links::signal_label(&d.signal)),
                _ => format!("Waiting for {}…", links::signal_words(&d.signal)),
            };
            ui.label(RichText::new(now).font(font_mono(type_scale::SMALL)).color(t.text_dim));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transfer_matches_core_shaping_without_smoothing() {
        let mut d = Draft::new("fx.rgb_split.amount", &Meta::float(0.0, [0.0, 1.0]));
        d.signal = "band.bass".into();
        d.attack_ms = 0.0;
        d.release_ms = 0.0;
        d.gain = 2.0;
        d.curve = Curve::Exp;
        d.range = Some([0.1, 0.5]);
        let mut rt = BindingRt::new(d.def()).unwrap();
        for x in [0.0, 0.1, 0.25, 0.4] {
            assert!((rt.shape(x, 1.0 / 30.0) - d.transfer(x)).abs() < 1e-9, "{x}");
        }
        d.gate = Some(0.3);
        assert_eq!(d.transfer(0.2), 0.1);
    }

    #[test]
    fn simulate_smooths_like_the_core() {
        let mut d = Draft::new("a.b", &Meta::default());
        d.signal = "lfo.slow".into();
        d.attack_ms = 200.0;
        let out = d.simulate(&[1.0; 30], 1.0 / 30.0);
        assert_eq!(out.len(), 30);
        assert!(out[0] < 0.2 && out[29] > 0.9, "{:?}", (out[0], out[29]));
    }

    #[test]
    fn new_bindings_write_a_fresh_file_and_existing_ones_edit_in_place() {
        let mut d = Draft::new("scene.duo.node.cam_kit.offset_y", &Meta::float(0.0, [-4000.0, 4000.0]));
        d.name = "Kick Shake!".into();
        d.signal = "band.kick".into();
        let a = d.write_args(None);
        assert_eq!(a.get_path("path").and_then(Value::as_str), Some("bindings/kick_shake.toml"));
        assert_eq!(a.get_path("set.signal").and_then(Value::as_str), Some("band.kick"));
        assert_eq!(a.get_path("set.attack").and_then(Value::as_str), Some("10ms"));
        assert!(a.get_path("table").is_none());
        assert!(d.delete_args(None).is_none(), "nothing to remove before it is saved");
        let two = "[[binding]]\ntarget='a'\nsignal='b'\n[[binding]]\ntarget='c'\nsignal='d'\n";
        d.existing = Some(("controllers#2".into(), Some("controllers/faders.toml".into())));
        let a = d.write_args(Some(two));
        assert_eq!(a.get_path("table").and_then(Value::as_str), Some("binding"));
        assert_eq!(a.get_path("index").and_then(Value::as_i64), Some(1));
        let del = d.delete_args(Some(two)).unwrap();
        assert_eq!((del.get_path("index").and_then(Value::as_i64), del.get_path("delete")), (Some(1), Some(&Value::Bool(true))));
        // an entry with an explicit name is found by it, not by position
        d.existing = Some(("faders#2".into(), Some("controllers/faders.toml".into())));
        let a = d.write_args(Some("[[binding]]\nname='faders#2'\ntarget='a'\nsignal='b'\n"));
        assert_eq!(a.get_path("match.name").and_then(Value::as_str), Some("faders#2"));
        d.existing = Some(("kick_shake".into(), Some("bindings/kick_shake.toml".into())));
        let a = d.write_args(Some("target = 'x'\nsignal = 'y'\n"));
        assert_eq!(a.get_path("path").and_then(Value::as_str), Some("bindings/kick_shake.toml"));
        assert!(a.get_path("table").is_none());
        let del = d.delete_args(Some("target = 'x'\nsignal = 'y'\n")).unwrap();
        assert!(del.get_path("table").is_none() && del.get_path("delete").is_some_and(Value::truthy), "a single-link file goes away");
        assert_eq!(sanitize("  "), "binding");
        let taken = ["bindings/cam_kick.toml".to_string(), "bindings/cam_kick_2.toml".to_string()];
        assert_eq!(unique_name("Cam kick", &taken), "cam_kick_3");
        assert_eq!(unique_name("cam_bass", &taken), "cam_bass");
    }

    #[test]
    fn a_saved_link_reads_back_the_same() {
        let mut d = Draft::new("scene.duo.node.cam.scale", &Meta::float(1.0, [0.01, 10.0]));
        d.signal = "band.kick".into();
        d.gain = 1.5;
        d.offset = 0.1;
        d.gate = Some(0.2);
        d.attack_ms = 20.0;
        d.release_ms = 0.0;
        d.curve = Curve::Smoothstep;
        d.invert = true;
        d.range = Some([1.0, 1.25]);
        d.auto_normalize = Some(8.0);
        d.mode = BindMode::Replace;
        d.scope = "scene.duo".into();
        d.enabled = false;
        // what project.write puts in the new file (null = key left out)
        let Some(Value::Map(set)) = d.write_args(None).get_path("set").cloned() else { panic!("no set") };
        let kept: std::collections::BTreeMap<String, Value> = set.into_iter().filter(|(_, v)| !v.is_null()).collect();
        let text = toml::to_string(&toml::Value::try_from(Value::Map(kept)).unwrap()).unwrap();
        let def: BindingDef = toml::from_str(&text).unwrap();
        let mut back = Draft::from_def(&def, Some("bindings/cam_scale_mod.toml".into()));
        assert_eq!(back.existing.as_ref().and_then(|(_, f)| f.as_deref()), Some("bindings/cam_scale_mod.toml"));
        back.name = d.name.clone();
        back.existing = None;
        assert_eq!(back, d, "{text}");
    }
}
