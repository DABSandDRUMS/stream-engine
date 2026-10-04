//! Right rail on the Live page (§15.4): pinned Chat (hover or right-click a message to
//! moderate) above Songs (compact queue), Activity (friendly event cards and veto windows),
//! or Mod (things waiting for your OK). Tabs select only the lower pane.
//! Data contract: docs/ui-data-contract.md.

use crate::app::App;
use crate::views::live::nice;
use crate::views::show::RailTab;
use egui::{Align, Color32, CornerRadius, Layout, Rect, RichText, Sense, Stroke, Vec2};
use se_proto::{Event, Role, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_medium, font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::time::Instant;

#[derive(Default)]
pub struct RailState {
    pub queue_gen: u64,
    pub queue_asked: Option<Instant>,
    pub audit_asked: Option<Instant>,
    pub event_filter: String,
    /// Activity also lists behind-the-scenes events (scene switches, effects, lights…).
    pub show_all: bool,
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

// ---- shared words & bits ------------------------------------------------------------------------

fn num(x: f64) -> String {
    if x.fract() == 0.0 { format!("{}", x as i64) } else { format!("{x:.2}") }
}

/// Thousands separators for counts people read ("10,000").
pub fn grouped(n: i64) -> String {
    let d = n.abs().to_string();
    let mut out = String::new();
    for (k, c) in d.chars().enumerate() {
        if k > 0 && (d.len() - k).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    if n < 0 { format!("-{out}") } else { out }
}

/// `$5.00`, `€3.50`, `12.00 SEK`.
pub fn money(amount: f64, currency: &str) -> String {
    match currency.to_ascii_uppercase().as_str() {
        "" | "USD" => format!("${amount:.2}"),
        "EUR" => format!("€{amount:.2}"),
        "GBP" => format!("£{amount:.2}"),
        c => format!("{amount:.2} {c}"),
    }
}

/// A viewer event in plain words ("drumfan cheered 500 bits") with its icon. `None` for
/// behind-the-scenes events.
pub fn phrase(ty: &str, user: &str, p: &Value) -> Option<(&'static str, String)> {
    let n = |k: &str| p.get_path(k).and_then(Value::as_f64).unwrap_or(0.0);
    let who = if user.is_empty() { "Someone" } else { user };
    Some(match ty {
        "twitch.follow" => (icon::HEART, format!("{who} followed")),
        "twitch.sub" => {
            let tier = n("tier") as i64;
            (icon::STAR, if tier > 1 { format!("{who} subscribed at tier {tier}") } else { format!("{who} subscribed") })
        }
        "twitch.resub" => (icon::STAR, format!("{who} resubscribed for {} months", num(n("months")))),
        "twitch.gift" | "twitch.gift_bomb" => {
            let c = n("count").max(1.0) as i64;
            (icon::GIFT, if c == 1 { format!("{who} gifted a sub") } else { format!("{who} gifted {c} subs") })
        }
        "twitch.cheer" => (icon::BOLT, format!("{who} cheered {} bits", num(n("bits").max(n("amount"))))),
        "twitch.raid" => (icon::USERS, format!("{who} raided with {} viewers", num(n("viewers").max(n("amount"))))),
        "twitch.redeem" => (icon::STAR, format!("{who} redeemed {}", s(p, "reward"))),
        "twitch.chat" => (icon::CHAT, format!("{who} wrote in chat")),
        "tip" | "kofi.tip" | "kofi.donation" => (icon::HEART, format!("{who} tipped {}", money(n("amount"), s(p, "currency")))),
        "giveaway.entered" => (icon::GIFT, format!("{who} entered the giveaway")),
        "giveaway.drawn" => (icon::GIFT, format!("{who} won the giveaway")),
        "queue.song_requested" => (icon::MUSIC, format!("{who} asked for a song")),
        _ => return None,
    })
}

/// Behind-the-scenes event in words ("Switched to Main").
fn system_line(e: &Event) -> String {
    let to = e.payload.get_path("to").and_then(Value::as_str).unwrap_or("");
    match e.ty.as_str() {
        "scene.take" => format!("Switched to {}", nice(to)),
        "mode.changed" => format!("Show mode: {}", nice(to)),
        ty => nice(&ty.replace('.', " ")),
    }
}

fn ago(ts: u64) -> String {
    let secs = se_clock::now().saturating_sub(ts) / 1_000_000_000;
    match secs {
        0..=9 => "just now".into(),
        10..=59 => format!("{secs} s ago"),
        60..=3599 => format!("{} min ago", secs / 60),
        _ => format!("{} h ago", secs / 3600),
    }
}

/// A thin progress bar across the available width.
fn thin_bar(ui: &mut egui::Ui, t: &Theme, frac: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 4.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(2), t.inset);
    let mut fill = rect;
    fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
    ui.painter().rect_filled(fill, CornerRadius::same(2), color);
}

/// Card tinted with a status color (things waiting for the streamer).
fn attention_card<R>(ui: &mut egui::Ui, t: &Theme, color: Color32, body: impl FnOnce(&mut egui::Ui) -> R) -> R {
    let r = egui::Frame::new()
        .fill(mix(t.surface, color, 0.07))
        .stroke(Stroke::new(1.0, mix(t.border, color, 0.45)))
        .corner_radius(CornerRadius::same(radius::CARD))
        .inner_margin(egui::Margin::same(spacing::M as i8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            body(ui)
        })
        .inner;
    ui.add_space(spacing::S);
    r
}

fn quote(ui: &mut egui::Ui, t: &Theme, text: &str) {
    if !text.is_empty() {
        ui.add(egui::Label::new(RichText::new(format!("“{text}”")).size(type_scale::BODY).color(t.fg)).wrap());
    }
}

/// "Show this alert?" card: headline, viewer text, countdown, Skip / Show now.
/// Returns `Some(true)` for Show now, `Some(false)` for Skip.
fn veto_card(ui: &mut egui::Ui, t: &Theme, question: &str, headline: &str, text: &str, rem_ms: f64, total_ms: f64) -> Option<bool> {
    let mut out = None;
    attention_card(ui, t, t.yellow, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(question).font(font_semibold(type_scale::SMALL + 0.5)).color(mix(t.yellow, t.fg, 0.3)));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("{:.1} s", rem_ms / 1000.0)).font(font_mono(type_scale::SMALL)).color(t.text_dim));
            });
        });
        ui.add(egui::Label::new(RichText::new(headline).font(font_semibold(type_scale::BODY + 0.5)).color(t.fg)).wrap());
        quote(ui, t, text);
        ui.add_space(spacing::XS);
        thin_bar(ui, t, (rem_ms / total_ms.max(1.0)) as f32, t.yellow);
        ui.add_space(spacing::S);
        ui.horizontal(|ui| {
            if widgets::button_ex(ui, t, Some(icon::CROSS), "Skip", Kind::Danger, Size::Medium, 0.0, true).on_hover_text("Don't show it on stream").clicked() {
                out = Some(false);
            }
            if widgets::button_ex(ui, t, Some(icon::CHECK), "Show now", Kind::Secondary, Size::Medium, 0.0, true).clicked() {
                out = Some(true);
            }
        });
    });
    out
}

