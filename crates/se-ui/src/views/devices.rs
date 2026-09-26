//! Devices view (§15.6): expected vs present devices with stable identities, camera sources
//! (signal, measured fps, drops, CPU), camera modes, and live camera-control editing. Pure
//! client of the `devices` / `sources` queries, `devices.*` / `source.*` state, and the
//! `devices.*` / `source.*` actions.

use crate::app::App;
use crate::model::Model;
use egui::{RichText, Ui};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::widgets::{self, LedState, icon};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const REFRESH_EVERY: Duration = Duration::from_secs(1);
const AFTER_ACTION: Duration = Duration::from_millis(250);
/// A value being dragged locally wins over the engine echo for this long.
const EDIT_HOLD: Duration = Duration::from_millis(1200);
const CAMERA: &str = "\u{f030}";
const KINDS: [(&str, &str); 6] =
    [("camera", "Cameras"), ("audio_node", "PipeWire audio"), ("audio_card", "Sound cards"), ("midi", "MIDI"), ("hid", "HID"), ("serial", "USB serial")];

#[derive(Clone, Default)]
struct DevicesState {
    refreshed: Option<Instant>,
    /// address → (value, when edited)
    edits: HashMap<String, (Value, Instant)>,
    /// expected id → label being typed
    renames: HashMap<String, String>,
    filter: String,
    show_all_kinds: bool,
}

impl DevicesState {
    fn refresh(&mut self, m: &mut Model, now: Instant) {
        if m.connected && self.refreshed.is_none_or(|t| now.duration_since(t) >= REFRESH_EVERY) {
            self.refreshed = Some(now);
            m.query("devices", Value::Null);
            m.query("sources", Value::Null);
        }
        self.edits.retain(|_, (_, at)| now.duration_since(*at) < EDIT_HOLD);
    }
    fn soon(&mut self, now: Instant) {
        self.refreshed = Some(now - REFRESH_EVERY + AFTER_ACTION);
    }
}

