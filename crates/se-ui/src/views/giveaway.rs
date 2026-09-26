//! Giveaways (§14.1) as one obvious flow: set it up (prize, the word people type, who can
//! enter) → people join (live count) → draw a winner (big button) → winner card → draw again or
//! start over. Entries sit on the right. Data: query `giveaway`; commands: `giveaway.*` actions.

use crate::app::App;
use egui::{Align, CornerRadius, Layout, RichText, Sense, Stroke, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_bold, font_medium, font_mono, font_semibold, mix, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};

#[derive(Clone)]
struct Form {
    title: String,
    keyword: String,
    min_role: String,
    /// Index into [`CLOSE_AFTER`].
    close_after: usize,
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
            close_after: 0,
            filter: String::new(),
            add_user: String::new(),
            add_role: "everyone".into(),
            last_query: 0.0,
        }
    }
}

/// Role key → who can enter.
const ROLES: [(&str, &str); 6] =
    [("everyone", "Everyone"), ("follower", "Followers"), ("sub", "Subscribers"), ("vip", "VIPs and mods"), ("mod", "Mods only"), ("owner", "Just you (test)")];

/// Auto-close choices (label, duration; empty = when you close it).
const CLOSE_AFTER: [(&str, &str); 6] = [
    ("When I close it", ""),
    ("After 1 minute", "1m"),
    ("After 2 minutes", "2m"),
    ("After 5 minutes", "5m"),
    ("After 10 minutes", "10m"),
    ("After 15 minutes", "15m"),
];

fn role_label(r: &str) -> &str {
    match r {
        "everyone" => "Viewer",
        "follower" => "Follower",
        "sub" => "Subscriber",
        "vip" => "VIP",
        "mod" => "Mod",
        "owner" => "You",
        other => other,
    }
}

fn act(app: &mut App, name: &str, args: Value) {
    app.m.command(Op::Action { name: name.into(), args });
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn f(v: &Value, k: &str) -> f64 {
    v.get_path(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
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
    if form.keyword.is_empty() {
        form.keyword = g.get_path("rules.keyword").and_then(Value::as_str).unwrap_or("!join").to_string();
    }
    let entries: Vec<Value> = g.get_path("entries").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let winners: Vec<Value> = g.get_path("winners").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let step = match state.as_str() {
        _ if !winners.is_empty() => 3,
        "open" => 1,
        "closed" => 2,
        _ => 0,
    };

    egui::ScrollArea::vertical().id_salt("giveaway-page").auto_shrink([false, false]).show(ui, |ui| {
        // One flow, centered with a comfortable width on wide screens.
        let full = ui.available_width();
        let w = full.min(1800.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            ui.add_space((full - w) / 2.0);
            ui.allocate_ui_with_layout(Vec2::new(w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(w);
                stepper(ui, &t, step);
                ui.add_space(spacing::L);
                let right = (w * 0.36).clamp(360.0, 640.0);
                let left = w - right - spacing::L;
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = 0.0;
                    ui.allocate_ui_with_layout(Vec2::new(left, 0.0), Layout::top_down(Align::Min), |ui| {
                        ui.set_width(left);
                        match step {
                            0 => setup(app, ui, &mut form, &g),
                            1 | 2 => running(app, ui, &g, &entries, step == 1),
                            _ => winner(app, ui, &g, &entries, &winners),
                        }
                    });
                    ui.add_space(spacing::L);
                    ui.allocate_ui_with_layout(Vec2::new(right, 0.0), Layout::top_down(Align::Min), |ui| {
                        ui.set_width(right);
                        entry_list(app, ui, &mut form, &g, &entries, state == "open");
                    });
                });
            });
        });
        ui.add_space(spacing::XL);
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}

/// 1 Set it up → 2 People join → 3 Draw → 4 Winner.
fn stepper(ui: &mut egui::Ui, t: &Theme, step: usize) {
    let steps = ["Set it up", "People join", "Draw a winner", "Winner!"];
    ui.horizontal(|ui| {
        for (k, label) in steps.iter().enumerate() {
            let (done, cur) = (k < step, k == step);
            let (r, _) = ui.allocate_exact_size(Vec2::splat(30.0), Sense::hover());
            let p = ui.painter();
            if done {
                p.circle_filled(r.center(), 14.0, mix(t.bg, t.green, 0.25));
                p.text(r.center(), egui::Align2::CENTER_CENTER, icon::CHECK, font(13.0), t.green);
            } else if cur {
                p.circle_filled(r.center(), 14.0, t.accent);
                p.text(r.center(), egui::Align2::CENTER_CENTER, (k + 1).to_string(), font_bold(14.0), t.on_accent);
            } else {
                p.circle_stroke(r.center(), 13.5, Stroke::new(1.5, t.border));
                p.text(r.center(), egui::Align2::CENTER_CENTER, (k + 1).to_string(), font_bold(14.0), t.text_faint);
            }
            ui.add_space(spacing::XS);
            ui.label(RichText::new(*label).font(if cur { font_semibold(type_scale::BODY + 0.5) } else { font_medium(type_scale::BODY + 0.5) }).color(if cur {
                t.fg
            } else if done {
                t.text_dim
            } else {
                t.text_faint
            }));
            if k + 1 < steps.len() {
                ui.add_space(spacing::S);
                let (line, _) = ui.allocate_exact_size(Vec2::new(48.0, 2.0), Sense::hover());
                ui.painter().rect_filled(line, CornerRadius::same(1), if done { mix(t.border, t.green, 0.5) } else { t.border });
                ui.add_space(spacing::S);
            }
        }
    });
}

fn form_row(ui: &mut egui::Ui, t: &Theme, label: &str, help: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(210.0, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(210.0);
            ui.label(RichText::new(label).font(font_medium(type_scale::BODY + 0.5)).color(t.fg));
            if !help.is_empty() {
                ui.add(egui::Label::new(RichText::new(help).size(type_scale::SMALL).color(t.text_dim)).wrap());
            }
        });
        ui.add_space(spacing::M);
        body(ui);
    });
    ui.add_space(spacing::L);
}

