//! Mix/Audio view (§8, §15.6): bus strips (meters, faders, mute, limiter, ducking), effect
//! chains with bypass/wet/triggers and parameters, inputs and sources (gain, A/V delay),
//! ducking, live analysis (spectrum, bands, onsets, beat), sounds, PipeWire links, and perf
//! (xruns, DSP load, quantum, latency, real-time scheduling). Client of the se-audio
//! `audio.mix` query, `audio.*` state/signals, and `audio.*` actions.

use crate::app::App;
use egui::{Color32, CornerRadius, Pos2, Rect, RichText, Sense, Stroke, Ui, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::widgets::{self, LedState, icon};
use std::time::{Duration, Instant};

const MIX_EVERY: Duration = Duration::from_millis(250);
const META_EVERY: Duration = Duration::from_secs(3);
const DB_MIN: f64 = -60.0;
const DB_MAX: f64 = 12.0;

#[derive(Clone, Default)]
struct AudioState {
    mix_at: Option<Instant>,
    meta_at: Option<Instant>,
    open_fx: Option<String>,
}

fn db_to_fader(db: f64) -> f32 {
    // audio taper: more travel near unity
    let x = ((db - DB_MIN) / (DB_MAX - DB_MIN)).clamp(0.0, 1.0);
    x.powf(1.0 / 1.6) as f32
}

fn fader_to_db(v: f32) -> f64 {
    DB_MIN + (v as f64).powf(1.6) * (DB_MAX - DB_MIN)
}

fn lin_db(x: f32) -> f32 {
    20.0 * x.max(1e-5).log10()
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn health_led(v: Option<&Value>) -> LedState {
    match v.and_then(|h| h.get_path("status")).and_then(Value::as_str) {
        Some("pass") => LedState::Healthy,
        Some("warn") => LedState::Armed,
        Some("fail") => LedState::Error,
        _ => LedState::Idle,
    }
}

pub fn ui(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    let id = ui.id().with("audio_view");
    let mut st: AudioState = ui.data_mut(|d| d.get_temp::<AudioState>(id).unwrap_or_default());
    let now = Instant::now();
    if app.m.connected {
        if st.mix_at.is_none_or(|x| now.duration_since(x) >= MIX_EVERY) {
            st.mix_at = Some(now);
            app.m.query("audio.mix", Value::Null);
        }
        if st.meta_at.is_none_or(|x| now.duration_since(x) >= META_EVERY) {
            st.meta_at = Some(now);
            app.m.fetch("audio.**");
        }
    }
    ui.ctx().request_repaint_after(Duration::from_millis(33));
    let mix = app.m.q("audio.mix").cloned().unwrap_or_default();

    header(app, ui, &t, &mix);
    ui.add_space(6.0);
    if mix.is_null() {
        ui.label(RichText::new("no audio data — engine offline or audio subsystem not running").color(t.fg_dim));
        ui.data_mut(|d| d.insert_temp(id, st));
        return;
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        widgets::section(ui, &t, icon::MIX, "Buses");
        egui::ScrollArea::horizontal().id_salt("bus_strips").show(ui, |ui| {
            ui.horizontal_top(|ui| {
                for b in mix.get_path("buses").and_then(Value::as_list).unwrap_or(&[]) {
                    bus_strip(app, ui, &t, b, &mix);
                    ui.add_space(4.0);
                }
            });
        });
        ui.add_space(8.0);
        widgets::section(ui, &t, icon::EFFECT, "Effect chains");
        for b in mix.get_path("buses").and_then(Value::as_list).unwrap_or(&[]).iter().chain(mix.get_path("inputs").and_then(Value::as_list).unwrap_or(&[])) {
            let fx = b.get_path("fx").and_then(Value::as_list).unwrap_or(&[]);
            if fx.is_empty() {
                continue;
            }
            ui.label(RichText::new(s(b, "address")).monospace().color(t.fg_dim));
            for f in fx {
                fx_row(app, ui, &t, f, &mix, &mut st);
            }
        }
        ui.add_space(8.0);
        ui.columns(2, |cols| {
            inputs(app, &mut cols[0], &t, &mix);
            ducking(app, &mut cols[1], &t, &mix);
        });
        ui.add_space(8.0);
        analysis(app, ui, &t, &mix);
        ui.add_space(8.0);
        ui.columns(2, |cols| {
            sounds(app, &mut cols[0], &t, &mix);
            links(&mut cols[1], &t, &mix);
        });
    });
    ui.data_mut(|d| d.insert_temp(id, st));
}

fn header(app: &mut App, ui: &mut Ui, t: &Theme, mix: &Value) {
    ui.horizontal_wrapped(|ui| {
        widgets::section(ui, t, icon::MIX, "Audio");
        let pw = mix.get_path("pipewire");
        let state = pw.map(|p| s(p, "state")).unwrap_or("");
        widgets::pill(ui, t, icon::DEVICE, &format!("PipeWire {}", if state.is_empty() { "—" } else { state }), health_led(app.m.get("health.audio.pipewire")));
        let rt = app.m.get("health.audio.rt").cloned();
        let rt_resp =
            widgets::pill(ui, t, icon::PERF, &format!("RT {}", mix.get_path("perf.rt_policy").and_then(Value::as_str).unwrap_or("?")), health_led(rt.as_ref()));
        if let Some(d) = rt.as_ref().and_then(|h| h.get_path("detail")).and_then(Value::as_str) {
            rt_resp.on_hover_text(d);
        }
        let xr = app.m.f("perf.audio.xruns") as i64;
        widgets::pill(ui, t, icon::WARN, &format!("{xr} xruns"), if xr == 0 { LedState::Healthy } else { LedState::Error });
        let load = app.m.f("perf.audio.load");
        widgets::pill(
            ui,
            t,
            icon::GPU,
            &format!("DSP {:.0}% · {:.3} ms", load * 100.0, app.m.f("perf.audio.dsp_ms")),
            if load < 0.5 {
                LedState::Healthy
            } else if load < 0.8 {
                LedState::Armed
            } else {
                LedState::Error
            },
        );
        widgets::pill(
            ui,
            t,
            icon::TIMELINE,
            &format!("{} fr @ {} Hz · {:.2} ms", app.m.f("perf.audio.quantum") as i64, app.m.f("perf.audio.rate") as i64, app.m.f("perf.audio.latency_ms")),
            LedState::Idle,
        );
        let allocs = app.m.f("perf.audio.allocs") as i64;
        if allocs > 0 {
            widgets::pill(ui, t, icon::WARN, &format!("{allocs} RT allocs"), LedState::Error);
        }
        widgets::pill(ui, t, icon::LINK, "OBS", health_led(app.m.get("health.audio.obs")))
            .on_hover_text(app.m.get("health.audio.obs").and_then(|h| h.get_path("detail")).and_then(Value::as_str).unwrap_or(""));
        ui.separator();
        let bpm = app.m.sig("beat.bpm").unwrap_or(0.0);
        if ui.button(format!("{} Tap {:.1} BPM", icon::PLAY, bpm)).on_hover_text("tap tempo (right-click: clear)").clicked() {
            app.m.action("audio.tap", Value::Null);
        } else if ui.ctx().input(|i| i.pointer.secondary_clicked()) && ui.rect_contains_pointer(ui.min_rect()) {
            app.m.action("audio.tap.clear", Value::Null);
        }
        let ducking = app.m.b("audio.duck.active");
        if ui.selectable_label(ducking, format!("{} Duck", icon::MIX)).clicked() {
            app.m.action("audio.duck", Value::map().with("on", !ducking));
        }
        if widgets::hold_button(ui, t, "Audio panic", t.bright_red, 0.6) {
            app.m.action("audio.panic", Value::Null);
        }
    });
}

fn fader_db(app: &mut App, ui: &mut Ui, t: &Theme, addr: &str, size: Vec2) {
    let db = app.m.get(addr).and_then(Value::as_f64).unwrap_or(0.0);
    let mut v = db_to_fader(db);
    if widgets::fader(ui, t, size, &mut v, false).changed() {
        let db = (fader_to_db(v) * 10.0).round() / 10.0;
        app.m.command(Op::Set { address: addr.into(), value: Value::Float(db) });
    }
    let r = ui.label(RichText::new(format!("{db:+.1} dB")).monospace().small());
    if r.double_clicked() {
        app.m.command(Op::Set { address: addr.into(), value: Value::Float(0.0) });
    }
}

fn toggle(app: &mut App, ui: &mut Ui, t: &Theme, addr: &str, label: &str, on_color: Color32) {
    let on = app.m.b(addr);
    let txt = RichText::new(label).small().color(if on { on_color } else { t.fg_dim });
    if ui.selectable_label(on, txt).clicked() {
        app.m.command(Op::Set { address: addr.into(), value: Value::Bool(!on) });
    }
}

fn bus_strip(app: &mut App, ui: &mut Ui, t: &Theme, b: &Value, mix: &Value) {
    let name = s(b, "name").to_string();
    let addr = s(b, "address").to_string();
    let frame = egui::Frame::new().fill(t.bg_dark).corner_radius(CornerRadius::same(6)).inner_margin(6.0);
    frame.show(ui, |ui| {
        ui.set_width(92.0);
        ui.vertical_centered(|ui| {
            ui.label(RichText::new(&name).strong());
            let node = s(b, "node");
            let consumers = mix.get_path("consumers").and_then(|c| c.get_path(node)).and_then(Value::as_list).map(|l| l.len()).unwrap_or(0);
            ui.label(RichText::new(if node.is_empty() { "—".into() } else { node.to_string() }).small().monospace().color(if consumers > 0 {
                t.green
            } else {
                t.fg_dim
            }))
            .on_hover_text(if consumers > 0 { format!("{consumers} consumer(s)") } else { "no consumer (not captured by OBS)".into() });
            ui.horizontal(|ui| {
                let lvl = app.m.sig(&format!("audio.{name}.level")).unwrap_or(0.0);
                let pk = app.m.sig(&format!("audio.{name}.peak")).unwrap_or(0.0);
                widgets::meter(ui, t, Vec2::new(12.0, 150.0), lvl, pk);
                fader_db_col(app, ui, t, &format!("{addr}.gain"));
            });
            let pk = app.m.sig(&format!("audio.{name}.peak")).unwrap_or(0.0);
            ui.label(RichText::new(format!("{:.1} dBFS", lin_db(pk))).small().color(if pk > 0.89 { t.bright_red } else { t.fg_dim }));
            ui.horizontal(|ui| {
                toggle(app, ui, t, &format!("{addr}.mute"), "M", t.bright_red);
                toggle(app, ui, t, &format!("{addr}.limiter"), "L", t.yellow);
            });
            if b.get_path("ducked").is_some_and(Value::truthy) {
                let d = mix.get_path("duck_db").and_then(Value::as_f64).unwrap_or(0.0);
                ui.label(RichText::new(format!("duck {d:.1} dB")).small().color(if d < -0.5 { t.modulated() } else { t.fg_dim }));
            }
        });
    });
}

fn fader_db_col(app: &mut App, ui: &mut Ui, t: &Theme, addr: &str) {
    ui.vertical(|ui| fader_db(app, ui, t, addr, Vec2::new(26.0, 130.0)));
}

fn fx_row(app: &mut App, ui: &mut Ui, t: &Theme, f: &Value, mix: &Value, st: &mut AudioState) {
    let addr = s(f, "address").to_string();
    let active = mix
        .get_path("fx_active")
        .and_then(Value::as_list)
        .unwrap_or(&[])
        .iter()
        .any(|x| s(x, "address") == addr && x.get_path("active").is_some_and(Value::truthy));
    let trigger = f.get_path("trigger").is_some_and(Value::truthy);
    ui.horizontal(|ui| {
        widgets::led(ui, t, if active { LedState::Active } else { LedState::Idle });
        let open = st.open_fx.as_deref() == Some(addr.as_str());
        if ui.selectable_label(open, RichText::new(format!("{} ({})", s(f, "name"), s(f, "kind"))).monospace()).clicked() {
            st.open_fx = if open { None } else { Some(addr.clone()) };
        }
        let lat = f.get_path("latency").and_then(Value::as_i64).unwrap_or(0);
        if lat > 0 {
            ui.label(RichText::new(format!("{lat} smp")).small().color(t.fg_dim));
        }
        let w = s(f, "when");
        if !w.is_empty() {
            ui.label(RichText::new(format!("when {w}")).small().italics().color(t.fg_dim));
        }
        let bypass = app.m.b(&format!("{addr}.bypass"));
        if ui.selectable_label(bypass, RichText::new("bypass").small()).clicked() {
            app.m.command(Op::Set { address: format!("{addr}.bypass"), value: Value::Bool(!bypass) });
        }
        let mut wet = app.m.f(&format!("{addr}.wet")) as f32;
        if ui.add(egui::Slider::new(&mut wet, 0.0..=1.0).text("wet").fixed_decimals(2)).changed() {
            app.m.command(Op::Set { address: format!("{addr}.wet"), value: Value::Float(wet as f64) });
        }
        let mut dry = app.m.f(&format!("{addr}.dry")) as f32;
        if ui.add(egui::Slider::new(&mut dry, 0.0..=1.0).text("dry").fixed_decimals(2)).changed() {
            app.m.command(Op::Set { address: format!("{addr}.dry"), value: Value::Float(dry as f64) });
        }
        if trigger {
            let env = app.m.f(&format!("{addr}.env")) as f32;
            let (rect, _) = ui.allocate_exact_size(Vec2::new(40.0, 8.0), Sense::hover());
            ui.painter().rect_filled(rect, CornerRadius::same(2), t.bg_darker);
            ui.painter().rect_filled(
                Rect::from_min_size(rect.min, Vec2::new(rect.width() * env.clamp(0.0, 1.0), rect.height())),
                CornerRadius::same(2),
                t.modulated(),
            );
            if ui.button(format!("{} fire", icon::PRESET)).clicked() {
                app.m.command(Op::Trigger { address: addr.clone(), payload: Value::Null });
            }
            if ui.button(format!("{} release", icon::STOP)).clicked() {
                app.m.command(Op::Release { address: addr.clone() });
            }
        }
    });
    if st.open_fx.as_deref() == Some(addr.as_str()) {
        ui.indent(("fxp", &addr), |ui| {
            for p in f.get_path("params").and_then(Value::as_list).unwrap_or(&[]) {
                let Some(pn) = p.as_str() else { continue };
                let pa = if pn.starts_with("patch.") { pn.to_string() } else { format!("{addr}.{pn}") };
                param_control(app, ui, &pa);
            }
        });
    }
}

/// A control generated from the address metadata.
fn param_control(app: &mut App, ui: &mut Ui, addr: &str) {
    let meta = app.m.meta.get(addr).cloned();
    let label = addr.rsplit('.').next().unwrap_or(addr).to_string();
    let v = app.m.get(addr).cloned().unwrap_or_default();
    ui.horizontal(|ui| {
        ui.label(RichText::new(&label).monospace());
        match meta.as_ref().map(|m| m.ty) {
            Some(se_proto::ValueType::Enum) => {
                let opts = meta.as_ref().map(|m| m.options.clone()).unwrap_or_default();
                let cur = v.as_str().unwrap_or("").to_string();
                egui::ComboBox::from_id_salt(addr).selected_text(&cur).show_ui(ui, |ui| {
                    for o in opts {
                        if ui.selectable_label(o == cur, &o).clicked() {
                            app.m.command(Op::Set { address: addr.into(), value: Value::Str(o) });
                        }
                    }
                });
            }
            Some(se_proto::ValueType::Bool) => {
                let mut b = v.truthy();
                if ui.checkbox(&mut b, "").changed() {
                    app.m.command(Op::Set { address: addr.into(), value: Value::Bool(b) });
                }
            }
            _ => {
                let [lo, hi] = meta.as_ref().and_then(|m| m.range).unwrap_or([0.0, 1.0]);
                let mut x = v.as_f64().unwrap_or(lo);
                let unit = meta.as_ref().and_then(|m| m.unit.clone()).unwrap_or_default();
                let log = unit == "Hz" && lo > 0.0;
                let r = ui.add(egui::Slider::new(&mut x, lo..=hi).logarithmic(log).suffix(format!(" {unit}")));
                if r.changed() {
                    app.m.command(Op::Set { address: addr.into(), value: Value::Float(x) });
                }
                if let Some(d) = meta.as_ref().and_then(|m| m.description.clone()) {
                    r.on_hover_text(d);
                }
            }
        }
    });
}

fn inputs(app: &mut App, ui: &mut Ui, t: &Theme, mix: &Value) {
    widgets::section(ui, t, icon::DEVICE, "Inputs & sources");
    let mut rows: Vec<(String, String, String)> = Vec::new();
    for i in mix.get_path("inputs").and_then(Value::as_list).unwrap_or(&[]) {
        rows.push((
            s(i, "name").to_string(),
            s(i, "address").to_string(),
            format!("{} ch {:?} → {}", s(i, "target"), i.get_path("channels").map(|c| c.to_string()).unwrap_or_default(), s(i, "bus")),
        ));
    }
    for sl in mix.get_path("slots").and_then(Value::as_list).unwrap_or(&[]) {
        let bus = s(sl, "bus");
        rows.push((s(sl, "name").to_string(), s(sl, "address").to_string(), if bus.is_empty() { "direct".into() } else { format!("→ {bus}") }));
    }
    egui::Grid::new("audio_inputs").num_columns(5).striped(true).show(ui, |ui| {
        for (name, addr, desc) in rows {
            ui.label(RichText::new(&name).strong()).on_hover_text(&desc);
            let lvl = app.m.sig(&format!("{addr}.level")).unwrap_or(0.0);
            let pk = app.m.sig(&format!("{addr}.peak")).unwrap_or(0.0);
            ui.horizontal(|ui| {
                let (rect, _) = ui.allocate_exact_size(Vec2::new(90.0, 8.0), Sense::hover());
                let x = ((lin_db(lvl) + 60.0) / 60.0).clamp(0.0, 1.0);
                let px = ((lin_db(pk) + 60.0) / 60.0).clamp(0.0, 1.0);
                let p = ui.painter();
                p.rect_filled(rect, CornerRadius::same(2), t.bg_darker);
                p.rect_filled(
                    Rect::from_min_size(rect.min, Vec2::new(rect.width() * x, rect.height())),
                    CornerRadius::same(2),
                    if pk > 0.89 { t.bright_red } else { t.green },
                );
                let xp = rect.left() + rect.width() * px;
                p.line_segment([Pos2::new(xp, rect.top()), Pos2::new(xp, rect.bottom())], Stroke::new(1.5, t.fg));
            });
            let gain = format!("{addr}.gain");
            let mut g = app.m.f(&gain) as f32;
            if ui.add(egui::DragValue::new(&mut g).range(-60.0..=24.0).speed(0.1).suffix(" dB")).changed() {
                app.m.command(Op::Set { address: gain, value: Value::Float(g as f64) });
            }
            let delay = format!("{addr}.delay_ms");
            let mut d = app.m.f(&delay) as f32;
            if ui.add(egui::DragValue::new(&mut d).range(0.0..=2000.0).speed(1.0).suffix(" ms")).on_hover_text("A/V delay").changed() {
                app.m.command(Op::Set { address: delay, value: Value::Float(d as f64) });
            }
            toggle(app, ui, t, &format!("{addr}.mute"), "M", t.bright_red);
            ui.end_row();
        }
    });
}

fn ducking(app: &mut App, ui: &mut Ui, t: &Theme, mix: &Value) {
    widgets::section(ui, t, icon::MIX, "Ducking");
    let d = mix.get_path("duck").cloned().unwrap_or_default();
    let list = |k: &str| d.get_path(k).and_then(Value::as_list).map(|l| l.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ")).unwrap_or_default();
    ui.label(RichText::new(format!("{} duck under {} {}", list("targets"), list("keys"), list("signals"))).small().color(t.fg_dim));
    ui.horizontal(|ui| {
        toggle(app, ui, t, "audio.duck.active", "manual duck", t.modulated());
        let amt = app.m.sig("audio.duck.amount").unwrap_or(0.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(120.0, 10.0), Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(2), t.bg_darker);
        ui.painter().rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * amt, rect.height())), CornerRadius::same(2), t.modulated());
        let talking = app.m.sig("mic.talking").unwrap_or(0.0) > 0.5;
        widgets::led(ui, t, if talking { LedState::Active } else { LedState::Idle });
        ui.label(RichText::new("mic.talking").small());
    });
    for (a, lo, hi, unit) in [("audio.duck.depth", -60.0, 0.0, " dB"), ("audio.duck.attack", 1.0, 2000.0, " ms"), ("audio.duck.release", 1.0, 5000.0, " ms")] {
        let mut v = app.m.f(a);
        ui.horizontal(|ui| {
            ui.label(RichText::new(a.rsplit('.').next().unwrap_or(a)).monospace());
            if ui.add(egui::Slider::new(&mut v, lo..=hi).suffix(unit)).changed() {
                app.m.command(Op::Set { address: a.into(), value: Value::Float(v) });
            }
        });
    }
}

