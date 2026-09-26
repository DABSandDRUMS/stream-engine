//! Backups & retention (§17.4) and remote mod access status (§12.3): runtime DB backups,
//! session-log retention, the recordings disk budget, and who is using the mod console.

use crate::app::App;
use egui::RichText;
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};

const GB: f64 = 1_000_000_000.0;

fn act(app: &mut App, name: &str) {
    app.m.command(Op::Action { name: name.into(), args: Value::Null });
}

fn health(app: &App, check: &str) -> (LedState, String) {
    let v = app.m.get(&format!("health.{check}"));
    let status = v.and_then(|v| v.get_path("status")).and_then(Value::as_str).unwrap_or("");
    let detail = v.and_then(|v| v.get_path("detail")).and_then(Value::as_str).unwrap_or("not reported yet").to_string();
    (
        match status {
            "pass" => LedState::Healthy,
            "warn" => LedState::Armed,
            "fail" => LedState::Error,
            _ => LedState::Idle,
        },
        detail,
    )
}

fn ago(ts: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let d = (now - ts).max(0);
    match d {
        0..=119 => format!("{d} s ago"),
        120..=7199 => format!("{} min ago", d / 60),
        7200..=172_799 => format!("{} h ago", d / 3600),
        _ => format!("{} days ago", d / 86_400),
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("maintenance-view");
    let now = ui.input(|i| i.time);
    let last: f64 = ui.data_mut(|d| d.get_temp(id)).unwrap_or(-10.0);
    if now - last > 2.0 {
        ui.data_mut(|d| d.insert_temp(id, now));
        app.m.query("retention", Value::Null);
    }
    let r = app.m.q("retention").cloned().unwrap_or_default();
    widgets::section(ui, &t, icon::SESSION, "Backups & retention");
    ui.add_space(6.0);
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let (led, detail) = health(app, "backup");
        widgets::card(ui, &t, icon::SESSION, "Runtime database", led, |ui| {
            ui.label(RichText::new(detail).color(t.fg_dim));
            ui.label(RichText::new(format!("folder: {}", r.get_path("backups.dir").and_then(Value::as_str).unwrap_or(""))).small().monospace());
            for b in r.get_path("backups.files").and_then(Value::as_list).unwrap_or(&[]).iter().take(5) {
                let path = b.get_path("path").and_then(Value::as_str).unwrap_or("");
                let name = path.rsplit('/').next().unwrap_or(path);
                ui.label(format!(
                    "{name}  {:.1} MB  {}",
                    b.get_path("bytes").and_then(Value::as_f64).unwrap_or(0.0) / 1e6,
                    ago(b.get_path("at").and_then(Value::as_i64).unwrap_or(0))
                ));
            }
            if ui.button("Back up now").clicked() {
                act(app, "retention.backup_now");
            }
        });
        ui.add_space(6.0);
        let prunable: Vec<String> =
            r.get_path("sessions.prunable").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str().map(String::from)).collect();
        widgets::card(ui, &t, icon::TIMELINE, "Session logs", if prunable.is_empty() { LedState::Healthy } else { LedState::Armed }, |ui| {
            ui.label(format!(
                "{} sessions, {:.1} GB",
                r.get_path("sessions.count").and_then(Value::as_i64).unwrap_or(0),
                r.get_path("sessions.bytes").and_then(Value::as_f64).unwrap_or(0.0) / GB
            ));
            if !prunable.is_empty() {
                ui.label(RichText::new(format!("past retention: {}", prunable.join(", "))).color(t.yellow));
            }
            ui.horizontal(|ui| {
                if ui.button("Rescan").clicked() {
                    act(app, "retention.scan");
                }
                if !prunable.is_empty() && widgets::hold_button(ui, &t, "Delete old sessions", t.yellow, 0.8) {
                    act(app, "retention.prune_sessions");
                }
            });
        });
        ui.add_space(6.0);
        let (led, detail) = health(app, "recordings");
        widgets::card(ui, &t, icon::REC, "Recordings", led, |ui| {
            let used = app.m.f("retention.recordings.used_gb");
            let budget = app.m.f("retention.recordings.budget_gb").max(0.001);
            ui.label(RichText::new(detail).color(t.fg_dim));
            ui.add(egui::ProgressBar::new((used / budget) as f32).text(format!("{used:.0} / {budget:.0} GB")).fill(match led {
                LedState::Error => t.bright_red,
                LedState::Armed => t.yellow,
                _ => t.green,
            }));
            ui.label(RichText::new(format!("folder: {}", r.get_path("recordings.dir").and_then(Value::as_str).unwrap_or(""))).small().monospace());
            let pending = r.get_path("recordings.pending").and_then(Value::as_list).unwrap_or(&[]).to_vec();
            if !pending.is_empty() {
                ui.label(RichText::new("Oldest recordings over budget (deleted only after confirmation or the configured grace period):").color(t.yellow));
                for p in &pending {
                    ui.label(
                        RichText::new(format!(
                            "{}  {:.1} GB  warned {}",
                            p.get_path("path").and_then(Value::as_str).unwrap_or(""),
                            p.get_path("bytes").and_then(Value::as_f64).unwrap_or(0.0) / GB,
                            ago(p.get_path("warned_at").and_then(Value::as_i64).unwrap_or(0))
                        ))
                        .small(),
                    );
                }
                if widgets::hold_button(ui, &t, "Delete these recordings", t.bright_red, 1.5) {
                    act(app, "retention.prune_recordings");
                }
            }
        });
        ui.add_space(6.0);
        let on = app.m.b("remote_mod.enabled");
        widgets::card(ui, &t, icon::MOD, "Remote mod console", if on { LedState::Healthy } else { LedState::Idle }, |ui| {
            if on {
                let active: Vec<String> =
                    app.m.get("remote_mod.active").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str().map(String::from)).collect();
                ui.label(if active.is_empty() {
                    "enabled — no moderators active in the last 30 min".to_string()
                } else {
                    format!("active: {}", active.join(", "))
                });
            } else {
                ui.label(RichText::new("off — set [remote_mod] enabled = true in project.toml (moderators sign in at https://<relay>/mod)").color(t.fg_dim));
            }
        });
    });
}
