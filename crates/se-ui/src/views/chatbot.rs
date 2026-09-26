//! Chatbot (§14.3): commands, timers, counters, quotes, and a chat tester.
//! Data: queries `bot.commands`, `bot.timers`, `bot.counters`, `bot.quotes`, `bot.files`;
//! commands: `bot.command.save|delete`, `bot.timer.save|delete`, `bot.counter.set|delete`,
//! `bot.quote.add|edit|delete`, `sim.chat`. Files are written comment-preserving.

use crate::app::App;
use egui::RichText;
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Commands,
    Timers,
    Counters,
    Quotes,
    Test,
}

const ROLES: [&str; 6] = ["everyone", "follower", "sub", "vip", "mod", "owner"];

/// Editable copy of a command or timer (strings as typed).
#[derive(Clone, Default, PartialEq)]
struct Draft {
    /// `(file, index)` of the entry being edited; `None` = new.
    at: Option<(String, i64)>,
    file: String,
    name: String,
    aliases: String,
    reply: String,
    role: String,
    cd_global: String,
    cd_user: String,
    modes: String,
    enabled: bool,
    usage: String,
    min_args: String,
    description: String,
    cmds: String,
    action: String,
    args: String,
    // timers
    every: String,
    min_lines: String,
}

#[derive(Clone, Default)]
struct Form {
    tab: Tab,
    filter: String,
    cmd: Option<Draft>,
    timer: Option<Draft>,
    counter_name: String,
    counter_value: String,
    quote_text: String,
    quote_edit: Option<(i64, String)>,
    test_msg: String,
    test_role: String,
    last_query: f64,
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

fn list(v: &Value, k: &str) -> Vec<String> {
    v.get_path(k)
        .and_then(Value::as_list)
        .map(|l| l.iter().map(|x| x.as_str().map(String::from).unwrap_or_else(|| x.to_string())).collect())
        .unwrap_or_default()
}

fn dur(ms: i64) -> String {
    if ms <= 0 {
        String::new()
    } else if ms % 60_000 == 0 {
        format!("{}m", ms / 60_000)
    } else if ms % 1000 == 0 {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

fn csv(s: &str) -> Value {
    let v: Vec<Value> = s.split(',').map(str::trim).filter(|x| !x.is_empty()).map(|x| Value::Str(x.into())).collect();
    if v.is_empty() { Value::Null } else { Value::List(v) }
}

fn opt(s: &str) -> Value {
    if s.trim().is_empty() { Value::Null } else { Value::Str(s.trim().into()) }
}

fn lines(s: &str) -> Value {
    let v: Vec<Value> = s.lines().map(str::trim).filter(|x| !x.is_empty()).map(|x| Value::Str(x.into())).collect();
    if v.is_empty() { Value::Null } else { Value::List(v) }
}

impl Draft {
    fn from_command(c: &Value) -> Draft {
        let args = c.get_path("args").and_then(Value::as_map).filter(|m| !m.is_empty()).map(|m| {
            let body: Vec<String> = m.iter().map(|(k, v)| format!("{k} = {}", toml_literal(v))).collect();
            format!("{{ {} }}", body.join(", "))
        });
        Draft {
            at: Some((s(c, "file").into(), i(c, "index"))),
            file: s(c, "file").into(),
            name: s(c, "name").into(),
            aliases: list(c, "aliases").join(", "),
            reply: s(c, "reply").into(),
            role: s(c, "role").into(),
            cd_global: dur(i(c, "cooldown_global_ms")),
            cd_user: dur(i(c, "cooldown_per_user_ms")),
            modes: list(c, "modes").join(", "),
            enabled: c.get_path("enabled").is_none_or(Value::truthy),
            usage: s(c, "usage").into(),
            min_args: match i(c, "min_args") {
                0 => String::new(),
                n => n.to_string(),
            },
            description: s(c, "description").into(),
            cmds: list(c, "do").join("\n"),
            action: s(c, "action").into(),
            args: args.unwrap_or_default(),
            ..Default::default()
        }
    }

    fn from_timer(t: &Value) -> Draft {
        Draft {
            at: Some((s(t, "file").into(), i(t, "index"))),
            file: s(t, "file").into(),
            name: s(t, "name").into(),
            reply: s(t, "reply").into(),
            modes: list(t, "modes").join(", "),
            enabled: t.get_path("enabled").is_none_or(Value::truthy),
            cmds: list(t, "do").join("\n"),
            every: dur(i(t, "every_ms")),
            min_lines: i(t, "min_chat_lines").to_string(),
            ..Default::default()
        }
    }

    fn command_fields(&self) -> Result<Value, String> {
        let mut cd = Value::map();
        if !self.cd_global.trim().is_empty() {
            cd = cd.with("global", self.cd_global.trim());
        }
        if !self.cd_user.trim().is_empty() {
            cd = cd.with("per_user", self.cd_user.trim());
        }
        let args = if self.args.trim().is_empty() {
            Value::Null
        } else {
            match Value::parse_text(self.args.trim()) {
                v @ Value::Map(_) => v,
                _ => return Err("args must be an inline table: { key = \"{user}\" }".into()),
            }
        };
        let min_args = match self.min_args.trim() {
            "" | "0" => Value::Null,
            n => Value::Int(n.parse::<i64>().map_err(|_| "min args must be a number")?),
        };
        Ok(Value::map()
            .with("name", self.name.trim())
            .with("aliases", csv(&self.aliases))
            .with("reply", opt(&self.reply))
            .with("role", if self.role.is_empty() || self.role == "everyone" { Value::Null } else { Value::Str(self.role.clone()) })
            .with("cooldown", if cd.as_map().is_some_and(|m| m.is_empty()) { Value::Null } else { cd })
            .with("modes", csv(&self.modes))
            .with("enabled", if self.enabled { Value::Null } else { Value::Bool(false) })
            .with("usage", opt(&self.usage))
            .with("min_args", min_args)
            .with("description", opt(&self.description))
            .with("do", lines(&self.cmds))
            .with("action", opt(&self.action))
            .with("args", args))
    }

    fn timer_fields(&self) -> Result<Value, String> {
        let n: i64 = if self.min_lines.trim().is_empty() { 0 } else { self.min_lines.trim().parse().map_err(|_| "min chat lines must be a number")? };
        Ok(Value::map()
            .with("name", opt(&self.name))
            .with("every", self.every.trim())
            .with("min_chat_lines", n)
            .with("modes", csv(&self.modes))
            .with("reply", opt(&self.reply))
            .with("do", lines(&self.cmds))
            .with("enabled", if self.enabled { Value::Null } else { Value::Bool(false) }))
    }
}

fn toml_literal(v: &Value) -> String {
    match v {
        Value::Str(s) => format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\"")),
        other => other.to_string(),
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("chatbot-form");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    if form.test_role.is_empty() {
        form.test_role = "everyone".into();
    }
    let now = ui.input(|i| i.time);
    if now - form.last_query > 1.0 {
        form.last_query = now;
        for q in ["bot.commands", "bot.timers", "bot.counters", "bot.quotes", "bot.files"] {
            app.m.query(q, Value::Null);
        }
    }
    let files_v = app.m.q("bot.files").cloned().unwrap_or_default();
    let files = list(&files_v, "files");
    let custom = s(&files_v, "custom_file").to_string();
    let errors: Vec<Value> = files_v.get_path("errors").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();

    ui.horizontal(|ui| {
        widgets::section(ui, &t, icon::BOT, "Chatbot");
        let health = app.m.get("health.bot").cloned().unwrap_or_default();
        let led = match s(&health, "status") {
            "pass" => LedState::Healthy,
            "fail" => LedState::Error,
            "" => LedState::Idle,
            _ => LedState::Armed,
        };
        widgets::pill(ui, &t, icon::CHECK, s(&health, "detail"), led);
        ui.separator();
        for (tab, label) in [(Tab::Commands, "Commands"), (Tab::Timers, "Timers"), (Tab::Counters, "Counters"), (Tab::Quotes, "Quotes"), (Tab::Test, "Test")] {
            ui.selectable_value(&mut form.tab, tab, label);
        }
    });
    for e in &errors {
        ui.label(RichText::new(format!("{} {}: {}", icon::WARN, s(e, "file"), s(e, "msg"))).color(t.bright_red));
    }
    ui.add_space(6.0);
    match form.tab {
        Tab::Commands => commands(app, ui, &mut form, &files, &custom),
        Tab::Timers => timers(app, ui, &mut form, &files, &custom),
        Tab::Counters => counters(app, ui, &mut form),
        Tab::Quotes => quotes(app, ui, &mut form),
        Tab::Test => test(app, ui, &mut form),
    }
    ui.data_mut(|d| d.insert_temp(id, form));
}

fn file_picker(ui: &mut egui::Ui, salt: &str, file: &mut String, files: &[String], custom: &str) {
    if file.is_empty() {
        *file = custom.to_string();
    }
    egui::ComboBox::from_id_salt(salt).selected_text(file.clone()).show_ui(ui, |ui| {
        for f in files {
            ui.selectable_value(file, f.clone(), f);
        }
    });
}

fn commands(app: &mut App, ui: &mut egui::Ui, form: &mut Form, files: &[String], custom: &str) {
    let t = app.t.clone();
    let cmds: Vec<Value> = app.m.q_list("bot.commands").to_vec();
    ui.columns(2, |cols| {
        let ui = &mut cols[0];
        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut form.filter).hint_text(format!("{} filter", icon::SEARCH)).desired_width(200.0));
            if ui.button(format!("{} New command", icon::PLAY)).clicked() {
                form.cmd = Some(Draft { enabled: true, role: "everyone".into(), file: custom.to_string(), name: "!".into(), ..Default::default() });
            }
        });
        let q = form.filter.to_lowercase();
        egui::ScrollArea::vertical().id_salt("bot-cmds").auto_shrink([false, false]).show(ui, |ui| {
            egui::Grid::new("bot-cmd-grid").striped(true).num_columns(4).spacing([10.0, 4.0]).show(ui, |ui| {
                for h in ["command", "does", "who", "file"] {
                    ui.label(RichText::new(h).small().color(t.fg_dim));
                }
                ui.end_row();
                for c in &cmds {
                    let name = s(c, "name");
                    if !q.is_empty() && !name.contains(&q) && !s(c, "reply").to_lowercase().contains(&q) {
                        continue;
                    }
                    let selected = form.cmd.as_ref().and_then(|d| d.at.clone()) == Some((s(c, "file").into(), i(c, "index")));
                    let enabled = c.get_path("enabled").is_none_or(Value::truthy);
                    let label = RichText::new(name).strong().color(if enabled { t.fg } else { t.fg_dim });
                    if ui.selectable_label(selected, label).clicked() {
                        form.cmd = Some(Draft::from_command(c));
                    }
                    let does = match s(c, "behavior") {
                        "reply" => s(c, "reply").chars().take(48).collect::<String>(),
                        b => b.to_string(),
                    };
                    ui.label(RichText::new(does).color(t.fg_dim));
                    ui.label(s(c, "role"));
                    ui.label(RichText::new(s(c, "file").trim_start_matches("commands/")).small().color(t.fg_dim));
                    ui.end_row();
                }
            });
        });

