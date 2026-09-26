//! Song requests (§13): what's playing (big, with pause/skip), what's up next (approve, reorder,
//! remove), the request rules in plain words, the song library, what was played, and the
//! YouTube key / public song list.
//! Data: queries `queue`, `queue.policy`, `queue.library`, `youtube.status`, `relay.status`;
//! commands: `queue.*`, `youtube.key.*`, `relay.*` actions.

use crate::app::App;
use crate::views::rail::grouped;
use egui::{Align, CornerRadius, Layout, RichText, Sense, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_bold, font_medium, font_mono, font_semibold, mix, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Rules,
    Library,
    History,
    Setup,
}

const TABS: [(Tab, &str); 4] = [(Tab::Rules, "Rules"), (Tab::Library, "Song library"), (Tab::History, "Played"), (Tab::Setup, "YouTube & web")];

#[derive(Clone, Default)]
struct Form {
    tab: Tab,
    add: String,
    reject_reason: String,
    library_filter: String,
    key: String,
    secret: String,
    /// Policy edits not yet saved (field → value).
    draft: BTreeMap<String, Value>,
    /// Text buffers for list fields.
    lists: BTreeMap<String, String>,
    last_query: f64,
    last_slow: f64,
}

fn act(app: &mut App, name: &str, args: Value) {
    app.m.command(Op::Action { name: name.into(), args });
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn i(v: &Value, k: &str) -> i64 {
    v.get_path(k).and_then(Value::as_i64).unwrap_or(0)
}

fn f(v: &Value, k: &str) -> f64 {
    v.get_path(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn mmss(secs: f64) -> String {
    let t = secs.max(0.0) as i64;
    if t >= 3600 { format!("{}:{:02}:{:02}", t / 3600, t / 60 % 60, t % 60) } else { format!("{}:{:02}", t / 60, t % 60) }
}

fn list(v: &Value, k: &str) -> Vec<Value> {
    v.get_path(k).and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default()
}

const ROLES: [(&str, &str); 5] = [("everyone", "Everyone"), ("follower", "Followers"), ("sub", "Subscribers"), ("vip", "VIPs"), ("mod", "Mods only")];

fn role_label(r: &str) -> &str {
    ROLES.iter().find(|(k, _)| *k == r).map(|(_, l)| *l).unwrap_or(r)
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("songs-form");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    let now = ui.input(|i| i.time);
    if now - form.last_query > 0.5 {
        form.last_query = now;
        app.m.query("queue", Value::Null);
    }
    if now - form.last_slow > 2.0 {
        form.last_slow = now;
        app.m.query("queue.policy", Value::Null);
        app.m.query("youtube.status", Value::Null);
        app.m.query("relay.status", Value::Null);
        if form.tab == Tab::Library {
            app.m.query("queue.library", Value::map().with("q", form.library_filter.trim()).with("limit", 300));
        }
    }
    let q = app.m.q("queue").cloned().unwrap_or_default();

    egui::ScrollArea::vertical().id_salt("songs-page").auto_shrink([false, false]).show(ui, |ui| {
        if key_banner(app, ui, &q) {
            form.tab = Tab::Setup;
            form.last_slow = 0.0;
        }
        let w = ui.available_width();
        let right = (w * 0.34).clamp(380.0, 720.0);
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = 0.0;
            let left = w - right - spacing::L;
            ui.allocate_ui_with_layout(Vec2::new(left, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(left);
                now_playing(app, ui, &q);
                ui.add_space(spacing::L);
                pending(app, ui, &mut form, &q);
                up_next(app, ui, &mut form, &q);
            });
            ui.add_space(spacing::L);
            ui.allocate_ui_with_layout(Vec2::new(right, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(right);
                let mut idx = TABS.iter().position(|(tb, _)| *tb == form.tab).unwrap_or(0);
                let labels: Vec<&str> = TABS.iter().map(|(_, l)| *l).collect();
                if widgets::segmented(ui, &t, &mut idx, &labels) {
                    form.tab = TABS[idx].0;
                    form.last_slow = 0.0;
                }
                ui.add_space(spacing::M);
                match form.tab {
                    Tab::Rules => rules(app, ui, &mut form, &q),
                    Tab::Library => library(app, ui, &mut form),
                    Tab::History => history(app, ui, &q),
                    Tab::Setup => setup(app, ui, &mut form),
                }
            });
        });
        ui.add_space(spacing::XL);
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}

/// "Song search needs a YouTube key" when searching isn't available.
/// "Song search needs a YouTube key" when searching isn't available. Returns true when the
/// streamer wants to add the key (switch to the YouTube sub-view).
fn key_banner(app: &mut App, ui: &mut egui::Ui, q: &Value) -> bool {
    let t = app.t.clone();
    let yt = app.m.q("youtube.status").cloned().unwrap_or_default();
    let key_set = yt.get_path("key_set").is_some_and(Value::truthy);
    let err = yt.get_path("last_error").and_then(Value::as_str).unwrap_or("").to_string();
    let lookup = s(q, "lookup");
    if key_set && err.is_empty() && lookup != "off" {
        return false;
    }
    let (tone, title, body) = if !key_set {
        (widgets::Tone::Info, "Song search needs a YouTube key", "Until you add one, viewers can only ask for songs that were already played here. It's free.")
    } else if !err.is_empty() {
        (widgets::Tone::Warn, "YouTube isn't answering", "Song search is paused. Check your YouTube key.")
    } else {
        (widgets::Tone::Warn, "Song search is off", "Viewers can only ask for songs that were already played here.")
    };
    let open = widgets::callout(ui, &t, tone, icon::SEARCH, title, body, Some(if key_set { "Check the key" } else { "Add a YouTube key" }));
    ui.add_space(spacing::L);
    open
}

/// Numbered steps for getting a YouTube key, with a button to Google's page.
fn key_steps(ui: &mut egui::Ui, t: &se_ui_kit::Theme) {
    widgets::details(ui, t, "yt-show-me-how", "Show me how", |ui| {
        for (n, step) in [
            "Open Google's page below and sign in with any Google account.",
            "If Google asks, make a project. Any name works.",
            "Press \"Enable\" for YouTube, then \"Create credentials\".",
            "Copy the key Google shows you and paste it above.",
        ]
        .iter()
        .enumerate()
        {
            ui.horizontal_top(|ui| {
                ui.label(RichText::new(format!("{}.", n + 1)).font(font_semibold(type_scale::BODY)).color(t.accent));
                ui.add(egui::Label::new(RichText::new(*step).color(t.fg)).wrap());
            });
        }
        ui.add_space(spacing::S);
        if widgets::button_ex(ui, t, Some(icon::LINK), "Open Google's page", Kind::Secondary, Size::Small, 0.0, true).clicked() {
            ui.ctx().open_url(egui::OpenUrl::new_tab("https://console.cloud.google.com/apis/library/youtube.googleapis.com"));
        }
    });
}

fn now_playing(app: &mut App, ui: &mut egui::Ui, q: &Value) {
    let t = app.t.clone();
    let cur = q.get_path("now").cloned().unwrap_or(Value::Null);
    let paused = q.get_path("paused").is_some_and(Value::truthy);
    let state = app.m.str("song.state").to_string();
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        if cur.is_null() {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon::PLAY).size(type_scale::LARGE).color(t.text_faint));
                ui.add_space(spacing::S);
                ui.vertical(|ui| {
                    ui.label(RichText::new("Nothing playing").font(font_semibold(type_scale::LARGE)).color(t.fg));
                    widgets::hint(ui, &t, "The next song starts here by itself.");
                });
            });
            return;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new("Now playing").font(font_semibold(type_scale::SMALL + 0.5)).color(t.text_dim));
            let (label, c) = match state.as_str() {
                "playing" => ("Playing", t.green),
                "paused" => ("Paused", t.yellow),
                "ad" => ("YouTube ad playing", t.yellow),
                "buffering" | "loading" => ("Loading…", t.text_dim),
                _ if paused => ("Paused", t.yellow),
                _ => ("", t.text_dim),
            };
            if !label.is_empty() {
                widgets::badge(ui, &t, label, c);
            }
        });
        ui.add_space(spacing::XS);
        ui.add(egui::Label::new(RichText::new(s(&cur, "title")).font(font_bold(type_scale::TITLE)).color(t.fg)).wrap());
        let by = if s(&cur, "channel").is_empty() {
            format!("Asked for by {}", s(&cur, "user"))
        } else {
            format!("Asked for by {}  ·  {}", s(&cur, "user"), s(&cur, "channel"))
        };
        ui.label(RichText::new(by).size(type_scale::LARGE).color(t.text_dim));
        ui.add_space(spacing::M);
        let dur = app.m.f("song.duration").max(f(&cur, "duration"));
        let pos = app.m.sig("song.position").map(f64::from).unwrap_or(f(&cur, "position"));
        let frac = if dur > 0.0 { (pos / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 8.0), Sense::hover());
        ui.painter().rect_filled(rect, CornerRadius::same(4), t.inset);
        let mut fill = rect;
        fill.set_width(rect.width() * frac);
        ui.painter().rect_filled(fill, CornerRadius::same(4), t.accent);
        ui.add_space(spacing::XS);
        ui.horizontal(|ui| {
            ui.label(RichText::new(mmss(pos)).font(font_mono(type_scale::SMALL + 0.5)).color(t.text_dim));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(mmss(dur)).font(font_mono(type_scale::SMALL + 0.5)).color(t.text_dim));
            });
        });
        ui.add_space(spacing::M);
        ui.horizontal(|ui| {
            let (ic, label, a) = if paused { (icon::PLAY, "Play", "queue.resume") } else { (icon::PAUSE, "Pause", "queue.pause") };
            if widgets::button_ex(ui, &t, Some(ic), label, Kind::Primary, Size::Medium, 110.0, true).clicked() {
                act(app, a, Value::Null);
            }
            if widgets::button_ex(ui, &t, Some(icon::RIGHT), "Skip", Kind::Secondary, Size::Medium, 100.0, true).on_hover_text("Play the next song").clicked() {
                act(app, "queue.skip", Value::Null);
            }
            ui.add_space(spacing::S);
            if widgets::button_ex(ui, &t, Some(icon::UNDO), "10 s", Kind::Ghost, Size::Medium, 0.0, true).on_hover_text("Back 10 seconds").clicked() {
                act(app, "queue.seek", Value::map().with("t", (pos - 10.0).max(0.0)));
            }
            if widgets::button_ex(ui, &t, Some(icon::RIGHT), "10 s", Kind::Ghost, Size::Medium, 0.0, true).on_hover_text("Forward 10 seconds").clicked() {
                act(app, "queue.seek", Value::map().with("t", pos + 10.0));
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::hold_button(ui, &t, "Hold to ban this song", t.bright_red, 0.8) {
                    act(app, "queue.ban_song", Value::Null);
                }
            });
        });
        let needed = i(&cur, "votes_needed");
        if needed > 0 {
            ui.add_space(spacing::S);
            widgets::hint(ui, &t, &format!("Viewers voting to skip: {} of {needed}", i(&cur, "votes")));
        }
    });
}