/// Headline for a policy entry (`{type,user,reward,amount,text}`).
fn policy_headline(p: &Value) -> (&'static str, String) {
    let amount = p.get_path("amount").and_then(Value::as_f64).unwrap_or(0.0);
    if s(p, "type") == "twitch.redeem" && amount > 0.0 {
        return (icon::STAR, format!("{} redeemed {} ({} points)", s(p, "user"), s(p, "reward"), grouped(amount as i64)));
    }
    phrase(s(p, "type"), s(p, "user"), p).unwrap_or_else(|| {
        let what = if s(p, "reward").is_empty() { nice(&s(p, "type").replace('.', " ")) } else { s(p, "reward").to_string() };
        (icon::MOD, format!("{} · {what}", if s(p, "user").is_empty() { "Someone" } else { s(p, "user") }))
    })
}

// ---- Activity ------------------------------------------------------------------------------------

pub fn events(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    // Veto windows (§12.1, §14.2): policy items (chat/points/bits with viewer text) and queued
    // alerts containing viewer text wait here before they reach the stream.
    let veto = pending(app, "veto");
    let alerts: Vec<Value> = list(app, "alerts.veto").to_vec();
    let mut act: Option<(&'static str, Value)> = None;
    if !veto.is_empty() || !alerts.is_empty() {
        for a in &alerts {
            let id = a.get_path("id").cloned().unwrap_or_default();
            let rem = a.get_path("remaining_ms").and_then(Value::as_f64).unwrap_or(0.0);
            let win = a.get_path("window_ms").and_then(Value::as_f64).unwrap_or(rem);
            let headline = if s(a, "title").is_empty() {
                phrase(&format!("twitch.{}", s(a, "kind")), s(a, "user"), a)
                    .map(|(_, l)| l)
                    .unwrap_or_else(|| format!("{} · {}", nice(s(a, "kind")), s(a, "user")))
            } else {
                s(a, "title").to_string()
            };
            match veto_card(ui, &t, "Show this alert?", &headline, s(a, "text"), rem, win) {
                Some(true) => act = Some(("alerts.approve", Value::map().with("id", id))),
                Some(false) => act = Some(("alerts.veto", Value::map().with("id", id))),
                None => {}
            }
        }
        for p in &veto {
            let id = p.get_path("id").cloned().unwrap_or_default();
            let (rem, total) = window_ms(p, se_clock::now());
            let (_, headline) = policy_headline(p);
            match veto_card(ui, &t, "Show this on stream?", &headline, s(p, "text"), rem, total) {
                Some(true) => act = Some(("mod.approve", Value::map().with("id", id))),
                Some(false) => act = Some(("mod.reject", Value::map().with("id", id))),
                None => {}
            }
        }
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
    if let Some((n, a)) = act {
        app.m.action(n, a);
    }

    ui.horizontal(|ui| {
        let mut idx = usize::from(app.show.rail_state.show_all);
        if widgets::segmented(ui, &t, &mut idx, &["Viewers", "Everything"]) {
            app.show.rail_state.show_all = idx == 1;
        }
        ui.add(
            se_ui_kit::widgets::field(&mut app.show.rail_state.event_filter).hint_text(format!("{}  Search", icon::SEARCH)).desired_width(ui.available_width()),
        );
    });
    ui.add_space(spacing::S);
    let f = app.show.rail_state.event_filter.to_lowercase();
    let show_all = app.show.rail_state.show_all;
    let mut want_trace = None;
    egui::ScrollArea::vertical().id_salt("rail-activity").auto_shrink([false, false]).show(ui, |ui| {
        let mut shown = 0;
        for e in app.m.events.iter().rev() {
            if e.ty.starts_with("twitch.chat") || e.ty == "beat" || e.ty.starts_with("band.") || e.ty.starts_with("midi.") {
                continue;
            }
            if e.ty.ends_with(".trigger") && e.actor.is_none() {
                continue;
            }
            let who = e.actor.as_ref().map(|a| a.name.as_str()).filter(|n| !n.is_empty()).unwrap_or_else(|| s(&e.payload, "user"));
            let viewer = phrase(&e.ty, who, &e.payload);
            if viewer.is_none() && !show_all {
                continue;
            }
            let (ic, line) = viewer.unwrap_or_else(|| (icon::DOT, system_line(e)));
            let msg = s(&e.payload, "message");
            if !f.is_empty() && !line.to_lowercase().contains(&f) && !msg.to_lowercase().contains(&f) {
                continue;
            }
            shown += 1;
            if shown > 200 {
                break;
            }
            let is_viewer = ic != icon::DOT;
            let r = activity_row(ui, &t, ic, &line, msg, &ago(e.ts), is_viewer);
            if r.on_hover_text("Click to see what this caused").clicked() {
                want_trace = Some(e.id);
            }
        }
        if shown == 0 {
            if !f.is_empty() {
                widgets::empty_state(ui, &t, icon::SEARCH, "No matches", "Nothing in the feed matches your search.", None);
            } else {
                widgets::empty_state(ui, &t, icon::ALERT, "No activity yet", "Follows, subs, cheers, raids and tips show up here as they happen.", None);
            }
        }
    });
    if let Some(id) = want_trace {
        app.m.trace_of(id);
        app.focus_panel(crate::panels::Panel::Trace);
    }
}

/// One feed entry: round icon, headline, optional viewer message, time.
fn activity_row(ui: &mut egui::Ui, t: &Theme, ic: &str, line: &str, msg: &str, when: &str, viewer: bool) -> egui::Response {
    let bg = ui.painter().add(egui::Shape::Noop);
    let inner = egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_top(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
            let c = if viewer { t.accent } else { t.text_faint };
            ui.painter().circle_filled(r.center(), 14.0, mix(t.bg, c, if viewer { 0.18 } else { 0.1 }));
            ui.painter().text(r.center(), egui::Align2::CENTER_CENTER, ic, se_ui_kit::theme::font(if viewer { 13.0 } else { 7.0 }), c);
            ui.add_space(spacing::S);
            ui.vertical(|ui| {
                let fid = if viewer { font_medium(type_scale::BODY) } else { se_ui_kit::theme::font(type_scale::SMALL + 0.5) };
                ui.add(egui::Label::new(RichText::new(line).font(fid).color(if viewer { t.fg } else { t.text_dim })).wrap());
                if !msg.is_empty() {
                    ui.add(egui::Label::new(RichText::new(format!("“{msg}”")).size(type_scale::SMALL + 0.5).color(t.text_dim)).wrap());
                }
                ui.label(RichText::new(when).size(type_scale::SMALL).color(t.text_faint));
            });
        });
    });
    let rect = inner.response.rect;
    let resp = ui.interact(rect, ui.id().with(("activity", rect.min.x as i32, rect.min.y as i32)), Sense::click());
    if resp.hovered() {
        ui.painter().set(bg, egui::Shape::rect_filled(rect, CornerRadius::same(radius::CONTROL), t.surface_hi));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

// ---- Chat ----------------------------------------------------------------------------------------

/// Twitch name colors can be too dark (or too light) for the theme; nudge them toward the text.
fn readable(t: &Theme, c: Color32) -> Color32 {
    let lum = (0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32) / 255.0;
    if !t.light && lum < 0.45 {
        mix(c, t.fg, 0.45)
    } else if t.light && lum > 0.6 {
        mix(c, Color32::BLACK, 0.35)
    } else {
        c
    }
}

fn role_marks(t: &Theme, roles: &[Role]) -> Vec<(&'static str, Color32, &'static str)> {
    roles
        .iter()
        .filter_map(|r| match r {
            Role::Owner => Some((icon::LIVE, t.bright_red, "Streamer")),
            Role::Mod => Some((icon::MOD, t.green, "Moderator")),
            Role::Vip => Some((icon::STAR, t.magenta, "VIP")),
            Role::Sub => Some((icon::HEART, t.accent, "Subscriber")),
            _ => None,
        })
        .collect()
}

pub fn chat(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let tw = crate::views::twitch::twitch_state(app);
    let connected = tw.connected();
    let (audience, color) = if !app.m.connected || !connected {
        ("Viewers unavailable".to_string(), t.text_dim)
    } else {
        match app.m.get("twitch.stream.live") {
            Some(Value::Bool(false)) => ("Offline".to_string(), t.text_dim),
            Some(Value::Bool(true)) => match app.m.get("twitch.stream.viewers").and_then(Value::as_i64).filter(|n| *n >= 0) {
                Some(n) => (format!("{} watching", grouped(n)), t.green),
                None => ("Viewers unavailable".to_string(), t.text_dim),
            },
            _ => ("Viewers unavailable".to_string(), t.text_dim),
        }
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new("Twitch").size(type_scale::SMALL).color(t.text_dim));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.label(RichText::new(audience).font(font_medium(type_scale::BODY)).color(color))
                .on_hover_text("Current Twitch viewers, not chat participants. Refreshes automatically with Twitch's viewer updates.");
            ui.label(RichText::new(icon::USERS).size(type_scale::BODY).color(color));
        });
    });
    ui.add_space(spacing::XS);
    let msgs = &app.m.chat_messages;
    if !msgs.is_empty() {
        ui.add(se_ui_kit::widgets::field(&mut app.show.chat_filter).hint_text(format!("{}  Search chat", icon::SEARCH)).desired_width(f32::INFINITY));
        ui.add_space(spacing::XS);
    }
    let f = app.show.chat_filter.to_lowercase();
    let bottom = if connected { 36.0 + spacing::S + 2.0 * ui.spacing().item_spacing.y } else { 0.0 };
    let mut act: Option<(&'static str, Value)> = None;
    let mut open_twitch = false;
    egui::ScrollArea::vertical()
        .id_salt("rail-chat")
        .stick_to_bottom(true)
        .auto_shrink([false, false])
        .min_scrolled_height(0.0)
        .max_height((ui.available_height() - bottom).max(0.0))
        .show(ui, |ui| {
            let mut any = false;
            for (n, e) in msgs.iter().enumerate() {
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
                if let Some(a) = chat_message(ui, &t, n, e, &actor, &msg) {
                    act = Some(a);
                }
            }
            if !any {
                if !f.is_empty() {
                    widgets::empty_state(ui, &t, icon::SEARCH, "No matches", "No message matches your search.", None);
                } else if connected {
                    widgets::empty_state(ui, &t, icon::CHAT, "Chat is quiet", "Messages from your viewers show up here.", None);
                } else {
                    open_twitch = widgets::empty_state(ui, &t, icon::UNLINK, &tw.title, &tw.body, tw.action());
                }
            }
        });
    if let Some((n, a)) = act {
        app.m.action(n, a);
    }
    if open_twitch {
        crate::views::twitch::twitch_do(app, ui.ctx(), &tw);
    }
    // You can only talk in chat once Twitch is connected.
    if !connected {
        return;
    }
    ui.add_space(spacing::S);
    ui.horizontal(|ui| {
        let w = ui.available_width() - 84.0;
        let r = ui.add(se_ui_kit::widgets::field(&mut app.show.chat_input).hint_text("Say something as your bot…").desired_width(w));
        let ready = !app.show.chat_input.trim().is_empty();
        let clicked = widgets::button_ex(ui, &t, None, "Send", Kind::Primary, Size::Medium, 76.0, ready).clicked();
        let send = clicked || (r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
        if send && ready {
            let text = std::mem::take(&mut app.show.chat_input);
            app.m.action("bot.say", Value::map().with("args", vec![text.trim().to_string()]));
            r.request_focus();
        }
    });
}

/// One chat line: role marks, colored name, message (emotes as chips). Hover shows quick mod
/// buttons; right-click opens every mod action. Returns the chosen action.
fn chat_message(ui: &mut egui::Ui, t: &Theme, n: usize, e: &Event, actor: &se_proto::Actor, msg: &str) -> Option<(&'static str, Value)> {
    let mut act = None;
    let id = e.payload.get_path("message_id").and_then(Value::as_str).unwrap_or("").to_string();
    let color = readable(t, e.payload.get_path("color").and_then(Value::as_str).and_then(se_ui_kit::theme::hex).unwrap_or(t.accent));
    let bg = ui.painter().add(egui::Shape::Noop);
    let row = egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 5)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            ui.spacing_mut().item_spacing.x = 4.0;
            for (ic, c, tip) in role_marks(t, &actor.roles) {
                ui.label(RichText::new(ic).size(type_scale::SMALL).color(c)).on_hover_text(tip);
                ui.add_space(3.0);
            }
            ui.label(RichText::new(&actor.name).font(font_semibold(type_scale::BODY)).color(color));
            let frags = e.payload.get_path("fragments").and_then(Value::as_list).filter(|l| !l.is_empty());
            match frags {
                Some(frags) => {
                    for fr in frags {
                        let text = s(fr, "text");
                        match s(fr, "type") {
                            "emote" | "cheermote" => {
                                ui.label(
                                    RichText::new(text).font(font_medium(type_scale::SMALL)).color(t.accent).background_color(mix(t.surface, t.accent, 0.14)),
                                );
                            }
                            "mention" => {
                                ui.label(RichText::new(text).font(font_semibold(type_scale::BODY)).color(t.fg));
                            }
                            _ => {
                                ui.label(RichText::new(text).size(type_scale::BODY).color(t.fg));
                            }
                        }
                    }
                }
                None => {
                    ui.label(RichText::new(msg).size(type_scale::BODY).color(t.fg));
                }
            }
        });
    });
    let rect = row.response.rect;
    let resp = ui.interact(rect, ui.id().with(("chat-msg", n, &id)), Sense::click());
    let hovered = ui.rect_contains_pointer(rect);
    if hovered {
        ui.painter().set(bg, egui::Shape::rect_filled(rect, CornerRadius::same(radius::CONTROL), t.surface_hi));
        // Quick actions float at the top-right of the hovered message.
        let bar = Rect::from_min_size(egui::pos2(rect.right() - 76.0, rect.top() + 2.0), Vec2::new(72.0, 28.0));
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar).layout(Layout::right_to_left(Align::Center)));
        child.painter().rect(bar, CornerRadius::same(radius::CONTROL), t.surface, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
        child.add_space(2.0);
        if widgets::icon_button(&mut child, t, icon::CLOCK, "Time out for 10 minutes").clicked() {
            act = Some(("mod.timeout", Value::map().with("user_id", actor.id.clone()).with("user", actor.name.clone()).with("duration_s", 600)));
        }
        if widgets::icon_button(&mut child, t, icon::TRASH, "Delete this message").clicked() {
            act = Some(("mod.delete", Value::map().with("message_id", id.clone())));
        }
    }
    resp.context_menu(|ui| {
        ui.label(RichText::new(&actor.name).font(font_semibold(type_scale::BODY)).color(color));
        ui.separator();
        if ui.button(format!("{}  Delete message", icon::TRASH)).clicked() {
            act = Some(("mod.delete", Value::map().with("message_id", id.clone())));
            ui.close();
        }
        for (label, secs) in [("Time out 1 minute", 60), ("Time out 10 minutes", 600), ("Time out 1 hour", 3600)] {
            if ui.button(format!("{}  {label}", icon::CLOCK)).clicked() {
                act = Some(("mod.timeout", Value::map().with("user_id", actor.id.clone()).with("user", actor.name.clone()).with("duration_s", secs)));
                ui.close();
            }
        }
        ui.separator();
        if widgets::hold_button(ui, t, "Hold to ban", t.bright_red, 0.8) {
            act = Some(("mod.ban", Value::map().with("user_id", actor.id.clone()).with("user", actor.name.clone())));
            ui.close();
        }
    });
    if hovered {
        resp.on_hover_text("Right-click for more (time out, ban)");
    }
    act
}