fn setup(app: &mut App, ui: &mut egui::Ui, form: &mut Form, g: &Value) {
    let t = app.t.clone();
    let mut start = false;
    widgets::titled(
        ui,
        &t,
        "Start a giveaway",
        "Viewers join by typing a word in chat. You draw a winner when you're ready.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            ui.add_space(spacing::S);
            let fw = (ui.available_width() - 230.0).clamp(200.0, 360.0);
            form_row(ui, &t, "Prize", "What are you giving away?", |ui| {
                ui.add(se_ui_kit::widgets::field(&mut form.title).hint_text("e.g. Signed drumsticks").desired_width(fw));
            });
            form_row(ui, &t, "How people join", "The word they type in chat.", |ui| {
                ui.add(se_ui_kit::widgets::field(&mut form.keyword).desired_width(fw));
            });
            form_row(ui, &t, "Who can join", "", |ui| {
                let sel = ROLES.iter().find(|(k, _)| *k == form.min_role).map(|(_, l)| *l).unwrap_or("Everyone");
                egui::ComboBox::from_id_salt("gw-minrole").selected_text(sel).width(fw).show_ui(ui, |ui| {
                    for (k, l) in ROLES {
                        ui.selectable_value(&mut form.min_role, k.to_string(), l);
                    }
                });
            });
            form_row(ui, &t, "Stop taking entries", "", |ui| {
                egui::ComboBox::from_id_salt("gw-close").selected_text(CLOSE_AFTER[form.close_after].0).width(fw).show_ui(ui, |ui| {
                    for (k, (l, _)) in CLOSE_AFTER.iter().enumerate() {
                        ui.selectable_value(&mut form.close_after, k, *l);
                    }
                });
            });
            let rules = fairness(g);
            if !rules.is_empty() {
                widgets::hint(ui, &t, &rules);
            }
            ui.add_space(spacing::L);
            let ready = !form.keyword.trim().is_empty();
            start = widgets::button_ex(ui, &t, Some(icon::GIFT), "Start giveaway", Kind::Primary, Size::Medium, 0.0, ready).clicked();
        },
    );
    if start {
        let mut args = Value::map().with("keyword", form.keyword.trim()).with("min_role", form.min_role.clone());
        if !form.title.trim().is_empty() {
            args = args.with("title", form.title.trim());
        }
        let dur = CLOSE_AFTER[form.close_after].1;
        if !dur.is_empty() {
            args = args.with("duration", dur);
        }
        act(app, "giveaway.open", args);
    }
}

