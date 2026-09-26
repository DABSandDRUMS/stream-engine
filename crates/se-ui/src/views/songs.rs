//! Song requests (§13): now playing, queue management (reorder/remove/approve/reject),
//! queue policy editor (§13.2), song library, YouTube quota, API key and relay link.
//! Data: queries `queue`, `queue.policy`, `queue.library`, `youtube.status`, `relay.status`;
//! commands: `queue.*`, `youtube.key.*`, `relay.*` actions.

use crate::app::App;
use egui::{RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Policy,
    Library,
    History,
    Setup,
}

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
    let open = q.get_path("open").is_some_and(Value::truthy);
    let paused = q.get_path("paused").is_some_and(Value::truthy);

    // ---- header ----------------------------------------------------------------------------
    ui.horizontal(|ui| {
        widgets::section(ui, &t, icon::QUEUE, "Song requests");
        widgets::pill(ui, &t, icon::LIVE, if open { "OPEN" } else { "CLOSED" }, if open { LedState::Active } else { LedState::Idle });
        if paused {
            widgets::pill(ui, &t, icon::STOP, "PAUSED", LedState::Armed);
        }
        let lookup = s(&q, "lookup");
        let (led, label) = match lookup {
            "full" => (LedState::Healthy, "search + links"),
            "links" => (LedState::Armed, "links + library only"),
            "library" => (LedState::Armed, "library only"),
            _ => (LedState::Error, "no API key"),
        };
        widgets::pill(ui, &t, icon::SEARCH, label, led);
        ui.label(RichText::new(format!("quota {}/{}", i(&q, "quota.used"), i(&q, "quota.limit"))).color(t.fg_dim));
        let url = s(&q, "url").to_string();
        if !url.is_empty() {
            ui.hyperlink_to(RichText::new(format!("{} public queue", icon::LINK)).color(t.accent), &url);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.button(if open { format!("{} Close requests", icon::STOP) } else { format!("{} Open requests", icon::PLAY) }).clicked() {
                act(app, if open { "queue.close" } else { "queue.open" }, Value::Null);
            }
            if ui.button(if paused { format!("{} Resume", icon::PLAY) } else { format!("{} Pause", icon::STOP) }).clicked() {
                act(app, if paused { "queue.resume" } else { "queue.pause" }, Value::Null);
            }
        });
    });
    ui.add_space(6.0);

    ui.columns(2, |cols| {
        // ---- left: now playing, upcoming, pending ---------------------------------------------
        let ui = &mut cols[0];
        let cur = q.get_path("now").cloned().unwrap_or(Value::Null);
        let state = app.m.str("song.state").to_string();
        let led = match state.as_str() {
            "playing" => LedState::Active,
            "paused" | "buffering" | "loading" | "ad" => LedState::Armed,
            _ => LedState::Idle,
        };
        widgets::card(ui, &t, icon::PLAY, "Now playing", led, |ui| {
            if cur.is_null() {
                ui.label(RichText::new("nothing playing").color(t.fg_dim));
                return;
            }
            ui.label(RichText::new(s(&cur, "title")).strong().size(16.0));
            ui.label(RichText::new(format!("{} · requested by {}", s(&cur, "channel"), s(&cur, "user"))).color(t.fg_dim));
            let dur = app.m.f("song.duration").max(f(&cur, "duration"));
            let pos = app.m.sig("song.position").map(f64::from).unwrap_or(f(&cur, "position"));
            let frac = if dur > 0.0 { (pos / dur).clamp(0.0, 1.0) as f32 } else { 0.0 };
            ui.add(egui::ProgressBar::new(frac).desired_height(6.0).fill(t.accent));
            ui.horizontal(|ui| {
                ui.label(RichText::new(format!("{} / {}", mmss(pos), mmss(dur))).monospace());
                ui.label(RichText::new(&state).color(if state == "ad" { t.yellow } else { t.fg_dim }));
                let needed = i(&cur, "votes_needed");
                if needed > 0 {
                    ui.label(RichText::new(format!("vote-skip {}/{needed}", i(&cur, "votes"))).color(t.fg_dim));
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button(format!("{} Skip", icon::CROSS)).clicked() {
                        act(app, "queue.skip", Value::Null);
                    }
                    if ui.small_button("ban song").on_hover_text("never allow this video again, and skip it").clicked() {
                        act(app, "queue.ban_song", Value::Null);
                    }
                    if ui.small_button("−10s").clicked() {
                        act(app, "queue.seek", Value::map().with("t", (pos - 10.0).max(0.0)));
                    }
                    if ui.small_button("+10s").clicked() {
                        act(app, "queue.seek", Value::map().with("t", pos + 10.0));
                    }
                });
            });
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut form.add).hint_text("YouTube link or search").desired_width(ui.available_width() - 90.0));
            let go = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
            if (ui.button(format!("{} Add", icon::PLAY)).clicked() || go) && !form.add.trim().is_empty() {
                act(app, "queue.request", Value::map().with("text", form.add.trim()));
                form.add.clear();
            }
        });
        ui.add_space(6.0);

        let pending = list(&q, "pending");
        if !pending.is_empty() {
            widgets::section(ui, &t, icon::MOD, &format!("Awaiting approval ({})", pending.len()));
            ui.add(egui::TextEdit::singleline(&mut form.reject_reason).hint_text("reject reason (optional)").desired_width(260.0));
            egui::Grid::new("songs-pending").striped(true).num_columns(4).spacing([10.0, 4.0]).show(ui, |ui| {
                for e in &pending {
                    ui.label(RichText::new(format!("#{}", i(e, "id"))).color(t.fg_dim));
                    ui.label(s(e, "title")).on_hover_text(s(e, "url"));
                    ui.label(RichText::new(format!("{} · {}", s(e, "user"), mmss(f(e, "duration")))).color(t.fg_dim));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new(format!("{} approve", icon::CHECK)).color(t.green)).clicked() {
                            act(app, "queue.approve", Value::map().with("id", i(e, "id")));
                        }
                        if ui.button(RichText::new(format!("{} reject", icon::CROSS)).color(t.red)).clicked() {
                            let mut args = Value::map().with("id", i(e, "id"));
                            if !form.reject_reason.trim().is_empty() {
                                args = args.with("reason", form.reject_reason.trim());
                            }
                            act(app, "queue.reject", args);
                        }
                    });
                    ui.end_row();
                }
            });
            ui.add_space(6.0);
        }

        let upcoming = list(&q, "upcoming");
        let total: f64 = upcoming.iter().map(|e| f(e, "duration")).sum();
        ui.horizontal(|ui| {
            widgets::section(ui, &t, icon::QUEUE, &format!("Up next ({})", upcoming.len()));
            ui.label(RichText::new(mmss(total)).color(t.fg_dim));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !upcoming.is_empty() && widgets::hold_button(ui, &t, "Clear queue", t.yellow, 0.8) {
                    act(app, "queue.clear", Value::Null);
                }
            });
        });
        egui::ScrollArea::vertical().id_salt("songs-upcoming").auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("songs-upcoming-grid").striped(true).num_columns(5).spacing([10.0, 4.0]).show(ui, |ui| {
                let n = upcoming.len() as i64;
                for e in &upcoming {
                    let (id, pos) = (i(e, "id"), i(e, "pos"));
                    ui.label(RichText::new(format!("{pos}")).color(t.fg_dim));
                    let title = if e.get_path("paid").is_some_and(Value::truthy) {
                        RichText::new(format!("★ {}", s(e, "title"))).color(t.yellow)
                    } else {
                        RichText::new(s(e, "title"))
                    };
                    ui.label(title).on_hover_text(format!("{}\n{}", s(e, "channel"), s(e, "url")));
                    ui.label(RichText::new(s(e, "user")).color(t.fg_dim));
                    ui.label(RichText::new(mmss(f(e, "duration"))).monospace().color(t.fg_dim));
                    ui.horizontal(|ui| {
                        if ui.add_enabled(pos > 1, egui::Button::new("▲").small()).on_hover_text("move up").clicked() {
                            act(app, "queue.reorder", Value::map().with("id", id).with("to", pos - 1));
                        }
                        if ui.add_enabled(pos < n, egui::Button::new("▼").small()).on_hover_text("move down").clicked() {
                            act(app, "queue.reorder", Value::map().with("id", id).with("to", pos + 1));
                        }
                        if ui.add_enabled(pos > 1, egui::Button::new("next").small()).on_hover_text("play next").clicked() {
                            act(app, "queue.reorder", Value::map().with("id", id).with("to", 1));
                        }
                        if ui.small_button(icon::CROSS).on_hover_text("remove").clicked() {
                            act(app, "queue.remove", Value::map().with("id", id));
                        }
                    });
                    ui.end_row();
                }
            });
        });

        // ---- right: policy / library / history / setup ------------------------------------------
        let ui = &mut cols[1];
        ui.horizontal(|ui| {
            for (tab, label) in [(Tab::Policy, "Policy"), (Tab::Library, "Library"), (Tab::History, "History"), (Tab::Setup, "YouTube & relay")] {
                if ui.selectable_label(form.tab == tab, label).clicked() {
                    form.tab = tab;
                    form.last_slow = 0.0;
                }
            }
        });
        ui.separator();
        match form.tab {
            Tab::Policy => policy(app, ui, &mut form),
            Tab::Library => library(app, ui, &mut form),
            Tab::History => {
                egui::ScrollArea::vertical().id_salt("songs-history").auto_shrink([false, false]).show(ui, |ui| {
                    egui::Grid::new("songs-history-grid").striped(true).num_columns(4).spacing([10.0, 4.0]).show(ui, |ui| {
                        for e in list(&q, "history") {
                            let st = s(&e, "status");
                            let c = match st {
                                "played" => t.green,
                                "error" => t.red,
                                _ => t.fg_dim,
                            };
                            ui.label(RichText::new(st).color(c));
                            ui.label(s(&e, "title")).on_hover_text(format!("{}\n{}", s(&e, "note"), s(&e, "url")));
                            ui.label(RichText::new(s(&e, "user")).color(t.fg_dim));
                            if ui.small_button("again").on_hover_text("queue it again").clicked() {
                                act(app, "queue.request", Value::map().with("text", s(&e, "video")));
                            }
                            ui.end_row();
                        }
                    });
                });
            }
            Tab::Setup => setup(app, ui, &mut form),
        }
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}

