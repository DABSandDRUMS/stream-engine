//! Twitch channel operations (§11): connection health, stream + ad schedule (snooze, upcoming
//! warning), polls, predictions, hype train, owned channel point rewards, raids, shoutouts,
//! stream markers, and the chat→screen delay. Authorization details live in Settings; chat,
//! AutoMod, and the policy queue in the Mod rail.
//!
//! Data: `twitch.*` state (`auth.*`, `eventsub.*`, `stream.*`, `ad.*`, `poll`, `prediction`,
//! `hype_train`, `rewards`, `delay*`), `health.twitch`, signals `twitch.viewers`,
//! `twitch.chat_rate`. Actions: `twitch.auth.{start,cancel}`, `twitch.ad.snooze`,
//! `twitch.poll.{start,end}`, `twitch.prediction.{start,lock,resolve,cancel}`, `twitch.raid`,
//! `twitch.raid.cancel`, `twitch.shoutout`, `twitch.marker`, `twitch.rewards.sync`,
//! `twitch.delay.set`.

use crate::app::App;
use egui::{RichText, Vec2};
use se_proto::Value;
use se_ui_kit::widgets::{self, LedState, icon};

#[derive(Clone, Default)]
struct Form {
    poll_title: String,
    poll_choices: String,
    poll_secs: String,
    pred_title: String,
    pred_outcomes: String,
    pred_secs: String,
    raid: String,
    shoutout: String,
    marker: String,
    delay_ms: String,
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn i(v: &Value, k: &str) -> i64 {
    v.get_path(k).and_then(Value::as_i64).unwrap_or(0)
}

fn clock(secs: i64) -> String {
    if secs < 0 {
        return "—".into();
    }
    let (h, m, s) = (secs / 3600, (secs / 60) % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn bar(ui: &mut egui::Ui, t: &se_ui_kit::Theme, frac: f32, label: &str) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width().min(320.0), 16.0), egui::Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, 3.0, t.bg_darker);
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    p.rect_filled(fill, 3.0, t.accent);
    p.text(rect.left_center() + Vec2::new(6.0, 0.0), egui::Align2::LEFT_CENTER, label, egui::FontId::proportional(12.0), t.fg_bright);
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("twitch-view");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    egui::ScrollArea::vertical().id_salt("twitch-scroll").show(ui, |ui| {
        connection(app, ui, &t);
        if app.m.str("twitch.auth.status") != "authorized" {
            return;
        }
        ui.add_space(6.0);
        ui.columns(2, |cols| {
            stream(app, &mut cols[0], &t, &mut form);
            rewards(app, &mut cols[1], &t);
        });
        ui.add_space(6.0);
        ui.columns(2, |cols| {
            poll(app, &mut cols[0], &t, &mut form);
            prediction(app, &mut cols[1], &t, &mut form);
        });
        ui.add_space(6.0);
        channel_actions(app, ui, &t, &mut form);
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}

fn connection(app: &mut App, ui: &mut egui::Ui, t: &se_ui_kit::Theme) {
    let health = app.m.get("health.twitch").cloned().unwrap_or_default();
    let state = match s(&health, "status") {
        "pass" => LedState::Healthy,
        "warn" => LedState::Armed,
        "fail" => LedState::Error,
        _ => LedState::Idle,
    };
    widgets::card(ui, t, icon::CHAT, "Twitch", state, |ui| {
        ui.horizontal_wrapped(|ui| {
            let status = app.m.str("twitch.auth.status").to_string();
            let login = app.m.str("twitch.auth.login").to_string();
            widgets::pill(
                ui,
                t,
                icon::LINK,
                &if login.is_empty() { status.clone() } else { format!("{status} · {login}") },
                if status == "authorized" { LedState::Healthy } else { LedState::Error },
            );
            let es = app.m.b("twitch.eventsub.connected");
            let n = app.m.f("twitch.eventsub.subscriptions") as i64;
            let failed = app.m.get("twitch.eventsub.failed").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
            let r = widgets::pill(
                ui,
                t,
                if es { icon::LINK } else { icon::UNLINK },
                &format!("EventSub {}", if es { format!("{n} subs") } else { "offline".into() }),
                if !es {
                    LedState::Error
                } else if failed.is_empty() {
                    LedState::Healthy
                } else {
                    LedState::Armed
                },
            );
            if !failed.is_empty() {
                r.on_hover_text(failed.iter().map(|f| format!("{}: {}", s(f, "type"), s(f, "error"))).collect::<Vec<_>>().join("\n"));
            }
            let reconnects = app.m.f("twitch.eventsub.reconnects") as i64;
            if reconnects > 0 {
                ui.label(RichText::new(format!("{reconnects} reconnects")).small().color(t.fg_dim));
            }
        });
        ui.label(RichText::new(s(&health, "detail")).color(t.fg_dim));
        match app.m.str("twitch.auth.status") {
            "pending" => {
                let code = app.m.str("twitch.auth.user_code").to_string();
                let uri = app.m.str("twitch.auth.verification_uri").to_string();
                ui.horizontal(|ui| {
                    ui.label("Open");
                    ui.hyperlink(&uri);
                    ui.label("and enter");
                    ui.label(RichText::new(&code).monospace().strong().size(20.0).color(t.accent));
                    ui.label(RichText::new(clock(app.m.f("twitch.auth.expires_in_s") as i64)).color(t.fg_dim));
                    if ui.button("Cancel").clicked() {
                        app.m.action("twitch.auth.cancel", Value::map().with("account", "broadcaster"));
                    }
                });
            }
            "authorized" => {}
            _ => {
                if app.m.str("twitch.client_id").is_empty() {
                    ui.label(RichText::new("Set [twitch] client_id in project.toml (see docs/twitch.md).").color(t.yellow));
                } else if ui.button(format!("{} Authorize with Twitch", icon::LINK)).clicked() {
                    app.m.action("twitch.auth.start", Value::map().with("account", "broadcaster"));
                }
            }
        }
    });
}

fn stream(app: &mut App, ui: &mut egui::Ui, t: &se_ui_kit::Theme, form: &mut Form) {
    let live = app.m.b("twitch.stream.live");
    widgets::card(ui, t, icon::LIVE, "Stream", if live { LedState::Active } else { LedState::Idle }, |ui| {
        ui.horizontal_wrapped(|ui| {
            widgets::pill(ui, t, icon::LIVE, if live { "LIVE" } else { "offline" }, if live { LedState::Active } else { LedState::Idle });
            if live {
                let up = unix_now() - app.m.f("twitch.stream.started_at") as i64;
                ui.label(format!("up {}", clock(up)));
            }
            ui.label(format!("{} viewers", app.m.sig("twitch.viewers").unwrap_or(0.0) as i64));
            ui.label(format!("{:.0} msg/min", app.m.sig("twitch.chat_rate").unwrap_or(0.0)));
        });
        let title = app.m.str("twitch.stream.title").to_string();
        if !title.is_empty() {
            ui.label(RichText::new(format!("{title} · {}", app.m.str("twitch.stream.category"))).color(t.fg_dim));
        }
        ui.separator();
        // ads (§11: a warning shows ahead of scheduled ads)
        let next = app.m.f("twitch.ad.next_in_s") as i64;
        let soon = (0..=120).contains(&next);
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{} Next ad", icon::ALERT)).color(if soon { t.yellow } else { t.fg }));
            ui.label(RichText::new(clock(next)).monospace().strong().color(if soon { t.yellow } else { t.fg }));
            ui.label(format!("{}s", app.m.f("twitch.ad.duration_s") as i64));
            let snoozes = app.m.f("twitch.ad.snoozes") as i64;
            if ui.add_enabled(snoozes > 0 && next >= 0, egui::Button::new(format!("Snooze ({snoozes})"))).clicked() {
                app.m.action("twitch.ad.snooze", Value::Null);
            }
        });
        if app.m.str("show.mode") == "ad_break" {
            ui.label(RichText::new("Ad break running — chat effects paused").color(t.yellow));
        }
        ui.separator();
        let hype = app.m.get("twitch.hype_train").cloned().unwrap_or_default();
        if app.m.b("twitch.hype_train.active") {
            ui.label(RichText::new(format!("Hype train level {}", i(&hype, "level"))).strong().color(t.magenta));
            bar(ui, t, app.m.f("twitch.hype_train.fraction") as f32, &format!("{}/{}", i(&hype, "progress"), i(&hype, "goal")));
        }
        ui.horizontal(|ui| {
            ui.label("Chat → screen delay");
            ui.label(RichText::new(format!("{} ms", app.m.f("twitch.delay_ms") as i64)).monospace()).on_hover_text(format!(
                "stream delay setting {} s + measured chat transit {} ms + configured viewer latency",
                app.m.f("twitch.delay.stream_s") as i64,
                app.m.f("twitch.delay.chat_ms") as i64
            ));
            ui.add(egui::TextEdit::singleline(&mut form.delay_ms).hint_text("manual ms").desired_width(80.0));
            if ui.button("Set").clicked() {
                let ms = form.delay_ms.trim().parse::<i64>().unwrap_or(-1);
                app.m.action("twitch.delay.set", Value::map().with("ms", ms));
            }
        });
    });
}