fn pending(app: &mut App, ui: &mut egui::Ui, form: &mut Form, q: &Value) {
    let t = app.t.clone();
    let pending = list(q, "pending");
    if pending.is_empty() {
        return;
    }
    let mut act_: Option<(&'static str, Value)> = None;
    egui::Frame::new()
        .fill(mix(t.surface, t.yellow, 0.06))
        .stroke(egui::Stroke::new(1.0, mix(t.border, t.yellow, 0.4)))
        .corner_radius(CornerRadius::same(12))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(RichText::new(format!("Waiting for your OK · {}", pending.len())).font(font_semibold(type_scale::LARGE)).color(t.fg));
                    ui.label(RichText::new("These requests only play once you say yes.").size(type_scale::SMALL + 0.5).color(t.text_dim));
                });
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    ui.add(se_ui_kit::widgets::field(&mut form.reject_reason).hint_text("Reason for saying no (optional)").desired_width(240.0));
                });
            });
            ui.add_space(spacing::M);
            for e in &pending {
                let id = i(e, "id");
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width() - 230.0);
                        ui.add(egui::Label::new(RichText::new(s(e, "title")).font(font_medium(type_scale::BODY + 0.5)).color(t.fg)).truncate())
                            .on_hover_text(s(e, "url"));
                        ui.label(RichText::new(format!("{} · {}", s(e, "user"), mmss(f(e, "duration")))).size(type_scale::SMALL + 0.5).color(t.text_dim));
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::button_ex(ui, &t, Some(icon::CROSS), "No", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                            let mut args = Value::map().with("id", id);
                            if !form.reject_reason.trim().is_empty() {
                                args = args.with("reason", form.reject_reason.trim());
                            }
                            act_ = Some(("queue.reject", args));
                        }
                        if widgets::button_ex(ui, &t, Some(icon::CHECK), "Play it", Kind::Primary, Size::Medium, 0.0, true).clicked() {
                            act_ = Some(("queue.approve", Value::map().with("id", id)));
                        }
                    });
                });
                ui.add_space(spacing::S);
            }
        });
    ui.add_space(spacing::L);
    if let Some((n, a)) = act_ {
        act(app, n, a);
    }
}