        let ui = &mut cols[1];
        let Some(d) = form.cmd.as_mut() else {
            ui.label(RichText::new("Select a command to edit, or create one. Placeholders: {user} {touser} {args} {1}…{9} {count} {uptime} {song} {random:a|b|c} and any state address like {goals.subs.current}.").color(t.fg_dim));
            return;
        };
        let title = if d.at.is_some() { format!("Edit {}", d.name) } else { "New command".to_string() };
        let mut save = false;
        let mut delete = false;
        let mut close = false;
        widgets::card(ui, &t, icon::BOT, &title, if d.enabled { LedState::Active } else { LedState::Idle }, |ui| {
            egui::Grid::new("bot-cmd-edit").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                ui.label("File");
                if d.at.is_some() {
                    ui.label(RichText::new(&d.file).monospace());
                } else {
                    file_picker(ui, "bot-cmd-file", &mut d.file, files, custom);
                }
                ui.end_row();
                ui.label("Name");
                ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(200.0));
                ui.end_row();
                ui.label("Aliases");
                ui.add(egui::TextEdit::singleline(&mut d.aliases).hint_text("!dc, !disc").desired_width(260.0));
                ui.end_row();
                ui.label("Reply");
                ui.add(egui::TextEdit::multiline(&mut d.reply).desired_rows(3).desired_width(f32::INFINITY));
                ui.end_row();
                ui.label("Who");
                egui::ComboBox::from_id_salt("bot-cmd-role").selected_text(format!("{}+", if d.role.is_empty() { "everyone" } else { &d.role })).show_ui(ui, |ui| {
                    for r in ROLES {
                        ui.selectable_value(&mut d.role, r.to_string(), format!("{r}+"));
                    }
                });
                ui.end_row();
                ui.label("Cooldown");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut d.cd_global).hint_text("global e.g. 30s").desired_width(110.0));
                    ui.add(egui::TextEdit::singleline(&mut d.cd_user).hint_text("per user e.g. 2m").desired_width(110.0));
                });
                ui.end_row();
                ui.label("Modes");
                ui.add(egui::TextEdit::singleline(&mut d.modes).hint_text("any (or: live, rehearsal)").desired_width(260.0));
                ui.end_row();
                ui.label("Enabled");
                ui.checkbox(&mut d.enabled, "");
                ui.end_row();
                ui.label("Needs args");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut d.min_args).hint_text("0").desired_width(40.0));
                    ui.add(egui::TextEdit::singleline(&mut d.usage).hint_text("usage reply when missing").desired_width(240.0));
                });
                ui.end_row();
                ui.label("Runs");
                ui.add(egui::TextEdit::multiline(&mut d.cmds).hint_text("one command per line, e.g. preset.fire hype").desired_rows(2).desired_width(f32::INFINITY));
                ui.end_row();
                ui.label("Action");
                ui.horizontal(|ui| {
                    ui.add(egui::TextEdit::singleline(&mut d.action).hint_text("e.g. queue.request").desired_width(140.0));
                    ui.add(egui::TextEdit::singleline(&mut d.args).hint_text("{ text = \"{args}\" }").desired_width(220.0));
                });
                ui.end_row();
                ui.label("Notes");
                ui.add(egui::TextEdit::singleline(&mut d.description).desired_width(f32::INFINITY));
                ui.end_row();
            });
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                save = ui.button(RichText::new(format!("{} Save", icon::CHECK)).strong()).clicked();
                close = ui.button("Close").clicked();
                if d.at.is_some() {
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        delete = widgets::hold_button(ui, &t, "Delete", t.bright_red, 0.6);
                    });
                }
            });
        });
        if save {
            match d.command_fields() {
                Ok(fields) => {
                    let mut args = Value::map().with("fields", fields).with("file", d.file.clone());
                    if let Some((_, idx)) = &d.at {
                        args = args.with("index", *idx);
                    }
                    let is_new = d.at.is_none();
                    act(app, "bot.command.save", args);
                    if is_new {
                        form.cmd = None;
                    }
                }
                Err(e) => app.m.toast(e, true),
            }
        } else if delete {
            if let Some((file, idx)) = d.at.clone() {
                act(app, "bot.command.delete", Value::map().with("file", file).with("index", idx));
            }
            form.cmd = None;
        } else if close {
            form.cmd = None;
        }
    });
}

