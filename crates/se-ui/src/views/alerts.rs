//! Alerts (§14.1–14.2): live queue with veto, test buttons (simulator), routing and
//! variations, queue policy, goals, and stats.
//! Data: queries `alerts`, `alerts.config`; state `alerts.*`, `stats.*`, `goals.*`;
//! commands: `alerts.veto|approve|skip|pause|resume|clear|replay`, `alerts.edit|add|remove|policy`,
//! `goals.set|add|reset|save|delete`, `stats.purge_simulated`, `sim.*`.

use crate::app::App;
use egui::RichText;
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};
use std::collections::BTreeMap;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Routing,
    Queue,
    Goals,
    Stats,
}

const LOOK: [(&str, &str); 9] = [
    ("title", "Title"),
    ("message", "Message"),
    ("sound", "Sound"),
    ("duration", "Duration"),
    ("priority", "Priority"),
    ("tts", "TTS"),
    ("tts_text", "TTS text"),
    ("voice", "Voice"),
    ("do", "Runs"),
];

#[derive(Clone, Default)]
struct Form {
    tab: Tab,
    /// `alerts.config` edits keyed by `file|path|field` (as typed).
    edits: BTreeMap<String, String>,
    policy: BTreeMap<String, String>,
    new_alert_file: String,
    new_alert_name: String,
    new_alert_when: String,
    new_var: BTreeMap<String, (String, String)>,
    goal_new: (String, String, String),
    goal_set: BTreeMap<String, String>,
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

fn fmt_num(x: f64) -> String {
    if x.fract() == 0.0 { format!("{}", x as i64) } else { format!("{x:.2}") }
}

fn dur(ms: i64) -> String {
    if ms <= 0 {
        String::new()
    } else if ms % 1000 == 0 {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

/// Text form of a look field from `alerts.config`.
fn look_text(look: &Value, k: &str) -> String {
    match k {
        "duration" => dur(i(look, "duration_ms")),
        "do" => look.get_path("do").and_then(Value::as_list).map(|l| l.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("; ")).unwrap_or_default(),
        _ => match look.get_path(k) {
            Some(Value::Str(s)) => s.clone(),
            Some(Value::Null) | None => String::new(),
            Some(v) => v.to_string(),
        },
    }
}

/// Typed field value from what was typed (empty = remove the field).
fn field_value(k: &str, text: &str) -> Result<Value, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(Value::Null);
    }
    Ok(match k {
        "priority" => Value::Int(t.parse().map_err(|_| format!("{k} must be a whole number"))?),
        "tts" | "enabled" | "veto" | "interrupt" => Value::Bool(matches!(t, "true" | "yes" | "on" | "1")),
        "do" => Value::List(t.split(';').map(str::trim).filter(|x| !x.is_empty()).map(|x| Value::Str(x.into())).collect()),
        _ => Value::Str(t.into()),
    })
}

const TESTS: [(&str, &str, &str); 12] = [
    ("follow", "sim.follow", ""),
    ("sub", "sim.sub", ""),
    ("sub tier 3", "sim.sub", "tier=3"),
    ("resub 12 mo", "sim.resub", "months=12 message='a year of drums!'"),
    ("gift 5", "sim.gift_bomb", "count=5"),
    ("gift bomb 50", "sim.gift_bomb", "count=50"),
    ("cheer 50", "sim.cheer", "bits=50 message='nice groove Kappa'"),
    ("cheer 1000", "sim.cheer", "bits=1000 message='Cheer1000 HYPE'"),
    ("cheer 10000", "sim.cheer", "bits=10000 message='take my bits'"),
    ("raid 40", "sim.raid", "viewers=40"),
    ("tip $5", "sim.tip", "amount=5 message='for new sticks'"),
    ("tip $50", "sim.tip", "amount=50 message='cymbal fund!'"),
];

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("alerts-form");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    let now = ui.input(|i| i.time);
    if now - form.last_query > 0.25 {
        form.last_query = now;
        app.m.query("alerts", Value::Null);
    }
    if now - form.last_slow > 2.0 {
        form.last_slow = now;
        app.m.query("alerts.config", Value::Null);
    }
    let live = app.m.q("alerts").cloned().unwrap_or_default();
    let cfg = app.m.q("alerts.config").cloned().unwrap_or_default();

