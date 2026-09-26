//! Right rail (§15.4): Events (veto countdown), Chat (inline mod actions), Queue (now playing,
//! upcoming, pending approvals), Mod (policy approvals, AutoMod holds, audit log).
//! Data contract: docs/ui-data-contract.md.

use crate::app::App;
use crate::views::show::RailTab;
use egui::RichText;
use se_proto::{Event, Value};
use se_ui_kit::widgets::{self, LedState, icon};
use std::time::Instant;

#[derive(Default)]
pub struct RailState {
    pub queue_gen: u64,
    pub queue_asked: Option<Instant>,
    pub audit_asked: Option<Instant>,
    pub event_filter: String,
}

fn event_icon(ty: &str) -> &'static str {
    match ty.split('.').next().unwrap_or("") {
        "twitch" => icon::CHAT,
        "tip" | "kofi" | "alert" | "alerts" => icon::ALERT,
        "scene" => icon::SCENE,
        "preset" => icon::PRESET,
        "mode" => icon::LIVE,
        "patch" => icon::PATCH,
        "audio" | "music" | "drums" | "band" => icon::MIX,
        "lights" => icon::LIGHT,
        "queue" | "song" => icon::QUEUE,
        "mod" | "policy" => icon::MOD,
        "obs" => icon::REC,
        _ => icon::TRACE,
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn list<'a>(app: &'a App, a: &str) -> &'a [Value] {
    app.m.get(a).and_then(Value::as_list).unwrap_or(&[])
}

/// Policy queue entries of one kind (`veto` or `approval`), see `policy.pending`.
fn pending(app: &App, kind: &str) -> Vec<Value> {
    list(app, "policy.pending").iter().filter(|p| s(p, "kind") == kind).cloned().collect()
}

/// Items needing attention per tab (shown in the tab label).
pub fn badge_count(app: &App, tab: RailTab) -> usize {
    match tab {
        RailTab::Events => pending(app, "veto").len() + list(app, "alerts.veto").len(),
        RailTab::Queue => app.m.get("queue.pending").and_then(Value::as_i64).unwrap_or(0).max(0) as usize,
        RailTab::Mod => pending(app, "approval").len() + app.m.get("twitch.automod.held").and_then(Value::as_i64).unwrap_or(0).max(0) as usize,
        RailTab::Chat => 0,
    }
}

/// Remaining and total window (ms) of a policy entry from master-clock `created_ns`/`expires_ns`.
pub fn window_ms(p: &Value, now_ns: u64) -> (f64, f64) {
    let n = |k: &str| p.get_path(k).and_then(Value::as_f64).unwrap_or(0.0);
    let (created, expires) = (n("created_ns"), n("expires_ns"));
    let rem = ((expires - now_ns as f64) / 1e6).max(0.0);
    let total = ((expires - created) / 1e6).max(1.0);
    (rem, total)
}

fn policy_card(app: &mut App, ui: &mut egui::Ui, p: &Value, veto: bool) {
    let t = app.t.clone();
    let id = p.get_path("id").cloned().unwrap_or_default();
    let (rem, total) = window_ms(p, se_clock::now());
    let what = if s(p, "reward").is_empty() { s(p, "type").to_string() } else { format!("{} · {}", s(p, "type"), s(p, "reward")) };
    let amount = p.get_path("amount").filter(|v| !v.is_null()).map(|v| format!(" · {v}")).unwrap_or_default();
    widgets::card(ui, &t, if veto { icon::ALERT } else { icon::MOD }, &format!("{what} · {}{amount}", s(p, "user")), LedState::Armed, |ui| {
        if !s(p, "text").is_empty() {
            ui.label(RichText::new(s(p, "text")).color(t.fg));
        }
        ui.horizontal(|ui| {
            if p.get_path("expires_ns").is_some() {
                ui.add(egui::ProgressBar::new((rem / total).clamp(0.0, 1.0) as f32).desired_width(140.0).text(format!("{:.1}s", rem / 1000.0)));
            }
            let (no, yes) = if veto { ("veto", "pass now") } else { ("reject", "approve") };
            if ui.button(RichText::new(format!("{} {no}", icon::CROSS)).color(t.bright_red)).clicked() {
                app.m.action("mod.reject", Value::map().with("id", id.clone()));
            }
            if ui.button(format!("{} {yes}", icon::CHECK)).clicked() {
                app.m.action("mod.approve", Value::map().with("id", id.clone()));
            }
        });
    });
}

