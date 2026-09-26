//! Giveaway draw UI (§14.1): open/close a chat-entry giveaway, watch weighted entries come in,
//! draw (and re-draw) winners. Data: query `giveaway`; commands: `giveaway.*` actions.

use crate::app::App;
use egui::{RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};

#[derive(Clone)]
struct Form {
    title: String,
    keyword: String,
    min_role: String,
    duration: String,
    filter: String,
    add_user: String,
    add_role: String,
    last_query: f64,
}

impl Default for Form {
    fn default() -> Self {
        Form {
            title: String::new(),
            keyword: String::new(),
            min_role: "everyone".into(),
            duration: String::new(),
            filter: String::new(),
            add_user: String::new(),
            add_role: "everyone".into(),
            last_query: 0.0,
        }
    }
}

const ROLES: [&str; 6] = ["everyone", "follower", "sub", "vip", "mod", "owner"];

fn act(app: &mut App, name: &str, args: Value) {
    app.m.command(Op::Action { name: name.into(), args });
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn f(v: &Value, k: &str) -> f64 {
    v.get_path(k).and_then(Value::as_f64).unwrap_or(0.0)
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("giveaway-form");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    let now = ui.input(|i| i.time);
    let entered_recently = app.m.events.back().is_some_and(|e| e.ty.starts_with("giveaway."));
    if now - form.last_query > if entered_recently { 0.25 } else { 0.75 } {
        form.last_query = now;
        app.m.query("giveaway", Value::Null);
    }
    let g = app.m.q("giveaway").cloned().unwrap_or_default();
    let state = s(&g, "state").to_string();
    let open = state == "open";
    if form.keyword.is_empty() {
        form.keyword = g.get_path("rules.keyword").and_then(Value::as_str).unwrap_or("!join").to_string();
    }

    ui.horizontal(|ui| {
        widgets::section(ui, &t, icon::ALERT, "Giveaway");
        let (led, label) = match state.as_str() {
            "open" => (LedState::Active, "OPEN"),
            "closed" => (LedState::Armed, "CLOSED"),
            _ => (LedState::Idle, "IDLE"),
        };
        widgets::pill(ui, &t, icon::LIVE, label, led);
        if !s(&g, "title").is_empty() {
            ui.label(RichText::new(s(&g, "title")).strong());
            ui.label(RichText::new(format!("enter with {}", s(&g, "keyword"))).color(t.fg_dim));
        }
        let closes = g.get_path("closes_at").and_then(Value::as_i64).unwrap_or(0);
        if open && closes > 0 {
            let left = closes - unix_now();
            ui.label(RichText::new(format!("closes in {}:{:02}", left.max(0) / 60, left.max(0) % 60)).color(t.yellow));
        }
    });
    ui.add_space(6.0);

    let entries: Vec<Value> = g.get_path("entries").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let winners: Vec<Value> = g.get_path("winners").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();

    ui.columns(2, |cols| {
        // --- left: controls + winners
        let ui = &mut cols[0];
        widgets::card(ui, &t, icon::SETTINGS, "Round", if open { LedState::Active } else { LedState::Idle }, |ui| {
            egui::Grid::new("gw-form").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("Prize");
                ui.add(
                    egui::TextEdit::singleline(&mut form.title)
                        .hint_text(if s(&g, "title").is_empty() { "what's being given away" } else { s(&g, "title") })
                        .desired_width(260.0),
                );
                ui.end_row();
                ui.label("Keyword");
                ui.add(egui::TextEdit::singleline(&mut form.keyword).desired_width(120.0));
                ui.end_row();
                ui.label("Who can enter");
                egui::ComboBox::from_id_salt("gw-minrole").selected_text(format!("{}+", form.min_role)).show_ui(ui, |ui| {
                    for r in ROLES {
                        ui.selectable_value(&mut form.min_role, r.to_string(), format!("{r}+"));
                    }
                });
                ui.end_row();
                ui.label("Auto-close");
                ui.add(egui::TextEdit::singleline(&mut form.duration).hint_text("e.g. 5m (empty = manual)").desired_width(160.0));
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                let label = if state == "closed" { "Reopen" } else { "Open" };
                if ui.add_enabled(!open, egui::Button::new(RichText::new(format!("{} {label}", icon::PLAY)).strong())).clicked() {
                    let mut args = Value::map().with("keyword", form.keyword.trim()).with("min_role", form.min_role.clone());
                    if !form.title.trim().is_empty() {
                        args = args.with("title", form.title.trim());
                    }
                    if !form.duration.trim().is_empty() {
                        args = args.with("duration", form.duration.trim());
                    }
                    act(app, "giveaway.open", args);
                }
                if ui.add_enabled(open, egui::Button::new(format!("{} Close", icon::STOP))).clicked() {
                    act(app, "giveaway.close", Value::Null);
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::hold_button(ui, &t, "Reset", t.yellow, 0.6) {
                        act(app, "giveaway.reset", Value::Null);
                    }
                });
            });
        });
        ui.add_space(8.0);
        let eligible = entries.iter().filter(|e| !e.get_path("won").is_some_and(Value::truthy)).count();
        let can_draw = state != "idle" && eligible > 0;
        let draw_label = if winners.is_empty() { "DRAW WINNER" } else { "DRAW AGAIN" };
        let hint = format!("{eligible} in the draw");
        if widgets::pad(
            ui,
            &t,
            Vec2::new(ui.available_width(), 84.0),
            icon::PRESET,
            draw_label,
            Some(if can_draw { t.accent } else { t.bg_light }),
            if can_draw { LedState::Armed } else { LedState::Idle },
            None,
            Some(&hint),
        )
        .clicked()
            && can_draw
        {
            act(app, "giveaway.draw", Value::Null);
        }
        ui.add_space(8.0);
        if let Some(w) = winners.last() {
            egui::Frame::new().fill(t.bg_dark).stroke(egui::Stroke::new(2.0, t.green)).corner_radius(8).inner_margin(12).show(ui, |ui| {
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new("WINNER").small().color(t.fg_dim));
                    ui.label(RichText::new(s(w, "user")).size(34.0).strong().color(t.green));
                    ui.label(
                        RichText::new(format!("{:.1}% chance · weight {:.2} · {} entries", f(w, "chance") * 100.0, f(w, "weight"), f(w, "entries") as i64))
                            .color(t.fg_dim),
                    );
                });
            });
        }
        if winners.len() > 1 {
            ui.add_space(6.0);
            widgets::section(ui, &t, icon::CHECK, "Earlier draws");
            for w in winners.iter().rev().skip(1) {
                ui.label(format!("{}  ({:.1}%)", s(w, "user"), f(w, "chance") * 100.0));
            }
        }

        // --- right: entries
        let ui = &mut cols[1];
        let tickets = f(&g, "tickets");
        ui.horizontal(|ui| {
            widgets::section(ui, &t, icon::CHAT, &format!("Entries ({})", entries.len()));
            ui.label(RichText::new(format!("{tickets:.1} tickets")).color(t.fg_dim));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.add(egui::TextEdit::singleline(&mut form.filter).hint_text(format!("{} filter", icon::SEARCH)).desired_width(140.0));
            });
        });
        let mut sorted: Vec<&Value> =
            entries.iter().filter(|e| form.filter.is_empty() || s(e, "user").to_lowercase().contains(&form.filter.to_lowercase())).collect();
        sorted.sort_by(|a, b| f(b, "weight").total_cmp(&f(a, "weight")).then_with(|| s(a, "user").cmp(s(b, "user"))));
        egui::ScrollArea::vertical().max_height(ui.available_height() - 40.0).auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("gw-entries").striped(true).num_columns(6).spacing([12.0, 4.0]).show(ui, |ui| {
                for h in ["user", "role", "months", "weight", "chance", ""] {
                    ui.label(RichText::new(h).small().color(t.fg_dim));
                }
                ui.end_row();
                for e in sorted {
                    let won = e.get_path("won").is_some_and(Value::truthy);
                    let name = RichText::new(s(e, "user"));
                    ui.label(if won { name.color(t.green).strong() } else { name });
                    ui.label(s(e, "role"));
                    ui.label(format!("{}", e.get_path("sub_months").and_then(Value::as_i64).unwrap_or(0)));
                    ui.label(format!("{:.2}", f(e, "weight")));
                    let chance = f(e, "chance");
                    ui.label(if won { RichText::new("won").color(t.green) } else { RichText::new(format!("{:.1}%", chance * 100.0)) });
                    if ui.small_button(icon::CROSS).on_hover_text("remove entry").clicked() {
                        act(app, "giveaway.remove", Value::map().with("user", s(e, "key")));
                    }
                    ui.end_row();
                }
            });
        });
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut form.add_user).hint_text("add viewer manually").desired_width(160.0));
            egui::ComboBox::from_id_salt("gw-addrole").selected_text(form.add_role.clone()).show_ui(ui, |ui| {
                for r in ROLES {
                    ui.selectable_value(&mut form.add_role, r.to_string(), r);
                }
            });
            if ui.add_enabled(open && !form.add_user.trim().is_empty(), egui::Button::new("Add")).clicked() {
                let user = std::mem::take(&mut form.add_user);
                act(app, "giveaway.enter", Value::map().with("user", user.trim()).with("role", form.add_role.clone()));
            }
        });
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