fn analysis(app: &mut App, ui: &mut Ui, t: &Theme, mix: &Value) {
    widgets::section(ui, t, icon::SCOPE, "Analysis");
    let src = mix.get_path("analysis.beat.source").and_then(Value::as_str).unwrap_or("—").to_string();
    ui.horizontal(|ui| {
        let phase = app.m.sig("beat.phase").unwrap_or(0.0);
        let (rect, _) = ui.allocate_exact_size(Vec2::new(24.0, 24.0), Sense::hover());
        let p = ui.painter();
        p.circle_stroke(rect.center(), 10.0, Stroke::new(1.5, t.fg_dim));
        let a = phase * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
        p.line_segment([rect.center(), rect.center() + Vec2::new(a.cos(), a.sin()) * 10.0], Stroke::new(2.0, t.accent));
        ui.label(
            RichText::new(format!("{:.1} BPM  conf {:.2}  source {src}", app.m.sig("beat.bpm").unwrap_or(0.0), app.m.sig("beat.confidence").unwrap_or(0.0)))
                .monospace(),
        );
    });
    ui.columns(2, |cols| {
        for (ui, prefix) in cols.iter_mut().zip(["band", "music"]) {
            ui.label(RichText::new(prefix).strong());
            ui.horizontal(|ui| {
                for k in ["kick", "snare", "hat"] {
                    let v = app.m.sig(&format!("{prefix}.{k}")).unwrap_or(0.0);
                    widgets::led(ui, t, if v > 0.3 { LedState::Active } else { LedState::Idle });
                    ui.label(RichText::new(k).small());
                }
                ui.label(RichText::new(format!("{:.1} LUFS", app.m.sig(&format!("{prefix}.lufs")).unwrap_or(-70.0))).small().monospace());
            });
            for k in ["bass", "mid", "high"] {
                let v = app.m.sig(&format!("{prefix}.{k}")).unwrap_or(0.0);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("{k:>4}")).small().monospace());
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(140.0, 6.0), Sense::hover());
                    let x = ((lin_db(v) + 60.0) / 60.0).clamp(0.0, 1.0);
                    ui.painter().rect_filled(rect, CornerRadius::same(2), t.bg_darker);
                    ui.painter().rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * x, rect.height())), CornerRadius::same(2), t.accent);
                });
            }
            // 1/3-octave spectrum
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width().min(320.0), 60.0), Sense::hover());
            let p = ui.painter_at(rect);
            p.rect_filled(rect, CornerRadius::same(3), t.bg_darker);
            let n = 31;
            let w = rect.width() / n as f32;
            for i in 0..n {
                let v = app.m.sig(&format!("{prefix}.b.{i}")).unwrap_or(0.0);
                let h = ((lin_db(v) + 70.0) / 70.0).clamp(0.0, 1.0) * rect.height();
                let r = Rect::from_min_max(
                    Pos2::new(rect.left() + i as f32 * w + 1.0, rect.bottom() - h),
                    Pos2::new(rect.left() + (i + 1) as f32 * w - 1.0, rect.bottom()),
                );
                p.rect_filled(r, CornerRadius::same(1), t.blue);
            }
        }
    });
}