    ui.horizontal(|ui| {
        widgets::section(ui, &t, icon::ALERT, "Alerts");
        let paused = live.get_path("paused").is_some_and(Value::truthy);
        let enabled = app.m.get("alerts.enabled").is_none_or(Value::truthy);
        widgets::pill(
            ui,
            &t,
            icon::LIVE,
            if !enabled {
                "OFF"
            } else if paused {
                "PAUSED"
            } else {
                "RUNNING"
            },
            if !enabled {
                LedState::Error
            } else if paused {
                LedState::Armed
            } else {
                LedState::Active
            },
        );
        if paused {
            ui.label(RichText::new(format!("({})", s(&live, "pause_reason"))).color(t.fg_dim));
        }
        let health = app.m.get("health.alerts").cloned().unwrap_or_default();
        if s(&health, "status") == "warn" {
            widgets::pill(ui, &t, icon::WARN, s(&health, "detail"), LedState::Armed);
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let label = if enabled { "Turn alerts off" } else { "Turn alerts on" };
            if ui.button(label).on_hover_text("Master switch (events are still counted)").clicked() {
                app.m.command(Op::Set { address: "alerts.enabled".into(), value: Value::Bool(!enabled) });
            }
            if ui
                .button(if paused && s(&live, "pause_reason") == "manual" { format!("{} Resume", icon::PLAY) } else { format!("{} Hold", icon::STOP) })
                .clicked()
            {
                act(app, if paused && s(&live, "pause_reason") == "manual" { "alerts.resume" } else { "alerts.pause" }, Value::Null);
            }
        });
    });
    ui.add_space(6.0);

    ui.columns(2, |cols| {
        live_panel(app, &mut cols[1], &live);
        let ui = &mut cols[0];
        ui.horizontal(|ui| {
            for (tab, label) in [(Tab::Routing, "Routing"), (Tab::Queue, "Queue policy"), (Tab::Goals, "Goals"), (Tab::Stats, "Stats")] {
                ui.selectable_value(&mut form.tab, tab, label);
            }
        });
        ui.separator();
        match form.tab {
            Tab::Routing => routing(app, ui, &mut form, &cfg),
            Tab::Queue => policy(app, ui, &mut form, &cfg),
            Tab::Goals => goals(app, ui, &mut form, &cfg),
            Tab::Stats => stats(app, ui),
        }
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}

fn live_panel(app: &mut App, ui: &mut egui::Ui, live: &Value) {
    let t = app.t.clone();
    widgets::section(ui, &t, icon::SIM, "Test (simulator)");
    ui.horizontal_wrapped(|ui| {
        for (label, action, args) in TESTS {
            if ui.button(label).clicked() {
                app.m.text(&format!("{action} {args}"));
            }
        }
    });
    ui.add_space(8.0);
    let cur = live.get_path("current").cloned().unwrap_or_default();
    widgets::card(ui, &t, icon::ALERT, "On screen", if cur.is_null() { LedState::Idle } else { LedState::Active }, |ui| {
        if cur.is_null() {
            ui.label(RichText::new("nothing").color(t.fg_dim));
            return;
        }
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("#{}", i(&cur, "id"))).color(t.fg_dim));
            ui.label(RichText::new(s(&cur, "title")).strong().size(16.0));
        });
        if !s(&cur, "message").is_empty() {
            ui.label(s(&cur, "message"));
        }
        let n = cur.get_path("recipients").and_then(Value::as_list).map(|l| l.len()).unwrap_or(0);
        ui.horizontal(|ui| {
            ui.label(
                RichText::new(format!(
                    "{} · {} · {}s left{}",
                    s(&cur, "kind"),
                    s(&cur, "variation"),
                    i(&cur, "remaining_ms") / 1000,
                    if n > 0 { format!(" · {n} recipients") } else { String::new() }
                ))
                .color(t.fg_dim),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button(RichText::new(format!("{} Veto", icon::CROSS)).color(t.bright_red)).clicked() {
                    act(app, "alerts.veto", Value::map().with("id", i(&cur, "id")));
                }
                if ui.button(format!("{} Skip", icon::STOP)).clicked() {
                    act(app, "alerts.skip", Value::Null);
                }
            });
        });
    });
    ui.add_space(6.0);
    let queue: Vec<Value> = live.get_path("queue").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    ui.horizontal(|ui| {
        widgets::section(ui, &t, icon::QUEUE, &format!("Queue ({})", queue.len()));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !queue.is_empty() && widgets::hold_button(ui, &t, "Clear", t.yellow, 0.6) {
                act(app, "alerts.clear", Value::Null);
            }
        });
    });
    egui::ScrollArea::vertical().id_salt("alerts-queue").max_height(260.0).auto_shrink([false, true]).show(ui, |ui| {
        for a in &queue {
            let aid = i(a, "id");
            let veto_ms = i(a, "veto_remaining_ms");
            egui::Frame::new()
                .fill(t.bg_dark)
                .stroke(egui::Stroke::new(1.0, if veto_ms > 0 { t.yellow } else { t.bg_light }))
                .corner_radius(6)
                .inner_margin(6)
                .show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(format!("#{aid}")).color(t.fg_dim));
                        ui.label(RichText::new(s(a, "title")).strong());
                        ui.label(RichText::new(format!("p{}", i(a, "priority"))).small().color(t.fg_dim));
                        if a.get_path("sim").is_some_and(Value::truthy) {
                            ui.label(RichText::new("sim").small().color(t.fg_dim));
                        }
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            if ui.button(RichText::new(format!("{} Veto", icon::CROSS)).color(t.bright_red)).clicked() {
                                act(app, "alerts.veto", Value::map().with("id", aid));
                            }
                            if veto_ms > 0 && ui.button(RichText::new(format!("{} Approve", icon::CHECK)).color(t.green)).clicked() {
                                act(app, "alerts.approve", Value::map().with("id", aid));
                            }
                        });
                    });
                    if !s(a, "message").is_empty() {
                        ui.label(RichText::new(format!("“{}”", s(a, "message"))).italics());
                    }
                    if veto_ms > 0 {
                        let window = app.m.get(&format!("alerts.veto.{aid}")).and_then(Value::as_f64).unwrap_or(veto_ms as f64 / 1000.0);
                        ui.add(
                            egui::ProgressBar::new((veto_ms as f32 / 3000.0).clamp(0.0, 1.0))
                                .desired_height(4.0)
                                .fill(t.yellow)
                                .text(format!("veto window {window:.1}s")),
                        );
                    }
                });
        }
    });
    ui.add_space(6.0);
    widgets::section(ui, &t, icon::SESSION, "Recent");
    let history: Vec<Value> = live.get_path("history").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    egui::ScrollArea::vertical().id_salt("alerts-history").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("alerts-history-grid").striped(true).num_columns(4).spacing([10.0, 3.0]).show(ui, |ui| {
            for h in &history {
                ui.label(RichText::new(format!("#{}", i(h, "id"))).color(t.fg_dim));
                ui.label(s(h, "title"));
                ui.label(RichText::new(format!("{}s ago", i(h, "ago_ms") / 1000)).small().color(t.fg_dim));
                if ui.small_button("replay").clicked() {
                    act(app, "alerts.replay", Value::map().with("id", i(h, "id")));
                }
                ui.end_row();
            }
        });
    });
}