fn up_next(app: &mut App, ui: &mut egui::Ui, form: &mut Form, q: &Value) {
    let t = app.t.clone();
    let upcoming = list(q, "upcoming");
    let total: f64 = upcoming.iter().map(|e| f(e, "duration")).sum();
    let sub = match upcoming.len() {
        0 => String::new(),
        1 => format!("1 song · {}", mmss(total)),
        n => format!("{n} songs · {} in total", mmss(total)),
    };
    let mut clear = false;
    let mut act_: Option<(&'static str, Value)> = None;
    widgets::titled(
        ui,
        &t,
        "Up next",
        &sub,
        |ui| {
            clear = !upcoming.is_empty() && widgets::hold_button(ui, &t, "Hold to clear", t.yellow, 0.8);
        },
        |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                // desired_width is the text area: leave room for the field's 10 px side margins,
                // the gap and the 100 px button so the two sit side by side at one height.
                let w = ui.available_width() - 100.0 - spacing::S - 24.0;
                let r = ui.add(
                    se_ui_kit::widgets::field(&mut form.add)
                        .hint_text("Add a song: paste a YouTube link or type a name")
                        .desired_width(w)
                        .min_size(Vec2::new(0.0, 36.0)),
                );
                let go = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let ready = !form.add.trim().is_empty();
                ui.add_space((ui.available_width() - 100.0).max(0.0));
                if (widgets::button_ex(ui, &t, Some(icon::PLUS), "Add", Kind::Secondary, Size::Medium, 100.0, ready).clicked() || go) && ready {
                    act_ = Some(("queue.request", Value::map().with("text", form.add.trim())));
                    form.add.clear();
                }
            });
            ui.add_space(spacing::M);
            if upcoming.is_empty() {
                widgets::empty_state(ui, &t, icon::QUEUE, "No songs waiting", "Viewers ask for songs with !sr in chat. You can add one above.", None);
                return;
            }
            let n = upcoming.len() as i64;
            for e in &upcoming {
                let (id, pos) = (i(e, "id"), i(e, "pos"));
                let bg = ui.painter().add(egui::Shape::Noop);
                let row = egui::Frame::new().inner_margin(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.add_sized([28.0, 20.0], egui::Label::new(RichText::new(pos.to_string()).font(font_mono(type_scale::BODY)).color(t.text_faint)));
                        ui.vertical(|ui| {
                            ui.set_width(ui.available_width() - 190.0);
                            ui.horizontal(|ui| {
                                if e.get_path("paid").is_some_and(Value::truthy) {
                                    widgets::badge(ui, &t, "Paid", t.yellow).on_hover_text("Paid requests go first");
                                }
                                ui.add(egui::Label::new(RichText::new(s(e, "title")).font(font_medium(type_scale::BODY + 0.5)).color(t.fg)).truncate())
                                    .on_hover_text(format!("{}\n{}", s(e, "channel"), s(e, "url")));
                            });
                            ui.label(RichText::new(format!("{} · {}", s(e, "user"), mmss(f(e, "duration")))).size(type_scale::SMALL + 0.5).color(t.text_dim));
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::icon_button(ui, &t, icon::TRASH, "Remove").clicked() {
                                act_ = Some(("queue.remove", Value::map().with("id", id)));
                            }
                            if pos < n && widgets::icon_button(ui, &t, icon::DOWN, "Move down").clicked() {
                                act_ = Some(("queue.reorder", Value::map().with("id", id).with("to", pos + 1)));
                            }
                            if pos > 1 && widgets::icon_button(ui, &t, icon::UP, "Move up").clicked() {
                                act_ = Some(("queue.reorder", Value::map().with("id", id).with("to", pos - 1)));
                            }
                            if pos > 1 && widgets::button_ex(ui, &t, None, "Play next", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                                act_ = Some(("queue.reorder", Value::map().with("id", id).with("to", 1)));
                            }
                        });
                    });
                });
                if ui.rect_contains_pointer(row.response.rect) {
                    ui.painter().set(bg, egui::Shape::rect_filled(row.response.rect, CornerRadius::same(8), t.surface_hi));
                }
            }
        },
    );
    if clear {
        act(app, "queue.clear", Value::Null);
    }
    if let Some((n, a)) = act_ {
        act(app, n, a);
    }
}