// ---- Songs ---------------------------------------------------------------------------------------

/// Title-case heading above a list in the rail.
fn rail_heading(ui: &mut egui::Ui, t: &Theme, text: &str) {
    ui.label(RichText::new(text).font(font_semibold(type_scale::BODY)).color(t.fg));
    ui.add_space(spacing::XS);
}

fn mmss(secs: f64) -> String {
    let t = secs.max(0.0) as i64;
    format!("{}:{:02}", t / 60, t % 60)
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
        let why = app.m.query_errors.get("queue").cloned().unwrap_or_else(|| "Loading the song list…".into());
        widgets::hint(ui, &t, &why);
        return;
    };
    let mut act: Option<(&'static str, Value)> = None;
    let open = q.get_path("open").is_some_and(Value::truthy);
    let paused = q.get_path("paused").is_some_and(Value::truthy);
    ui.horizontal(|ui| {
        let mut on = open;
        if widgets::toggle(ui, &t, &mut on).changed() {
            act = Some((if on { "queue.open" } else { "queue.close" }, Value::Null));
        }
        ui.label(RichText::new(if open { "Taking requests" } else { "Requests closed" }).font(font_medium(type_scale::BODY)).color(t.fg));
        if let Some(qt) = q.get_path("quota") {
            let rem = qt.get_path("remaining").and_then(Value::as_i64).unwrap_or(0);
            if rem < 500 {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    widgets::badge(ui, &t, "Few searches left today", t.yellow);
                });
            }
        }
    });
    ui.add_space(spacing::S);
    egui::ScrollArea::vertical().id_salt("rail-songs").auto_shrink([false, false]).show(ui, |ui| {
        let pending = q.get_path("pending").and_then(Value::as_list).unwrap_or(&[]).to_vec();
        let upcoming = q.get_path("upcoming").and_then(Value::as_list).unwrap_or(&[]).to_vec();
        let now_playing = q.get_path("now").filter(|v| !v.is_null());
        if now_playing.is_none() && pending.is_empty() && upcoming.is_empty() {
            widgets::empty_state(
                ui,
                &t,
                icon::MUSIC,
                "No songs yet",
                if open { "Viewers ask for songs with !sr in chat." } else { "Turn on requests above so viewers can ask with !sr." },
                None,
            );
            return;
        }
        match now_playing {
            Some(now) => {
                widgets::panel(ui, &t, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(RichText::new("Now playing").font(font_semibold(type_scale::SMALL + 0.5)).color(t.text_dim));
                    ui.add(egui::Label::new(RichText::new(s(now, "title")).font(font_semibold(type_scale::BODY + 0.5)).color(t.fg)).wrap());
                    ui.label(RichText::new(format!("Asked for by {}", s(now, "user"))).size(type_scale::SMALL + 0.5).color(t.text_dim));
                    let dur = now.get_path("duration").and_then(Value::as_f64).unwrap_or(0.0);
                    let pos = app.m.sig("song.position").map(f64::from).or_else(|| now.get_path("position").and_then(Value::as_f64)).unwrap_or(0.0);
                    ui.add_space(spacing::XS);
                    if dur > 0.0 {
                        thin_bar(ui, &t, (pos / dur) as f32, t.accent);
                    }
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("{} / {}", mmss(pos), mmss(dur))).font(font_mono(type_scale::SMALL)).color(t.text_dim));
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::icon_button(ui, &t, icon::RIGHT, "Skip this song").clicked() {
                                act = Some(("queue.skip", Value::Null));
                            }
                            let (ic, tip, a) = if paused { (icon::PLAY, "Play", "queue.resume") } else { (icon::PAUSE, "Pause", "queue.pause") };
                            if widgets::icon_button(ui, &t, ic, tip).clicked() {
                                act = Some((a, Value::Null));
                            }
                        });
                    });
                });
            }
            None => {
                widgets::hint(ui, &t, "Nothing is playing right now.");
            }
        }
        if !pending.is_empty() {
            ui.add_space(spacing::M);
            rail_heading(ui, &t, &format!("Waiting for your OK ({})", pending.len()));
            for e in &pending {
                let id = e.get_path("id").cloned().unwrap_or_default();
                attention_card(ui, &t, t.yellow, |ui| {
                    ui.add(egui::Label::new(RichText::new(s(e, "title")).font(font_medium(type_scale::BODY)).color(t.fg)).wrap());
                    ui.label(
                        RichText::new(format!("{} · {}", s(e, "user"), mmss(e.get_path("duration").and_then(Value::as_f64).unwrap_or(0.0))))
                            .size(type_scale::SMALL)
                            .color(t.text_dim),
                    );
                    ui.add_space(spacing::XS);
                    ui.horizontal(|ui| {
                        if widgets::button_ex(ui, &t, Some(icon::CHECK), "Play it", Kind::Primary, Size::Small, 0.0, true).clicked() {
                            act = Some(("queue.approve", Value::map().with("id", id.clone())));
                        }
                        if widgets::button_ex(ui, &t, Some(icon::CROSS), "No thanks", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                            act = Some(("queue.reject", Value::map().with("id", id.clone())));
                        }
                    });
                });
            }
        }
        ui.add_space(spacing::M);
        rail_heading(ui, &t, &format!("Up next ({})", upcoming.len()));
        if upcoming.is_empty() {
            widgets::hint(ui, &t, "Nothing else waiting.");
        }
        let n = upcoming.len();
        for (i, e) in upcoming.iter().enumerate() {
            let id = e.get_path("id").cloned().unwrap_or_default();
            let bg = ui.painter().add(egui::Shape::Noop);
            let row = egui::Frame::new().inner_margin(egui::Margin::symmetric(8, 6)).show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.horizontal_top(|ui| {
                    ui.add_sized([20.0, 18.0], egui::Label::new(RichText::new(format!("{}", i + 1)).font(font_mono(type_scale::SMALL)).color(t.text_faint)));
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width());
                        let title = if e.get_path("paid").is_some_and(Value::truthy) {
                            format!("{}  {}", icon::STAR, s(e, "title"))
                        } else {
                            s(e, "title").to_string()
                        };
                        ui.add(egui::Label::new(RichText::new(title).font(font_medium(type_scale::BODY)).color(t.fg)).truncate());
                        let dur = e.get_path("duration").and_then(Value::as_f64).unwrap_or(0.0);
                        ui.label(RichText::new(format!("{} · {}", s(e, "user"), mmss(dur))).size(type_scale::SMALL).color(t.text_dim));
                    });
                });
            });
            let rect = row.response.rect;
            if ui.rect_contains_pointer(rect) {
                ui.painter().set(bg, egui::Shape::rect_filled(rect, CornerRadius::same(radius::CONTROL), t.surface_hi));
                let bar = Rect::from_min_size(egui::pos2(rect.right() - 100.0, rect.center().y - 14.0), Vec2::new(96.0, 28.0));
                let mut child = ui.new_child(egui::UiBuilder::new().max_rect(bar).layout(Layout::right_to_left(Align::Center)));
                child.painter().rect(bar, CornerRadius::same(radius::CONTROL), t.surface, Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
                child.add_space(2.0);
                if widgets::icon_button(&mut child, &t, icon::TRASH, "Remove").clicked() {
                    act = Some(("queue.remove", Value::map().with("id", id.clone())));
                }
                if i + 1 < n && widgets::icon_button(&mut child, &t, icon::DOWN, "Move down").clicked() {
                    act = Some(("queue.reorder", Value::map().with("id", id.clone()).with("to", (i + 2) as i64)));
                }
                if i > 0 && widgets::icon_button(&mut child, &t, icon::UP, "Move up").clicked() {
                    act = Some(("queue.reorder", Value::map().with("id", id.clone()).with("to", i as i64)));
                }
            }
        }
    });
    if let Some((n, a)) = act {
        app.m.action(n, a);
        app.show.rail_state.queue_asked = None;
    }
}