/// One editable look field; returns `(key, typed text)` when it changed.
fn look_row(ui: &mut egui::Ui, edits: &mut BTreeMap<String, String>, key: String, label: &str, current: String, multiline: bool) {
    ui.label(label);
    let buf = edits.entry(key).or_insert(current);
    if multiline {
        ui.add(egui::TextEdit::multiline(buf).desired_rows(1).desired_width(f32::INFINITY));
    } else {
        ui.add(egui::TextEdit::singleline(buf).desired_width(f32::INFINITY));
    }
    ui.end_row();
}

fn save_fields(app: &mut App, form: &mut Form, file: &str, path: Value, prefix: &str, originals: &[(String, String)]) {
    let mut fields = Value::map();
    let mut n = 0;
    for (k, orig) in originals {
        let key = format!("{prefix}{k}");
        let Some(text) = form.edits.get(&key) else { continue };
        if text == orig {
            continue;
        }
        match field_value(k, text) {
            Ok(v) => {
                fields = fields.with(k.clone(), v);
                n += 1;
            }
            Err(e) => {
                app.m.toast(e, true);
                return;
            }
        }
    }
    if n == 0 {
        return;
    }
    act(app, "alerts.edit", Value::map().with("file", file).with("path", path).with("fields", fields));
    form.edits.retain(|k, _| !k.starts_with(prefix));
}