// ---- rules ---------------------------------------------------------------------------------------

/// Current value of a policy field (draft first).
fn pol(form: &Form, current: &Value, key: &str) -> Value {
    form.draft.get(key).cloned().unwrap_or_else(|| current.get_path(key).cloned().unwrap_or(Value::Null))
}

fn set_pol(form: &mut Form, current: &Value, key: &str, v: Value) {
    if current.get_path(key) == Some(&v) {
        form.draft.remove(key);
    } else {
        form.draft.insert(key.to_string(), v);
    }
}

fn rule_row(ui: &mut egui::Ui, t: &Theme, label: &str, help: &str, body: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.set_width((ui.available_width() - 190.0).max(140.0));
            ui.label(RichText::new(label).font(font_medium(type_scale::BODY)).color(t.fg));
            if !help.is_empty() {
                ui.add(egui::Label::new(RichText::new(help).size(type_scale::SMALL).color(t.text_dim)).wrap());
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), body);
    });
    ui.add_space(spacing::M);
}

fn rules(app: &mut App, ui: &mut egui::Ui, form: &mut Form, q: &Value) {
    let t = app.t.clone();
    let p = app.m.q("queue.policy").cloned().unwrap_or_default();
    let fields = list(&p, "fields");
    let current = p.get_path("policy").cloned().unwrap_or_default();
    let open = q.get_path("open").is_some_and(Value::truthy);
    let url = s(q, "url").to_string();
    widgets::titled(
        ui,
        &t,
        "Request rules",
        "Who can ask for songs, and how many.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let mut on = open;
            if widgets::toggle_row(ui, &t, "Viewers can request songs", if open { "Open: !sr works in chat." } else { "Closed: !sr is turned off." }, &mut on)
                .changed()
            {
                act(app, if on { "queue.open" } else { "queue.close" }, Value::Null);
            }
            ui.add_space(spacing::M);
            if fields.is_empty() {
                widgets::hint(ui, &t, "Loading the rules…");
                return;
            }
            rule_row(ui, &t, "Who can request", "", |ui| {
                let mut sel = pol(form, &current, "min_role").as_str().unwrap_or("everyone").to_string();
                egui::ComboBox::from_id_salt("songs-min-role").selected_text(role_label(&sel).to_string()).width(170.0).show_ui(ui, |ui| {
                    for (k, l) in ROLES {
                        ui.selectable_value(&mut sel, k.to_string(), l);
                    }
                });
                set_pol(form, &current, "min_role", Value::from(sel));
            });
            rule_row(ui, &t, "Longest song", "", |ui| {
                let mut mins = pol(form, &current, "max_duration_s").as_f64().unwrap_or(0.0) / 60.0;
                if ui
                    .add(
                        egui::DragValue::new(&mut mins)
                            .range(0.0..=240.0)
                            .speed(0.25)
                            .custom_formatter(|v, _| if v.fract() == 0.0 { format!("{v:.0}") } else { format!("{v:.1}") })
                            .suffix(" min"),
                    )
                    .changed()
                {
                    set_pol(form, &current, "max_duration_s", Value::Int((mins * 60.0).round() as i64));
                }
            });
            rule_row(ui, &t, "Songs per person", "How many one viewer can have waiting.", |ui| {
                let mut n = pol(form, &current, "max_per_user").as_i64().unwrap_or(0);
                if ui.add(egui::DragValue::new(&mut n).range(0..=100)).changed() {
                    set_pol(form, &current, "max_per_user", Value::Int(n));
                }
            });
            rule_row(ui, &t, "Most songs in the list", "", |ui| {
                let mut n = pol(form, &current, "max_queue").as_i64().unwrap_or(0);
                if ui.add(egui::DragValue::new(&mut n).range(0..=1000)).changed() {
                    set_pol(form, &current, "max_queue", Value::Int(n));
                }
            });
            rule_row(ui, &t, "Check requests first", "You or a mod says yes before a song is added.", |ui| {
                let mut sel = pol(form, &current, "approval").as_str().unwrap_or("off").to_string();
                let below = pol(form, &current, "approval_below").as_str().unwrap_or("sub").to_string();
                let label = |k: &str| match k {
                    "off" => "No".to_string(),
                    "all" => "Every request".to_string(),
                    _ => format!("Below {}", role_label(&below)),
                };
                egui::ComboBox::from_id_salt("songs-approval").selected_text(label(&sel)).width(170.0).show_ui(ui, |ui| {
                    for k in ["off", "all", "role"] {
                        ui.selectable_value(&mut sel, k.to_string(), label(k));
                    }
                });
                set_pol(form, &current, "approval", Value::from(sel));
            });
            if pol(form, &current, "approval").as_str() == Some("role") {
                rule_row(ui, &t, "Check requests from people below", "", |ui| {
                    let mut sel = pol(form, &current, "approval_below").as_str().unwrap_or("sub").to_string();
                    egui::ComboBox::from_id_salt("songs-approval-below").selected_text(role_label(&sel).to_string()).width(170.0).show_ui(ui, |ui| {
                        for (k, l) in ROLES {
                            ui.selectable_value(&mut sel, k.to_string(), l);
                        }
                    });
                    set_pol(form, &current, "approval_below", Value::from(sel));
                });
            }
            rule_row(ui, &t, "Paid requests go first", "Requests paid with bits or points skip the line.", |ui| {
                let mut b = pol(form, &current, "paid_skip_line").truthy();
                if widgets::toggle(ui, &t, &mut b).changed() {
                    set_pol(form, &current, "paid_skip_line", Value::Bool(b));
                }
            });
            widgets::details(ui, &t, "songs-all-rules", "All rules", |ui| all_rules(app, ui, form, &fields, &current));
            ui.add_space(spacing::M);
            let dirty = !form.draft.is_empty();
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, &t, Some(icon::CHECK), "Save rules", Kind::Primary, Size::Medium, 0.0, dirty).clicked() {
                    let args = Value::Map(std::mem::take(&mut form.draft));
                    act(app, "queue.policy.set", args);
                    form.lists.clear();
                    app.m.query("queue.policy", Value::Null);
                }
                if dirty {
                    if widgets::button_ex(ui, &t, Some(icon::UNDO), "Undo changes", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                        form.draft.clear();
                        form.lists.clear();
                    }
                } else {
                    ui.add_space(spacing::S);
                    widgets::hint(ui, &t, "Everything is saved.");
                }
            });
        },
    );
    if !url.is_empty() {
        ui.add_space(spacing::L);
        widgets::panel(ui, &t, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Public song list").font(font_semibold(type_scale::LARGE)).color(t.fg));
            widgets::hint(ui, &t, "A web page where viewers can see what's coming up.");
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, &t, Some(icon::LINK), "Open", Kind::Secondary, Size::Medium, 0.0, true).clicked() {
                    ui.ctx().open_url(egui::OpenUrl::new_tab(&url));
                }
                if widgets::button_ex(ui, &t, Some(icon::COPY), "Copy link", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                    ui.ctx().copy_text(url.clone());
                }
            });
        });
    }
}

