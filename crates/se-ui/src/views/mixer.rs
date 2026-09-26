//! Mixer view (§8.7, §15.6): the StudioLive 16R as the engine sees it. Channel strips for the
//! main mix or any aux/FX send mix, bus masters, meters, mix snapshots (store / recall /
//! crossfade), panic, and MIDI mapping hints. Faders only read state, so they move with the
//! console, UC Surface, MIDI, and presets live; dragging sends `set` (the adapter rate-limits).
//!
//! Data: `mixer.16r.*` state, signals `mixer.16r.meter.*`, queries `mixer.status`,
//! `mixer.coverage` (writable controls), `mixer.snapshots`, `bindings`; state
//! `controllers.learn.{active,target}`. Actions: `mixer.snapshot.{store,recall}`,
//! `mixer.panic`, `mixer.reconnect`, `midi.learn {target}`.

use crate::app::App;
use egui::{RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};
use std::collections::{BTreeMap, HashSet};

const P: &str = "mixer.16r";
/// 0 dB on the console's fader law (reference `logVolumeToLinear(0)` / 100).
const UNITY: f64 = 0.7252;

#[derive(Clone, Default)]
struct Form {
    /// "main", "aux.N", or "fx.N".
    mix: String,
    store_name: String,
    store_label: String,
    fades: BTreeMap<String, f64>,
    last_status: f64,
    last_lists: f64,
    discover: bool,
}