fn sounds(app: &mut App, ui: &mut Ui, t: &Theme, mix: &Value) {
    widgets::section(ui, t, icon::PLAY, "Sounds");
    ui.horizontal_wrapped(|ui| {
        for snd in mix.get_path("sounds").and_then(Value::as_list).unwrap_or(&[]) {
            if let Some(n) = snd.as_str()
                && ui.button(format!("{} {n}", icon::PLAY)).clicked()
            {
                app.m.action("audio.play", Value::map().with("sound", n));
            }
        }
        if ui.button(format!("{} stop", icon::STOP)).clicked() {
            app.m.action("audio.stop", Value::Null);
        }
    });
    let pads = mix.get_path("pads").and_then(Value::as_list).unwrap_or(&[]);
    if !pads.is_empty() {
        ui.label(RichText::new("drum pads").strong());
        ui.horizontal_wrapped(|ui| {
            for p in pads {
                let Some(n) = p.as_str() else { continue };
                let v = app.m.sig(&format!("drums.{n}")).unwrap_or(0.0);
                widgets::led(ui, t, if v > 0.0 { LedState::Active } else { LedState::Idle });
                let a = format!("audio.drums.{n}.threshold");
                let mut th = app.m.f(&a) as f32;
                ui.label(n);
                if ui.add(egui::DragValue::new(&mut th).range(-80.0..=0.0).suffix(" dB")).changed() {
                    app.m.command(Op::Set { address: a, value: Value::Float(th as f64) });
                }
            }
        });
    }
}

fn links(ui: &mut Ui, t: &Theme, mix: &Value) {
    widgets::section(ui, t, icon::LINK, "PipeWire links");
    for l in mix.get_path("links").and_then(Value::as_list).unwrap_or(&[]) {
        ui.horizontal(|ui| {
            let ok = l.get_path("ok").is_some_and(Value::truthy);
            widgets::led(ui, t, if ok { LedState::Healthy } else { LedState::Error });
            ui.label(RichText::new(s(l, "port")).monospace().small());
            ui.label(RichText::new(s(l, "target")).small().color(t.fg_dim));
        });
    }
    for e in mix.get_path("errors").and_then(Value::as_list).unwrap_or(&[]) {
        ui.label(RichText::new(format!("{} {}", icon::WARN, e.as_str().unwrap_or(""))).color(t.bright_red));
    }
}