/// Every policy field the engine offers, generic editor (behind "All rules").
fn all_rules(app: &mut App, ui: &mut egui::Ui, form: &mut Form, fields: &[Value], current: &Value) {
    let t = app.t.clone();
    let mut group = "";
    for fd in fields {
        let key = s(fd, "key").to_string();
        if s(fd, "group") != group {
            group = s(fd, "group");
            widgets::section(ui, &t, "", &group.to_uppercase());
        }
        let base = current.get_path(&key).cloned().unwrap_or(Value::Null);
        let mut v = form.draft.get(&key).cloned().unwrap_or_else(|| base.clone());
        rule_row(ui, &t, s(fd, "label"), "", |ui| match s(fd, "type") {
            "bool" => {
                let mut b = v.truthy();
                if widgets::toggle(ui, &t, &mut b).changed() {
                    v = Value::Bool(b);
                }
            }
            "int" => {
                let mut n = v.as_i64().unwrap_or(0);
                let (lo, hi) = (i(fd, "range.0"), i(fd, "range.1"));
                let unit = match s(fd, "unit") {
                    "s" => "seconds",
                    "min" => "minutes",
                    u => u,
                };
                if ui.add(egui::DragValue::new(&mut n).range(lo..=hi).suffix(format!(" {unit}"))).changed() {
                    v = Value::Int(n);
                }
            }
            "enum" => {
                let mut sel = v.as_str().unwrap_or("").to_string();
                egui::ComboBox::from_id_salt(format!("songs-pol-{key}")).selected_text(crate::views::live::nice(&sel)).show_ui(ui, |ui| {
                    for o in list(fd, "options") {
                        let o = o.as_str().unwrap_or("").to_string();
                        ui.selectable_value(&mut sel, o.clone(), crate::views::live::nice(&o));
                    }
                });
                if Some(sel.as_str()) != v.as_str() {
                    v = Value::from(sel);
                }
            }
            _ => {
                let joined = || {
                    v.as_list()
                        .map(|l| l.iter().map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string())).collect::<Vec<_>>().join(", "))
                        .unwrap_or_default()
                };
                let buf = form.lists.entry(key.clone()).or_insert_with(joined);
                if ui.add(egui::TextEdit::multiline(buf).desired_rows(1).desired_width(180.0).hint_text("Comma separated")).changed() {
                    v = Value::from(buf.clone());
                }
            }
        });
        if v != base {
            form.draft.insert(key, v);
        } else {
            form.draft.remove(&key);
        }
    }
    if widgets::hold_button(ui, &t, "Hold to reset all rules", t.yellow, 0.8) {
        act(app, "queue.policy.reset", Value::Null);
        form.draft.clear();
        form.lists.clear();
    }
}