fn routing(app: &mut App, ui: &mut egui::Ui, form: &mut Form, cfg: &Value) {
    let t = app.t.clone();
    let alerts: Vec<Value> = cfg.get_path("alerts").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let files: Vec<String> =
        cfg.get_path("files").and_then(Value::as_list).map(|l| l.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default();
    ui.label(RichText::new("The first enabled alert whose `when` matches (and `if` holds) handles an event; its first matching variation overrides fields. Placeholders: {user} {amount} {money} {message} {tier} {months} {count} {viewers} and payload fields. Empty = remove.").small().color(t.fg_dim));
    egui::ScrollArea::vertical().id_salt("alerts-routing").auto_shrink([false, false]).show(ui, |ui| {
        for a in &alerts {
            let file = s(a, "file").to_string();
            let idx = i(a, "index");
            let prefix = format!("{file}|{idx}|");
            let enabled = a.get_path("enabled").is_some_and(Value::truthy);
            let header = format!(
                "{} {}  ·  {}  {}",
                if enabled { icon::CHECK } else { icon::CROSS },
                s(a, "name"),
                s(a, "when"),
                if s(a, "if").is_empty() { String::new() } else { format!("if {}", s(a, "if")) }
            );
            egui::CollapsingHeader::new(RichText::new(header).strong()).id_salt(&prefix).show(ui, |ui| {
                let look = a.get_path("look").cloned().unwrap_or_default();
                let mut originals: Vec<(String, String)> = vec![("when".into(), s(a, "when").into()), ("if".into(), s(a, "if").into())];
                originals.extend(LOOK.iter().map(|(k, _)| (k.to_string(), look_text(&look, k))));
                egui::Grid::new(format!("{prefix}grid")).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                    look_row(ui, &mut form.edits, format!("{prefix}when"), "When", s(a, "when").into(), false);
                    look_row(ui, &mut form.edits, format!("{prefix}if"), "If", s(a, "if").into(), false);
                    for (k, label) in LOOK {
                        look_row(ui, &mut form.edits, format!("{prefix}{k}"), label, look_text(&look, k), k == "message");
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button(RichText::new(format!("{} Save", icon::CHECK)).strong()).clicked() {
                        save_fields(app, form, &file, Value::List(vec!["alert".into(), Value::Int(idx)]), &prefix, &originals);
                    }
                    let (flip, label) = if enabled { (false, "Disable") } else { (true, "Enable") };
                    if ui.button(label).clicked() {
                        act(
                            app,
                            "alerts.edit",
                            Value::map()
                                .with("file", file.clone())
                                .with("path", Value::List(vec!["alert".into(), Value::Int(idx)]))
                                .with("fields", Value::map().with("enabled", if flip { Value::Null } else { Value::Bool(false) })),
                        );
                    }
                    let veto = a.get_path("veto").is_some_and(Value::truthy);
                    if ui
                        .button(if veto { "Veto window: on" } else { "Veto window: off" })
                        .on_hover_text("Alerts with viewer text wait for a mod veto")
                        .clicked()
                    {
                        act(
                            app,
                            "alerts.edit",
                            Value::map()
                                .with("file", file.clone())
                                .with("path", Value::List(vec!["alert".into(), Value::Int(idx)]))
                                .with("fields", Value::map().with("veto", if veto { Value::Bool(false) } else { Value::Null })),
                        );
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::hold_button(ui, &t, "Delete", t.bright_red, 0.6) {
                            act(
                                app,
                                "alerts.remove",
                                Value::map().with("file", file.clone()).with("path", Value::List(vec![])).with("key", "alert").with("index", idx),
                            );
                        }
                    });
                });
                let vars: Vec<Value> = a.get_path("variations").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
                for (vi, v) in vars.iter().enumerate() {
                    let vprefix = format!("{prefix}v{vi}|");
                    let vlook = v.get_path("look").cloned().unwrap_or_default();
                    egui::CollapsingHeader::new(format!("variation {}  ·  if {}", s(v, "name"), s(v, "if"))).id_salt(&vprefix).show(ui, |ui| {
                        let mut originals: Vec<(String, String)> = vec![("name".into(), s(v, "name").into()), ("if".into(), s(v, "if").into())];
                        originals.extend(LOOK.iter().map(|(k, _)| (k.to_string(), look_text(&vlook, k))));
                        egui::Grid::new(format!("{vprefix}grid")).num_columns(2).spacing([8.0, 4.0]).show(ui, |ui| {
                            look_row(ui, &mut form.edits, format!("{vprefix}name"), "Name", s(v, "name").into(), false);
                            look_row(ui, &mut form.edits, format!("{vprefix}if"), "If", s(v, "if").into(), false);
                            for (k, label) in LOOK {
                                look_row(ui, &mut form.edits, format!("{vprefix}{k}"), label, look_text(&vlook, k), k == "message");
                            }
                        });
                        ui.horizontal(|ui| {
                            let path = Value::List(vec!["alert".into(), Value::Int(idx), "variation".into(), Value::Int(vi as i64)]);
                            if ui.button(RichText::new(format!("{} Save", icon::CHECK)).strong()).clicked() {
                                save_fields(app, form, &file, path, &vprefix, &originals);
                            }
                            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                                if widgets::hold_button(ui, &t, "Delete", t.bright_red, 0.6) {
                                    act(
                                        app,
                                        "alerts.remove",
                                        Value::map()
                                            .with("file", file.clone())
                                            .with("path", Value::List(vec!["alert".into(), Value::Int(idx)]))
                                            .with("key", "variation")
                                            .with("index", vi as i64),
                                    );
                                }
                            });
                        });
                    });
                }
                let nv = form.new_var.entry(prefix.clone()).or_default();
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut nv.0).hint_text("variation name").desired_width(110.0));
                    ui.add(egui::TextEdit::singleline(&mut nv.1).hint_text("if (e.g. amount >= 500)").desired_width(180.0));
                    if ui.add_enabled(!nv.1.trim().is_empty(), egui::Button::new("Add variation")).clicked() {
                        let fields = Value::map().with("name", nv.0.trim()).with("if", nv.1.trim());
                        act(
                            app,
                            "alerts.add",
                            Value::map()
                                .with("file", file.clone())
                                .with("path", Value::List(vec!["alert".into(), Value::Int(idx)]))
                                .with("key", "variation")
                                .with("fields", fields),
                        );
                        *nv = Default::default();
                    }
                });
            });
        }
        ui.add_space(8.0);
        widgets::section(ui, &t, icon::PLAY, "New alert");
        ui.horizontal(|ui| {
            if form.new_alert_file.is_empty() {
                form.new_alert_file = files.first().cloned().unwrap_or_else(|| "alerts/custom.toml".into());
            }
            egui::ComboBox::from_id_salt("alerts-new-file").selected_text(form.new_alert_file.clone()).show_ui(ui, |ui| {
                for f in &files {
                    ui.selectable_value(&mut form.new_alert_file, f.clone(), f);
                }
                ui.selectable_value(&mut form.new_alert_file, "alerts/custom.toml".to_string(), "alerts/custom.toml");
            });
            ui.add(egui::TextEdit::singleline(&mut form.new_alert_name).hint_text("name").desired_width(100.0));
            ui.add(egui::TextEdit::singleline(&mut form.new_alert_when).hint_text("when (event, e.g. twitch.redeem)").desired_width(200.0));
            if ui.add_enabled(!form.new_alert_when.trim().is_empty(), egui::Button::new("Add")).clicked() {
                let fields = Value::map().with("name", form.new_alert_name.trim()).with("when", form.new_alert_when.trim()).with("title", "{user}");
                act(
                    app,
                    "alerts.add",
                    Value::map().with("file", form.new_alert_file.clone()).with("path", Value::List(vec![])).with("key", "alert").with("fields", fields),
                );
                form.new_alert_name.clear();
                form.new_alert_when.clear();
            }
        });
    });
}

