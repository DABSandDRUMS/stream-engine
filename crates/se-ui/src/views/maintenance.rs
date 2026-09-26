//! Settings → Backups (§17.4, §12.3): the runtime database backup (last backup in words, back up
//! now, how to restore), the recordings disk budget as a bar with "clean up old recordings",
//! stream-log retention.

use crate::app::App;
use egui::{CornerRadius, Rect, RichText, Ui, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_bold, font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};

const GB: f64 = 1_000_000_000.0;

fn act(app: &mut App, name: &str) {
    app.m.command(Op::Action { name: name.into(), args: Value::Null });
}

/// `health.<check>`: (status color, status, detail).
fn health(app: &App, t: &Theme, check: &str) -> (egui::Color32, String, String) {
    let v = app.m.get(&format!("health.{check}"));
    let status = v.and_then(|v| v.get_path("status")).and_then(Value::as_str).unwrap_or("").to_string();
    let detail = v.and_then(|v| v.get_path("detail")).and_then(Value::as_str).unwrap_or("").to_string();
    let c = match status.as_str() {
        "pass" => t.green,
        "warn" => t.yellow,
        "fail" => t.bright_red,
        _ => t.text_dim,
    };
    (c, status, detail)
}

fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// "just now", "12 minutes ago", "3 hours ago", "yesterday", "4 days ago".
fn ago(ts: i64) -> String {
    let d = (now_s() - ts).max(0);
    match d {
        0..=89 => "just now".into(),
        90..=3599 => format!("{} minutes ago", (d + 30) / 60),
        3600..=5399 => "an hour ago".into(),
        5400..=86_399 => format!("{} hours ago", (d + 1800) / 3600),
        86_400..=172_799 => "yesterday".into(),
        _ => format!("{} days ago", d / 86_400),
    }
}

/// "850 MB", "12.4 GB".
fn size_words(bytes: f64) -> String {
    if bytes < GB { format!("{:.0} MB", bytes / 1e6) } else { format!("{:.1} GB", bytes / GB) }
}

fn file_name(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

pub fn ui(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("maintenance-view");
    let now = ui.input(|i| i.time);
    let last: f64 = ui.data_mut(|d| d.get_temp(id)).unwrap_or(-10.0);
    if now - last > 2.0 {
        ui.data_mut(|d| d.insert_temp(id, now));
        app.m.query("retention", Value::Null);
    }
    let r = app.m.q("retention").cloned();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let w = ui.available_width();
        ui.set_max_width(w);
        let Some(r) = r else {
            widgets::panel(ui, &t, |ui| {
                ui.set_width(ui.available_width());
                let (title, body) = if app.m.connected {
                    ("Checking your backups…", "This takes a second.")
                } else {
                    ("Stream Engine isn't running", "Backups show up here once it's running.")
                };
                widgets::empty_state(ui, &t, icon::SAVE, title, body, None);
            });
            return;
        };
        if w >= 1100.0 {
            let n = if w >= 2200.0 { 3 } else { 2 };
            let gaps = ui.spacing().item_spacing;
            ui.spacing_mut().item_spacing.x = spacing::L;
            ui.columns(n, |cols| {
                for c in cols.iter_mut() {
                    c.spacing_mut().item_spacing = gaps;
                }
                backup(app, &mut cols[0], &t, &r);
                recordings(app, &mut cols[1], &t, &r);
                logs(app, &mut cols[if n == 3 { 2 } else { 0 }], &t, &r);
            });
        } else {
            backup(app, ui, &t, &r);
            recordings(app, ui, &t, &r);
            logs(app, ui, &t, &r);
        }
        ui.add_space(spacing::XL);
    });
}