// ---- library / played / setup --------------------------------------------------------------------

fn library(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    let rows: Vec<Value> = app.m.q_list("queue.library").to_vec();
    widgets::titled(
        ui,
        &t,
        "Song library",
        "Every song that was asked for before. These work even without YouTube search.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if ui
                .add(
                    se_ui_kit::widgets::field(&mut form.library_filter).hint_text(format!("{}  Search the library", icon::SEARCH)).desired_width(f32::INFINITY),
                )
                .changed()
            {
                form.last_slow = 0.0;
            }
            ui.add_space(spacing::S);
            if rows.is_empty() {
                widgets::empty_state(
                    ui,
                    &t,
                    icon::MUSIC,
                    if form.library_filter.is_empty() { "The library is empty" } else { "No matches" },
                    "Songs land here after someone asks for them.",
                    None,
                );
                return;
            }
            widgets::hint(ui, &t, &format!("{} songs", grouped(rows.len() as i64)));
            let mut req = None;
            {
                for e in &rows {
                    let bad = !s(e, "unplayable").is_empty();
                    ui.horizontal(|ui| {
                        ui.vertical(|ui| {
                            ui.set_width(ui.available_width() - 90.0);
                            let title = RichText::new(s(e, "title")).font(font_medium(type_scale::BODY)).color(if bad { t.text_faint } else { t.fg });
                            ui.add(egui::Label::new(if bad { title.strikethrough() } else { title }).truncate()).on_hover_text(if bad {
                                s(e, "unplayable")
                            } else {
                                s(e, "url")
                            });
                            let plays = i(e, "plays");
                            ui.label(
                                RichText::new(format!(
                                    "{} · {} · played {plays} time{}",
                                    s(e, "channel"),
                                    mmss(f(e, "duration")),
                                    if plays == 1 { "" } else { "s" }
                                ))
                                .size(type_scale::SMALL)
                                .color(t.text_dim),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            if widgets::button_ex(ui, &t, Some(icon::PLUS), "Add", Kind::Ghost, Size::Small, 0.0, !bad)
                                .on_hover_text("Add to Up next")
                                .clicked()
                            {
                                req = Some(s(e, "video").to_string());
                            }
                        });
                    });
                    ui.add_space(spacing::XS);
                }
            }
            if let Some(v) = req {
                act(app, "queue.request", Value::map().with("text", v));
            }
        },
    );
}