const POLICY: [(&str, &str, &str); 10] = [
    ("min_spacing", "Gap between alerts", "min_spacing_ms"),
    ("max_on_screen", "Max on screen", "max_on_screen_ms"),
    ("max_queue", "Max queued", "max_queue"),
    ("interrupt", "Big alerts interrupt (true/false)", "interrupt"),
    ("interrupt_margin", "Interrupt margin (priority)", "interrupt_margin"),
    ("on_interrupt", "Interrupted alert (requeue/drop)", "on_interrupt"),
    ("pause_modes", "Pause in modes", "pause_modes"),
    ("veto_window", "Veto window", "veto_window_ms"),
    ("gift_window", "Gift-bomb window", "gift_window_ms"),
    ("max_message", "Max message length", "max_message"),
];

fn policy(app: &mut App, ui: &mut egui::Ui, form: &mut Form, cfg: &Value) {
    let t = app.t.clone();
    let q = cfg.get_path("queue").cloned().unwrap_or_default();
    let current = |src: &str| -> String {
        match q.get_path(src) {
            Some(Value::List(l)) => l.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
            Some(Value::Int(ms)) if src.ends_with("_ms") => dur(*ms),
            Some(Value::Str(s)) => s.clone(),
            Some(v) => v.to_string(),
            None => String::new(),
        }
    };
    egui::Grid::new("alerts-policy").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
        for (k, label, src) in POLICY {
            ui.label(label);
            let buf = form.policy.entry(k.to_string()).or_insert_with(|| current(src));
            ui.add(egui::TextEdit::singleline(buf).desired_width(200.0));
            ui.end_row();
        }
    });
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        if ui.button(RichText::new(format!("{} Save policy", icon::CHECK)).strong()).clicked() {
            let mut fields = Value::map();
            let mut bad = None;
            for (k, _, src) in POLICY {
                let Some(text) = form.policy.get(k) else { continue };
                if *text == current(src) {
                    continue;
                }
                let t = text.trim();
                let v = match k {
                    "max_queue" | "interrupt_margin" | "max_message" => t.parse::<i64>().map(Value::Int).map_err(|_| format!("{k} must be a number")),
                    "interrupt" => Ok(Value::Bool(matches!(t, "true" | "yes" | "on" | "1"))),
                    "pause_modes" => Ok(Value::List(t.split(',').map(str::trim).filter(|x| !x.is_empty()).map(|x| Value::Str(x.into())).collect())),
                    _ => Ok(Value::Str(t.into())),
                };
                match v {
                    Ok(v) => fields = fields.with(k, v),
                    Err(e) => bad = Some(e),
                }
            }
            match bad {
                Some(e) => app.m.toast(e, true),
                None => {
                    act(app, "alerts.policy", Value::map().with("fields", fields));
                    form.policy.clear();
                }
            }
        }
        if ui.button("Revert").clicked() {
            form.policy.clear();
        }
    });
    ui.label(RichText::new("Durations like 1s, 500ms, 15s. Changes write alerts/queue.toml (comments kept).").small().color(t.fg_dim));
}