// ---- Mod -----------------------------------------------------------------------------------------

/// Requests that need a mod's OK (channel points, paid actions…). Returns how many were shown.
pub fn approvals(app: &mut App, ui: &mut egui::Ui) -> usize {
    let t = app.t.clone();
    let items = pending(app, "approval");
    let mut act: Option<(&'static str, Value)> = None;
    for p in &items {
        let id = p.get_path("id").cloned().unwrap_or_default();
        let (ic, headline) = policy_headline(p);
        attention_card(ui, &t, t.accent, |ui| {
            ui.label(RichText::new("Needs your OK").font(font_semibold(type_scale::SMALL + 0.5)).color(t.text_dim));
            ui.horizontal(|ui| {
                ui.label(RichText::new(ic).color(t.accent));
                ui.add_space(2.0);
                ui.add(egui::Label::new(RichText::new(&headline).font(font_semibold(type_scale::BODY)).color(t.fg)).wrap());
            });
            quote(ui, &t, s(p, "text"));
            if p.get_path("expires_ns").is_some() {
                let (rem, total) = window_ms(p, se_clock::now());
                ui.add_space(spacing::XS);
                thin_bar(ui, &t, (rem / total) as f32, t.accent);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
            }
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, &t, Some(icon::CHECK), "Approve", Kind::Primary, Size::Small, 0.0, true).clicked() {
                    act = Some(("mod.approve", Value::map().with("id", id.clone())));
                }
                if widgets::button_ex(ui, &t, Some(icon::CROSS), "Reject", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                    act = Some(("mod.reject", Value::map().with("id", id.clone())));
                }
            });
        });
    }
    if let Some((n, a)) = act {
        app.m.action(n, a);
    }
    items.len()
}