fn describe(e: &Event) -> String {
    let p = |k: &str| e.payload.get_path(k).map(|v| v.to_string()).unwrap_or_default();
    match e.ty.as_str() {
        "twitch.cheer" => format!(" {} bits", p("bits")),
        "twitch.raid" => format!(" {} viewers", p("viewers")),
        "twitch.gift" | "twitch.gift_bomb" => format!(" ×{}", p("count")),
        "twitch.sub" => format!(" tier {}", p("tier")),
        "twitch.redeem" => format!(" {}", p("reward")),
        "tip" => format!(" {} {}", p("amount"), p("currency")),
        "scene.take" | "mode.changed" => format!(" → {}", p("to")),
        _ => String::new(),
    }
}

pub fn events(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    // Veto windows (§12.1, §14.2): policy items (chat/points/bits with user text) and queued
    // alerts containing user text wait here before they reach the stream.
    let veto = pending(app, "veto");
    let alerts: Vec<Value> = list(app, "alerts.veto").to_vec();
    if !veto.is_empty() || !alerts.is_empty() {
        widgets::section(ui, &t, icon::MOD, "Veto window");
        for v in &veto {
            policy_card(app, ui, v, true);
        }
        for a in &alerts {
            let id = a.get_path("id").cloned().unwrap_or_default();
            let rem = a.get_path("remaining_ms").and_then(Value::as_f64).unwrap_or(0.0);
            let win = a.get_path("window_ms").and_then(Value::as_f64).unwrap_or(rem).max(1.0);
            widgets::card(ui, &t, icon::ALERT, &format!("alert {} · {}", s(a, "kind"), s(a, "user")), LedState::Armed, |ui| {
                if !s(a, "text").is_empty() {
                    ui.label(RichText::new(s(a, "text")).color(t.fg));
                }
                ui.horizontal(|ui| {
                    ui.add(egui::ProgressBar::new((rem / win).clamp(0.0, 1.0) as f32).desired_width(140.0).text(format!("{:.1}s", rem / 1000.0)));
                    if ui.button(RichText::new(format!("{} veto", icon::CROSS)).color(t.bright_red)).clicked() {
                        app.m.action("alerts.veto", Value::map().with("id", id.clone()));
                    }
                    if ui.button(format!("{} pass now", icon::CHECK)).clicked() {
                        app.m.action("alerts.approve", Value::map().with("id", id.clone()));
                    }
                });
            });
        }
        ui.separator();
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
    ui.add(egui::TextEdit::singleline(&mut app.show.rail_state.event_filter).hint_text(format!("{} filter events", icon::SEARCH)).desired_width(f32::INFINITY));
    let f = app.show.rail_state.event_filter.to_lowercase();
    let mut want_trace = None;
    egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        let mut shown = 0;
        for e in app.m.events.iter().filter(|e| !e.ty.ends_with(".trigger") || e.actor.is_some()) {
            if e.ty.starts_with("twitch.chat") || e.ty == "beat" || e.ty.starts_with("band.") || e.ty.starts_with("midi.") {
                continue;
            }
            let who = e.actor.as_ref().map(|a| format!(" · {}", a.name)).unwrap_or_default();
            let line = format!("{} {}{who}{}", event_icon(&e.ty), e.ty, describe(e));
            if !f.is_empty() && !line.to_lowercase().contains(&f) {
                continue;
            }
            shown += 1;
            let r = ui.add(egui::Label::new(RichText::new(line).color(if e.actor.is_some() { t.fg } else { t.fg_dim })).sense(egui::Sense::click()));
            if r.on_hover_text("click: show what this caused").clicked() {
                want_trace = Some(e.id);
            }
        }
        if shown == 0 {
            ui.label(RichText::new("no events yet — try the simulator (Build mode)").color(t.fg_dim));
        }
    });
    if let Some(id) = want_trace {
        app.m.trace_of(id);
        app.mode = crate::app::Mode::Build;
        app.focus_panel(crate::panels::Panel::Trace);
    }
}