/// The house rules in words, from `[giveaway]` in the project.
fn fairness(g: &Value) -> String {
    let w = |r: &str| g.get_path(&format!("rules.weights.{r}")).and_then(Value::as_f64).unwrap_or(1.0);
    let mut better: Vec<&str> = Vec::new();
    if w("sub") > w("everyone") {
        better.push("Subscribers");
    }
    if w("vip") > w("everyone") {
        better.push("VIPs");
    }
    let mut out = String::new();
    if !better.is_empty() {
        out.push_str(&format!("{} get a better chance. ", better.join(" and ")));
    }
    let owner_out = g.get_path("rules.exclude_roles").and_then(Value::as_list).is_some_and(|l| l.iter().any(|r| r.as_str() == Some("owner")));
    let bots_out = g.get_path("rules.exclude_users").and_then(Value::as_list).is_some_and(|l| !l.is_empty());
    match (owner_out, bots_out) {
        (true, true) => out.push_str("You and chat bots can't win."),
        (true, false) => out.push_str("You can't win your own giveaway."),
        (false, true) => out.push_str("Chat bots can't win."),
        _ => {}
    }
    out
}

/// Open (people joining) or closed (ready to draw).
fn running(app: &mut App, ui: &mut egui::Ui, g: &Value, entries: &[Value], open: bool) {
    let t = app.t.clone();
    let n = entries.len();
    let (mut close, mut draw, mut reopen, mut reset) = (false, false, false, false);
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        ui.vertical_centered(|ui| {
            ui.add_space(spacing::M);
            let title = if s(g, "title").is_empty() { "Giveaway".to_string() } else { s(g, "title").to_string() };
            widgets::badge(ui, &t, if open { "Open — people can join" } else { "Closed — ready to draw" }, if open { t.green } else { t.yellow });
            ui.add_space(spacing::S);
            ui.label(RichText::new(title).font(font_bold(type_scale::TITLE)).color(t.fg));
            if open {
                ui.label(RichText::new(format!("Viewers type {} in chat to join", s(g, "keyword"))).size(type_scale::LARGE).color(t.text_dim));
            }
            ui.add_space(spacing::L);
            ui.label(RichText::new(n.to_string()).font(font_bold(72.0)).color(t.fg));
            ui.label(RichText::new(if n == 1 { "person joined" } else { "people joined" }).size(type_scale::LARGE).color(t.text_dim));
            let closes = g.get_path("closes_at").and_then(Value::as_i64).unwrap_or(0);
            if open && closes > 0 {
                let left = (closes - unix_now()).max(0);
                ui.add_space(spacing::S);
                ui.label(RichText::new(format!("Entries close in {}:{:02}", left / 60, left % 60)).font(font_medium(type_scale::BODY)).color(t.yellow));
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            }
            ui.add_space(spacing::XL);
            let can_draw = entries.iter().any(|e| !e.get_path("won").is_some_and(Value::truthy));
            draw = widgets::button_ex(ui, &t, Some(icon::GIFT), "Draw a winner", Kind::Primary, Size::Large, 300.0, can_draw).clicked();
            if !can_draw {
                ui.add_space(spacing::XS);
                widgets::hint(ui, &t, "Waiting for the first person to join.");
            } else if open {
                ui.add_space(spacing::XS);
                widgets::hint(ui, &t, "Drawing closes entries first.");
            }
            ui.add_space(spacing::M);
            ui.horizontal(|ui| {
                // Center the secondary actions under the big button.
                let w = 360.0_f32.min(ui.available_width());
                ui.add_space(((ui.available_width() - w) / 2.0).max(0.0));
                if open {
                    close = widgets::button_ex(ui, &t, Some(icon::STOP), "Stop taking entries", Kind::Secondary, Size::Medium, 0.0, true).clicked();
                } else {
                    reopen = widgets::button_ex(ui, &t, Some(icon::PLAY), "Let people join again", Kind::Secondary, Size::Medium, 0.0, true).clicked();
                }
                reset = widgets::hold_button(ui, &t, "Hold to cancel", t.yellow, 0.8);
            });
            ui.add_space(spacing::M);
        });
    });
    if draw {
        act(app, "giveaway.draw", Value::Null);
    }
    if close {
        act(app, "giveaway.close", Value::Null);
    }
    if reopen {
        let mut args = Value::map().with("keyword", s(g, "keyword")).with("min_role", if s(g, "min_role").is_empty() { "everyone" } else { s(g, "min_role") });
        if !s(g, "title").is_empty() {
            args = args.with("title", s(g, "title"));
        }
        act(app, "giveaway.open", args);
    }
    if reset {
        act(app, "giveaway.reset", Value::Null);
    }
}