fn goals(app: &mut App, ui: &mut egui::Ui, form: &mut Form, cfg: &Value) {
    let t = app.t.clone();
    let goals: Vec<Value> = cfg.get_path("goals").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    for g in &goals {
        let name = s(g, "name").to_string();
        let cur = app.m.f(&format!("goals.{name}.current"));
        let target = f(g, "target");
        widgets::card(
            ui,
            &t,
            icon::SCOPE,
            &format!("{}  ({name}, counts {})", s(g, "label"), s(g, "counts")),
            if cur >= target { LedState::Healthy } else { LedState::Active },
            |ui| {
                ui.add(
                    egui::ProgressBar::new(if target > 0.0 { (cur / target) as f32 } else { 0.0 })
                        .text(format!("{} / {}", fmt_num(cur), fmt_num(target)))
                        .fill(t.accent),
                );
                ui.horizontal(|ui| {
                    let buf = form.goal_set.entry(name.clone()).or_default();
                    ui.add(egui::TextEdit::singleline(buf).hint_text("value").desired_width(70.0));
                    let v = buf.trim().parse::<f64>();
                    if ui.add_enabled(v.is_ok(), egui::Button::new("Set progress")).clicked() {
                        act(app, "goals.set", Value::map().with("name", name.clone()).with("value", v.clone().unwrap_or(0.0)));
                        form.goal_set.remove(&name);
                    } else if ui.add_enabled(v.is_ok(), egui::Button::new("Set target")).clicked() {
                        act(app, "goals.save", Value::map().with("name", name.clone()).with("fields", Value::map().with("target", v.unwrap_or(1.0))));
                        form.goal_set.remove(&name);
                    }
                    if ui.small_button("+1").clicked() {
                        act(app, "goals.add", Value::map().with("name", name.clone()).with("amount", 1.0));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if widgets::hold_button(ui, &t, "Remove goal", t.bright_red, 0.8) {
                            act(app, "goals.delete", Value::map().with("name", name.clone()));
                        }
                        if widgets::hold_button(ui, &t, "Reset", t.yellow, 0.6) {
                            act(app, "goals.reset", Value::map().with("name", name.clone()));
                        }
                    });
                });
            },
        );
        ui.add_space(4.0);
    }
    ui.add_space(6.0);
    widgets::section(ui, &t, icon::PLAY, "New goal");
    ui.horizontal(|ui| {
        let (name, counts, target) = &mut form.goal_new;
        ui.add(egui::TextEdit::singleline(name).hint_text("name (one word)").desired_width(120.0));
        if counts.is_empty() {
            *counts = "subs".into();
        }
        egui::ComboBox::from_id_salt("goal-counts").selected_text(counts.clone()).show_ui(ui, |ui| {
            for k in ["follows", "subs", "sub_points", "bits", "tips", "gifts", "raids"] {
                ui.selectable_value(counts, k.to_string(), k);
            }
        });
        ui.add(egui::TextEdit::singleline(target).hint_text("target").desired_width(80.0));
        let tv = target.trim().parse::<f64>();
        if ui.add_enabled(!name.trim().is_empty() && tv.as_ref().is_ok_and(|x| *x > 0.0), egui::Button::new("Add")).clicked() {
            let fields = Value::map().with("counts", counts.clone()).with("target", tv.unwrap_or(1.0)).with("label", name.trim().replace('_', " "));
            act(app, "goals.save", Value::map().with("name", name.trim()).with("fields", fields));
            form.goal_new = Default::default();
        }
    });
    ui.label(RichText::new("Goals live in project.toml [goals.*]; progress is stored in the runtime DB and survives restarts.").small().color(t.fg_dim));
}