fn policy(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    let p = app.m.q("queue.policy").cloned().unwrap_or_default();
    let fields = list(&p, "fields");
    if fields.is_empty() {
        ui.label(RichText::new("waiting for the engine…").color(t.fg_dim));
        return;
    }
    let current = p.get_path("policy").cloned().unwrap_or_default();
    egui::ScrollArea::vertical().id_salt("songs-policy").auto_shrink([false, false]).max_height(ui.available_height() - 36.0).show(ui, |ui| {
        let mut group = "";
        for fd in &fields {
            let key = s(fd, "key").to_string();
            if s(fd, "group") != group {
                group = s(fd, "group");
                ui.add_space(4.0);
                ui.label(RichText::new(group).strong().color(t.accent));
            }
            let base = current.get_path(&key).cloned().unwrap_or(Value::Null);
            let mut v = form.draft.get(&key).cloned().unwrap_or_else(|| base.clone());
            ui.horizontal(|ui| {
                ui.add_sized(Vec2::new(210.0, 18.0), egui::Label::new(s(fd, "label")));
                match s(fd, "type") {
                    "bool" => {
                        let mut b = v.truthy();
                        if ui.checkbox(&mut b, "").changed() {
                            v = Value::Bool(b);
                        }
                    }
                    "int" => {
                        let mut n = v.as_i64().unwrap_or(0);
                        let (lo, hi) = (i(fd, "range.0"), i(fd, "range.1"));
                        if ui.add(egui::DragValue::new(&mut n).range(lo..=hi).suffix(format!(" {}", s(fd, "unit")))).changed() {
                            v = Value::Int(n);
                        }
                    }
                    "enum" => {
                        let mut sel = v.as_str().unwrap_or("").to_string();
                        egui::ComboBox::from_id_salt(format!("songs-pol-{key}")).selected_text(sel.clone()).show_ui(ui, |ui| {
                            for o in list(fd, "options") {
                                let o = o.to_string();
                                ui.selectable_value(&mut sel, o.clone(), o);
                            }
                        });
                        if Some(sel.as_str()) != v.as_str() {
                            v = Value::from(sel);
                        }
                    }
                    _ => {
                        let joined = || v.as_list().map(|l| l.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(", ")).unwrap_or_default();
                        let buf = form.lists.entry(key.clone()).or_insert_with(joined);
                        if ui
                            .add(egui::TextEdit::multiline(buf).desired_rows(1).desired_width(ui.available_width()).hint_text("comma or newline separated"))
                            .changed()
                        {
                            v = Value::from(buf.clone());
                        }
                    }
                }
            });
            if v != base {
                form.draft.insert(key, v);
            } else {
                form.draft.remove(&key);
            }
        }
    });
    ui.horizontal(|ui| {
        let dirty = !form.draft.is_empty();
        if ui.add_enabled(dirty, egui::Button::new(RichText::new(format!("{} Save policy ({})", icon::CHECK, form.draft.len())).strong())).clicked() {
            let args = Value::Map(std::mem::take(&mut form.draft));
            act(app, "queue.policy.set", args);
            form.lists.clear();
            app.m.query("queue.policy", Value::Null);
        }
        if ui.add_enabled(dirty, egui::Button::new("Discard")).clicked() {
            form.draft.clear();
            form.lists.clear();
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::hold_button(ui, &t, "Reset to defaults", t.yellow, 0.8) {
                act(app, "queue.policy.reset", Value::Null);
                form.draft.clear();
                form.lists.clear();
            }
        });
    });
}