fn backup(app: &mut App, ui: &mut Ui, t: &Theme, r: &Value) {
    let (color, status, detail) = health(app, t, "backup");
    let files = r.get_path("backups.files").and_then(Value::as_list).unwrap_or(&[]).to_vec();
    let newest = files.iter().filter_map(|b| b.get_path("at").and_then(Value::as_i64)).max();
    widgets::titled(
        ui,
        t,
        "Backup",
        "Stream Engine saves a copy of your settings and history every day: songs, counters, clips and paired phones.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(44.0), egui::Sense::hover());
                let (ic, c) = if newest.is_some() && status != "fail" { (icon::CHECK, t.green) } else { (icon::WARN, color) };
                ui.painter().circle_filled(r.center(), 21.0, mix(t.surface, c, 0.2));
                ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, ic, se_ui_kit::theme::font(18.0), c);
                ui.add_space(spacing::S);
                ui.vertical(|ui| {
                    let head = match newest {
                        Some(at) => format!("Last backup {}", ago(at)),
                        None => "No backup yet".to_string(),
                    };
                    ui.label(RichText::new(head).font(font_semibold(type_scale::HEADING)).color(t.fg));
                    let line = match status.as_str() {
                        "fail" => "The last backup didn't work. Try Back up now.",
                        "warn" => "It's been a while. Back up now to be safe.",
                        _ if newest.is_none() => "Make the first one now.",
                        _ => "Everything is saved. Backups wait while you're live.",
                    };
                    ui.label(RichText::new(line).color(t.text_dim));
                });
            });
            ui.add_space(spacing::M);
            if widgets::button_ex(ui, t, Some(icon::SAVE), "Back up now", Kind::Primary, Size::Medium, 0.0, true).clicked() {
                act(app, "retention.backup_now");
            }
            ui.add_space(spacing::M);
            widgets::details(ui, t, "restore", "How to restore a backup", |ui| {
                widgets::hint(
                    ui,
                    t,
                    "Restoring puts your settings and history back to how it was on that day. It has to be stopped first, so do it when you're not live.",
                );
                ui.add_space(spacing::XS);
                for (n, step) in ["Stop Stream Engine.", "Unpack the backup over the current one.", "Start Stream Engine again."].iter().enumerate() {
                    ui.label(RichText::new(format!("{}. {step}", n + 1)).color(t.fg));
                }
                let latest =
                    files.iter().max_by_key(|b| b.get_path("at").and_then(Value::as_i64).unwrap_or(0)).and_then(|b| b.get_path("path").and_then(Value::as_str));
                ui.add_space(spacing::XS);
                let enabled = latest.is_some();
                if widgets::button_ex(ui, t, Some(icon::COPY), "Copy the steps as terminal commands", Kind::Secondary, Size::Small, 0.0, enabled)
                    .on_hover_text("Uses the newest backup. Paste them into a terminal.")
                    .clicked()
                    && let Some(path) = latest
                {
                    ui.ctx().copy_text(restore_commands(path));
                    app.m.toast("Restore steps copied", false);
                }
            });
            widgets::details(ui, t, "backup-details", "Details", |ui| {
                if !detail.is_empty() {
                    widgets::fact(ui, t, "Status", &detail);
                }
                widgets::fact(ui, t, "Folder", r.get_path("backups.dir").and_then(Value::as_str).unwrap_or(""));
                for b in files.iter().take(5) {
                    let path = b.get_path("path").and_then(Value::as_str).unwrap_or("");
                    let mb = b.get_path("bytes").and_then(Value::as_f64).unwrap_or(0.0) / 1e6;
                    let at = b.get_path("at").and_then(Value::as_i64).unwrap_or(0);
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(file_name(path)).font(font_mono(type_scale::SMALL)).color(t.text_dim));
                        ui.label(RichText::new(format!("{mb:.1} MB · {}", ago(at))).size(type_scale::SMALL).color(t.text_dim));
                    });
                }
            });
        },
    );
    ui.add_space(spacing::L);
}

/// The documented restore (engine stopped) for one backup file.
fn restore_commands(backup: &str) -> String {
    let dir = backup.rsplit_once("/backups/").map(|(d, _)| d.to_string()).unwrap_or_else(|| "~/.local/share/stream-engine".into());
    format!(
        "systemctl --user stop stream-engine\nzstd -d '{backup}' -o '{dir}/runtime.db' -f\nrm -f '{dir}/runtime.db-wal' '{dir}/runtime.db-shm'\nsystemctl --user start stream-engine\n"
    )
}

/// Disk bar: used of budget, colored by how full it is.
fn space_bar(ui: &mut Ui, t: &Theme, frac: f32, color: egui::Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 14.0), egui::Sense::hover());
    let p = ui.painter();
    let r = CornerRadius::same(radius::PILL);
    p.rect_filled(rect, r, t.inset);
    let w = rect.width() * frac.clamp(0.0, 1.0);
    if w > 0.5 {
        p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(w.max(14.0), rect.height())), r, color);
    }
}