pub fn chat(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    // Chat after removals: deleted messages, purged users (ban/timeout/clear), and full clears.
    let mut msgs: Vec<&se_proto::Event> = Vec::new();
    for e in app.m.events.iter() {
        match e.ty.as_str() {
            "twitch.chat" => msgs.push(e),
            "twitch.chat.delete" => {
                let id = e.payload.get_path("message_id").and_then(Value::as_str);
                msgs.retain(|m| m.payload.get_path("message_id").and_then(Value::as_str) != id);
            }
            "twitch.user.purge" => {
                let uid = e.payload.get_path("user_id").and_then(Value::as_str);
                msgs.retain(|m| m.payload.get_path("user_id").and_then(Value::as_str) != uid && m.actor.as_ref().map(|a| a.id.as_str()) != uid);
            }
            "twitch.chat.clear" => msgs.clear(),
            _ => {}
        }
    }
    let msgs: Vec<se_proto::Event> = msgs.into_iter().cloned().collect();
    let f = app.show.chat_filter.to_lowercase();
    ui.add(egui::TextEdit::singleline(&mut app.show.chat_filter).hint_text(format!("{} search chat", icon::SEARCH)).desired_width(f32::INFINITY));
    let bottom = 34.0;
    let mut act: Option<(&'static str, Value)> = None;
    egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).max_height((ui.available_height() - bottom).max(60.0)).show(ui, |ui| {
        let mut any = false;
        for e in &msgs {
            let id = e.payload.get_path("message_id").and_then(Value::as_str).unwrap_or("");
            let mut actor = e.actor.clone().unwrap_or_default();
            if actor.name.is_empty() {
                actor.name = s(&e.payload, "user").to_string();
            }
            if actor.id.is_empty() {
                actor.id = s(&e.payload, "user_id").to_string();
            }
            let msg = e.payload.get_path("message").map(|v| v.as_str().map(String::from).unwrap_or_else(|| v.to_string())).unwrap_or_default();
            if !f.is_empty() && !msg.to_lowercase().contains(&f) && !actor.name.to_lowercase().contains(&f) {
                continue;
            }
            any = true;
            let color = e.payload.get_path("color").and_then(Value::as_str).and_then(se_ui_kit::theme::hex).unwrap_or(t.accent);
            let row = ui.horizontal_wrapped(|ui| {
                let roles: String = actor
                    .roles
                    .iter()
                    .filter_map(|r| match r {
                        se_proto::Role::Owner => Some("\u{f03d}"),
                        se_proto::Role::Mod => Some(icon::MOD),
                        se_proto::Role::Vip => Some("\u{f005}"),
                        se_proto::Role::Sub => Some("\u{f004}"),
                        _ => None,
                    })
                    .collect();
                if !roles.is_empty() {
                    ui.label(RichText::new(roles).small().color(t.fg_dim));
                }
                ui.label(RichText::new(&actor.name).strong().color(color));
                ui.label(&msg);
            });
            row.response.context_menu(|ui| {
                ui.label(RichText::new(format!("{} {}", icon::MOD, actor.name)).strong());
                if ui.button(format!("{} delete message", icon::CROSS)).clicked() {
                    act = Some(("mod.delete", Value::map().with("message_id", id)));
                    ui.close();
                }
                for (label, secs) in [("timeout 1m", 60), ("timeout 10m", 600), ("timeout 1h", 3600)] {
                    if ui.button(label).clicked() {
                        act = Some(("mod.timeout", Value::map().with("user_id", actor.id.clone()).with("user", actor.name.clone()).with("duration_s", secs)));
                        ui.close();
                    }
                }
                ui.separator();
                if widgets::hold_button(ui, &t, "hold: BAN", t.bright_red, 0.8) {
                    act = Some(("mod.ban", Value::map().with("user_id", actor.id.clone()).with("user", actor.name.clone())));
                    ui.close();
                }
            });
            row.response.on_hover_text("right-click: moderate");
        }
        if !any {
            ui.label(
                RichText::new(if app.m.b("twitch.eventsub.connected") { "no chat messages yet" } else { "Twitch not connected (Settings → Accounts)" })
                    .color(t.fg_dim),
            );
        }
    });
    if let Some((n, a)) = act {
        app.m.action(n, a);
    }
    ui.horizontal(|ui| {
        let r = ui.add(egui::TextEdit::singleline(&mut app.show.chat_input).hint_text("say as the bot…").desired_width(ui.available_width() - 60.0));
        let send = ui.button("send").clicked() || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
        if send && !app.show.chat_input.trim().is_empty() {
            let text = std::mem::take(&mut app.show.chat_input);
            app.m.action("bot.say", Value::map().with("args", vec![text.trim().to_string()]));
        }
    });
}

