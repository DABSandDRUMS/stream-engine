//! Explicit project capture inputs, independent of the running audio graph.

use crate::app::App;
use egui::{Align, RichText, Ui};
use se_proto::Value;
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_semibold, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size};
use std::time::{Duration, Instant};

const QUERY: &str = "project.audio.inputs";

fn text<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get_path(key).and_then(Value::as_str).unwrap_or("")
}

fn list<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get_path(key).and_then(Value::as_list).unwrap_or(&[])
}

#[derive(Clone)]
struct Draft {
    existing: bool,
    /// Stable ID; derived from the name for new inputs and never shown.
    name: String,
    /// The operator's name for the input ("16R output").
    label: String,
    target: String,
    stereo: bool,
    first: i64,
    second: i64,
    bus: String,
    gain: f64,
    mute: bool,
    delay_ms: f64,
    dirty: bool,
}

impl Draft {
    fn new() -> Self {
        Self {
            existing: false,
            name: String::new(),
            label: String::new(),
            target: String::new(),
            stereo: false,
            first: 1,
            second: 2,
            bus: String::new(),
            gain: 0.0,
            mute: false,
            delay_ms: 0.0,
            dirty: false,
        }
    }

    fn from_input(input: &Value) -> Self {
        let channels = list(input, "channels");
        Self {
            existing: true,
            name: text(input, "name").into(),
            label: text(input, "label").into(),
            target: text(input, "target").into(),
            stereo: channels.len() == 2,
            first: channels.first().and_then(Value::as_i64).unwrap_or(1),
            second: channels.get(1).and_then(Value::as_i64).unwrap_or(2),
            bus: text(input, "bus").into(),
            gain: input.get_path("gain").and_then(Value::as_f64).unwrap_or(0.0),
            mute: input.get_path("mute").is_some_and(Value::truthy),
            delay_ms: input.get_path("delay_ms").and_then(Value::as_f64).unwrap_or(0.0),
            dirty: false,
        }
    }

    fn args(&self) -> Value {
        let channels = if self.stereo { vec![Value::Int(self.first), Value::Int(self.second)] } else { vec![Value::Int(self.first)] };
        let args = Value::map()
            .with("name", self.name.clone())
            .with("label", self.label.trim())
            .with("target", self.target.clone())
            .with("channels", Value::List(channels))
            .with("bus", self.bus.clone())
            .with("gain", self.gain)
            .with("mute", self.mute)
            .with("delay_ms", self.delay_ms);
        if self.existing { args } else { args.with("create", true) }
    }
}

#[derive(Clone)]
struct Pending {
    name: String,
    removing: bool,
    since: Instant,
}

#[derive(Clone, Default)]
pub(super) struct State {
    open: bool,
    draft: Option<Draft>,
    removing: Option<Value>,
    pending: Option<Pending>,
    notice: Option<(String, bool)>,
    queried_at: Option<Instant>,
    connection: u64,
    events_seen: u64,
    focus_editor: bool,
}

impl State {
    pub(super) fn refresh(&mut self, app: &mut App) {
        let now = Instant::now();
        let new_events = app.m.events_seen.saturating_sub(self.events_seen) as usize;
        let mut refresh = self.connection != app.m.conn_gen;
        self.connection = app.m.conn_gen;
        for event in app.m.events.iter().skip(app.m.events.len().saturating_sub(new_events)) {
            match event.ty.as_str() {
                "project.reloaded" => {
                    refresh = true;
                    if self.draft.as_ref().is_some_and(|draft| draft.dirty) && self.pending.is_none() {
                        self.notice = Some(("Project reloaded. Your unsaved input changes are still here.".into(), false));
                    }
                }
                "project.audio.inputs.changed" => {
                    refresh = true;
                    if self.pending.as_ref().is_some_and(|pending| pending.name == text(&event.payload, "name")) {
                        let pending = self.pending.take().unwrap();
                        self.notice = Some((if pending.removing { "Removed capture input." } else { "Saved capture settings." }.into(), false));
                        self.draft = None;
                        self.removing = None;
                        self.focus_editor = true;
                    }
                }
                "project.audio.input.failed" if self.pending.as_ref().is_some_and(|pending| pending.name == text(&event.payload, "name")) => {
                    self.pending = None;
                    self.notice = Some((text(&event.payload, "error").to_string(), true));
                    refresh = true;
                }
                _ => {}
            }
        }
        self.events_seen = app.m.events_seen;
        if let Some(pending) = &self.pending
            && (!app.m.connected || now.duration_since(pending.since) > Duration::from_secs(20))
        {
            self.notice =
                Some(("The engine has not confirmed this change. Check the saved inputs before trying again; your draft is still here.".into(), true));
            self.pending = None;
            refresh = true;
        }
        if app.m.connected && (refresh || self.queried_at.is_none_or(|at| now.duration_since(at) >= Duration::from_secs(3))) {
            app.m.query(QUERY, Value::Null);
            app.m.query("audio.devices", Value::Null);
            if refresh {
                app.m.query("audio.mix", Value::Null);
            }
            self.queried_at = Some(now);
        }
    }