fn winner(app: &mut App, ui: &mut egui::Ui, g: &Value, entries: &[Value], winners: &[Value]) {
    let t = app.t.clone();
    let Some(w) = winners.last() else { return };
    let (mut again, mut reset) = (false, false);
    egui::Frame::new()
        .fill(mix(t.surface, t.green, 0.08))
        .stroke(Stroke::new(1.5, mix(t.border, t.green, 0.6)))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::same(24))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.vertical_centered(|ui| {
                ui.label(RichText::new(icon::GIFT).size(34.0).color(t.green));
                ui.add_space(spacing::S);
                let title = s(g, "title");
                ui.label(
                    RichText::new(if title.is_empty() { "The winner is".to_string() } else { format!("{title} goes to") })
                        .size(type_scale::LARGE)
                        .color(t.text_dim),
                );
                ui.label(RichText::new(s(w, "user")).font(font_bold(56.0)).color(t.fg));
                ui.add_space(spacing::XS);
                ui.label(RichText::new(format!("They had a {:.0}% chance out of {} people", f(w, "chance") * 100.0, entries.len())).color(t.text_dim));
                widgets::details(ui, &t, "gw-winner-details", "Details", |ui| {
                    ui.label(
                        RichText::new(format!("{:.1}% chance · weight {:.2} · {} entries", f(w, "chance") * 100.0, f(w, "weight"), f(w, "entries") as i64))
                            .font(font_mono(type_scale::SMALL))
                            .color(t.text_dim),
                    );
                });
                ui.add_space(spacing::XL);
                let left = entries.iter().filter(|e| !e.get_path("won").is_some_and(Value::truthy)).count();
                again = widgets::button_ex(ui, &t, Some(icon::SHUFFLE), "Draw again", Kind::Secondary, Size::Medium, 200.0, left > 0)
                    .on_hover_text("Winner didn't answer? Pick someone else.")
                    .clicked();
                ui.add_space(spacing::S);
                widgets::hint(ui, &t, &if left == 1 { "1 person left in the draw".to_string() } else { format!("{left} people left in the draw") });
                ui.add_space(spacing::M);
                reset = widgets::hold_button(ui, &t, "Hold to finish and start a new one", t.accent, 0.6);
            });
        });
    if winners.len() > 1 {
        ui.add_space(spacing::L);
        widgets::titled(
            ui,
            &t,
            "Drawn before",
            "People who were picked earlier in this giveaway.",
            |_| {},
            |ui| {
                ui.set_width(ui.available_width());
                for w in winners.iter().rev().skip(1) {
                    widgets::list_row(ui, &t, icon::STAR, s(w, "user"), "", &format!("{:.0}% chance", f(w, "chance") * 100.0), false);
                }
            },
        );
    }
    if again {
        act(app, "giveaway.draw", Value::Null);
    }
    if reset {
        act(app, "giveaway.reset", Value::Null);
    }
}