fn history(app: &mut App, ui: &mut egui::Ui, q: &Value) {
    let t = app.t.clone();
    let rows = list(q, "history");
    widgets::titled(
        ui,
        &t,
        "Played",
        "What already played this session.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if rows.is_empty() {
                widgets::empty_state(ui, &t, icon::CLOCK, "Nothing played yet", "Songs show up here after they play.", None);
                return;
            }
            let mut req = None;
            for e in &rows {
                ui.horizontal(|ui| {
                    ui.vertical(|ui| {
                        ui.set_width(ui.available_width() - 100.0);
                        ui.add(egui::Label::new(RichText::new(s(e, "title")).font(font_medium(type_scale::BODY)).color(t.fg)).truncate())
                            .on_hover_text(s(e, "url"));
                        ui.horizontal(|ui| {
                            let (label, c) = match s(e, "status") {
                                "played" => ("Played", t.green),
                                "error" => ("Couldn't play", t.bright_red),
                                "skipped" => ("Skipped", t.text_dim),
                                "rejected" => ("Said no", t.text_dim),
                                "removed" => ("Removed", t.text_dim),
                                other => (other, t.text_dim),
                            };
                            let r = widgets::badge(ui, &t, &crate::views::live::nice(label), c);
                            if !s(e, "note").is_empty() {
                                r.on_hover_text(s(e, "note"));
                            }
                            ui.label(RichText::new(s(e, "user")).size(type_scale::SMALL).color(t.text_dim));
                        });
                    });
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        if widgets::button_ex(ui, &t, Some(icon::UNDO), "Again", Kind::Ghost, Size::Small, 0.0, true)
                            .on_hover_text("Add it to Up next again")
                            .clicked()
                        {
                            req = Some(s(e, "video").to_string());
                        }
                    });
                });
                ui.add_space(spacing::S);
            }
            if let Some(v) = req {
                act(app, "queue.request", Value::map().with("text", v));
            }
        },
    );
}