/// Chat messages Twitch AutoMod is holding back. Returns how many were shown.
pub fn automod(app: &mut App, ui: &mut egui::Ui) -> usize {
    let t = app.t.clone();
    let held = list(app, "twitch.automod.queue").to_vec();
    let mut act: Option<(&'static str, Value)> = None;
    for h in &held {
        let id = h.get_path("message_id").cloned().unwrap_or_default();
        attention_card(ui, &t, t.yellow, |ui| {
            ui.label(RichText::new("Held back by Twitch").font(font_semibold(type_scale::SMALL + 0.5)).color(t.text_dim));
            ui.horizontal(|ui| {
                ui.label(RichText::new(s(h, "user")).font(font_semibold(type_scale::BODY)).color(t.fg));
                let cat = if s(h, "category").is_empty() { s(h, "reason") } else { s(h, "category") };
                if !cat.is_empty() {
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        widgets::badge(ui, &t, &nice(cat), t.yellow)
                            .on_hover_text(format!("AutoMod level {}", h.get_path("level").map(|v| v.to_string()).unwrap_or_default()));
                    });
                }
            });
            quote(ui, &t, s(h, "text"));
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, &t, Some(icon::CHECK), "Allow", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    act = Some(("mod.automod.approve", Value::map().with("message_id", id.clone())));
                }
                if widgets::button_ex(ui, &t, Some(icon::CROSS), "Deny", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                    act = Some(("mod.automod.deny", Value::map().with("message_id", id.clone())));
                }
            });
        });
    }
    if let Some((n, a)) = act {
        app.m.action(n, a);
    }
    held.len()
}