fn timers(app: &mut App, ui: &mut egui::Ui, form: &mut Form, files: &[String], custom: &str) {
    let t = app.t.clone();
    let timers: Vec<Value> = app.m.q_list("bot.timers").to_vec();
    ui.columns(2, |cols| {
        let ui = &mut cols[0];
        if ui.button(format!("{} New timer", icon::PLAY)).clicked() {
            form.timer = Some(Draft {
                enabled: true,
                every: "20m".into(),
                min_lines: "10".into(),
                modes: "live".into(),
                file: custom.to_string(),
                ..Default::default()
            });
        }
        let mode = app.m.str("show.mode").to_string();
        egui::Grid::new("bot-timers").striped(true).num_columns(5).spacing([10.0, 4.0]).show(ui, |ui| {
            for h in ["timer", "every", "chat lines", "next", "message"] {
                ui.label(RichText::new(h).small().color(t.fg_dim));
            }
            ui.end_row();
            for tm in &timers {
                let selected = form.timer.as_ref().and_then(|d| d.at.clone()) == Some((s(tm, "file").into(), i(tm, "index")));
                let modes = list(tm, "modes");
                let in_mode = modes.is_empty() || modes.contains(&mode);
                ui.horizontal(|ui| {
                    widgets::led(
                        ui,
                        &t,
                        if !tm.get_path("enabled").is_none_or(Value::truthy) {
                            LedState::Idle
                        } else if in_mode {
                            LedState::Active
                        } else {
                            LedState::Armed
                        },
                    );
                    if ui.selectable_label(selected, s(tm, "name")).clicked() {
                        form.timer = Some(Draft::from_timer(tm));
                    }
                });
                ui.label(dur(i(tm, "every_ms")));
                ui.label(format!("{} / {}", i(tm, "lines"), i(tm, "min_chat_lines")));
                ui.label(match tm.get_path("next_in_ms").and_then(Value::as_i64) {
                    Some(ms) => format!("{}:{:02}", ms / 60_000, ms / 1000 % 60),
                    None => format!("waits for {}", list(tm, "modes").join("/")),
                });
                ui.label(RichText::new(s(tm, "reply").chars().take(50).collect::<String>()).color(t.fg_dim));
                ui.end_row();
            }
        });

        let ui = &mut cols[1];
        let Some(d) = form.timer.as_mut() else {
            ui.label(
                RichText::new("Timers post a message every interval, once enough chat lines have passed, only in their modes (default: live).").color(t.fg_dim),
            );
            return;
        };
        let (mut save, mut delete, mut close) = (false, false, false);
        widgets::card(
            ui,
            &t,
            icon::TIMELINE,
            if d.at.is_some() { "Edit timer" } else { "New timer" },
            if d.enabled { LedState::Active } else { LedState::Idle },
            |ui| {
                egui::Grid::new("bot-timer-edit").num_columns(2).spacing([10.0, 6.0]).show(ui, |ui| {
                    ui.label("File");
                    if d.at.is_some() {
                        ui.label(RichText::new(&d.file).monospace());
                    } else {
                        file_picker(ui, "bot-timer-file", &mut d.file, files, custom);
                    }
                    ui.end_row();
                    ui.label("Name");
                    ui.add(egui::TextEdit::singleline(&mut d.name).desired_width(200.0));
                    ui.end_row();
                    ui.label("Every");
                    ui.add(egui::TextEdit::singleline(&mut d.every).hint_text("20m (min 1m)").desired_width(100.0));
                    ui.end_row();
                    ui.label("Min chat lines");
                    ui.add(egui::TextEdit::singleline(&mut d.min_lines).desired_width(60.0));
                    ui.end_row();
                    ui.label("Modes");
                    ui.add(egui::TextEdit::singleline(&mut d.modes).desired_width(200.0));
                    ui.end_row();
                    ui.label("Message");
                    ui.add(egui::TextEdit::multiline(&mut d.reply).desired_rows(3).desired_width(f32::INFINITY));
                    ui.end_row();
                    ui.label("Runs");
                    ui.add(egui::TextEdit::multiline(&mut d.cmds).hint_text("optional commands, one per line").desired_rows(2).desired_width(f32::INFINITY));
                    ui.end_row();
                    ui.label("Enabled");
                    ui.checkbox(&mut d.enabled, "");
                    ui.end_row();
                });
                ui.horizontal(|ui| {
                    save = ui.button(RichText::new(format!("{} Save", icon::CHECK)).strong()).clicked();
                    close = ui.button("Close").clicked();
                    if d.at.is_some() {
                        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                            delete = widgets::hold_button(ui, &t, "Delete", t.bright_red, 0.6);
                        });
                    }
                });
            },
        );
        if save {
            match d.timer_fields() {
                Ok(fields) => {
                    let mut args = Value::map().with("fields", fields).with("file", d.file.clone());
                    if let Some((_, idx)) = &d.at {
                        args = args.with("index", *idx);
                    }
                    let is_new = d.at.is_none();
                    act(app, "bot.timer.save", args);
                    if is_new {
                        form.timer = None;
                    }
                }
                Err(e) => app.m.toast(e, true),
            }
        } else if delete {
            if let Some((file, idx)) = d.at.clone() {
                act(app, "bot.timer.delete", Value::map().with("file", file).with("index", idx));
            }
            form.timer = None;
        } else if close {
            form.timer = None;
        }
    });
}