fn library(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        if ui.add(egui::TextEdit::singleline(&mut form.library_filter).hint_text(format!("{} filter", icon::SEARCH)).desired_width(220.0)).changed() {
            form.last_slow = 0.0;
        }
        ui.label(RichText::new(format!("{} songs", app.m.q_list("queue.library").len())).color(t.fg_dim));
    });
    let rows: Vec<Value> = app.m.q_list("queue.library").to_vec();
    egui::ScrollArea::vertical().id_salt("songs-library").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("songs-library-grid").striped(true).num_columns(5).spacing([10.0, 4.0]).show(ui, |ui| {
            for h in ["title", "channel", "length", "plays", ""] {
                ui.label(RichText::new(h).small().color(t.fg_dim));
            }
            ui.end_row();
            for e in rows {
                let bad = !s(&e, "unplayable").is_empty();
                let title = RichText::new(s(&e, "title"));
                ui.label(if bad { title.color(t.fg_dim).strikethrough() } else { title }).on_hover_text(if bad { s(&e, "unplayable") } else { s(&e, "url") });
                ui.label(RichText::new(s(&e, "channel")).color(t.fg_dim));
                ui.label(RichText::new(mmss(f(&e, "duration"))).monospace());
                ui.label(format!("{}", i(&e, "plays")));
                if ui.add_enabled(!bad, egui::Button::new("queue").small()).clicked() {
                    act(app, "queue.request", Value::map().with("text", s(&e, "video")));
                }
                ui.end_row();
            }
        });
    });
}