fn recordings(app: &mut App, ui: &mut Ui, t: &Theme, r: &Value) {
    let (color, status, detail) = health(app, t, "recordings");
    let used = app.m.f("retention.recordings.used_gb");
    let budget = app.m.f("retention.recordings.budget_gb");
    let pending = r.get_path("recordings.pending").and_then(Value::as_list).unwrap_or(&[]).to_vec();
    widgets::titled(
        ui,
        t,
        "Space for recordings",
        "OBS recordings of your streams, kept for making clips.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{used:.0} GB")).font(font_bold(type_scale::TITLE)).color(t.fg));
                if budget > 0.0 {
                    ui.label(RichText::new(format!("of {budget:.0} GB used")).size(type_scale::LARGE).color(t.text_dim));
                }
            });
            ui.add_space(spacing::XS);
            let frac = if budget > 0.0 { (used / budget) as f32 } else { 0.0 };
            let bar = match status.as_str() {
                "fail" => t.bright_red,
                "warn" => t.yellow,
                _ => t.accent,
            };
            space_bar(ui, t, frac, bar);
            ui.add_space(spacing::S);
            let line = match status.as_str() {
                "fail" => "The disk is almost full. Clean up old recordings before your next stream.",
                "warn" => "Getting full. Clean up old recordings soon.",
                _ if budget > 0.0 => "Plenty of room.",
                _ => "",
            };
            if !line.is_empty() {
                ui.label(RichText::new(line).color(if status == "pass" { t.text_dim } else { color }));
            }
            ui.add_space(spacing::M);
            if pending.is_empty() {
                widgets::hint(ui, t, "Nothing to clean up. Old recordings are only deleted when you say so.");
            } else {
                let gb: f64 = pending.iter().filter_map(|p| p.get_path("bytes").and_then(Value::as_f64)).sum::<f64>() / GB;
                ui.label(
                    RichText::new(format!("{} oldest recordings ({gb:.1} GB) can go:", pending.len()))
                        .font(se_ui_kit::theme::font_medium(type_scale::BODY))
                        .color(t.fg),
                );
                ui.add_space(spacing::XS);
                for p in &pending {
                    let path = p.get_path("path").and_then(Value::as_str).unwrap_or("");
                    let size = p.get_path("bytes").and_then(Value::as_f64).unwrap_or(0.0) / GB;
                    widgets::list_row(ui, t, icon::FILM, file_name(path), "", &format!("{size:.1} GB"), false).on_hover_text(path);
                }
                ui.add_space(spacing::S);
                if widgets::hold_button(ui, t, "Clean up old recordings", t.bright_red, 1.5) {
                    act(app, "retention.prune_recordings");
                }
                widgets::hint(ui, t, "Press and hold to delete the recordings above. Nothing is deleted while you're live or recording.");
            }
            ui.add_space(spacing::S);
            widgets::details(ui, t, "recordings-details", "Details", |ui| {
                if !detail.is_empty() {
                    widgets::fact(ui, t, "Status", &detail);
                }
                widgets::fact(ui, t, "Folder", r.get_path("recordings.dir").and_then(Value::as_str).unwrap_or(""));
                for p in &pending {
                    let warned = p.get_path("warned_at").and_then(Value::as_i64).unwrap_or(0);
                    widgets::hint(ui, t, &format!("{} · flagged {}", file_name(p.get_path("path").and_then(Value::as_str).unwrap_or("")), ago(warned)));
                }
            });
        },
    );
    ui.add_space(spacing::L);
}

fn logs(app: &mut App, ui: &mut Ui, t: &Theme, r: &Value) {
    let count = r.get_path("sessions.count").and_then(Value::as_i64).unwrap_or(0);
    let gb = r.get_path("sessions.bytes").and_then(Value::as_f64).unwrap_or(0.0) / GB;
    let prunable: Vec<String> =
        r.get_path("sessions.prunable").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str().map(String::from)).collect();
    widgets::titled(
        ui,
        t,
        "Stream history",
        "What happened during each stream (events, markers), used for clips and stats.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new(format!("{count} streams saved · {}", size_words(gb * GB))).color(t.fg));
            if prunable.is_empty() {
                widgets::hint(ui, t, "Old streams are tidied up after 90 days. The newest ones are always kept.");
            } else {
                ui.label(RichText::new(format!("{} streams are older than you keep them.", prunable.len())).color(t.yellow));
            }
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                if !prunable.is_empty() && widgets::hold_button(ui, t, "Delete old stream history", t.yellow, 0.8) {
                    act(app, "retention.prune_sessions");
                }
                if widgets::button_ex(ui, t, Some(icon::SEARCH), "Check again", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                    act(app, "retention.scan");
                }
            });
            if !prunable.is_empty() {
                widgets::details(ui, t, "sessions-details", "Details", |ui| {
                    widgets::hint(ui, t, &format!("Past keeping time: {}", prunable.join(", ")));
                });
            }
        },
    );
    ui.add_space(spacing::L);
}