fn rewards(app: &mut App, ui: &mut egui::Ui, t: &se_ui_kit::Theme) {
    let list = app.m.get("twitch.rewards").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let bad = list.iter().any(|r| s(r, "status").starts_with("error"));
    widgets::card(ui, t, icon::PRESET, "Channel point rewards", if bad { LedState::Error } else { LedState::Idle }, |ui| {
        if list.is_empty() {
            ui.label(RichText::new("No rewards/*.toml in the project.").color(t.fg_dim));
        }
        egui::Grid::new("twitch-rewards").striped(true).show(ui, |ui| {
            for r in &list {
                ui.label(RichText::new(s(r, "title")).strong());
                ui.label(format!("{}", i(r, "cost")));
                ui.label(if r.get_path("enabled").is_some_and(Value::truthy) { "on" } else { "off" });
                let st = s(r, "status");
                ui.label(RichText::new(st).color(if st.starts_with("error") { t.red } else { t.fg_dim }));
                ui.end_row();
            }
        });
        if ui.button("Sync from rewards/*.toml").clicked() {
            app.m.action("twitch.rewards.sync", Value::Null);
        }
    });
}

fn poll(app: &mut App, ui: &mut egui::Ui, t: &se_ui_kit::Theme, form: &mut Form) {
    let active = app.m.b("twitch.poll.active");
    let p = app.m.get("twitch.poll").cloned().unwrap_or_default();
    widgets::card(ui, t, icon::QUEUE, "Poll", if active { LedState::Active } else { LedState::Idle }, |ui| {
        if !s(&p, "title").is_empty() {
            ui.label(RichText::new(s(&p, "title")).strong());
            let total = i(&p, "total_votes").max(1);
            for c in p.get_path("choices").and_then(Value::as_list).unwrap_or(&[]) {
                let votes = i(c, "votes");
                let win = !active && s(&p, "winner") == s(c, "id");
                bar(ui, t, votes as f32 / total as f32, &format!("{}{} — {votes}", if win { "★ " } else { "" }, s(c, "title")));
            }
            if active {
                ui.label(format!("ends in {}", clock(i(&p, "ends_at") - unix_now())));
                if ui.button("End poll").clicked() {
                    app.m.action("twitch.poll.end", Value::Null);
                }
            }
        }
        if !active {
            ui.add(egui::TextEdit::singleline(&mut form.poll_title).hint_text("Question").desired_width(f32::INFINITY));
            ui.add(egui::TextEdit::singleline(&mut form.poll_choices).hint_text("Choice a | choice b | …").desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut form.poll_secs).hint_text("duration (2m)").desired_width(90.0));
                if ui.button("Start poll").clicked() && !form.poll_title.trim().is_empty() {
                    let dur = if form.poll_secs.trim().is_empty() { "2m".to_string() } else { form.poll_secs.trim().to_string() };
                    app.m.action(
                        "twitch.poll.start",
                        Value::map().with("title", form.poll_title.trim()).with("choices", form.poll_choices.trim()).with("duration", dur),
                    );
                }
            });
        }
    });
}