fn counters(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    let counters: Vec<Value> = app.m.q_list("bot.counters").to_vec();
    ui.label(
        RichText::new("Counters back {count} in replies (per command) and `!setcounter`; each is also state `bot.counter.<name>` for rules and overlays.")
            .color(t.fg_dim),
    );
    ui.add_space(4.0);
    egui::Grid::new("bot-counters").striped(true).num_columns(3).spacing([12.0, 4.0]).show(ui, |ui| {
        for c in &counters {
            let name = s(c, "name").to_string();
            let v = i(c, "value");
            ui.label(RichText::new(&name).strong());
            ui.label(RichText::new(v.to_string()).monospace().size(16.0));
            ui.horizontal(|ui| {
                if ui.small_button("−1").clicked() {
                    act(app, "bot.counter.set", Value::map().with("name", name.clone()).with("value", v - 1));
                }
                if ui.small_button("+1").clicked() {
                    act(app, "bot.counter.set", Value::map().with("name", name.clone()).with("value", v + 1));
                }
                if ui.small_button("reset").clicked() {
                    act(app, "bot.counter.set", Value::map().with("name", name.clone()).with("value", 0));
                }
                if widgets::hold_button(ui, &t, "Delete", t.bright_red, 0.5) {
                    act(app, "bot.counter.delete", Value::map().with("name", name.clone()));
                }
            });
            ui.end_row();
        }
    });
    ui.add_space(6.0);
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut form.counter_name).hint_text("counter name").desired_width(160.0));
        ui.add(egui::TextEdit::singleline(&mut form.counter_value).hint_text("value").desired_width(70.0));
        let v = form.counter_value.trim().parse::<i64>();
        if ui.add_enabled(!form.counter_name.trim().is_empty() && v.is_ok(), egui::Button::new("Set")).clicked() {
            act(app, "bot.counter.set", Value::map().with("name", form.counter_name.trim()).with("value", v.unwrap_or(0)));
            form.counter_name.clear();
            form.counter_value.clear();
        }
    });
}