fn entry_list(app: &mut App, ui: &mut egui::Ui, form: &mut Form, g: &Value, entries: &[Value], open: bool) {
    let t = app.t.clone();
    let mut remove = None;
    let mut add = None;
    let sub = match entries.len() {
        0 => String::new(),
        1 => "1 person".to_string(),
        n => format!("{n} people"),
    };
    widgets::titled(
        ui,
        &t,
        "Who joined",
        &sub,
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if entries.is_empty() {
                let kw = if s(g, "keyword").is_empty() { form.keyword.clone() } else { s(g, "keyword").to_string() };
                widgets::empty_state(ui, &t, icon::USERS, "Nobody yet", &format!("People show up here when they type {kw} in chat."), None);
            } else {
                if entries.len() > 8 {
                    ui.add(se_ui_kit::widgets::field(&mut form.filter).hint_text(format!("{}  Find someone", icon::SEARCH)).desired_width(f32::INFINITY));
                    ui.add_space(spacing::S);
                }
                let flt = form.filter.to_lowercase();
                let mut sorted: Vec<&Value> = entries.iter().filter(|e| flt.is_empty() || s(e, "user").to_lowercase().contains(&flt)).collect();
                sorted.sort_by(|a, b| f(b, "weight").total_cmp(&f(a, "weight")).then_with(|| s(a, "user").cmp(s(b, "user"))));
                {
                    for e in sorted {
                        let won = e.get_path("won").is_some_and(Value::truthy);
                        let months = e.get_path("sub_months").and_then(Value::as_i64).unwrap_or(0);
                        let who = if months > 0 { format!("{} · {months} months", role_label(s(e, "role"))) } else { role_label(s(e, "role")).to_string() };
                        let bg = ui.painter().add(egui::Shape::Noop);
                        let row = egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 6)).show(ui, |ui| {
                            ui.set_width(ui.available_width());
                            ui.horizontal(|ui| {
                                ui.vertical(|ui| {
                                    ui.set_width(ui.available_width() - 150.0);
                                    ui.add(
                                        egui::Label::new(RichText::new(s(e, "user")).font(font_medium(type_scale::BODY + 0.5)).color(if won {
                                            t.green
                                        } else {
                                            t.fg
                                        }))
                                        .truncate(),
                                    );
                                    ui.label(RichText::new(who).size(type_scale::SMALL).color(t.text_dim));
                                });
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if widgets::icon_button(ui, &t, icon::CROSS, "Remove from the giveaway").clicked() {
                                        remove = Some(s(e, "key").to_string());
                                    }
                                    if won {
                                        widgets::badge(ui, &t, "Won", t.green);
                                    } else {
                                        ui.label(RichText::new(format!("{:.0}%", f(e, "chance") * 100.0)).font(font_mono(type_scale::BODY)).color(t.text_dim))
                                            .on_hover_text(format!("Chance to win · {:.1}× tickets", f(e, "weight")));
                                    }
                                });
                            });
                        });
                        if ui.rect_contains_pointer(row.response.rect) {
                            ui.painter().set(bg, egui::Shape::rect_filled(row.response.rect, CornerRadius::same(8), t.surface_hi));
                        }
                    }
                }
            }
            ui.add_space(spacing::S);
            widgets::details(ui, &t, "gw-add", "Add someone by hand", |ui| {
                ui.horizontal(|ui| {
                    ui.add(se_ui_kit::widgets::field(&mut form.add_user).hint_text("Their name").desired_width(160.0));
                    egui::ComboBox::from_id_salt("gw-addrole").selected_text(role_label(&form.add_role).to_string()).width(120.0).show_ui(ui, |ui| {
                        for (k, _) in ROLES {
                            ui.selectable_value(&mut form.add_role, k.to_string(), role_label(k));
                        }
                    });
                    if widgets::button_ex(ui, &t, Some(icon::PLUS), "Add", Kind::Secondary, Size::Small, 0.0, open && !form.add_user.trim().is_empty())
                        .clicked()
                    {
                        add = Some((std::mem::take(&mut form.add_user), form.add_role.clone()));
                    }
                });
                if !open {
                    widgets::hint(ui, &t, "Only while the giveaway is open.");
                }
                ui.label(RichText::new(format!("{:.1} tickets in the draw", f(g, "tickets").max(0.0))).size(type_scale::SMALL).color(t.text_faint));
            });
        },
    );
    if let Some(user) = remove {
        act(app, "giveaway.remove", Value::map().with("user", user));
    }
    if let Some((user, role)) = add {
        act(app, "giveaway.enter", Value::map().with("user", user.trim()).with("role", role));
    }
}