fn meter_pos(level: f32) -> f32 {
    // dBFS over a 60 dB window: linear amplitude would hide everything below −20 dB
    if level <= 1e-6 { 0.0 } else { ((20.0 * level.log10() + 60.0) / 60.0).clamp(0.0, 1.0) }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

struct Strip {
    /// Address segment below the mixer prefix (`ch.3`, `ret.1`, `fxret.2`, `tb`).
    seg: String,
    label: String,
    meter: Option<String>,
}

fn strips(app: &App) -> Vec<Strip> {
    let mut v = Vec::new();
    let n = app.m.get(&format!("{P}.channels")).and_then(Value::as_i64).unwrap_or(0).max(0);
    for c in 1..=n {
        let name = app.m.str(&format!("{P}.ch.{c}.name"));
        v.push(Strip {
            seg: format!("ch.{c}"),
            label: if name.is_empty() { format!("{c}") } else { name.to_string() },
            meter: Some(format!("{P}.meter.ch.{c}")),
        });
    }
    for (kind, label) in [("ret", "Ret"), ("fxret", "FX Ret")] {
        for c in 1.. {
            if !app.m.has(&format!("{P}.{kind}.{c}.fader")) {
                break;
            }
            let name = app.m.str(&format!("{P}.{kind}.{c}.name"));
            v.push(Strip { seg: format!("{kind}.{c}"), label: if name.is_empty() { format!("{label} {c}") } else { name.to_string() }, meter: None });
        }
    }
    if app.m.has(&format!("{P}.tb.fader")) {
        v.push(Strip { seg: "tb".into(), label: "Talkback".into(), meter: None });
    }
    v
}

fn buses(app: &App, kind: &str) -> Vec<(i64, String)> {
    let n = app.m.get(&format!("{P}.{}", if kind == "aux" { "auxes" } else { "fxes" })).and_then(Value::as_i64).unwrap_or(0).max(0);
    (1..=n)
        .map(|b| {
            let name = app.m.str(&format!("{P}.{kind}.{b}.name"));
            (b, if name.is_empty() { format!("{} {b}", if kind == "aux" { "Aux" } else { "FX" }) } else { name.to_string() })
        })
        .collect()
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("mixer-view");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    if form.mix.is_empty() {
        form.mix = "main".into();
    }
    let now = ui.input(|i| i.time);
    if app.m.connected && now - form.last_status > 1.0 {
        form.last_status = now;
        app.m.query("mixer.status", Value::Null);
    }
    if app.m.connected && now - form.last_lists > 3.0 {
        form.last_lists = now;
        app.m.query("mixer.coverage", Value::Null);
        app.m.query("mixer.snapshots", Value::Null);
        app.m.query("bindings", Value::Null);
    }

    header(app, ui, &mut form);
    if !app.m.b(&format!("{P}.connected")) {
        ui.data_mut(|d| d.insert_temp(id, form));
        return;
    }
    ui.add_space(4.0);

    let writable: HashSet<String> =
        app.m.q_list("mixer.coverage").iter().filter(|c| c.get_path("writable").is_some_and(Value::truthy)).map(|c| s(c, "address").to_string()).collect();
    // binding target → signal, for MIDI hints
    let mut bound: BTreeMap<String, String> = BTreeMap::new();
    for b in app.m.q_list("bindings") {
        let target = s(b, "target");
        if target.starts_with(P) {
            bound.insert(target.to_string(), s(b, "signal").to_string());
        }
    }

    // mix selector
    ui.horizontal_wrapped(|ui| {
        widgets::section(ui, &t, icon::MIX, "Mix");
        if ui.selectable_label(form.mix == "main", "Main").clicked() {
            form.mix = "main".into();
        }
        for (b, name) in buses(app, "aux") {
            let key = format!("aux.{b}");
            if ui.selectable_label(form.mix == key, name).on_hover_text(format!("sends to aux {b} (monitor mix)")).clicked() {
                form.mix = key;
            }
        }
        for (b, name) in buses(app, "fx") {
            let key = format!("fx.{b}");
            if ui.selectable_label(form.mix == key, name).on_hover_text(format!("sends to FX bus {b}")).clicked() {
                form.mix = key;
            }
        }
    });

    let h = (ui.available_height() * 0.55).clamp(120.0, 360.0);
    let mix = form.mix.clone();
    egui::ScrollArea::horizontal().id_salt("mixer-strips").show(ui, |ui| {
        ui.horizontal(|ui| {
            for st in strips(app) {
                let level_addr = if mix == "main" { format!("{P}.{}.fader", st.seg) } else { format!("{P}.{mix}.{}.send", st.seg) };
                if !app.m.has(&level_addr) {
                    continue;
                }
                strip_ui(app, ui, &st.label, &level_addr, &format!("{P}.{}.mute", st.seg), st.meter.as_deref(), h, &writable, &bound, mix == "main");
            }
            ui.separator();
            let (label, master, mute, meter) = match mix.as_str() {
                "main" => ("Main".to_string(), format!("{P}.main.fader"), format!("{P}.main.mute"), Some(format!("{P}.meter.main"))),
                m => {
                    let name = app.m.str(&format!("{P}.{m}.name")).to_string();
                    (if name.is_empty() { m.to_string() } else { name }, format!("{P}.{m}.fader"), format!("{P}.{m}.mute"), Some(format!("{P}.meter.{m}")))
                }
            };
            if app.m.has(&master) {
                strip_ui(app, ui, &label, &master, &mute, meter.as_deref(), h, &writable, &bound, true);
            }
        });
    });
    if let Some(fx) = mix.strip_prefix("fx.") {
        fx_params(app, ui, fx);
    }

    ui.add_space(6.0);
    ui.columns(2, |cols| {
        snapshots_ui(app, &mut cols[0], &mut form);
        midi_hints(app, &mut cols[1], &bound);
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}

fn header(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    let st = app.m.q("mixer.status").cloned().unwrap_or_default();
    let connected = app.m.b(&format!("{P}.connected"));
    let health = app.m.get("health.mixer").cloned().unwrap_or_default();
    let hs = s(&health, "status");
    let led = match (connected, hs) {
        (true, "pass") => LedState::Healthy,
        (true, _) => LedState::Armed,
        (false, _) if app.m.has(&format!("{P}.connected")) => LedState::Error,
        _ => LedState::Idle,
    };
    ui.horizontal_wrapped(|ui| {
        widgets::led(ui, &t, led);
        let title = if connected {
            format!("{} · fw {} · {}", app.m.str(&format!("{P}.model")), app.m.str(&format!("{P}.firmware")), app.m.str(&format!("{P}.host")))
        } else {
            "StudioLive 16R — not connected".into()
        };
        ui.label(RichText::new(title).strong());
        if connected && !st.get_path("meters").is_some_and(Value::truthy) {
            ui.label(RichText::new("no meters").color(t.yellow));
        }
        if app.m.sig("mic.talking").is_some_and(|v| v > 0.5) {
            widgets::pill(ui, &t, icon::LIVE, "talking", LedState::Active);
        }
        if ui.button("Reconnect").on_hover_text("drop the session and rediscover").clicked() {
            app.m.action("mixer.reconnect", Value::Null);
        }
        if ui.button("Scan LAN").on_hover_text("listen for console broadcasts, then probe TCP 53000").clicked() {
            form.discover = true;
            app.m.query("mixer.discover", Value::map().with("seconds", 4));
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::hold_button(ui, &t, "Safe mix", t.bright_red, 0.8) {
                app.m.action("mixer.panic", Value::Null);
            }
        });
    });
    let detail = s(&health, "detail");
    if !detail.is_empty() && hs != "pass" {
        ui.label(RichText::new(detail).small().color(if hs == "fail" { t.bright_red } else { t.yellow }));
    }
    if form.discover
        && let Some(d) = app.m.q("mixer.discover")
    {
        ui.horizontal_wrapped(|ui| {
            let list = d.get_path("devices").and_then(Value::as_list).unwrap_or(&[]);
            if list.is_empty() {
                ui.label(RichText::new("no console answered").color(t.fg_dim));
            }
            for dev in list {
                ui.label(RichText::new(format!("{} {} ({}, via {})", s(dev, "model"), s(dev, "host"), s(dev, "serial"), s(dev, "via"))).monospace());
            }
            if let Some(h) = d.get_path("firewall_hint").and_then(Value::as_str) {
                ui.label(RichText::new(format!("broadcasts blocked? {h}")).small().color(t.fg_dim));
            }
            if ui.small_button("hide").clicked() {
                form.discover = false;
            }
        });
    }
}

#[allow(clippy::too_many_arguments)]
fn strip_ui(
    app: &mut App,
    ui: &mut egui::Ui,
    label: &str,
    level_addr: &str,
    mute_addr: &str,
    meter: Option<&str>,
    h: f32,
    writable: &HashSet<String>,
    bound: &BTreeMap<String, String>,
    show_mute: bool,
) {
    let t = app.t.clone();
    let muted = app.m.b(mute_addr);
    let can_level = writable.contains(level_addr);
    let can_mute = writable.contains(mute_addr);
    let value = app.m.f(level_addr);
    let modulated = app.is_modulated(level_addr);
    let learning = app.m.b("controllers.learn.active") && app.m.str("controllers.learn.target") == level_addr;
    ui.vertical(|ui| {
        ui.set_width(52.0);
        let short: String = label.chars().take(7).collect();
        ui.label(RichText::new(short).small().strong().color(if muted { t.fg_dim } else { t.fg })).on_hover_text(level_addr);
        ui.horizontal(|ui| {
            let level = meter.and_then(|m| app.m.sig(m)).unwrap_or(0.0);
            widgets::meter(ui, &t, Vec2::new(8.0, h), meter_pos(level), meter_pos(level));
            let mut pos = value as f32;
            let r = ui.add_enabled_ui(can_level, |ui| widgets::fader(ui, &t, Vec2::new(20.0, h), &mut pos, modulated)).inner;
            if r.changed() {
                app.m.command(Op::Set { address: level_addr.to_string(), value: Value::Float(pos as f64) });
            }
            if r.double_clicked() && can_level {
                app.m.command(Op::Set { address: level_addr.to_string(), value: Value::Float(UNITY) });
            }
            let hover = if can_level { "drag: set · double-click: 0 dB" } else { "readonly ([mixer] writable)" };
            r.on_hover_text(hover);
        });
        ui.horizontal(|ui| {
            if show_mute && app.m.has(mute_addr) {
                let m =
                    ui.add_enabled(can_mute, egui::Button::new(RichText::new("M").small().color(if muted { t.bright_red } else { t.fg_dim })).selected(muted));
                if m.clicked() {
                    app.m.command(Op::Set { address: mute_addr.to_string(), value: Value::Bool(!muted) });
                }
            }
            let db_addr = level_addr.strip_suffix(".fader").map(|b| format!("{b}.db"));
            let txt = match db_addr.and_then(|a| app.m.get(&a).and_then(Value::as_f64)) {
                Some(db) if db <= -100.0 => "-∞".to_string(),
                Some(db) => format!("{db:+.1}"),
                None => format!("{:.0}%", value * 100.0),
            };
            ui.label(RichText::new(txt).small().monospace().color(t.fg_dim));
        });
        // MIDI mapping hint
        match bound.get(level_addr) {
            Some(sig) => {
                let short = sig.strip_prefix("midi.").unwrap_or(sig);
                ui.label(RichText::new(format!("{} {short}", icon::CONTROLLER)).small().color(t.accent)).on_hover_text(format!("bound to {sig}"));
            }
            None if learning => {
                ui.label(RichText::new("move a control…").small().color(t.yellow));
            }
            None if can_level
                && ui
                    .small_button(RichText::new(format!("{} learn", icon::CONTROLLER)).small())
                    .on_hover_text("MIDI learn: move a fader/encoder to bind it here")
                    .clicked() =>
            {
                app.m.action("midi.learn", Value::map().with("target", level_addr));
            }
            None => {}
        }
    });
}

fn fx_params(app: &mut App, ui: &mut egui::Ui, fx: &str) {
    let t = app.t.clone();
    let ty = app.m.str(&format!("{P}.fx.{fx}.type")).to_string();
    let params: Vec<(String, f64)> =
        app.m.under(&format!("{P}.fx.{fx}.param")).filter_map(|(a, v)| Some((a.rsplit('.').next()?.to_string(), v.as_f64()?))).collect();
    if ty.is_empty() && params.is_empty() {
        return;
    }
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new(format!("FX {fx}: {ty}")).strong());
        for (k, v) in params {
            ui.label(RichText::new(format!("{k} {:.0}%", v * 100.0)).small().color(t.fg_dim));
        }
        ui.label(RichText::new("(readback — edit FX on the console or UC Surface)").small().color(t.fg_dim));
    });
}