    pub(super) fn edit(&mut self, input: &Value) {
        if self.draft.is_some() || self.pending.is_some() || self.removing.is_some() {
            self.notice = Some(("Save or cancel the current input change first.".into(), true));
            return;
        }
        self.open = true;
        self.draft = Some(Draft::from_input(input));
        self.focus_editor = true;
        self.notice = None;
        self.queried_at = None;
    }

    pub(super) fn remove(&mut self, input: &Value) {
        if self.draft.is_some() || self.pending.is_some() || self.removing.is_some() {
            self.notice = Some(("Save or cancel the current input change first.".into(), true));
            return;
        }
        self.open = true;
        self.removing = Some(input.clone());
        self.focus_editor = true;
        self.notice = None;
        self.queried_at = None;
    }

    pub(super) fn ui(&mut self, app: &mut App, ui: &mut Ui, t: &Theme) {
        let config = app.m.q(QUERY).cloned().unwrap_or_default();
        let inputs = list(&config, "inputs");
        widgets::panel(ui, t, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_wrapped(|ui| {
                let heading = ui.label(RichText::new("Capture inputs").font(font_semibold(type_scale::LARGE)).color(t.fg));
                if self.focus_editor && self.draft.is_none() && self.removing.is_none() {
                    heading.scroll_to_me(Some(Align::TOP));
                    self.focus_editor = false;
                }
                let idle = self.draft.is_none() && self.removing.is_none() && self.pending.is_none();
                if widgets::button_ex(ui, t, None, "Add input", Kind::Primary, Size::Small, 0.0, idle).clicked() {
                    self.open = true;
                    self.draft = Some(Draft::new());
                    self.focus_editor = true;
                    self.notice = None;
                    self.queried_at = None;
                }
                if widgets::button(ui, t, "Manage inputs", Kind::Secondary).clicked() {
                    self.open = !self.open || self.draft.is_some() || self.removing.is_some();
                    self.queried_at = None;
                }
                if widgets::button_ex(ui, t, None, "Refresh inputs", Kind::Ghost, Size::Small, 0.0, app.m.connected).clicked() {
                    self.queried_at = None;
                }
            });
            widgets::hint(ui, t, "Hardware and virtual capture sources feed mix buses. App sounds are managed by the feature that plays them.");
            if !app.m.connected {
                widgets::hint(ui, t, "Connect to the engine to load or save inputs. You can still prepare a draft.");
            }
            if let Some(error) = app.m.query_errors.get(QUERY) {
                ui.label(RichText::new(format!("Couldn't load saved inputs: {error}")).color(t.bright_red));
            } else if config.is_null() {
                widgets::hint(ui, t, "Loading saved inputs…");
            } else if inputs.is_empty() {
                widgets::hint(ui, t, "No capture inputs configured. Add only the sources you want to capture.");
            } else {
                widgets::hint(
                    ui,
                    t,
                    &format!("{} saved capture input{}. Internal routing is under Advanced.", inputs.len(), if inputs.len() == 1 { "" } else { "s" }),
                );
            }
            for error in list(&config, "errors").iter().filter_map(Value::as_str) {
                ui.label(RichText::new(error).color(t.bright_red));
            }
            if let Some((notice, error)) = &self.notice {
                ui.label(RichText::new(notice).color(if *error { t.bright_red } else { t.green }));
            }
            if let Some(pending) = &self.pending {
                ui.label(
                    RichText::new(if pending.removing {
                        "Removing the input… Waiting for the project to reload."
                    } else {
                        "Saving the input… Waiting for the project to reload."
                    })
                    .color(t.text_dim),
                );
            }
            if self.open {
                ui.add_space(spacing::S);
                for input in inputs {
                    ui.push_id(("saved-input", text(input, "name")), |ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.label(
                                RichText::new(crate::views::mix::input_label(input, app.m.q("audio.devices").unwrap_or(&Value::Null))).strong().color(t.fg),
                            );
                            self.row_actions(ui, t, input);
                        });
                    });
                }
                if self.draft.is_some() {
                    self.editor(app, ui, t, &config);
                }
                if self.removing.is_some() {
                    self.confirm_remove(app, ui, t, &config);
                }
                ui.add_space(spacing::S);
                widgets::hint(ui, t, "Recording inputs are chosen separately in Settings. Adding an input here does not change recording selection.");
            }
        });
    }

    pub(super) fn row_actions(&mut self, ui: &mut Ui, t: &Theme, input: &Value) {
        let idle = self.draft.is_none() && self.removing.is_none() && self.pending.is_none();
        let label = crate::views::mix::input_label(input, &Value::Null);
        let edit = widgets::button_ex(ui, t, None, "Edit input", Kind::Ghost, Size::Small, 0.0, idle);
        edit.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, idle, format!("Edit input {label}")));
        if edit.clicked() {
            self.edit(input);
        }
        let remove = widgets::button_ex(ui, t, None, "Remove input", Kind::Ghost, Size::Small, 0.0, idle);
        remove.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, idle, format!("Remove input {label}")));
        if remove.clicked() {
            self.remove(input);
        }
    }

    fn editor(&mut self, app: &mut App, ui: &mut Ui, t: &Theme, config: &Value) {
        let devices = app.m.q("audio.devices").cloned().unwrap_or_default();
        let devices = devices.as_list().unwrap_or(&[]);
        let mut save = false;
        let mut cancel = false;
        let draft = self.draft.as_mut().unwrap();
        ui.separator();
        let heading = ui.label(
            RichText::new(if draft.existing {
                format!("Edit input {}", crate::views::mix::input_label(&draft.args(), &Value::List(devices.to_vec())))
            } else {
                "New capture input".into()
            })
            .strong()
            .color(t.fg),
        );
        if self.focus_editor {
            heading.scroll_to_me(Some(Align::TOP));
            self.focus_editor = false;
        }
        ui.add_enabled_ui(self.pending.is_none(), |ui| {
            ui.horizontal(|ui| {
                let label = ui.label("Name");
                let changed = ui
                    .add(widgets::field(&mut draft.label).hint_text("What this input is, e.g. 16R output").desired_width(320.0))
                    .labelled_by(label.id)
                    .changed();
                draft.dirty |= changed;
                if changed && !draft.existing {
                    let own = draft.bus.is_empty() || draft.bus == draft.name;
                    draft.name = input_id(&draft.label, list(config, "inputs"));
                    if own {
                        draft.bus = draft.name.clone();
                    }
                }
            });
            let selected = devices.iter().find(|device| text(device, "name") == draft.target && is_capture(device));
            let selected_text = selected.map(device_label).unwrap_or_else(|| "Manual target".into());
            egui::ComboBox::from_label("Capture device").selected_text(selected_text).width(400.0).show_ui(ui, |ui| {
                for device in devices.iter().filter(|device| is_capture(device)) {
                    if ui.selectable_label(draft.target == text(device, "name"), device_label(device)).clicked() {
                        draft.target = text(device, "name").into();
                        draft.dirty = true;
                    }
                }
                if ui.selectable_label(selected.is_none(), "Manual target").clicked() {
                    draft.target.clear();
                    draft.dirty = true;
                }
            });
            if let Some(error) = app.m.query_errors.get("audio.devices") {
                widgets::hint(ui, t, &format!("Device discovery unavailable: {error}. Enter an exact source target below."));
            } else if !devices.iter().any(is_capture) {
                widgets::hint(ui, t, "No capture devices discovered. You can enter the exact source target manually.");
            }
            ui.horizontal(|ui| {
                let label = ui.label("Source target");
                draft.dirty |=
                    ui.add(widgets::field(&mut draft.target).hint_text("Exact PipeWire source name").desired_width(480.0)).labelled_by(label.id).changed();
            });
            widgets::hint(ui, t, "Device choices use their exact source names. A manually saved target can reconnect when that device becomes available.");
            ui.horizontal(|ui| {
                ui.label("Device channels");
                draft.dirty |= ui.selectable_value(&mut draft.stereo, false, "One channel (mono)").changed();
                draft.dirty |= ui.selectable_value(&mut draft.stereo, true, "Two channels (stereo)").changed();
            });
            ui.horizontal(|ui| {
                let label = ui.label(if draft.stereo { "Left channel" } else { "Channel" });
                draft.dirty |= ui.add(egui::DragValue::new(&mut draft.first).range(1..=256)).labelled_by(label.id).changed();
                if draft.stereo {
                    let label = ui.label("Right channel");
                    draft.dirty |= ui.add(egui::DragValue::new(&mut draft.second).range(1..=256)).labelled_by(label.id).changed();
                }
                if let Some(device) = devices.iter().find(|device| text(device, "name") == draft.target) {
                    let ports = device.get_path("ports").and_then(Value::as_i64).unwrap_or(0);
                    widgets::hint(ui, t, &format!("{ports} channels discovered; numbering starts at 1."));
                }
            });
            // Only routing that exists, plus this input's own channel; never unused built-in IDs.
            let running = app.m.q("audio.mix").and_then(|mix| mix.get_path("buses")).and_then(Value::as_list);
            let mut choices: Vec<String> = match running {
                Some(buses) => buses.iter().map(|bus| text(bus, "name").to_string()).collect(),
                None => list(config, "buses")
                    .iter()
                    .filter(|bus| bus.get_path("configured").is_some_and(Value::truthy))
                    .map(|bus| text(bus, "name").to_string())
                    .collect(),
            };
            choices.retain(|name| !name.is_empty() && name != "program" && *name != draft.name);
            let own = |draft: &Draft| !draft.name.is_empty() && draft.bus == draft.name;
            let route_label = |draft: &Draft| {
                if own(draft) {
                    "Own channel".to_string()
                } else if draft.bus.is_empty() {
                    "Choose where it goes".to_string()
                } else {
                    crate::views::mix::bus_label(&draft.bus)
                }
            };
            egui::ComboBox::from_label("Route to").selected_text(route_label(draft)).width(240.0).show_ui(ui, |ui| {
                if !draft.name.is_empty() && ui.selectable_label(own(draft), "Own channel").clicked() {
                    draft.bus = draft.name.clone();
                    draft.dirty = true;
                }
                for name in &choices {
                    draft.dirty |= ui.selectable_value(&mut draft.bus, name.clone(), crate::views::mix::bus_label(name)).changed();
                }
            });
            ui.horizontal_wrapped(|ui| {
                let label = ui.label("Saved gain");
                draft.dirty |= ui.add(egui::DragValue::new(&mut draft.gain).range(-60.0..=24.0).speed(0.1).suffix(" dB")).labelled_by(label.id).changed();
                let label = ui.label("Delay");
                let max_delay = config.get_path("max_delay_ms").and_then(Value::as_f64).unwrap_or(10000.0).max(draft.delay_ms);
                draft.dirty |=
                    ui.add(egui::DragValue::new(&mut draft.delay_ms).range(0.0..=max_delay).speed(1.0).suffix(" ms")).labelled_by(label.id).changed();
                draft.dirty |= ui.checkbox(&mut draft.mute, "Start muted").changed();
            });
            let named = !draft.label.trim().is_empty();
            let channels_valid = !draft.stereo || draft.first != draft.second;
            if !channels_valid {
                ui.label(RichText::new("Choose different left and right channels.").color(t.bright_red));
            }
            let valid = named && !draft.name.is_empty() && !draft.target.trim().is_empty() && !draft.bus.is_empty() && channels_valid;
            ui.horizontal(|ui| {
                save = widgets::button_ex(ui, t, None, "Save input", Kind::Primary, Size::Small, 0.0, app.m.connected && valid).clicked();
                cancel = widgets::button(ui, t, "Cancel", Kind::Secondary).clicked();
                if draft.dirty {
                    widgets::hint(ui, t, "Unsaved changes");
                }
            });
        });
        if save {
            self.pending = Some(Pending { name: draft.name.clone(), removing: false, since: Instant::now() });
            self.notice = None;
            app.m.action("project.audio.input.save", draft.args());
        } else if cancel {
            self.draft = None;
            self.notice = None;
        }
    }

    fn confirm_remove(&mut self, app: &mut App, ui: &mut Ui, t: &Theme, config: &Value) {
        let selected = self.removing.as_ref().unwrap();
        let name = text(selected, "name");
        let input = list(config, "inputs").iter().find(|input| text(input, "name") == name).unwrap_or(selected);
        let references = list(input, "references");
        ui.separator();
        let heading = ui.label(
            RichText::new(format!("Remove capture input {}?", crate::views::mix::input_label(input, app.m.q("audio.devices").unwrap_or(&Value::Null))))
                .strong()
                .color(t.fg),
        );
        if self.focus_editor {
            heading.scroll_to_me(Some(Align::TOP));
            self.focus_editor = false;
        }
        widgets::hint(ui, t, "This removes its saved capture settings and effects, not the device or its destination mix bus.");
        if !references.is_empty() {
            ui.label(RichText::new("This input is still used. Update these references before removing it:").color(t.yellow));
            for reference in references.iter().filter_map(Value::as_str) {
                ui.label(reference);
            }
        }
        let mut remove = false;
        let mut cancel = false;
        ui.add_enabled_ui(self.pending.is_none(), |ui| {
            ui.horizontal(|ui| {
                remove =
                    widgets::button_ex(ui, t, None, "Confirm remove input", Kind::Danger, Size::Small, 0.0, app.m.connected && references.is_empty()).clicked();
                cancel = widgets::button(ui, t, "Cancel", Kind::Secondary).clicked();
            });
        });
        if remove {
            self.pending = Some(Pending { name: name.into(), removing: true, since: Instant::now() });
            self.notice = None;
            app.m.action("project.audio.input.remove", Value::map().with("name", name));
        } else if cancel {
            self.removing = None;
            self.notice = None;
        }
    }
}

fn is_capture(device: &Value) -> bool {
    text(device, "class").starts_with("Audio/Source") && !text(device, "name").starts_with("se-")
}

fn device_label(device: &Value) -> String {
    let description = text(device, "description");
    let kind = if text(device, "class") == "Audio/Source/Virtual" { "Virtual source" } else { "Capture device" };
    format!("{} · {kind}", if description.is_empty() { text(device, "name") } else { description })
}

/// A stable, unused ID from the operator's name ("16R output" → `16r_output`).
fn input_id(label: &str, saved: &[Value]) -> String {
    let mut base = String::new();
    for c in label.trim().chars() {
        if c.is_ascii_alphanumeric() {
            base.push(c.to_ascii_lowercase());
        } else if !base.ends_with('_') && !base.is_empty() {
            base.push('_');
        }
    }
    let base = base.trim_end_matches('_');
    let base = if base.is_empty() { "input" } else { base };
    let taken = |id: &str| saved.iter().any(|input| text(input, "name") == id);
    if !taken(base) {
        return base.to_string();
    }
    (2..).map(|n| format!("{base}_{n}")).find(|id| !taken(id)).unwrap_or_default()
}