fn quotes(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    let quotes: Vec<Value> = app.m.q_list("bot.quotes").to_vec();
    ui.horizontal(|ui| {
        ui.add(egui::TextEdit::singleline(&mut form.quote_text).hint_text("new quote").desired_width(ui.available_width() - 80.0));
        if ui.add_enabled(!form.quote_text.trim().is_empty(), egui::Button::new(format!("{} Add", icon::PLAY))).clicked() {
            act(app, "bot.quote.add", Value::map().with("text", form.quote_text.trim()).with("by", "ui"));
            form.quote_text.clear();
        }
    });
    ui.add_space(4.0);
    egui::ScrollArea::vertical().id_salt("bot-quotes").auto_shrink([false, false]).show(ui, |ui| {
        egui::Grid::new("bot-quotes-grid").striped(true).num_columns(4).spacing([10.0, 4.0]).show(ui, |ui| {
            for q in quotes.iter().rev() {
                let qid = i(q, "id");
                ui.label(RichText::new(format!("#{qid}")).color(t.fg_dim));
                match &mut form.quote_edit {
                    Some((eid, text)) if *eid == qid => {
                        ui.add(egui::TextEdit::singleline(text).desired_width(420.0));
                    }
                    _ => {
                        ui.label(s(q, "text"));
                    }
                }
                ui.label(RichText::new(format!("{} · {}", s(q, "added_by"), crate_date(i(q, "created_at")))).small().color(t.fg_dim));
                ui.horizontal(|ui| {
                    let editing = form.quote_edit.as_ref().is_some_and(|(e, _)| *e == qid);
                    if editing {
                        if ui.small_button(icon::CHECK).clicked()
                            && let Some((_, text)) = form.quote_edit.take()
                        {
                            act(app, "bot.quote.edit", Value::map().with("id", qid).with("text", text));
                        }
                    } else if ui.small_button("edit").clicked() {
                        form.quote_edit = Some((qid, s(q, "text").to_string()));
                    }
                    if widgets::hold_button(ui, &t, "Delete", t.bright_red, 0.5) {
                        act(app, "bot.quote.delete", Value::map().with("id", qid));
                    }
                });
                ui.end_row();
            }
        });
    });
}