fn queue_entry(ui: &mut egui::Ui, t: &se_ui_kit::Theme, e: &Value) {
    let dur = e.get_path("duration").and_then(Value::as_f64).unwrap_or(0.0);
    ui.label(RichText::new(s(e, "title")).strong().color(t.fg));
    ui.label(
        RichText::new(format!(
            "{} · {} · {}:{:02}{}",
            s(e, "user"),
            s(e, "channel"),
            dur as i64 / 60,
            dur as i64 % 60,
            if e.get_path("paid").is_some_and(Value::truthy) { " · paid" } else { "" }
        ))
        .small()
        .color(t.fg_dim),
    );
}

pub fn queue(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let qgen = app.m.gen_of("queue") + app.m.gen_of("song");
    let stale = app.show.rail_state.queue_asked.is_none_or(|a| a.elapsed().as_secs() >= 5);
    if app.m.connected && (qgen != app.show.rail_state.queue_gen || stale) {
        app.show.rail_state.queue_gen = qgen;
        app.show.rail_state.queue_asked = Some(Instant::now());
        app.m.query("queue", Value::Null);
    }
    let Some(q) = app.m.q("queue").cloned() else {
        let why = app.m.query_errors.get("queue").cloned().unwrap_or_else(|| "waiting for the song queue…".into());
        ui.label(RichText::new(why).color(t.fg_dim));
        return;
    };
    let mut act: Option<(&'static str, Value)> = None;
    ui.horizontal(|ui| {
        let open = q.get_path("open").is_some_and(Value::truthy);
        let paused = q.get_path("paused").is_some_and(Value::truthy);
        widgets::pill(ui, &t, icon::QUEUE, if open { "open" } else { "closed" }, if open { LedState::Healthy } else { LedState::Idle });
        if ui.small_button(if open { "close" } else { "open" }).clicked() {
            act = Some((if open { "queue.close" } else { "queue.open" }, Value::Null));
        }
        if ui.small_button(if paused { format!("{} resume", icon::PLAY) } else { format!("{} pause", icon::STOP) }).clicked() {
            act = Some((if paused { "queue.resume" } else { "queue.pause" }, Value::Null));
        }
        if ui.small_button("skip").clicked() {
            act = Some(("queue.skip", Value::Null));
        }
        if let Some(qt) = q.get_path("quota") {
            let rem = qt.get_path("remaining").and_then(Value::as_i64).unwrap_or(0);
            ui.label(RichText::new(format!("quota {rem}")).small().color(if rem < 500 { t.yellow } else { t.fg_dim }));
        }
    });
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        widgets::section(ui, &t, icon::PLAY, "Now playing");
        match q.get_path("now").filter(|v| !v.is_null()) {
            Some(now) => {
                queue_entry(ui, &t, now);
                let dur = now.get_path("duration").and_then(Value::as_f64).unwrap_or(0.0);
                let pos = app.m.sig("song.position").map(f64::from).or_else(|| now.get_path("position").and_then(Value::as_f64)).unwrap_or(0.0);
                if dur > 0.0 {
                    ui.add(egui::ProgressBar::new((pos / dur).clamp(0.0, 1.0) as f32).text(format!(
                        "{}:{:02} / {}:{:02}",
                        pos as i64 / 60,
                        pos as i64 % 60,
                        dur as i64 / 60,
                        dur as i64 % 60
                    )));
                }
            }
            None => {
                ui.label(RichText::new("nothing playing").color(t.fg_dim));
            }
        }
        let pending = q.get_path("pending").and_then(Value::as_list).unwrap_or(&[]).to_vec();
        if !pending.is_empty() {
            widgets::section(ui, &t, icon::MOD, &format!("Pending approval ({})", pending.len()));
            for e in &pending {
                let id = e.get_path("id").cloned().unwrap_or_default();
                widgets::card(ui, &t, icon::QUEUE, s(e, "user"), LedState::Armed, |ui| {
                    queue_entry(ui, &t, e);
                    ui.horizontal(|ui| {
                        if ui.button(format!("{} approve", icon::CHECK)).clicked() {
                            act = Some(("queue.approve", Value::map().with("id", id.clone())));
                        }
                        if ui.button(format!("{} reject", icon::CROSS)).clicked() {
                            act = Some(("queue.reject", Value::map().with("id", id.clone())));
                        }
                    });
                });
            }
        }
        let upcoming = q.get_path("upcoming").and_then(Value::as_list).unwrap_or(&[]).to_vec();
        widgets::section(ui, &t, icon::QUEUE, &format!("Upcoming ({})", upcoming.len()));
        if upcoming.is_empty() {
            ui.label(RichText::new("queue is empty").color(t.fg_dim));
        }
        let n = upcoming.len();
        for (i, e) in upcoming.iter().enumerate() {
            let id = e.get_path("id").cloned().unwrap_or_default();
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{}", i + 1)).monospace().color(t.fg_dim));
                ui.vertical(|ui| queue_entry(ui, &t, e));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button(icon::CROSS).on_hover_text("remove").clicked() {
                        act = Some(("queue.remove", Value::map().with("id", id.clone())));
                    }
                    if i + 1 < n && ui.small_button("▼").clicked() {
                        act = Some(("queue.reorder", Value::map().with("id", id.clone()).with("to", (i + 2) as i64)));
                    }
                    if i > 0 && ui.small_button("▲").clicked() {
                        act = Some(("queue.reorder", Value::map().with("id", id.clone()).with("to", i as i64)));
                    }
                });
            });
        }
    });
    if let Some((n, a)) = act {
        app.m.action(n, a);
        app.show.rail_state.queue_asked = None;
    }
}

