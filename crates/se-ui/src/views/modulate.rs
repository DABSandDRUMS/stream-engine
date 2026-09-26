//! Signal links (§15.5, Settings → Troubleshooting): every binding in a list, and an editor to
//! create or change one — pick a signal, shape it (strength/shift/gate/rise/fall/curve/range/auto
//! level/combine/only-when) with a live transfer curve and the shaped output over the signal's
//! recent history (computed with the core's own shaping code), then save to
//! `bindings/<name>.toml` through `project.write`.

use crate::app::App;
use crate::views::live::nice;
use egui::{Align, Layout, RichText, Ui, Vec2};
use se_core::bindings::{BindingRt, apply_curve};
use se_core::config::{BindMode, BindingDef, Curve, Dur};
use se_proto::{Meta, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_mono, font_semibold, spacing, type_scale};
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

    /// `project.write` arguments: path + keys (removals as null so edits drop old options).
    pub fn write_args(&self, file_text: Option<&str>) -> Value {
        let d = self.def();
        let mut set = Value::map()
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
            .with("scope", d.scope.clone().map(Value::Str).unwrap_or_default());
        match &self.existing {
            Some((name, Some(path))) => {
                let multi = file_text.and_then(|t| t.parse::<toml::Table>().ok()).is_some_and(|t| t.get("binding").is_some_and(|b| b.is_array()));
                if multi {
                    let stem_index = name.rsplit_once('#').and_then(|(_, n)| n.parse::<i64>().ok()).map(|n| n - 1);
                    let args = Value::map().with("path", path.clone()).with("table", "binding");
                    match stem_index {
                        Some(i) => args.with("index", i).with("set", set),
                        None => args.with("match", Value::map().with("name", name.clone())).with("set", set),
                    }
                } else {
                    Value::map().with("path", path.clone()).with("set", set)
                }
            }
            _ => {
                set = set.with("name", Value::Null);
                Value::map().with("path", format!("bindings/{}.toml", sanitize(&self.name))).with("set", set)
            }
        }
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

#[derive(Default)]
pub struct ModulateState {
    pub draft: Option<Draft>,
    pub signal_filter: String,
    pending_file: Option<String>,
}

impl ModulateState {
    pub fn start(&mut self, target: &str, meta: &Meta, _current: &Value) {
        self.draft = Some(Draft::new(target, meta));
        self.signal_filter.clear();
    }

    /// Edit an existing binding (row of the `bindings` query + its definition from `config.bindings`).
    pub fn edit_existing(&mut self, b: &Value) {
        let s = |k: &str| b.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
        let mut d = Draft::new(&s("target"), &Meta::default());
        d.name = s("name");
        d.signal = s("signal");
        d.enabled = b.get_path("enabled").is_none_or(Value::truthy);
        let file = b.get_path("file").and_then(Value::as_str).filter(|f| !f.is_empty()).map(String::from);
        d.existing = Some((d.name.clone(), file.clone()));
        self.pending_file = file;
        self.draft = Some(d);
    }

    fn fill_from_config(&mut self, cfg: &Value) {
        let Some(d) = self.draft.as_mut() else { return };
        let Some((name, _)) = d.existing.clone() else { return };
        let Some(def) = cfg.as_list().and_then(|l| l.iter().find(|x| x.get_path("name").and_then(Value::as_str) == Some(name.as_str()))) else { return };
        let Ok(def): Result<BindingDef, _> = serde_json::from_value(serde_json::Value::from(def)) else { return };
        let keep = d.existing.clone();
        *d = Draft {
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
            existing: keep,
        };
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

/// Plain words for a curve (the file keeps [`curve_name`]).
fn curve_words(c: Curve) -> &'static str {
    match c {
        Curve::Linear => "Straight",
        Curve::Exp => "Slow start",
        Curve::Log => "Fast start",
        Curve::Smoothstep => "Soft S-curve",
        Curve::FaderTaper => "Like a fader",
    }
}

/// Form row: dim label on the left (with the file key as tooltip), control on the right.
fn row(ui: &mut Ui, t: &Theme, label: &str, key: &str, add: impl FnOnce(&mut Ui)) {
    ui.label(RichText::new(label).color(t.text_dim)).on_hover_text(key);
    ui.horizontal(add);
    ui.end_row();
}

pub fn ui(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    if app.build.modulate.draft.is_none() {
        list(app, ui, &t);
        return;
    }
    // definitions of existing bindings (shaping details) and the file they live in
    if app.build.modulate.draft.as_ref().is_some_and(|d| d.existing.is_some()) {
        if app.m.q("config.bindings").is_none() {
            app.m.query("config.bindings", Value::Null);
        } else if let Some(cfg) = app.m.q("config.bindings").cloned()
            && app.build.modulate.draft.as_ref().is_some_and(|d| d.signal.is_empty() || d.target.is_empty() || d.attack_ms == 10.0 && d.release_ms == 150.0)
        {
            app.build.modulate.fill_from_config(&cfg);
        }
        if let Some(f) = app.build.modulate.pending_file.take() {
            app.m.query_as(&format!("project.read:{f}"), "project.read", Value::map().with("path", f));
        }
    }
    let mut signals: Vec<String> = app.m.signals.keys().cloned().collect();
    signals.sort();
    let mut save = false;
    let mut cancel = false;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let form_w = (ui.available_width() * 0.5).clamp(420.0, 640.0);
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(Vec2::new(form_w, 10.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(form_w);
                let st = &mut app.build.modulate;
                let Some(d) = st.draft.as_mut() else { return };
                let title = if d.existing.is_some() { link_name(&d.name) } else { "New link".to_string() };
                ui.label(RichText::new(title).font(font_semibold(type_scale::LARGE)).color(t.fg));
                widgets::hint(ui, &t, "Make a setting follow a live signal, e.g. zoom that pulses with the kick drum.");
                ui.add_space(spacing::M);
                egui::Grid::new("modulate").num_columns(2).spacing([16.0, 10.0]).min_col_width(130.0).show(ui, |ui| {
                    row(ui, &t, "Setting", "target", |ui| {
                        if d.existing.is_none() {
                            // a new link's name follows the setting until you type your own
                            let auto = Draft::new(&d.target, &Meta::default()).name;
                            let r = ui.add(
                                se_ui_kit::widgets::field(&mut d.target)
                                    .font(font_mono(type_scale::SMALL + 0.5))
                                    .hint_text("scene.duo.node.cam.zoom")
                                    .desired_width(300.0),
                            );
                            if r.changed() && d.name == auto {
                                d.name = Draft::new(&d.target, &Meta::default()).name;
                            }
                        } else {
                            ui.label(RichText::new(&d.target).font(font_mono(type_scale::SMALL + 0.5)).color(t.modulated()));
                        }
                    });
                    row(ui, &t, "Name", "name", |ui| {
                        ui.add_enabled(d.existing.is_none(), se_ui_kit::widgets::field(&mut d.name).desired_width(240.0));
                    });
                    row(ui, &t, "Follows", "signal", |ui| {
                        egui::ComboBox::from_id_salt("mod-signal")
                            .width(260.0)
                            .selected_text(if d.signal.is_empty() { "Pick a signal…" } else { d.signal.as_str() })
                            .show_ui(ui, |ui| {
                                ui.add(se_ui_kit::widgets::field(&mut st.signal_filter).hint_text("Search (band, music, midi…)"));
                                let f = st.signal_filter.to_lowercase();
                                for s in signals.iter().filter(|s| f.is_empty() || s.to_lowercase().contains(&f)).take(200) {
                                    if ui.selectable_label(d.signal == *s, s).clicked() {
                                        d.signal = s.clone();
                                    }
                                }
                            });
                    });
                    row(ui, &t, "Strength", "gain", |ui| {
                        ui.add(egui::DragValue::new(&mut d.gain).speed(0.01).range(-20.0..=20.0).prefix("× "));
                    });
                    row(ui, &t, "Shift", "offset", |ui| {
                        ui.add(egui::DragValue::new(&mut d.offset).speed(0.01).range(-10.0..=10.0));
                    });
                    row(ui, &t, "Upside down", "invert", |ui| {
                        widgets::toggle(ui, &t, &mut d.invert);
                    });
                    row(ui, &t, "Ignore below", "gate", |ui| {
                        let mut on = d.gate.is_some();
                        if widgets::toggle(ui, &t, &mut on).changed() {
                            d.gate = on.then_some(0.1);
                        }
                        if let Some(g) = d.gate.as_mut() {
                            ui.add(egui::Slider::new(g, 0.0..=1.0).max_decimals(3));
                        }
                    });
                    row(ui, &t, "Rise / fall time", "attack / release", |ui| {
                        ui.add(egui::DragValue::new(&mut d.attack_ms).range(0.0..=5000.0).speed(1.0).suffix(" ms"));
                        ui.label(RichText::new("/").color(t.text_faint));
                        ui.add(egui::DragValue::new(&mut d.release_ms).range(0.0..=10000.0).speed(2.0).suffix(" ms"));
                    });
                    row(ui, &t, "Curve", "curve", |ui| {
                        egui::ComboBox::from_id_salt("mod-curve").selected_text(curve_words(d.curve)).show_ui(ui, |ui| {
                            for c in [Curve::Linear, Curve::Exp, Curve::Log, Curve::Smoothstep, Curve::FaderTaper] {
                                ui.selectable_value(&mut d.curve, c, curve_words(c));
                            }
                        });
                    });
                    row(ui, &t, "Output range", "range", |ui| {
                        let mut on = d.range.is_some();
                        if widgets::toggle(ui, &t, &mut on).changed() {
                            d.range = on.then_some([0.0, 1.0]);
                        }
                        if let Some(r) = d.range.as_mut() {
                            ui.add(egui::DragValue::new(&mut r[0]).speed(0.01));
                            ui.label(RichText::new("to").color(t.text_faint));
                            ui.add(egui::DragValue::new(&mut r[1]).speed(0.01));
                        }
                    });
                    row(ui, &t, "Auto level", "auto_normalize", |ui| {
                        let mut on = d.auto_normalize.is_some();
                        if widgets::toggle(ui, &t, &mut on).on_hover_text("Same feel on quiet and loud songs").changed() {
                            d.auto_normalize = on.then_some(10.0);
                        }
                        if let Some(w) = d.auto_normalize.as_mut() {
                            ui.add(egui::DragValue::new(w).range(1.0..=120.0).suffix(" s window"));
                        }
                    });
                    row(ui, &t, "Combine", "mode", |ui| {
                        let modes = [BindMode::Add, BindMode::Multiply, BindMode::Replace];
                        let mut i = modes.iter().position(|m| *m == d.mode).unwrap_or(0);
                        if widgets::segmented(ui, &t, &mut i, &["Add", "Multiply", "Replace"]) {
                            d.mode = modes[i];
                        }
                    });
                    row(ui, &t, "Only when", "scope", |ui| {
                        ui.add(se_ui_kit::widgets::field(&mut d.scope).hint_text("always · scene.duo · preset.hype · mode.live").desired_width(300.0));
                    });
                });
                ui.add_space(spacing::M);
                let err = BindingRt::new(d.def()).err();
                if let Some(e) = &err {
                    ui.label(RichText::new(format!("{} {e}", icon::WARN)).size(type_scale::SMALL + 0.5).color(t.bright_red));
                    ui.add_space(spacing::XS);
                }
                ui.horizontal(|ui| {
                    let ok = !d.signal.is_empty() && !d.target.is_empty() && !d.name.trim().is_empty() && err.is_none();
                    if widgets::button_ex(ui, &t, Some(icon::CHECK), "Save link", Kind::Primary, Size::Medium, 0.0, ok).clicked() {
                        save = true;
                    }
                    if widgets::button(ui, &t, "Close", Kind::Ghost).clicked() {
                        cancel = true;
                    }
                });
                let dest = match &d.existing {
                    Some((_, Some(f))) => f.clone(),
                    _ => format!("bindings/{}.toml", sanitize(&d.name)),
                };
                widgets::hint(ui, &t, &format!("Saved in {dest}"));
            });
            ui.add_space(spacing::XL);
            // live preview
            ui.vertical(|ui| {
                let d = app.build.modulate.draft.clone().unwrap_or_else(|| Draft::new("", &Meta::default()));
                let hist: Vec<f32> = app.m.signals.get(&d.signal).map(|h| h.iter().copied().collect()).unwrap_or_default();
                let shaped = d.simulate(&hist, 1.0 / 30.0);
                let out_range = d.range.map(|[a, b]| [a.min(b) as f32, a.max(b) as f32]).unwrap_or([0.0, 1.0]);
                widgets::section(ui, &t, "", "What it does");
                widgets::hint(ui, &t, "Left: signal in → setting out. Right: the signal (faint) and what the setting does with it.");
                ui.add_space(spacing::S);
                ui.horizontal_top(|ui| {
                    let f = |x: f64| d.transfer(x);
                    se_ui_kit::curve::transfer_curve(ui, &t, Vec2::new(170.0, 150.0), &f, d.range.unwrap_or([0.0, 1.0]), hist.last().map(|v| *v as f64));
                    ui.vertical(|ui| {
                        se_ui_kit::curve::dual_scope(ui, &t, Vec2::new((ui.available_width() - 8.0).max(160.0), 150.0), &hist, &shaped, out_range);
                        let now = match (hist.last(), shaped.last()) {
                            _ if d.signal.is_empty() => "Pick a signal to see it move.".to_string(),
                            (Some(v), Some(o)) => format!("{} is {v:.3} → the setting gets {o:.3}", d.signal),
                            _ => format!("Waiting for {}…", d.signal),
                        };
                        ui.label(RichText::new(now).font(font_mono(type_scale::SMALL)).color(t.text_dim));
                    });
                });
            });
        });
    });
    if save && let Some(d) = app.build.modulate.draft.clone() {
        let file_text = match d.existing.as_ref().and_then(|(_, f)| f.clone()) {
            Some(f) => app.m.q(&format!("project.read:{f}")).and_then(|v| v.get_path("text")).and_then(Value::as_str).map(String::from),
            None => None,
        };
        let args = d.write_args(file_text.as_deref());
        app.m.action("project.write", args);
        app.m.toast(format!("Saved the link {}", link_name(&d.name)), false);
        app.m.refresh_soon();
    }
    if cancel {
        app.build.modulate.draft = None;
    }
}