fn snapshots_ui(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    widgets::card(ui, &t, icon::PRESET, "Mix snapshots", LedState::Idle, |ui| {
        let q = app.m.q("mixer.snapshots").cloned().unwrap_or_default();
        let list = q.get_path("snapshots").and_then(Value::as_list).unwrap_or(&[]).to_vec();
        if list.is_empty() {
            ui.label(RichText::new("No snapshots yet (mixes/*.toml). Store the current mix below; `safe` is what Panic recalls.").color(t.fg_dim));
        }
        for snap in &list {
            let name = s(snap, "name").to_string();
            let default_fade = snap.get_path("fade_ms").and_then(Value::as_f64).unwrap_or(0.0) / 1000.0;
            let fade = form.fades.entry(name.clone()).or_insert(if default_fade > 0.0 { default_fade } else { 2.0 });
            ui.horizontal(|ui| {
                ui.label(RichText::new(s(snap, "label")).strong())
                    .on_hover_text(format!("mixes/{name}.toml · {} values", snap.get_path("values").and_then(Value::as_i64).unwrap_or(0)));
                if ui.button("Recall").on_hover_text("instant").clicked() {
                    app.m.action("mixer.snapshot.recall", Value::map().with("snapshot", name.clone()).with("fade", 0));
                }
                ui.add(egui::DragValue::new(fade).range(0.1..=60.0).speed(0.1).suffix(" s"));
                if ui.button("Crossfade").clicked() {
                    app.m.action("mixer.snapshot.recall", Value::map().with("snapshot", name.clone()).with("fade", (*fade * 1000.0).round() as i64));
                }
            });
        }
        for e in q.get_path("errors").and_then(Value::as_list).unwrap_or(&[]) {
            ui.label(RichText::new(e.as_str().unwrap_or("")).small().color(t.bright_red));
        }
        ui.separator();
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut form.store_name).hint_text("name (e.g. brb, safe)").desired_width(120.0));
            ui.add(egui::TextEdit::singleline(&mut form.store_label).hint_text("label").desired_width(90.0));
            let ok = !form.store_name.is_empty() && form.store_name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if ui.add_enabled(ok, egui::Button::new("Store current")).on_hover_text("channel faders, mutes, pans and sends (not output levels)").clicked() {
                let mut args = Value::map().with("snapshot", form.store_name.clone());
                if !form.store_label.is_empty() {
                    args = args.with("label", form.store_label.clone());
                }
                app.m.action("mixer.snapshot.store", args);
                form.store_name.clear();
                form.store_label.clear();
                form.last_lists = 0.0;
            }
        });
        ui.label(RichText::new("Presets recall with `mix = { snapshot = \"brb\", fade = \"2s\" }`.").small().color(t.fg_dim));
    });
}

fn midi_hints(app: &mut App, ui: &mut egui::Ui, bound: &BTreeMap<String, String>) {
    let t = app.t.clone();
    widgets::card(ui, &t, icon::CONTROLLER, "MIDI → 16R", LedState::Idle, |ui| {
        if bound.is_empty() {
            ui.label(RichText::new("No controller is bound to the mixer yet.").color(t.fg_dim));
        }
        for (target, sig) in bound {
            ui.horizontal(|ui| {
                ui.label(RichText::new(sig).monospace().color(t.accent));
                ui.label("→");
                ui.label(RichText::new(target.strip_prefix(&format!("{P}.")).unwrap_or(target)).monospace());
            });
        }
        ui.label(
            RichText::new(
                "Use `learn` under a fader, or add to controllers/faders.toml:\n[[binding]]\ntarget = \"mixer.16r.ch.3.fader\"\nsignal = \"midi.xtouch.fader.3\"\ntakeover = \"pickup\"   # no jumps on non-motorized faders\nfeedback = true\nFader values are console positions (0.72 ≈ 0 dB), so keep the curve linear; sends are ≤ 50 Hz per control.",
            )
            .small()
            .monospace()
            .color(t.fg_dim),
        );
    });
}