fn stats(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let st = |a: &str| {
        app.m.get(a).map(|v| match v {
            Value::Str(s) if s.is_empty() => "—".to_string(),
            Value::Str(s) => s.clone(),
            Value::Float(x) => fmt_num(*x),
            other => other.to_string(),
        })
    };
    widgets::section(ui, &t, icon::SESSION, "This stream");
    egui::Grid::new("stats-session").num_columns(6).spacing([14.0, 4.0]).show(ui, |ui| {
        for k in ["follows", "subs", "gifts", "bits", "tips", "raids"] {
            ui.label(RichText::new(k).small().color(t.fg_dim));
        }
        ui.end_row();
        for k in ["follows", "subs", "gifts", "bits", "tips", "raids"] {
            ui.label(RichText::new(st(&format!("stats.session.{k}")).unwrap_or_else(|| "0".into())).strong().size(18.0));
        }
        ui.end_row();
    });
    ui.add_space(6.0);
    widgets::section(ui, &t, icon::ALERT, "Labels");
    egui::Grid::new("stats-labels").striped(true).num_columns(3).spacing([12.0, 4.0]).show(ui, |ui| {
        let rows = [
            ("Latest follower", "stats.latest.follow.user", ""),
            ("Latest sub", "stats.latest.sub.user", ""),
            ("Latest gifter", "stats.latest.gift.user", "stats.latest.gift.count"),
            ("Latest cheer", "stats.latest.cheer.user", "stats.latest.cheer.amount"),
            ("Latest tip", "stats.latest.tip.user", "stats.latest.tip.amount"),
            ("Latest raid", "stats.latest.raid.user", "stats.latest.raid.viewers"),
            ("Top cheer (stream)", "stats.top.cheer.session.user", "stats.top.cheer.session.amount"),
            ("Top cheer (all time)", "stats.top.cheer.alltime.user", "stats.top.cheer.alltime.amount"),
            ("Top tip (stream)", "stats.top.tip.session.user", "stats.top.tip.session.amount"),
            ("Top tip (all time)", "stats.top.tip.alltime.user", "stats.top.tip.alltime.amount"),
            ("Top gifter (stream)", "stats.top.gift.session.user", "stats.top.gift.session.amount"),
            ("Top gifter (all time)", "stats.top.gift.alltime.user", "stats.top.gift.alltime.amount"),
        ];
        for (label, user, amount) in rows {
            ui.label(RichText::new(label).color(t.fg_dim));
            ui.label(RichText::new(st(user).unwrap_or_else(|| "—".into())).strong());
            ui.label(if amount.is_empty() { String::new() } else { st(amount).unwrap_or_default() });
            ui.end_row();
        }
    });
    ui.add_space(8.0);
    ui.horizontal(|ui| {
        if widgets::hold_button(ui, &t, "Purge simulated data", t.yellow, 1.0) {
            act(app, "stats.purge_simulated", Value::Null);
        }
        ui.label(RichText::new("Removes everything the simulator added to stats, goals, and top chatters.").small().color(t.fg_dim));
    });
}