pub fn ui(app: &mut App, ui: &mut Ui) {
    let id = ui.make_persistent_id("devices_view");
    let now = Instant::now();
    let mut st = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<DevicesState>(id)));
    st.refresh(&mut app.m, now);
    let t = app.t.clone();
    let mut out = Vec::new();
    view(&app.m, &t, &mut st, ui, &mut out, now);
    if !out.is_empty() {
        st.soon(now);
    }
    ui.data_mut(|d| d.insert_temp(id, st));
    for op in out {
        app.m.command(op);
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}
fn b(v: &Value, k: &str) -> bool {
    v.get_path(k).is_some_and(Value::truthy)
}
fn f(v: &Value, k: &str) -> f64 {
    v.get_path(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn health_led(status: &str) -> LedState {
    match status {
        "pass" => LedState::Healthy,
        "warn" => LedState::Armed,
        "fail" => LedState::Error,
        _ => LedState::Idle,
    }
}

fn action(name: &str, args: Value) -> Op {
    Op::Action { name: name.into(), args }
}

fn view(m: &Model, t: &Theme, st: &mut DevicesState, ui: &mut Ui, out: &mut Vec<Op>, now: Instant) {
    ui.horizontal(|ui| {
        widgets::section(ui, t, icon::DEVICE, "Devices");
        for (label, addr) in [("devices", "health.devices"), ("sources", "health.sources")] {
            let h = m.get(addr).cloned().unwrap_or_default();
            widgets::pill(
                ui,
                t,
                if label == "devices" { icon::DEVICE } else { CAMERA },
                &format!("{label}: {}", s(&h, "status").to_uppercase()),
                health_led(s(&h, "status")),
            )
            .on_hover_text(s(&h, "detail"));
        }
        let connected16 = m.b("mixer.16r.connected");
        widgets::pill(
            ui,
            t,
            icon::MIX,
            if connected16 {
                "16R connected"
            } else if m.has("mixer.16r.connected") {
                "16R disconnected"
            } else {
                "16R not configured"
            },
            if connected16 { LedState::Healthy } else { LedState::Idle },
        );
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(format!("{} rescan", icon::SEARCH)).on_hover_text("re-enumerate udev devices").clicked() {
                out.push(action("devices.rescan", Value::Null));
            }
            ui.add(egui::TextEdit::singleline(&mut st.filter).hint_text("filter").desired_width(160.0));
        });
    });
    let Some(reg) = m.q("devices") else {
        ui.label(RichText::new(if m.connected { "loading devices…" } else { "engine offline" }).color(t.fg_dim));
        return;
    };
    let devices: Vec<Value> = reg.get_path("devices").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let expected: Vec<Value> = reg.get_path("expected").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let sources: Vec<Value> = m.q_list("sources").to_vec();
    let flt = st.filter.to_lowercase();
    let matches = |v: &Value| flt.is_empty() || ["id", "name", "label", "identity", "path"].iter().any(|k| s(v, k).to_lowercase().contains(&flt));

    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        // --- expected -------------------------------------------------------------------
        widgets::section(ui, t, icon::CHECK, "Expected (project.toml [devices.expected])");
        egui::Grid::new("dev_expected").striped(true).num_columns(5).spacing([12.0, 4.0]).show(ui, |ui| {
            for e in expected.iter().filter(|e| matches(e)) {
                let eid = s(e, "id").to_string();
                let present = b(e, "present");
                ui.horizontal(|ui| {
                    widgets::led(
                        ui,
                        t,
                        if present {
                            LedState::Healthy
                        } else if b(e, "optional") {
                            LedState::Armed
                        } else {
                            LedState::Error
                        },
                    );
                    ui.label(RichText::new(&eid).monospace());
                });
                let label = st.renames.entry(eid.clone()).or_insert_with(|| s(e, "label").to_string());
                let r = ui.add(egui::TextEdit::singleline(label).desired_width(220.0));
                let changed = label.as_str() != s(e, "label");
                if changed && (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) || ui.small_button("rename").clicked()) {
                    out.push(action("devices.rename", Value::map().with("id", eid.as_str()).with("label", label.as_str())));
                }
                if !r.has_focus() && !changed {
                    *label = s(e, "label").to_string();
                }
                ui.label(RichText::new(s(e, "kind")).color(t.fg_dim));
                ui.label(RichText::new(if present { "present" } else { "MISSING" }).color(if present { t.green } else { t.bright_red }));
                ui.label(RichText::new(s(e, "identity")).monospace().small().color(t.fg_dim));
                ui.end_row();
            }
        });
        if expected.is_empty() {
            ui.label(RichText::new("No expected devices declared — use “expect” on a device below.").color(t.fg_dim));
        }
        ui.add_space(8.0);

        // --- camera sources ---------------------------------------------------------------
        widgets::section(ui, t, CAMERA, "Camera & media sources");
        ui.horizontal_wrapped(|ui| {
            for src in sources.iter().filter(|v| matches(v)) {
                source_card(m, t, st, ui, src, &devices, out, now);
            }
        });
        if sources.is_empty() {
            ui.label(RichText::new("No sources (add sources/<name>.toml).").color(t.fg_dim));
        }
        ui.add_space(8.0);

        // --- registry -----------------------------------------------------------------------
        ui.horizontal(|ui| {
            widgets::section(ui, t, icon::DEVICE, "Connected devices");
            ui.checkbox(&mut st.show_all_kinds, "include HID/sound cards");
        });
        let source_names: Vec<String> = sources.iter().filter(|v| s(v, "kind") == "camera").map(|v| s(v, "name").to_string()).collect();
        for (kind, title) in KINDS {
            if !st.show_all_kinds && matches!(kind, "hid" | "audio_card") {
                continue;
            }
            let list: Vec<&Value> = devices.iter().filter(|d| s(d, "kind") == kind && matches(d)).collect();
            if list.is_empty() {
                continue;
            }
            egui::CollapsingHeader::new(format!("{title} ({})", list.len())).id_salt(("dev_kind", kind)).default_open(kind == "camera").show(ui, |ui| {
                for d in list {
                    device_row(t, ui, d, &source_names, out);
                }
            });
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn source_card(m: &Model, t: &Theme, st: &mut DevicesState, ui: &mut Ui, src: &Value, devices: &[Value], out: &mut Vec<Op>, now: Instant) {
    let name = s(src, "name").to_string();
    let a = |k: &str| format!("source.{name}.{k}");
    let signal = m.get(&a("signal")).map(Value::truthy).unwrap_or(b(src, "signal"));
    let capturing = m.get(&a("capturing")).map(Value::truthy).unwrap_or(b(src, "capturing"));
    let used = b(src, "used");
    let state = if !used {
        LedState::Idle
    } else if b(src, "missing") || !s(src, "error").is_empty() && !capturing {
        LedState::Error
    } else if signal {
        LedState::Healthy
    } else {
        LedState::Armed
    };
    ui.allocate_ui(egui::vec2(420.0, 10.0), |ui| {
        widgets::card(ui, t, if s(src, "kind") == "file" { icon::PLAY } else { CAMERA }, &format!("{name} — {}", s(src, "label")), state, |ui| {
            let fps = m.get(&a("fps")).and_then(Value::as_f64).unwrap_or(f(src, "measured_fps"));
            let dropped = m.get(&a("dropped")).and_then(Value::as_i64).unwrap_or(f(src, "dropped") as i64);
            let cpu = m.get(&a("cpu")).and_then(Value::as_f64).unwrap_or(f(src, "cpu"));
            ui.horizontal(|ui| {
                ui.label(
                    RichText::new(if !used {
                        "not in use"
                    } else if !capturing {
                        "not capturing"
                    } else if signal {
                        "LIVE"
                    } else {
                        "NO SIGNAL"
                    })
                    .strong()
                    .color(match state {
                        LedState::Healthy => t.green,
                        LedState::Error => t.bright_red,
                        LedState::Armed => t.yellow,
                        _ => t.fg_dim,
                    }),
                );
                ui.label(format!("{fps:.2} / {:.0} fps", f(src, "fps")));
                ui.label(RichText::new(format!("drop {dropped}")).color(if dropped > 0 { t.yellow } else { t.fg_dim }));
                ui.label(RichText::new(format!("cpu {cpu:.1}%")).color(t.fg_dim));
            });
            let size = src
                .get_path("size")
                .and_then(Value::as_list)
                .map(|l| format!("{}x{}", l.first().and_then(Value::as_i64).unwrap_or(0), l.get(1).and_then(Value::as_i64).unwrap_or(0)))
                .unwrap_or_else(|| format!("{}x{}", f(src, "width"), f(src, "height")));
            ui.label(
                RichText::new(format!(
                    "{} {size} → slot {} {}{}",
                    s(src, "format"),
                    s(src, "slot_format"),
                    s(src, "decoder"),
                    if s(src, "matrix").is_empty() { String::new() } else { format!(" ({} {})", s(src, "matrix"), s(src, "range")) }
                ))
                .small()
                .color(t.fg_dim),
            );
            ui.label(RichText::new(format!("{}  {}", s(src, "device"), s(src, "path"))).monospace().small().color(t.fg_dim))
                .on_hover_text("configured device (stable identity) and the node it resolved to");
            for k in ["error", "config_error"] {
                if !s(src, k).is_empty() {
                    ui.label(RichText::new(s(src, k)).small().color(t.bright_red));
                }
            }
            if s(src, "kind") == "camera" {
                ui.horizontal(|ui| {
                    let cams: Vec<&Value> = devices.iter().filter(|d| s(d, "kind") == "camera").collect();
                    egui::ComboBox::from_id_salt(("assign", &name)).selected_text("assign camera…").width(160.0).show_ui(ui, |ui| {
                        for c in cams {
                            if ui.selectable_label(s(c, "identity") == s(src, "identity"), format!("{} ({})", s(c, "label"), s(c, "path"))).clicked() {
                                out.push(action("source.assign", Value::map().with("source", name.as_str()).with("identity", s(c, "identity"))));
                            }
                        }
                    });
                    if ui.small_button("reopen").clicked() {
                        out.push(action("source.reopen", Value::map().with("source", name.as_str())));
                    }
                    if ui.small_button("save controls").on_hover_text("write the current control values to sources/<name>.toml [controls]").clicked() {
                        out.push(action("source.save_controls", Value::map().with("source", name.as_str())));
                    }
                });
                controls(m, t, st, ui, &name, src, out, now);
            } else {
                ui.horizontal(|ui| {
                    ui.label(format!(
                        "{:.1} / {:.1} s",
                        m.get(&a("position")).and_then(Value::as_f64).unwrap_or(0.0),
                        m.get(&a("duration")).and_then(Value::as_f64).unwrap_or(0.0)
                    ));
                    if ui.small_button("restart").clicked() {
                        out.push(action("source.restart", Value::map().with("source", name.as_str())));
                    }
                    let paused = m.b(&a("paused"));
                    if ui.small_button(if paused { "play" } else { "pause" }).clicked() {
                        out.push(Op::Set { address: a("paused"), value: Value::Bool(!paused) });
                    }
                });
            }
        });
    });
}

#[allow(clippy::too_many_arguments)]
fn controls(m: &Model, t: &Theme, st: &mut DevicesState, ui: &mut Ui, name: &str, src: &Value, out: &mut Vec<Op>, now: Instant) {
    let list: Vec<Value> = src.get_path("controls").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    if list.is_empty() {
        ui.label(RichText::new("no camera controls").small().color(t.fg_dim));
        return;
    }
    egui::CollapsingHeader::new(format!("controls ({})", list.len())).id_salt(("ctrls", name)).show(ui, |ui| {
        egui::Grid::new(("ctrl_grid", name)).num_columns(3).spacing([8.0, 2.0]).show(ui, |ui| {
            for c in &list {
                let cname = s(c, "name");
                let addr = format!("source.{name}.ctrl.{cname}");
                let inactive = b(c, "inactive");
                let ro = b(c, "read_only");
                let live =
                    st.edits.get(&addr).map(|(v, _)| v.clone()).or_else(|| m.get(&addr).cloned()).or_else(|| c.get_path("value").cloned()).unwrap_or_default();
                ui.label(RichText::new(cname).color(if inactive { t.fg_dim } else { t.fg })).on_hover_text(format!(
                    "{} — device value {}{}",
                    s(c, "label"),
                    c.get_path("value").map(|v| v.to_string()).unwrap_or_default(),
                    if inactive { " (inactive: an auto mode controls it)" } else { "" }
                ));
                let mut new: Option<Value> = None;
                ui.add_enabled_ui(!ro, |ui| match s(c, "type") {
                    "bool" => {
                        let mut v = live.truthy();
                        if ui.checkbox(&mut v, "").changed() {
                            new = Some(Value::Bool(v));
                        }
                    }
                    "enum" => {
                        let menu: Vec<Value> = c.get_path("menu").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
                        let cur = live.as_str().unwrap_or("").to_string();
                        let shown = menu.iter().find(|x| s(x, "name") == cur).map(|x| s(x, "label").to_string()).unwrap_or(cur.clone());
                        egui::ComboBox::from_id_salt(("menu", &addr)).selected_text(shown).width(180.0).show_ui(ui, |ui| {
                            for it in &menu {
                                if ui.selectable_label(s(it, "name") == cur, s(it, "label")).clicked() {
                                    new = Some(Value::Str(s(it, "name").to_string()));
                                }
                            }
                        });
                    }
                    _ => {
                        let (lo, hi) = (f(c, "min") as i64, f(c, "max") as i64);
                        let mut v = live.as_i64().unwrap_or(lo);
                        let step = f(c, "step").max(1.0);
                        if ui.add(egui::Slider::new(&mut v, lo..=hi).step_by(step).clamping(egui::SliderClamping::Always)).changed() {
                            new = Some(Value::Int(v));
                        }
                    }
                });
                if ui.small_button("↺").on_hover_text("release the override (back to the configured value)").clicked() {
                    st.edits.remove(&addr);
                    out.push(Op::Release { address: addr.clone() });
                }
                if let Some(v) = new {
                    st.edits.insert(addr.clone(), (v.clone(), now));
                    out.push(Op::Set { address: addr, value: v });
                }
                ui.end_row();
            }
        });
    });
}

fn device_row(t: &Theme, ui: &mut Ui, d: &Value, source_names: &[String], out: &mut Vec<Op>) {
    let expected = d.get_path("expected").and_then(Value::as_str).map(str::to_string);
    ui.horizontal(|ui| {
        widgets::led(ui, t, if expected.is_some() { LedState::Healthy } else { LedState::Idle });
        ui.label(RichText::new(s(d, "label")).strong());
        ui.label(RichText::new(s(d, "path")).monospace().color(t.fg_dim));
        if let Some(e) = &expected {
            ui.label(RichText::new(format!("= {e}")).color(t.accent));
        } else if ui.small_button("expect").on_hover_text("add to project.toml [devices.expected]").clicked() {
            out.push(action("devices.expect", Value::map().with("identity", s(d, "identity"))));
        }
        if !s(d, "source").is_empty() {
            ui.label(RichText::new(format!("→ {}", s(d, "source"))).color(t.green));
        }
        if let Some(sig) = d.get_path("signal").filter(|v| !v.is_null()) {
            ui.label(RichText::new(s(d, "input_status")).color(if sig.truthy() { t.green } else { t.yellow }));
        }
    });
    ui.indent(("devrow", s(d, "identity")), |ui| {
        ui.label(RichText::new(s(d, "identity")).monospace().small().color(t.fg_dim)).on_hover_text("stable identity: survives reboots and replugging");
        let mut facts = Vec::new();
        for k in ["usb", "serial", "driver", "card", "port"] {
            if !s(d, k).is_empty() {
                facts.push(format!("{k} {}", s(d, k)));
            }
        }
        if let Some(Value::Map(x)) = d.get_path("extra") {
            for (k, v) in x {
                facts.push(format!("{k} {}", v.as_str().unwrap_or("")));
            }
        }
        if !facts.is_empty() {
            ui.label(RichText::new(facts.join(" · ")).small().color(t.fg_dim));
        }
        if s(d, "kind") == "camera" {
            if let Some(cur) = d.get_path("current") {
                ui.label(
                    RichText::new(format!(
                        "current: {} {}x{} @ {:.2} fps ({} {})",
                        s(cur, "format"),
                        f(cur, "width"),
                        f(cur, "height"),
                        f(cur, "fps"),
                        s(cur, "matrix"),
                        s(cur, "range")
                    ))
                    .small(),
                );
            }
            let modes: Vec<Value> = d.get_path("modes").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
            egui::CollapsingHeader::new(format!("{} modes", modes.len())).id_salt(("modes", s(d, "identity"))).show(ui, |ui| {
                for md in &modes {
                    let rates: Vec<String> = md
                        .get_path("fps")
                        .and_then(Value::as_list)
                        .unwrap_or(&[])
                        .iter()
                        .filter_map(Value::as_f64)
                        .map(|r| format!("{r:.2}").trim_end_matches('0').trim_end_matches('.').to_string())
                        .collect();
                    ui.label(
                        RichText::new(format!("{:<6} {:>4}x{:<4} {}", s(md, "format"), f(md, "width"), f(md, "height"), rates.join(" / "))).monospace().small(),
                    );
                }
            });
            if s(d, "source").is_empty() && !source_names.is_empty() {
                egui::ComboBox::from_id_salt(("use_as", s(d, "identity"))).selected_text("use as source…").show_ui(ui, |ui| {
                    for n in source_names {
                        if ui.selectable_label(false, n).clicked() {
                            out.push(action("source.assign", Value::map().with("source", n.as_str()).with("identity", s(d, "identity"))));
                        }
                    }
                });
            }
        }
    });
    ui.add_space(4.0);
}