fn setup(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    let yt = app.m.q("youtube.status").cloned().unwrap_or_default();
    let relay = app.m.q("relay.status").cloned().unwrap_or_default();
    let key_set = yt.get_path("key_set").is_some_and(Value::truthy);
    let mut remove_key = false;
    widgets::titled(
        ui,
        &t,
        "YouTube",
        if key_set { "Song search is on. Your key is kept safe in the system keyring." } else { "Song search needs a free YouTube key." },
        |ui| {
            widgets::badge(ui, &t, if key_set { "Connected" } else { "No key" }, if key_set { t.green } else { t.yellow });
        },
        |ui| {
            ui.set_width(ui.available_width());
            if let Some(e) = yt.get_path("last_error").and_then(Value::as_str) {
                ui.label(RichText::new(format!("YouTube said: {e}")).color(t.bright_red));
                ui.add_space(spacing::S);
            }
            if key_set {
                let (used, limit) = (i(&yt, "quota.used"), i(&yt, "quota.limit"));
                ui.label(
                    RichText::new(format!("Used {} of {} today · resets {}", grouped(used), grouped(limit), reset_in(i(&yt, "quota.resets_at")))).color(t.fg),
                );
                let frac = if limit > 0 { used as f32 / limit as f32 } else { 0.0 };
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
                ui.painter().rect_filled(rect, CornerRadius::same(3), t.inset);
                let mut fill = rect;
                fill.set_width(rect.width() * frac.clamp(0.0, 1.0));
                ui.painter().rect_filled(fill, CornerRadius::same(3), if frac > 0.9 { t.yellow } else { t.accent });
                ui.add_space(spacing::XS);
                widgets::hint(
                    ui,
                    &t,
                    &format!("{} searches and {} link lookups today. A search costs much more than a link.", i(&yt, "quota.searches"), i(&yt, "quota.lists")),
                );
                ui.add_space(spacing::M);
            }
            ui.horizontal(|ui| {
                ui.add(
                    se_ui_kit::widgets::field(&mut form.key)
                        .password(true)
                        .hint_text(if key_set { "Paste a new key to replace it" } else { "Paste your YouTube key" })
                        .desired_width(ui.available_width() - 90.0),
                );
                if widgets::button_ex(ui, &t, None, "Save", Kind::Primary, Size::Medium, 80.0, !form.key.trim().is_empty()).clicked() {
                    act(app, "youtube.key.set", Value::map().with("key", form.key.trim()));
                    form.key.clear();
                }
            });
            if !key_set {
                key_steps(ui, &t);
            }
            widgets::details(ui, &t, "yt-details", "Details", |ui| {
                let days = list(&yt, "days");
                if days.len() > 1 {
                    ui.label(
                        RichText::new(
                            days.iter().take(7).map(|d| format!("{} {}", s(d, "day").get(5..).unwrap_or(""), i(d, "used"))).collect::<Vec<_>>().join("  ·  "),
                        )
                        .font(font_mono(type_scale::SMALL))
                        .color(t.text_dim),
                    );
                }
                if key_set {
                    remove_key = widgets::hold_button(ui, &t, "Hold to remove the key", t.bright_red, 0.8);
                }
            });
        },
    );
    if remove_key {
        act(app, "youtube.key.clear", Value::Null);
    }
    ui.add_space(spacing::L);
    let connected = relay.get_path("connected").is_some_and(Value::truthy);
    let configured = relay.get_path("configured").is_some_and(Value::truthy);
    let mut reconnect = false;
    widgets::titled(
        ui,
        &t,
        "Public song list & tips",
        "A web page with your song list, Ko-fi tips, and mods helping from their browser.",
        |ui| {
            let (label, c) = if connected {
                ("Connected", t.green)
            } else if configured {
                ("Not connected", t.bright_red)
            } else {
                ("Not set up", t.text_dim)
            };
            widgets::badge(ui, &t, label, c);
        },
        |ui| {
            ui.set_width(ui.available_width());
            let qurl = s(&relay, "queue_url").to_string();
            if !qurl.is_empty() {
                ui.horizontal(|ui| {
                    if widgets::button_ex(ui, &t, Some(icon::LINK), "Open the song list page", Kind::Secondary, Size::Medium, 0.0, true).clicked() {
                        ui.ctx().open_url(egui::OpenUrl::new_tab(&qurl));
                    }
                    if widgets::button_ex(ui, &t, Some(icon::COPY), "Copy link", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                        ui.ctx().copy_text(qurl.clone());
                    }
                });
            } else if !configured {
                widgets::hint(ui, &t, "Needs a Cloudflare account and your own web address. Settings → Get started has the steps.");
            }
            if configured && !connected && widgets::button_ex(ui, &t, Some(icon::LINK), "Connect again", Kind::Secondary, Size::Medium, 0.0, true).clicked() {
                reconnect = true;
            }
            widgets::details(ui, &t, "relay-details", "Details", |ui| {
                ui.label(
                    RichText::new(if s(&relay, "url").is_empty() { "No address set ([relay] url in project.toml)" } else { s(&relay, "url") })
                        .font(font_mono(type_scale::SMALL))
                        .color(t.text_dim),
                );
                ui.label(RichText::new(s(&relay, "detail")).size(type_scale::SMALL).color(t.text_dim));
                ui.horizontal(|ui| {
                    ui.add(se_ui_kit::widgets::field(&mut form.secret).password(true).hint_text("Shared password (same as RELAY_SECRET)").desired_width(260.0));
                    if widgets::button_ex(ui, &t, None, "Save", Kind::Secondary, Size::Small, 0.0, form.secret.trim().len() >= 32).clicked() {
                        act(app, "relay.secret.set", Value::map().with("secret", form.secret.trim()));
                        form.secret.clear();
                    }
                    if widgets::button_ex(ui, &t, None, "Reconnect", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                        reconnect = true;
                    }
                });
            });
        },
    );
    if reconnect {
        act(app, "relay.reconnect", Value::Null);
    }
}

fn reset_in(at: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let left = (at - now).max(0);
    if left >= 3600 { format!("in {} h {} min", left / 3600, left / 60 % 60) } else { format!("in {} min", left / 60) }
}
