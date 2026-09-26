//! Status bar (§15.4, both modes): live state + uptime, per-output health, recording, mode
//! picker, GPU frame time, warnings (opens the list), layout, Clean, Panic (hold).

use crate::app::{App, Mode, ViewId};
use crate::panels::Panel;
use egui::RichText;
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};

/// Something the operator should know about, with where to look.
#[derive(Clone, Debug, PartialEq)]
pub struct Warning {
    pub text: String,
    pub fail: bool,
    pub open: Option<Panel>,
}

/// `hh:mm:ss` for a duration in seconds.
pub fn hms(secs: i64) -> String {
    let s = secs.max(0);
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

pub fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Every current warning, most severe first.
pub fn warnings(app: &App) -> Vec<Warning> {
    let mut w = Vec::new();
    if !app.m.connected {
        w.push(Warning { text: format!("engine offline: {}", app.m.last_disconnect.as_deref().unwrap_or("connecting")), fail: true, open: None });
        return w;
    }
    for (a, v) in app.m.under("health") {
        let st = v.get_path("status").and_then(Value::as_str).unwrap_or("");
        if st == "warn" || st == "fail" {
            let detail = v.get_path("detail").and_then(Value::as_str).unwrap_or("");
            w.push(Warning { text: format!("{}: {detail}", a.trim_start_matches("health.")), fail: st == "fail", open: None });
        }
    }
    let errs = app.m.f("project.errors") as i64;
    if errs > 0 {
        w.push(Warning { text: format!("{errs} project file error(s) — last good version is live"), fail: false, open: Some(Panel::Console) });
    }
    for (a, v) in app.m.under("patch") {
        if a.ends_with(".error") && v.as_str().is_some_and(|s| !s.is_empty()) {
            w.push(Warning { text: format!("{}: {}", a.trim_end_matches(".error"), v.as_str().unwrap_or("")), fail: false, open: Some(Panel::Console) });
        }
    }
    if app.m.has("obs.link") && !app.m.b("obs.link") {
        w.push(Warning { text: "OBS plugin not connected".into(), fail: true, open: None });
    }
    for c in ["wide", "tall"] {
        if app.m.b(&format!("obs.stale.{c}")) {
            w.push(Warning { text: format!("OBS sees the {c} canvas as stale"), fail: true, open: None });
        }
    }
    if app.m.b("obs.fallback.active") {
        w.push(Warning { text: "OBS is showing the fallback scene".into(), fail: true, open: None });
    }
    for (a, v) in app.m.under("obs.output") {
        if let Some(id) = a.strip_suffix(".dropped").and_then(|p| p.strip_prefix("obs.output."))
            && v.as_f64().is_some_and(|d| d > 0.0)
        {
            let label = app.m.str(&format!("obs.output.{id}.label")).to_string();
            w.push(Warning { text: format!("{}: {} dropped frames", if label.is_empty() { id } else { &label }, v), fail: false, open: None });
        }
    }
    let gpu = app.m.f("perf.gpu_ms");
    if gpu > 8.0 {
        w.push(Warning {
            text: format!("GPU frame time {gpu:.1} ms (budget 8 ms) — UI previews reduced"),
            fail: gpu > 14.0,
            open: Some(Panel::View(ViewId::Performance)),
        });
    }
    let xruns = app.m.f("perf.audio.xruns") as i64;
    if xruns > 0 {
        w.push(Warning { text: format!("{xruns} audio xruns"), fail: false, open: Some(Panel::View(ViewId::Performance)) });
    }
    if app.m.b("show.panic") {
        w.push(Warning { text: "PANIC is active".into(), fail: true, open: None });
    }
    w.sort_by_key(|x| !x.fail);
    w
}

fn outputs(app: &App) -> Vec<(String, bool, f64, f64)> {
    let mut ids: Vec<String> = app.m.under("obs.output").filter_map(|(a, _)| a.strip_prefix("obs.output.")?.strip_suffix(".kind").map(String::from)).collect();
    ids.dedup();
    let mut out: Vec<(String, bool, f64, f64)> = ids
        .into_iter()
        .filter(|id| app.m.str(&format!("obs.output.{id}.kind")) == "stream")
        .map(|id| {
            let p = format!("obs.output.{id}");
            let label = app.m.str(&format!("{p}.label"));
            (
                if label.is_empty() { id.clone() } else { label.to_string() },
                app.m.b(&format!("{p}.active")),
                app.m.f(&format!("{p}.kbps")),
                app.m.f(&format!("{p}.dropped")),
            )
        })
        .collect();
    if out.is_empty() && app.m.has("obs.stream.active") {
        out.push(("Stream".into(), app.m.b("obs.stream.active"), app.m.f("obs.stream.kbps"), app.m.f("obs.stream.dropped")));
    }
    out
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.horizontal_centered(|ui| {
        let mode = app.m.str("show.mode").to_string();
        let since = app.m.get("show.live_since").and_then(Value::as_i64).unwrap_or(0);
        let on_air = since > 0;
        let (txt, st) = if !app.m.connected {
            ("ENGINE OFFLINE".to_string(), LedState::Error)
        } else if on_air {
            (format!("{} {}", if mode == "live" { "LIVE".to_string() } else { mode.to_uppercase() }, hms((now_ms() - since) / 1000)), LedState::Active)
        } else {
            (if mode.is_empty() { "…".into() } else { mode.to_uppercase() }, LedState::Idle)
        };
        widgets::led(ui, &t, st);
        ui.label(RichText::new(txt).strong().monospace().color(if on_air { t.tally_program() } else { t.fg }));
        ui.separator();
        let mode_label = if app.mode == Mode::Show { "SHOW" } else { "BUILD" };
        if ui
            .selectable_label(false, RichText::new(mode_label).strong().color(t.accent))
            .on_hover_text(format!("Show/Build ({})", app.keys_label("mode.toggle")))
            .clicked()
        {
            app.toggle_mode();
        }
        ui.separator();
        for (label, active, kbps, dropped) in outputs(app) {
            let text = if active { format!("{label} {:.1}Mb/s {} drop", kbps / 1000.0, dropped as i64) } else { format!("{label} off") };
            let s = if !active {
                LedState::Idle
            } else if dropped > 0.0 {
                LedState::Armed
            } else {
                LedState::Healthy
            };
            widgets::pill(ui, &t, icon::LIVE, &text, s);
        }
        if app.m.has("obs.record.active") {
            let rec = app.m.b("obs.record.active");
            widgets::pill(ui, &t, icon::REC, if rec { "REC" } else { "rec off" }, if rec { LedState::Active } else { LedState::Idle });
        } else if !app.m.has("obs.link") {
            widgets::pill(ui, &t, icon::REC, "OBS —", LedState::Idle).on_hover_text("OBS link not running");
        }
        ui.separator();
        egui::ComboBox::from_id_salt("mode").selected_text(format!("mode: {}", if mode.is_empty() { "?" } else { &mode })).show_ui(ui, |ui| {
            let modes: Vec<String> = app.m.q_list("modes").iter().filter_map(|v| v.as_str().map(String::from)).collect();
            for md in modes {
                if ui.selectable_label(md == mode, &md).clicked() {
                    app.m.command(Op::ModeSet { mode: md });
                }
            }
        });
        let gpu = app.m.f("perf.gpu_ms");
        if app.m.has("perf.gpu_ms") {
            let s = if gpu > 12.0 {
                LedState::Error
            } else if gpu > 8.0 {
                LedState::Armed
            } else {
                LedState::Healthy
            };
            if widgets::pill(ui, &t, icon::GPU, &format!("GPU {gpu:.1}ms"), s).on_hover_text("open Performance").clicked() {
                app.view = Some(ViewId::Performance);
            }
        }
        let warns = warnings(app);
        let n = warns.len();
        let s = if warns.iter().any(|w| w.fail) {
            LedState::Error
        } else if n > 0 {
            LedState::Armed
        } else {
            LedState::Healthy
        };
        let pill = widgets::pill(ui, &t, icon::WARN, &format!("{n}"), s).on_hover_text("warnings");
        egui::Popup::menu(&pill).show(|ui| {
            ui.set_min_width(360.0);
            if warns.is_empty() {
                ui.label(RichText::new(format!("{} all clear", icon::CHECK)).color(t.green));
            }
            for w in &warns {
                let c = if w.fail { t.bright_red } else { t.yellow };
                let r = ui.add(
                    egui::Label::new(RichText::new(format!("{} {}", if w.fail { icon::CROSS } else { icon::WARN }, w.text)).color(c))
                        .sense(egui::Sense::click()),
                );
                if let Some(p) = w.open
                    && r.clicked()
                {
                    app.open_panel(p);
                    ui.close();
                }
            }
            ui.separator();
            if ui.button("preflight…").clicked() {
                app.m.query("preflight", Value::Null);
                app.open_panel(Panel::Console);
                ui.close();
            }
        });
        ui.separator();
        ui.menu_button(format!("{} views", icon::SEARCH), |ui| {
            if ui.selectable_label(app.view.is_none(), "main").clicked() {
                app.view = None;
                ui.close();
            }
            for (v, ic, label) in ViewId::ALL {
                if ui.selectable_label(app.view == Some(*v), format!("{ic} {label}")).clicked() {
                    app.view = Some(*v);
                    ui.close();
                }
            }
        });
        let layout = app.layout_name();
        if ui
            .selectable_label(false, RichText::new(format!("{} {layout}", icon::SETTINGS)).small().color(t.fg_dim))
            .on_hover_text(format!("layout ({} to switch)", app.keys_label("layout.next")))
            .clicked()
        {
            app.next_layout();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::hold_button(ui, &t, "PANIC", t.bright_red, 0.8) {
                app.m.command(Op::Panic);
            }
            if ui.button(RichText::new("CLEAN").strong()).on_hover_text(format!("Clear chat effects ({})", app.keys_label("clean"))).clicked() {
                app.m.command(Op::Clean);
            }
            ui.separator();
            let conn = if app.m.connected {
                format!("{} {}", icon::LINK, app.m.session)
            } else {
                format!("{} {}", icon::UNLINK, app.m.last_disconnect.as_deref().unwrap_or("connecting…"))
            };
            ui.label(RichText::new(conn).small().color(if app.m.connected { t.fg_dim } else { t.bright_red }));
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hms_formats_hours() {
        assert_eq!(hms(0), "00:00:00");
        assert_eq!(hms(2 * 3600 + 14 * 60 + 33), "02:14:33");
        assert_eq!(hms(-5), "00:00:00");
    }
}