/// Every link, with "New link"; picking one opens it in the editor.
fn list(app: &mut App, ui: &mut Ui, t: &Theme) {
    let links = app.m.q_list("bindings").to_vec();
    if links.is_empty() {
        if widgets::empty_state(
            ui,
            t,
            icon::LINK,
            "No signal links yet",
            "A link makes a setting move with a signal, like zoom that pulses with the bass.",
            Some("Make a link"),
        ) {
            app.build.modulate.draft = Some(Draft::new("", &Meta::default()));
        }
        return;
    }
    ui.horizontal(|ui| {
        widgets::hint(ui, t, "Pick a link to change it.");
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::button_ex(ui, t, Some(icon::PLUS), "New link", Kind::Primary, Size::Medium, 0.0, true).clicked() {
                app.build.modulate.draft = Some(Draft::new("", &Meta::default()));
            }
        });
    });
    ui.add_space(spacing::S);
    let mut open = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for b in &links {
            let s = |k: &str| b.get_path(k).and_then(Value::as_str).unwrap_or("");
            let active = b.get_path("active").is_some_and(Value::truthy);
            let on = b.get_path("enabled").is_none_or(Value::truthy);
            let trailing = if !on {
                "Off"
            } else if active {
                "Moving now"
            } else {
                "Waiting"
            };
            if widgets::list_row(ui, t, icon::LINK, &link_name(s("name")), &format!("{} follows {}", s("target"), s("signal")), trailing, false).clicked() {
                open = Some(b.clone());
            }
        }
    });
    if let Some(b) = open {
        app.build.modulate.edit_existing(&b);
    }
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
        d.existing = Some(("controllers#2".into(), Some("controllers/faders.toml".into())));
        let a = d.write_args(Some("[[binding]]\ntarget='a'\nsignal='b'\n[[binding]]\ntarget='c'\nsignal='d'\n"));
        assert_eq!(a.get_path("table").and_then(Value::as_str), Some("binding"));
        assert_eq!(a.get_path("index").and_then(Value::as_i64), Some(1));
        d.existing = Some(("kick_shake".into(), Some("bindings/kick_shake.toml".into())));
        let a = d.write_args(Some("target = 'x'\nsignal = 'y'\n"));
        assert_eq!(a.get_path("path").and_then(Value::as_str), Some("bindings/kick_shake.toml"));
        assert!(a.get_path("table").is_none());
        assert_eq!(sanitize("  "), "binding");
    }
}