fn setup(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    let yt = app.m.q("youtube.status").cloned().unwrap_or_default();
    let relay = app.m.q("relay.status").cloned().unwrap_or_default();
    let key_set = yt.get_path("key_set").is_some_and(Value::truthy);
    widgets::card(ui, &t, icon::SEARCH, "YouTube Data API", if key_set { LedState::Healthy } else { LedState::Error }, |ui| {
        ui.label(format!("API key: {}", if key_set { "stored in the keyring" } else { "not set" }));
        if let Some(e) = yt.get_path("last_error").and_then(Value::as_str) {
            ui.label(RichText::new(e).color(t.red));
        }
        ui.label(
            RichText::new(format!(
                "today (Pacific): {} of {} units · {} searches · {} lookups · resets {}",
                i(&yt, "quota.used"),
                i(&yt, "quota.limit"),
                i(&yt, "quota.searches"),
                i(&yt, "quota.lists"),
                reset_in(i(&yt, "quota.resets_at"))
            ))
            .color(t.fg_dim),
        );
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut form.key).password(true).hint_text("paste API key (AIza…)").desired_width(300.0));
            if ui.add_enabled(!form.key.trim().is_empty(), egui::Button::new("Save key")).clicked() {
                act(app, "youtube.key.set", Value::map().with("key", form.key.trim()));
                form.key.clear();
            }
            if key_set && widgets::hold_button(ui, &t, "Remove key", t.red, 0.8) {
                act(app, "youtube.key.clear", Value::Null);
            }
        });
        let days = list(&yt, "days");
        if days.len() > 1 {
            ui.label(
                RichText::new(days.iter().take(7).map(|d| format!("{} {}", &s(d, "day")[5..], i(d, "used"))).collect::<Vec<_>>().join("  ·  "))
                    .small()
                    .color(t.fg_dim),
            );
        }
    });
    ui.add_space(8.0);
    let connected = relay.get_path("connected").is_some_and(Value::truthy);
    let configured = relay.get_path("configured").is_some_and(Value::truthy);
    widgets::card(
        ui,
        &t,
        icon::LINK,
        "Cloudflare relay",
        if connected {
            LedState::Healthy
        } else if configured {
            LedState::Error
        } else {
            LedState::Idle
        },
        |ui| {
            ui.label(format!("link: {}", if s(&relay, "url").is_empty() { "— (set [relay] url in project.toml)" } else { s(&relay, "url") }));
            ui.label(RichText::new(s(&relay, "detail")).color(if connected { t.green } else { t.fg_dim }));
            if !s(&relay, "queue_url").is_empty() {
                ui.hyperlink_to(s(&relay, "queue_url"), s(&relay, "queue_url"));
            }
            ui.horizontal(|ui| {
                ui.add(egui::TextEdit::singleline(&mut form.secret).password(true).hint_text("shared secret (same as RELAY_SECRET)").desired_width(300.0));
                if ui.add_enabled(form.secret.trim().len() >= 32, egui::Button::new("Save secret")).clicked() {
                    act(app, "relay.secret.set", Value::map().with("secret", form.secret.trim()));
                    form.secret.clear();
                }
                if ui.button("Reconnect").clicked() {
                    act(app, "relay.reconnect", Value::Null);
                }
            });
        },
    );
}

fn reset_in(at: i64) -> String {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let left = (at - now).max(0);
    format!("in {}h {:02}m", left / 3600, left / 60 % 60)
}
