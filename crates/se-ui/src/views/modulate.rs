//! Modulate (§15.5): create or edit a binding for an address — pick a signal, shape it
//! (gain/offset/gate/attack/release/curve/range/auto-normalize/mode/scope) with a live transfer
//! curve and the shaped output over the signal's recent history (computed with the core's own
//! shaping code), then save to `bindings/<name>.toml` through `project.write`.

use crate::app::App;
use egui::{RichText, Vec2};
use se_core::bindings::{BindingRt, apply_curve};
use se_core::config::{BindMode, BindingDef, Curve, Dur};
use se_proto::{Meta, Value};
use se_ui_kit::widgets::{self, icon};

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

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::section(ui, &t, icon::BINDING, "Modulate");
    if app.build.modulate.draft.is_none() {
        ui.label(RichText::new("click ~ next to a parameter in the inspector, or a binding in the library").color(t.fg_dim));
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
    {
        let st = &mut app.build.modulate;
        let Some(d) = st.draft.as_mut() else { return };
        egui::Grid::new("modulate").num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
            ui.label("target");
            ui.label(RichText::new(&d.target).monospace().color(t.modulated()));
            ui.end_row();
            ui.label("name");
            ui.add_enabled(d.existing.is_none(), egui::TextEdit::singleline(&mut d.name).desired_width(200.0));
            ui.end_row();
            ui.label("signal");
            ui.vertical(|ui| {
                egui::ComboBox::from_id_salt("mod-signal")
                    .width(240.0)
                    .selected_text(if d.signal.is_empty() { "pick a signal…" } else { d.signal.as_str() })
                    .show_ui(ui, |ui| {
                        ui.add(egui::TextEdit::singleline(&mut st.signal_filter).hint_text("filter (band.*, music.*, midi.*)"));
                        let f = st.signal_filter.to_lowercase();
                        for s in signals.iter().filter(|s| f.is_empty() || s.to_lowercase().contains(&f)).take(200) {
                            if ui.selectable_label(d.signal == *s, s).clicked() {
                                d.signal = s.clone();
                            }
                        }
                    });
            });
            ui.end_row();
            ui.label("gain / offset");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut d.gain).speed(0.01).range(-20.0..=20.0));
                ui.add(egui::DragValue::new(&mut d.offset).speed(0.01).range(-10.0..=10.0));
                ui.checkbox(&mut d.invert, "invert");
            });
            ui.end_row();
            ui.label("gate");
            ui.horizontal(|ui| {
                let mut on = d.gate.is_some();
                if ui.checkbox(&mut on, "").changed() {
                    d.gate = on.then_some(0.1);
                }
                if let Some(g) = d.gate.as_mut() {
                    ui.add(egui::Slider::new(g, 0.0..=1.0).max_decimals(3));
                }
            });
            ui.end_row();
            ui.label("attack / release");
            ui.horizontal(|ui| {
                ui.add(egui::DragValue::new(&mut d.attack_ms).range(0.0..=5000.0).speed(1.0).suffix(" ms"));
                ui.add(egui::DragValue::new(&mut d.release_ms).range(0.0..=10000.0).speed(2.0).suffix(" ms"));
            });
            ui.end_row();
            ui.label("curve");
            egui::ComboBox::from_id_salt("mod-curve").selected_text(curve_name(d.curve)).show_ui(ui, |ui| {
                for c in [Curve::Linear, Curve::Exp, Curve::Log, Curve::Smoothstep, Curve::FaderTaper] {
                    ui.selectable_value(&mut d.curve, c, curve_name(c));
                }
            });
            ui.end_row();
            ui.label("range");
            ui.horizontal(|ui| {
                let mut on = d.range.is_some();
                if ui.checkbox(&mut on, "").changed() {
                    d.range = on.then_some([0.0, 1.0]);
                }
                if let Some(r) = d.range.as_mut() {
                    ui.add(egui::DragValue::new(&mut r[0]).speed(0.01));
                    ui.label("…");
                    ui.add(egui::DragValue::new(&mut r[1]).speed(0.01));
                }
            });
            ui.end_row();
            ui.label("auto-normalize");
            ui.horizontal(|ui| {
                let mut on = d.auto_normalize.is_some();
                if ui.checkbox(&mut on, "").on_hover_text("adaptive gain: same feel on quiet and loud songs").changed() {
                    d.auto_normalize = on.then_some(10.0);
                }
                if let Some(w) = d.auto_normalize.as_mut() {
                    ui.add(egui::DragValue::new(w).range(1.0..=120.0).suffix(" s window"));
                }
            });
            ui.end_row();
            ui.label("mode");
            ui.horizontal(|ui| {
                for m in [BindMode::Add, BindMode::Multiply, BindMode::Replace] {
                    ui.selectable_value(&mut d.mode, m, mode_name(m));
                }
            });
            ui.end_row();
            ui.label("scope");
            ui.add(egui::TextEdit::singleline(&mut d.scope).hint_text("always · scene.duo · preset.hype · mode.live · expression").desired_width(240.0));
            ui.end_row();
        });
        ui.horizontal(|ui| {
            let ok = !d.signal.is_empty() && !d.target.is_empty() && !d.name.trim().is_empty() && BindingRt::new(d.def()).is_ok();
            if ui.add_enabled(ok, egui::Button::new(RichText::new(format!("{} save binding", icon::CHECK)).strong())).clicked() {
                save = true;
            }
            if ui.button("close").clicked() {
                cancel = true;
            }
            if let Err(e) = BindingRt::new(d.def()) {
                ui.label(RichText::new(e).color(t.bright_red));
            }
            let dest = match &d.existing {
                Some((_, Some(f))) => f.clone(),
                _ => format!("bindings/{}.toml", sanitize(&d.name)),
            };
            ui.label(RichText::new(format!("→ {dest}")).small().color(t.fg_dim));
        });
    }
    // live preview
    let d = app.build.modulate.draft.clone().unwrap_or_else(|| Draft::new("", &Meta::default()));
    let hist: Vec<f32> = app.m.signals.get(&d.signal).map(|h| h.iter().copied().collect()).unwrap_or_default();
    let shaped = d.simulate(&hist, 1.0 / 30.0);
    let out_range = d.range.map(|[a, b]| [a.min(b) as f32, a.max(b) as f32]).unwrap_or([0.0, 1.0]);
    ui.horizontal(|ui| {
        let f = |x: f64| d.transfer(x);
        se_ui_kit::curve::transfer_curve(ui, &t, Vec2::new(160.0, 120.0), &f, d.range.unwrap_or([0.0, 1.0]), hist.last().map(|v| *v as f64));
        ui.vertical(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} (raw) → shaped {}",
                    if d.signal.is_empty() { "—" } else { &d.signal },
                    shaped.last().map(|v| format!("{v:.3}")).unwrap_or_default()
                ))
                .small(),
            );
            se_ui_kit::curve::dual_scope(ui, &t, Vec2::new((ui.available_width() - 8.0).max(160.0), 104.0), &hist, &shaped, out_range);
        });
    });
    if save {
        let file_text = match app.build.modulate.draft.as_ref().and_then(|d| d.existing.as_ref()).and_then(|(_, f)| f.clone()) {
            Some(f) => app.m.q(&format!("project.read:{f}")).and_then(|v| v.get_path("text")).and_then(Value::as_str).map(String::from),
            None => None,
        };
        let args = d.write_args(file_text.as_deref());
        app.m.action("project.write", args);
        app.m.toast(format!("saved binding {}", d.name), false);
        app.m.refresh_soon();
    }
    if cancel {
        app.build.modulate.draft = None;
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