fn prediction(app: &mut App, ui: &mut egui::Ui, t: &se_ui_kit::Theme, form: &mut Form) {
    let active = app.m.b("twitch.prediction.active");
    let p = app.m.get("twitch.prediction").cloned().unwrap_or_default();
    widgets::card(ui, t, icon::SIM, "Prediction", if active { LedState::Active } else { LedState::Idle }, |ui| {
        if !s(&p, "title").is_empty() {
            ui.label(RichText::new(format!("{} ({})", s(&p, "title"), s(&p, "status"))).strong());
            let outcomes = p.get_path("outcomes").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
            let total: i64 = outcomes.iter().map(|o| i(o, "points")).sum::<i64>().max(1);
            for o in &outcomes {
                ui.horizontal(|ui| {
                    bar(ui, t, i(o, "points") as f32 / total as f32, &format!("{} — {} pts, {} users", s(o, "title"), i(o, "points"), i(o, "users")));
                    if active && s(&p, "status") == "locked" && ui.button("Winner").clicked() {
                        app.m.action("twitch.prediction.resolve", Value::map().with("winner", s(o, "id")));
                    }
                });
            }
            if active {
                ui.horizontal(|ui| {
                    if s(&p, "status") != "locked" && ui.button("Lock").clicked() {
                        app.m.action("twitch.prediction.lock", Value::Null);
                    }
                    if ui.button("Cancel (refund)").clicked() {
                        app.m.action("twitch.prediction.cancel", Value::Null);
                    }
                });
            }
        }
        if !active {
            ui.add(egui::TextEdit::singleline(&mut form.pred_title).hint_text("Prediction").desired_width(f32::INFINITY));
            ui.add(egui::TextEdit::singleline(&mut form.pred_outcomes).hint_text("Outcome a | outcome b").desired_width(f32::INFINITY));
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut form.pred_secs).hint_text("window (2m)").desired_width(90.0));
                if ui.button("Start prediction").clicked() && !form.pred_title.trim().is_empty() {
                    let window = if form.pred_secs.trim().is_empty() { "2m".to_string() } else { form.pred_secs.trim().to_string() };
                    app.m.action(
                        "twitch.prediction.start",
                        Value::map().with("title", form.pred_title.trim()).with("outcomes", form.pred_outcomes.trim()).with("window", window),
                    );
                }
            });
        }
    });
}

fn channel_actions(app: &mut App, ui: &mut egui::Ui, t: &se_ui_kit::Theme, form: &mut Form) {
    widgets::card(ui, t, icon::LINK, "Channel", LedState::Idle, |ui| {
        ui.horizontal_wrapped(|ui| {
            ui.add(egui::TextEdit::singleline(&mut form.shoutout).hint_text("channel").desired_width(140.0));
            if ui.button("Shoutout").clicked() && !form.shoutout.trim().is_empty() {
                app.m.action("twitch.shoutout", Value::map().with("user", form.shoutout.trim()));
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut form.raid).hint_text("raid target").desired_width(140.0));
            if !form.raid.trim().is_empty() && widgets::hold_button(ui, t, "Hold to raid", t.red, 1.0) {
                app.m.action("twitch.raid", Value::map().with("user", form.raid.trim()));
            }
            if ui.button("Cancel raid").clicked() {
                app.m.action("twitch.raid.cancel", Value::Null);
            }
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut form.marker).hint_text("marker note").desired_width(160.0));
            if ui.button(format!("{} Marker", icon::REC)).clicked() {
                app.m.action("twitch.marker", Value::map().with("description", form.marker.trim()));
                form.marker.clear();
            }
        });
    });
}