/// Everything anyone did (you, mods, chat, the web), newest first — behind a "Details" disclosure.
pub fn audit(app: &mut App, ui: &mut egui::Ui, salt: &str) {
    let t = app.t.clone();
    widgets::details(ui, &t, salt, "History", |ui| {
        if app.m.connected && app.show.rail_state.audit_asked.is_none_or(|a| a.elapsed().as_secs() >= 3) {
            app.show.rail_state.audit_asked = Some(Instant::now());
            app.m.query("audit", Value::map().with("n", 100));
        }
        let audit = app.m.q_list("audit").to_vec();
        if audit.is_empty() {
            widgets::hint(ui, &t, "Nothing has been done yet.");
        }
        for a in audit.iter().take(100) {
            let ok = a.get_path("ok").is_some_and(Value::truthy);
            ui.horizontal(|ui| {
                ui.label(RichText::new(if ok { icon::CHECK } else { icon::CROSS }).size(type_scale::SMALL).color(if ok { t.green } else { t.bright_red }));
                let who = if s(a, "actor").is_empty() { nice(s(a, "origin")) } else { s(a, "actor").to_string() };
                ui.label(RichText::new(who).size(type_scale::SMALL).color(t.text_dim));
                let r = ui.add(egui::Label::new(RichText::new(s(a, "op")).font(font_mono(type_scale::SMALL)).color(t.fg)).truncate());
                if !ok {
                    r.on_hover_text(s(a, "error"));
                }
            });
        }
    });
}