pub fn moderation(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let mut act: Option<(&'static str, Value)> = None;
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        let approvals = pending(app, "approval");
        widgets::section(ui, &t, icon::MOD, &format!("Needs approval ({})", approvals.len()));
        if approvals.is_empty() {
            ui.label(RichText::new("nothing waiting").color(t.fg_dim));
        }
        for p in &approvals {
            policy_card(app, ui, p, false);
        }
        let held = list(app, "twitch.automod.queue").to_vec();
        widgets::section(ui, &t, icon::WARN, &format!("AutoMod holds ({})", held.len()));
        if held.is_empty() {
            ui.label(RichText::new("no held messages").color(t.fg_dim));
        }
        for h in &held {
            let id = h.get_path("message_id").cloned().unwrap_or_default();
            widgets::card(ui, &t, icon::CHAT, s(h, "user"), LedState::Armed, |ui| {
                ui.label(s(h, "text"));
                let cat = if s(h, "category").is_empty() { s(h, "reason") } else { s(h, "category") };
                ui.label(RichText::new(format!("{cat} (level {})", h.get_path("level").map(|v| v.to_string()).unwrap_or_default())).small().color(t.fg_dim));
                ui.horizontal(|ui| {
                    if ui.button(format!("{} allow", icon::CHECK)).clicked() {
                        act = Some(("mod.automod.approve", Value::map().with("message_id", id.clone())));
                    }
                    if ui.button(format!("{} deny", icon::CROSS)).clicked() {
                        act = Some(("mod.automod.deny", Value::map().with("message_id", id.clone())));
                    }
                });
            });
        }
        widgets::section(ui, &t, icon::SESSION, "Audit log");
        if app.m.connected && app.show.rail_state.audit_asked.is_none_or(|a| a.elapsed().as_secs() >= 3) {
            app.show.rail_state.audit_asked = Some(Instant::now());
            app.m.query("audit", Value::map().with("n", 100));
        }
        let audit = app.m.q_list("audit").to_vec();
        if audit.is_empty() {
            ui.label(RichText::new("audit log is empty").color(t.fg_dim));
        }
        egui::Grid::new("audit").striped(true).num_columns(3).show(ui, |ui| {
            for a in audit.iter().take(100) {
                let ok = a.get_path("ok").is_some_and(Value::truthy);
                let who = format!("{}{}", s(a, "origin"), if s(a, "actor").is_empty() { String::new() } else { format!(" · {}", s(a, "actor")) });
                ui.label(RichText::new(if ok { icon::CHECK } else { icon::CROSS }).color(if ok { t.green } else { t.bright_red }));
                ui.label(RichText::new(who).small().color(t.fg_dim));
                let op = RichText::new(s(a, "op")).monospace();
                if ok {
                    ui.label(op);
                } else {
                    ui.label(op).on_hover_text(s(a, "error"));
                }
                ui.end_row();
            }
        });
    });
    if let Some((n, a)) = act {
        app.m.action(n, a);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_window_uses_master_clock_deadlines() {
        let p = Value::map().with("created_ns", 1_000_000_000i64).with("expires_ns", 11_000_000_000i64);
        let (rem, total) = window_ms(&p, 6_000_000_000);
        assert_eq!((rem, total), (5000.0, 10000.0));
        assert_eq!(window_ms(&p, 20_000_000_000).0, 0.0, "expired entries show 0, never negative");
    }
}