fn crate_date(unix: i64) -> String {
    let days = unix.div_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    format!("{:04}-{m:02}-{d:02}", yoe + era * 400 + i64::from(m <= 2))
}

fn test(app: &mut App, ui: &mut egui::Ui, form: &mut Form) {
    let t = app.t.clone();
    ui.label(
        RichText::new("Sends a simulated chat line through the real pipeline (policy, cooldowns, replies). Replies go to Twitch chat when connected.")
            .color(t.fg_dim),
    );
    ui.horizontal(|ui| {
        egui::ComboBox::from_id_salt("bot-test-role").selected_text(form.test_role.clone()).show_ui(ui, |ui| {
            for r in ROLES {
                ui.selectable_value(&mut form.test_role, r.to_string(), r);
            }
        });
        let r = ui.add(egui::TextEdit::singleline(&mut form.test_msg).hint_text("!discord").desired_width(ui.available_width() - 80.0));
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        if (ui.button(format!("{} Send", icon::SIM)).clicked() || enter) && !form.test_msg.trim().is_empty() {
            act(app, "sim.chat", Value::map().with("message", form.test_msg.trim()).with("role", form.test_role.clone()).with("user", "tester"));
            form.test_msg.clear();
        }
    });
    ui.add_space(6.0);
    widgets::section(ui, &t, icon::CHAT, "Chat and bot replies");
    egui::ScrollArea::vertical().id_salt("bot-test-log").stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        for e in app.m.events.iter().filter(|e| e.ty == "twitch.chat" || e.ty == "bot.said") {
            ui.horizontal_wrapped(|ui| {
                if e.ty == "bot.said" {
                    ui.label(RichText::new(format!("{} bot", icon::BOT)).strong().color(t.green));
                    ui.label(s(&e.payload, "text"));
                    ui.label(RichText::new(s(&e.payload, "source")).small().color(t.fg_dim));
                } else {
                    let who = e.actor.as_ref().map(|a| a.name.as_str()).unwrap_or("?");
                    ui.label(RichText::new(who).strong().color(t.accent));
                    ui.label(s(&e.payload, "message"));
                }
            });
        }
    });
}