pub fn moderation(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    egui::ScrollArea::vertical().id_salt("rail-mod").auto_shrink([false, false]).show(ui, |ui| {
        let n = approvals(app, ui) + automod(app, ui);
        if n == 0 {
            widgets::empty_state(
                ui,
                &t,
                icon::CHECK,
                "All clear",
                "Nothing needs your OK right now. Held chat messages and requests that need approval show up here.",
                None,
            );
        }
        ui.add_space(spacing::S);
        audit(app, ui, "rail-audit");
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_audience_tracks_updates_without_presenting_unknown_or_cached_counts_as_live() {
        use egui_kittest::{Harness, kittest::Queryable};

        struct ChatApp(App);
        impl eframe::App for ChatApp {
            fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
                egui::CentralPanel::default().show(ui, |ui| chat(&mut self.0, ui));
            }
        }
        let mut h = Harness::builder().with_size([320.0, 400.0]).build_eframe(|cc| {
            let mut app = App::new(
                cc,
                crate::UiOpts { socket: Some("/nonexistent/se-ui-audience-test.sock".into()), layout: Some("single".into()), program_only: false },
            );
            app.m.connected = true;
            app.m.state.insert("twitch.auth.status".into(), Value::from("authorized"));
            app.m.state.insert("twitch.stream.live".into(), Value::Bool(true));
            ChatApp(app)
        });
        h.run_steps(3);
        assert!(h.query_by_label_contains("watching").is_none(), "an unknown count is not zero viewers");
        for n in [0, 1, 1234] {
            h.state_mut().0.m.apply(se_proto::wire::ServerMsg::State { changes: vec![("twitch.stream.viewers".into(), Value::Int(n))] });
            h.run_steps(3);
            let label = format!("{} watching", grouped(n));
            h.get_by_label(&label);
            assert_eq!(h.query_all_by_label_contains("watching").count(), 1, "only the latest count is displayed");
        }
        h.state_mut().0.m.apply(se_proto::wire::ServerMsg::State { changes: vec![("twitch.stream.live".into(), Value::Bool(false))] });
        h.run_steps(3);
        assert!(h.query_by_label_contains("watching").is_none(), "offline must hide the last live count");
        h.state_mut().0.m.state.insert("twitch.stream.live".into(), Value::Bool(true));
        h.state_mut().0.m.connected = false;
        h.run_steps(3);
        assert!(h.query_by_label_contains("watching").is_none(), "a disconnected engine's cached count is not current");
        h.state_mut().0.m.connected = true;
        h.state_mut().0.m.state.insert("twitch.auth.status".into(), Value::from("pending"));
        h.run_steps(3);
        assert!(h.query_by_label_contains("watching").is_none(), "reauthorizing Twitch must hide the previous account's count");
    }

    #[test]
    fn policy_window_uses_master_clock_deadlines() {
        let p = Value::map().with("created_ns", 1_000_000_000i64).with("expires_ns", 11_000_000_000i64);
        let (rem, total) = window_ms(&p, 6_000_000_000);
        assert_eq!((rem, total), (5000.0, 10000.0));
        assert_eq!(window_ms(&p, 20_000_000_000).0, 0.0, "expired entries show 0, never negative");
    }
}
