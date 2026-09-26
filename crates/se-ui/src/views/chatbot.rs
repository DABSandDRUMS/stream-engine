//! Chat commands (§14.3): what viewers can type in chat (!discord → a reply), timed messages,
//! counters, quotes, and a place to try a command as a viewer would. Everything is plain words;
//! file names, extra commands and actions sit under "Details".
//! Data: queries `bot.commands`, `bot.timers`, `bot.counters`, `bot.quotes`, `bot.files`;
//! commands: `bot.command.save|delete`, `bot.timer.save|delete`, `bot.counter.set|delete`,
//! `bot.quote.add|edit|delete`, `sim.chat`. Files are written comment-preserving.

use crate::app::App;
use crate::views::live::nice;
use crate::views::rules::{Words, capitalize, friendly_text};
use egui::{Align, Layout, RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_mono, font_semibold, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, LedState, Size, icon};

const TABS: [&str; 5] = ["Commands", "Timed messages", "Counters", "Quotes", "Try it"];

/// (role, who can use it).
const ROLES: [(&str, &str); 6] = [
    ("everyone", "Everyone"),
    ("follower", "Followers and up"),
    ("sub", "Subscribers and up"),
    ("vip", "VIPs and up"),
    ("mod", "Moderators"),
    ("owner", "Only you"),
];

/// Placeholders a reply can use: (text, what it becomes).
const PLACEHOLDERS: [(&str, &str); 7] = [
    ("{user}", "their name"),
    ("{touser}", "who they mention"),
    ("{args}", "what they typed after"),
    ("{count}", "a counter"),
    ("{uptime}", "time on air"),
    ("{song}", "current song"),
    ("{random:a|b|c}", "a random pick"),
];

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
    /// `bot.commands` behavior (`reply`, `builtin <name>`, `action <name>`, `do`).
    behavior: String,
    // timers
    every: String,
    min_lines: String,
}

#[derive(Clone, Default)]
struct Form {
    tab: usize,
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

/// "20 min", "45 s" for sentences.
fn dur_words(ms: i64) -> String {
    if ms >= 60_000 && ms % 60_000 == 0 { format!("{} min", ms / 60_000) } else { format!("{} s", ms / 1000) }
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

fn clip(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= n { s } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

fn role_words(role: &str) -> &'static str {
    ROLES.iter().find(|(r, _)| *r == role).map_or("Everyone", |(_, w)| *w)
}

/// Mode names: `brb` → `BRB`, `ad_break` → `Ad break`.
fn pretty(m: &str) -> String {
    if !m.is_empty() && m.len() <= 3 && m.chars().all(|c| c.is_ascii_alphabetic()) { m.to_ascii_uppercase() } else { nice(m) }
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
            behavior: s(c, "behavior").into(),
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
                _ => return Err("The action's settings (Details) need to look like { text = \"{args}\" }.".into()),
            }
        };
        let min_args = match self.min_args.trim() {
            "" | "0" => Value::Null,
            n => Value::Int(n.parse::<i64>().map_err(|_| "“Words needed after it” must be a number.")?),
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
        let n: i64 =
            if self.min_lines.trim().is_empty() { 0 } else { self.min_lines.trim().parse().map_err(|_| "The number of chat messages must be a number.")? };
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

/// What a command does, in words.
fn does(c: &Value, words: &Words) -> String {
    let desc = s(c, "description");
    let behavior = s(c, "behavior");
    let (kind, rest) = behavior.split_once(' ').unwrap_or((behavior, ""));
    match kind {
        "reply" => clip(&friendly_text(s(c, "reply")), 70),
        _ if !desc.is_empty() => clip(&friendly_text(desc), 70),
        "builtin" => match rest {
            "addcom" => "Adds a new command from chat".into(),
            "editcom" => "Changes a command from chat".into(),
            "delcom" => "Removes a command from chat".into(),
            "commands" => "Lists the commands".into(),
            "quote" => "Shows a saved quote".into(),
            "addquote" => "Saves a quote".into(),
            "delquote" => "Removes a quote".into(),
            "setcounter" => "Sets a counter".into(),
            other => format!("Built in: {}", nice(other)),
        },
        "action" => match rest {
            "alerts.veto" => "Cancels the alert on screen".into(),
            "alerts.skip" => "Skips the current alert".into(),
            "tts.skip" => "Skips the voice message".into(),
            "tts.clear" => "Clears waiting voice messages".into(),
            "queue.request" => "Requests a song".into(),
            "queue.skip" => "Skips the song".into(),
            _ => "Does something in Stream Engine".into(),
        },
        "do" => words.command(&list(c, "do").join("; ")),
        _ => String::new(),
    }
}

fn chip(ui: &mut egui::Ui, t: &Theme, label: &str, on: bool) -> egui::Response {
    widgets::chip(ui, t, "", label, on)
}

/// Center `body` in a column at most `max_w` wide (wide screens).
fn centered(ui: &mut egui::Ui, max_w: f32, body: impl FnOnce(&mut egui::Ui)) {
    let avail = ui.available_width();
    let h = ui.available_height();
    let w = avail.min(max_w);
    ui.horizontal_top(|ui| {
        ui.add_space(((avail - w) / 2.0 - ui.spacing().item_spacing.x).max(0.0));
        ui.allocate_ui_with_layout(Vec2::new(w, h), Layout::top_down(Align::Min), |ui| {
            ui.set_width(w);
            body(ui);
        });
    });
}

fn toggle_csv(csv: &mut String, item: &str) {
    let mut items: Vec<String> = csv.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect();
    match items.iter().position(|x| x == item) {
        Some(i) => {
            items.remove(i);
        }
        None => items.push(item.to_string()),
    }
    *csv = items.join(", ");
}

/// Mode chips over a comma-separated list (none picked = any mode).
fn mode_chips(ui: &mut egui::Ui, t: &Theme, csv: &mut String, modes: &[String]) {
    let chosen: Vec<String> = csv.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect();
    let mut flip = None;
    ui.horizontal_wrapped(|ui| {
        for m in modes.iter().chain(chosen.iter().filter(|c| !modes.contains(c))) {
            if chip(ui, t, &pretty(m), chosen.contains(m)).clicked() {
                flip = Some(m.clone());
            }
        }
    });
    if let Some(m) = flip {
        toggle_csv(csv, &m);
    }
}

fn label(ui: &mut egui::Ui, t: &Theme, text: &str) {
    ui.label(RichText::new(text).color(t.text_dim));
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

    centered(ui, 2400.0, |ui| tab_body(app, ui, &t, &mut form, &files, &custom, &errors));
    ui.data_mut(|d| d.insert_temp(id, form));
}

#[allow(clippy::too_many_arguments)]
fn tab_body(app: &mut App, ui: &mut egui::Ui, t: &Theme, form: &mut Form, files: &[String], custom: &str, errors: &[Value]) {
    let t = t.clone();
    ui.horizontal(|ui| {
        widgets::segmented(ui, &t, &mut form.tab, &TABS);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let health = app.m.get("health.bot").cloned().unwrap_or_default();
            let twitch = crate::views::twitch::twitch_state(app);
            let (led, text) = match s(&health, "status") {
                // the bot only answers once it can reach Twitch chat
                "pass" if !twitch.connected() => (LedState::Armed, "Chat bot: Not connected"),
                "pass" => (LedState::Healthy, "Chat bot: Working"),
                "fail" => (LedState::Error, "Chat bot: Not connected"),
                "" => (LedState::Idle, "Chat bot: Off"),
                _ => (LedState::Armed, "Chat bot: Needs a look"),
            };
            let tip =
                if twitch.connected() { s(&health, "detail").to_string() } else { format!("Connect Twitch so the bot can answer in chat. {}", twitch.body) };
            widgets::pill(ui, &t, if led == LedState::Healthy { icon::CHECK } else { icon::WARN }, text, led).on_hover_text(tip);
        });
    });
    for e in errors {
        ui.add_space(spacing::S);
        widgets::callout(
            ui,
            &t,
            widgets::Tone::Warn,
            icon::WARN,
            "Some commands didn't load",
            &format!("A command file has a mistake, so the bot skips it until it's fixed: {}", s(e, "msg")),
            None,
        );
    }
    ui.add_space(spacing::L);
    match form.tab {
        0 => commands(app, ui, &t, form, files, custom),
        1 => timers(app, ui, &t, form, files, custom),
        2 => counters(app, ui, &t, form),
        3 => quotes(app, ui, &t, form),
        _ => test(app, ui, &t, form),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    List,
    Editor,
}

/// List on the left, editor on the right (stacked when narrow). `body` draws one side per call.
fn columns(ui: &mut egui::Ui, mut body: impl FnMut(&mut egui::Ui, Side)) {
    let avail = ui.available_size();
    if avail.x < 820.0 {
        egui::ScrollArea::vertical().id_salt("bot-narrow").auto_shrink([false, false]).show(ui, |ui| {
            body(ui, Side::List);
            ui.add_space(spacing::L);
            body(ui, Side::Editor);
        });
        return;
    }
    let total = avail.x.min(2400.0);
    let list_w = (total * 0.36).clamp(380.0, 760.0);
    let ed_w = total - list_w - spacing::L;
    ui.horizontal_top(|ui| {
        ui.add_space(((avail.x - total) / 2.0 - ui.spacing().item_spacing.x).max(0.0));
        ui.allocate_ui_with_layout(Vec2::new(list_w, avail.y), Layout::top_down(Align::Min), |ui| body(ui, Side::List));
        ui.add_space(spacing::L - ui.spacing().item_spacing.x);
        ui.allocate_ui_with_layout(Vec2::new(ed_w, avail.y), Layout::top_down(Align::Min), |ui| {
            egui::ScrollArea::vertical().id_salt("bot-editor").auto_shrink([false, false]).show(ui, |ui| body(ui, Side::Editor));
        });
    });
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

// ---- commands ------------------------------------------------------------------------------------

fn commands(app: &mut App, ui: &mut egui::Ui, t: &Theme, form: &mut Form, files: &[String], custom: &str) {
    let cmds: Vec<Value> = app.m.q_list("bot.commands").to_vec();
    let modes: Vec<String> = app.m.q_list("modes").iter().filter_map(|v| v.as_str().map(String::from)).collect();
    let words = Words::new(app);
    let height = ui.available_height();
    let mut new = false;
    let (mut save, mut delete, mut close, mut close_x) = (false, false, false, false);
    columns(ui, |ui, side| match side {
        Side::List => {
            widgets::titled(
                ui,
                t,
                "Your commands",
                &format!("{} commands viewers can type", cmds.len()),
                |ui| new = widgets::button_ex(ui, t, Some(icon::PLUS), "New command", Kind::Primary, Size::Medium, 0.0, true).clicked(),
                |ui| {
                    ui.set_width(ui.available_width());
                    ui.add(
                        se_ui_kit::widgets::field(&mut form.filter)
                            .hint_text(format!("{}  Find a command", icon::SEARCH))
                            .margin(egui::Margin::symmetric(10, 7))
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_space(spacing::S);
                    if cmds.is_empty() {
                        widgets::empty_state(
                            ui,
                            t,
                            icon::CHAT,
                            "No commands yet",
                            "A command is something viewers type, like !discord, and the bot answers.",
                            None,
                        );
                        return;
                    }
                    let q = form.filter.to_lowercase();
                    egui::ScrollArea::vertical().id_salt("bot-cmds").auto_shrink([false, true]).max_height((height - 140.0).max(120.0)).show(ui, |ui| {
                        // your own replies first, then the built-in ones
                        let mut last = None;
                        for c in cmds.iter().filter(|c| s(c, "behavior") == "reply").chain(cmds.iter().filter(|c| s(c, "behavior") != "reply")) {
                            let reply = s(c, "behavior") == "reply";
                            if last != Some(reply) {
                                if last.is_some() {
                                    ui.add_space(spacing::S);
                                }
                                let n = cmds.iter().filter(|c| (s(c, "behavior") == "reply") == reply).count();
                                widgets::section(ui, t, "", &if reply { format!("Answers ({n})") } else { format!("Built in ({n})") });
                                last = Some(reply);
                            }
                            let name = s(c, "name");
                            if !q.is_empty() && !name.to_lowercase().contains(&q) && !s(c, "reply").to_lowercase().contains(&q) {
                                continue;
                            }
                            let selected = form.cmd.as_ref().and_then(|d| d.at.clone()) == Some((s(c, "file").into(), i(c, "index")));
                            let enabled = c.get_path("enabled").is_none_or(Value::truthy);
                            let trailing = match (enabled, s(c, "role")) {
                                (false, _) => "Off".to_string(),
                                (true, "" | "everyone") => String::new(),
                                (true, r) => role_words(r).to_string(),
                            };
                            let glyph = if reply { icon::CHAT } else { icon::BOLT };
                            let sub = does(c, &words);
                            let sub = if trailing.is_empty() { sub } else { clip(&sub, 52) };
                            if widgets::list_row(ui, t, glyph, name, &sub, &trailing, selected).clicked() {
                                form.cmd = Some(Draft::from_command(c));
                            }
                        }
                    });
                },
            );
        }
        Side::Editor => {
            let Some(d) = form.cmd.as_mut() else {
                widgets::panel(ui, t, |ui| {
                    ui.set_width(ui.available_width());
                    new |= widgets::empty_state(
                        ui,
                        t,
                        icon::CHAT,
                        "Pick a command to change it",
                        "Or make a new one, like !discord that answers with your invite link.",
                        None,
                    );
                });
                return;
            };
            let title = if d.at.is_some() { d.name.clone() } else { "New command".to_string() };
            let can_save = !d.name.trim().is_empty();
            let mut save_top = false;
            widgets::titled(
                ui,
                t,
                &title,
                "What viewers type, and what the bot does.",
                |ui| {
                    close_x = widgets::icon_button(ui, t, icon::CROSS, "Close").clicked();
                    save_top = widgets::button_ex(ui, t, Some(icon::CHECK), "Save", Kind::Primary, Size::Medium, 0.0, can_save).clicked();
                },
                |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        label(ui, t, "Command");
                        ui.add(se_ui_kit::widgets::field(&mut d.name).font(font_semibold(type_scale::LARGE)).hint_text("!discord").desired_width(220.0));
                        ui.add_space(spacing::M);
                        widgets::toggle(ui, t, &mut d.enabled).on_hover_text("Off: the bot ignores it");
                        label(ui, t, if d.enabled { "On" } else { "Off" });
                    });
                    ui.horizontal(|ui| {
                        label(ui, t, "Also works as");
                        ui.add(se_ui_kit::widgets::field(&mut d.aliases).hint_text("!dc, !disc").desired_width(ui.available_width() - 8.0));
                    });
                    ui.add_space(spacing::M);
                    let builtin = d.behavior.starts_with("builtin") || d.behavior.starts_with("action");
                    if builtin && d.reply.is_empty() {
                        widgets::hint(ui, t, "This command is built in. You can still change who can use it and how often.");
                    } else {
                        widgets::section(ui, t, "", "The bot answers");
                        ui.add(
                            egui::TextEdit::multiline(&mut d.reply)
                                .hint_text("Join the Discord: https://discord.gg/…")
                                .desired_rows(3)
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal_wrapped(|ui| {
                            label(ui, t, "Insert");
                            for (ph, what) in PLACEHOLDERS {
                                if widgets::chip(ui, t, "", &capitalize(what), false)
                                    .on_hover_text(format!("Becomes {what} when it's used. Types {ph}."))
                                    .clicked()
                                {
                                    d.reply.push_str(ph);
                                }
                            }
                        });
                    }
                    ui.add_space(spacing::M);
                    widgets::section(ui, t, "", "Who and how often");
                    ui.horizontal(|ui| {
                        label(ui, t, "Who can use it");
                        egui::ComboBox::from_id_salt("bot-cmd-role").width(200.0).selected_text(role_words(&d.role)).show_ui(ui, |ui| {
                            for (r, w) in ROLES {
                                ui.selectable_value(&mut d.role, r.to_string(), w);
                            }
                        });
                    });
                    ui.horizontal(|ui| {
                        label(ui, t, "Wait between uses");
                        ui.add(se_ui_kit::widgets::field(&mut d.cd_global).hint_text("30s").desired_width(70.0));
                        ui.add_space(spacing::M);
                        label(ui, t, "Same viewer waits");
                        ui.add(se_ui_kit::widgets::field(&mut d.cd_user).hint_text("2m").desired_width(70.0));
                    });
                    widgets::hint(ui, t, "Times like 30s or 2m. Leave empty for no wait.");
                    ui.add_space(spacing::S);
                    label(ui, t, "Only during these show modes (none picked: always)");
                    mode_chips(ui, t, &mut d.modes, &modes);
                    ui.add_space(spacing::M);
                    widgets::section(ui, t, "", "If they leave something out");
                    ui.horizontal(|ui| {
                        label(ui, t, "Words needed after it");
                        ui.add(se_ui_kit::widgets::field(&mut d.min_args).hint_text("0").desired_width(40.0));
                        label(ui, t, "then answer");
                        ui.add(se_ui_kit::widgets::field(&mut d.usage).hint_text("Try: !so @name").desired_width(ui.available_width() - 8.0));
                    });
                    ui.add_space(spacing::M);
                    ui.horizontal(|ui| {
                        label(ui, t, "Notes (just for you)");
                        ui.add(se_ui_kit::widgets::field(&mut d.description).desired_width(ui.available_width() - 8.0));
                    });
                    ui.add_space(spacing::S);
                    widgets::details(ui, t, "bot-cmd-details", "Details", |ui| {
                        ui.horizontal(|ui| {
                            label(ui, t, "Saved in");
                            if d.at.is_some() {
                                ui.label(RichText::new(&d.file).font(font_mono(type_scale::SMALL + 0.5)).color(t.fg));
                            } else {
                                file_picker(ui, "bot-cmd-file", &mut d.file, files, custom);
                            }
                        });
                        label(ui, t, "Also runs (one command per line)");
                        ui.add(
                            egui::TextEdit::multiline(&mut d.cmds)
                                .hint_text("preset.fire hype")
                                .font(font_mono(type_scale::BODY - 1.0))
                                .desired_rows(2)
                                .desired_width(f32::INFINITY),
                        );
                        ui.horizontal(|ui| {
                            label(ui, t, "Action");
                            ui.add(
                                se_ui_kit::widgets::field(&mut d.action)
                                    .hint_text("queue.request")
                                    .font(font_mono(type_scale::BODY - 1.0))
                                    .desired_width(160.0),
                            );
                            ui.add(
                                se_ui_kit::widgets::field(&mut d.args)
                                    .hint_text("{ text = \"{args}\" }")
                                    .font(font_mono(type_scale::BODY - 1.0))
                                    .desired_width(ui.available_width() - 8.0),
                            );
                        });
                    });
                    ui.add_space(spacing::M);
                    if d.at.is_some() {
                        ui.horizontal(|ui| {
                            delete = widgets::hold_button(ui, t, "Delete command", t.bright_red, 0.6);
                            widgets::hint(ui, t, "Press and hold to delete.");
                        });
                    }
                },
            );
            save |= save_top;
        }
    });
    close |= close_x;
    if new {
        form.cmd =
            Some(Draft { enabled: true, role: "everyone".into(), file: custom.to_string(), name: "!".into(), behavior: "reply".into(), ..Default::default() });
    }
    let Some(d) = form.cmd.clone() else { return };
    if save {
        match d.command_fields() {
            Ok(fields) => {
                let mut args = Value::map().with("fields", fields).with("file", d.file.clone());
                if let Some((_, idx)) = &d.at {
                    args = args.with("index", *idx);
                }
                act(app, "bot.command.save", args);
                app.m.toast(format!("Saved {}.", d.name.trim()), false);
                if d.at.is_none() {
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
}

// ---- timed messages ------------------------------------------------------------------------------

fn timers(app: &mut App, ui: &mut egui::Ui, t: &Theme, form: &mut Form, files: &[String], custom: &str) {
    let timers: Vec<Value> = app.m.q_list("bot.timers").to_vec();
    let modes: Vec<String> = app.m.q_list("modes").iter().filter_map(|v| v.as_str().map(String::from)).collect();
    let mode = app.m.str("show.mode").to_string();
    let mut new = false;
    let (mut save, mut delete, mut close, mut close_x) = (false, false, false, false);
    columns(ui, |ui, side| match side {
        Side::List => {
            widgets::titled(
                ui,
                t,
                "Timed messages",
                "The bot posts these every so often while chat is active.",
                |ui| new = widgets::button_ex(ui, t, Some(icon::PLUS), "New timed message", Kind::Primary, Size::Medium, 0.0, true).clicked(),
                |ui| {
                    ui.set_width(ui.available_width());
                    if timers.is_empty() {
                        widgets::empty_state(ui, t, icon::CLOCK, "No timed messages yet", "Like a reminder to follow, posted every 20 minutes.", None);
                        return;
                    }
                    for tm in &timers {
                        let selected = form.timer.as_ref().and_then(|d| d.at.clone()) == Some((s(tm, "file").into(), i(tm, "index")));
                        let enabled = tm.get_path("enabled").is_none_or(Value::truthy);
                        let tm_modes = list(tm, "modes");
                        let in_mode = tm_modes.is_empty() || tm_modes.contains(&mode);
                        let title = if s(tm, "name").is_empty() { clip(s(tm, "reply"), 40) } else { nice(s(tm, "name")) };
                        let sub = format!("Every {} · after {} chat messages", dur_words(i(tm, "every_ms")), i(tm, "min_chat_lines"));
                        let trailing = match (enabled, in_mode, tm.get_path("next_in_ms").and_then(Value::as_i64)) {
                            (false, ..) => "Off".to_string(),
                            (true, false, _) | (true, _, None) => format!("waits for {}", tm_modes.iter().map(|m| pretty(m)).collect::<Vec<_>>().join(" / ")),
                            (true, true, Some(ms)) => format!("next in {}:{:02}", ms / 60_000, ms / 1000 % 60),
                        };
                        if widgets::list_row(ui, t, icon::CLOCK, &title, &sub, &trailing, selected).clicked() {
                            form.timer = Some(Draft::from_timer(tm));
                        }
                    }
                },
            );
        }
        Side::Editor => {
            let Some(d) = form.timer.as_mut() else {
                widgets::panel(ui, t, |ui| {
                    ui.set_width(ui.available_width());
                    widgets::empty_state(ui, t, icon::CLOCK, "Pick a timed message to change it", "Or make a new one with New timed message.", None);
                });
                return;
            };
            let title = if d.at.is_some() { "Timed message" } else { "New timed message" };
            let can_save = !d.every.trim().is_empty();
            let mut save_top = false;
            widgets::titled(
                ui,
                t,
                title,
                "Posted in chat every so often.",
                |ui| {
                    close_x = widgets::icon_button(ui, t, icon::CROSS, "Close").clicked();
                    save_top = widgets::button_ex(ui, t, Some(icon::CHECK), "Save", Kind::Primary, Size::Medium, 0.0, can_save).clicked();
                },
                |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        label(ui, t, "Name");
                        ui.add(se_ui_kit::widgets::field(&mut d.name).hint_text("follow reminder").desired_width(240.0));
                        ui.add_space(spacing::M);
                        widgets::toggle(ui, t, &mut d.enabled);
                        label(ui, t, if d.enabled { "On" } else { "Off" });
                    });
                    ui.add_space(spacing::S);
                    widgets::section(ui, t, "", "Message");
                    ui.add(egui::TextEdit::multiline(&mut d.reply).hint_text("Enjoying the stream? Hit follow!").desired_rows(3).desired_width(f32::INFINITY));
                    ui.add_space(spacing::M);
                    widgets::section(ui, t, "", "When");
                    ui.horizontal(|ui| {
                        label(ui, t, "Every");
                        ui.add(se_ui_kit::widgets::field(&mut d.every).hint_text("20m").desired_width(70.0));
                        label(ui, t, "once at least");
                        ui.add(se_ui_kit::widgets::field(&mut d.min_lines).hint_text("10").desired_width(50.0));
                        label(ui, t, "chat messages have passed");
                    });
                    widgets::hint(ui, t, "At least 1 minute apart. Chat messages keep it from posting into an empty chat.");
                    ui.add_space(spacing::S);
                    label(ui, t, "Only during these show modes (none picked: always)");
                    mode_chips(ui, t, &mut d.modes, &modes);
                    ui.add_space(spacing::S);
                    widgets::details(ui, t, "bot-timer-details", "Details", |ui| {
                        ui.horizontal(|ui| {
                            label(ui, t, "Saved in");
                            if d.at.is_some() {
                                ui.label(RichText::new(&d.file).font(font_mono(type_scale::SMALL + 0.5)).color(t.fg));
                            } else {
                                file_picker(ui, "bot-timer-file", &mut d.file, files, custom);
                            }
                        });
                        label(ui, t, "Also runs (one command per line)");
                        ui.add(egui::TextEdit::multiline(&mut d.cmds).font(font_mono(type_scale::BODY - 1.0)).desired_rows(2).desired_width(f32::INFINITY));
                    });
                    ui.add_space(spacing::M);
                    if d.at.is_some() {
                        ui.horizontal(|ui| {
                            delete = widgets::hold_button(ui, t, "Delete timed message", t.bright_red, 0.6);
                            widgets::hint(ui, t, "Press and hold to delete.");
                        });
                    }
                },
            );
            save |= save_top;
        }
    });
    close |= close_x;
    if new {
        form.timer =
            Some(Draft { enabled: true, every: "20m".into(), min_lines: "10".into(), modes: "live".into(), file: custom.to_string(), ..Default::default() });
    }
    let Some(d) = form.timer.clone() else { return };
    if save {
        match d.timer_fields() {
            Ok(fields) => {
                let mut args = Value::map().with("fields", fields).with("file", d.file.clone());
                if let Some((_, idx)) = &d.at {
                    args = args.with("index", *idx);
                }
                act(app, "bot.timer.save", args);
                app.m.toast("Timed message saved.", false);
                if d.at.is_none() {
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
}

// ---- counters ------------------------------------------------------------------------------------

fn counters(app: &mut App, ui: &mut egui::Ui, t: &Theme, form: &mut Form) {
    let counters: Vec<Value> = app.m.q_list("bot.counters").to_vec();
    egui::ScrollArea::vertical().id_salt("bot-counters").auto_shrink([false, false]).show(ui, |ui| centered(ui, 1100.0, |ui| {
        widgets::titled(ui, t, "Counters", "Numbers your commands can show, like drumsticks thrown.", |_| {}, |ui| {
            ui.set_width(ui.available_width());
            if counters.is_empty() {
                widgets::empty_state(ui, t, icon::PLUS, "No counters yet", "Add one below, then show it in a command's answer with Insert → A counter.", None);
            }
            for (n, c) in counters.iter().enumerate() {
                let name = s(c, "name").to_string();
                let v = i(c, "value");
                egui::Frame::new()
                    .fill(t.surface_hi)
                    .corner_radius(egui::CornerRadius::same(se_ui_kit::theme::radius::CONTROL))
                    .inner_margin(egui::Margin::symmetric(14, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.add_sized([220.0, 28.0], egui::Label::new(RichText::new(nice(&name)).font(font_semibold(type_scale::BODY + 1.0)).color(t.fg)).truncate());
                            ui.add_sized([90.0, 28.0], egui::Label::new(RichText::new(v.to_string()).font(font_mono(type_scale::HEADING)).color(t.fg)));
                            if widgets::button(ui, t, "−1", Kind::Secondary).clicked() {
                                act(app, "bot.counter.set", Value::map().with("name", name.clone()).with("value", v - 1));
                            }
                            if widgets::button(ui, t, "+1", Kind::Secondary).clicked() {
                                act(app, "bot.counter.set", Value::map().with("name", name.clone()).with("value", v + 1));
                            }
                            if widgets::button(ui, t, "Reset", Kind::Ghost).clicked() {
                                act(app, "bot.counter.set", Value::map().with("name", name.clone()).with("value", 0));
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.push_id(("counter", n), |ui| {
                                    if widgets::hold_button(ui, t, "Delete", t.bright_red, 0.5) {
                                        act(app, "bot.counter.delete", Value::map().with("name", name.clone()));
                                    }
                                });
                            });
                        });
                    });
                ui.add_space(spacing::XS);
            }
            ui.add_space(spacing::M);
            widgets::section(ui, t, "", "Add a counter");
            ui.horizontal(|ui| {
                ui.add(se_ui_kit::widgets::field(&mut form.counter_name).hint_text("Name, like drumsticks").margin(egui::Margin::symmetric(10, 7)).desired_width(220.0));
                ui.add(se_ui_kit::widgets::field(&mut form.counter_value).hint_text("Starts at 0").margin(egui::Margin::symmetric(10, 7)).desired_width(100.0));
                let v = if form.counter_value.trim().is_empty() { Ok(0) } else { form.counter_value.trim().parse::<i64>() };
                if widgets::button_ex(ui, t, Some(icon::PLUS), "Add counter", Kind::Primary, Size::Medium, 0.0, !form.counter_name.trim().is_empty() && v.is_ok()).clicked() {
                    act(app, "bot.counter.set", Value::map().with("name", form.counter_name.trim()).with("value", v.unwrap_or(0)));
                    form.counter_name.clear();
                    form.counter_value.clear();
                }
            });
            ui.add_space(spacing::S);
            widgets::details(ui, t, "bot-counter-details", "Details", |ui| {
                widgets::hint(ui, t, "Mods can set a counter from chat with !setcounter. Each counter is also the setting bot.counter.<name>, so reactions and overlays can use it.");
            });
        });
    }));
}

// ---- quotes --------------------------------------------------------------------------------------

fn quotes(app: &mut App, ui: &mut egui::Ui, t: &Theme, form: &mut Form) {
    let quotes: Vec<Value> = app.m.q_list("bot.quotes").to_vec();
    egui::ScrollArea::vertical().id_salt("bot-quotes").auto_shrink([false, false]).show(ui, |ui| {
        centered(ui, 1200.0, |ui| {
            widgets::titled(
                ui,
                t,
                "Quotes",
                "Funny moments to bring back. Viewers show one with !quote.",
                |_| {},
                |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.add(
                            se_ui_kit::widgets::field(&mut form.quote_text)
                                .hint_text("Something worth remembering…")
                                .margin(egui::Margin::symmetric(10, 7))
                                .desired_width(ui.available_width() - 150.0),
                        );
                        if widgets::button_ex(ui, t, Some(icon::PLUS), "Add quote", Kind::Primary, Size::Medium, 0.0, !form.quote_text.trim().is_empty())
                            .clicked()
                        {
                            act(app, "bot.quote.add", Value::map().with("text", form.quote_text.trim()).with("by", "ui"));
                            form.quote_text.clear();
                        }
                    });
                    ui.add_space(spacing::M);
                    if quotes.is_empty() {
                        widgets::empty_state(ui, t, icon::CHAT, "No quotes yet", "Add one above, or let viewers save them with !addquote.", None);
                        return;
                    }
                    for q in quotes.iter().rev() {
                        let qid = i(q, "id");
                        egui::Frame::new()
                            .fill(t.surface_hi)
                            .corner_radius(egui::CornerRadius::same(se_ui_kit::theme::radius::CONTROL))
                            .inner_margin(egui::Margin::symmetric(14, 10))
                            .show(ui, |ui| {
                                ui.set_width(ui.available_width());
                                ui.horizontal(|ui| {
                                    ui.label(RichText::new(format!("#{qid}")).font(font_mono(type_scale::SMALL + 0.5)).color(t.text_faint));
                                    let editing = form.quote_edit.as_ref().is_some_and(|(e, _)| *e == qid);
                                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                        ui.push_id(("quote", qid), |ui| {
                                            if widgets::hold_button(ui, t, "Delete", t.bright_red, 0.5) {
                                                act(app, "bot.quote.delete", Value::map().with("id", qid));
                                            }
                                        });
                                        if editing {
                                            if widgets::button_ex(ui, t, Some(icon::CHECK), "Save", Kind::Primary, Size::Small, 0.0, true).clicked()
                                                && let Some((_, text)) = form.quote_edit.take()
                                            {
                                                act(app, "bot.quote.edit", Value::map().with("id", qid).with("text", text));
                                            }
                                        } else if widgets::button_ex(ui, t, Some(icon::EDIT), "Edit", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                                            form.quote_edit = Some((qid, s(q, "text").to_string()));
                                        }
                                        ui.with_layout(Layout::left_to_right(Align::Center), |ui| match &mut form.quote_edit {
                                            Some((eid, text)) if *eid == qid => {
                                                ui.add(se_ui_kit::widgets::field(text).desired_width(ui.available_width()));
                                            }
                                            _ => {
                                                ui.add(egui::Label::new(RichText::new(format!("“{}”", s(q, "text"))).color(t.fg)).wrap());
                                            }
                                        });
                                    });
                                });
                                let by = match s(q, "added_by") {
                                    "" | "ui" => "you".to_string(),
                                    who => who.to_string(),
                                };
                                ui.label(
                                    RichText::new(format!("Added by {by} on {}", crate_date(i(q, "created_at")))).size(type_scale::SMALL).color(t.text_dim),
                                );
                            });
                        ui.add_space(spacing::XS);
                    }
                },
            );
        })
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

// ---- try it --------------------------------------------------------------------------------------

fn test(app: &mut App, ui: &mut egui::Ui, t: &Theme, form: &mut Form) {
    let height = ui.available_height();
    widgets::titled(
        ui,
        t,
        "Try a command",
        "Type like a viewer would. The bot answers in your Twitch chat when it's connected.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                label(ui, t, "Pretend to be");
                egui::ComboBox::from_id_salt("bot-test-role").width(180.0).selected_text(role_words(&form.test_role)).show_ui(ui, |ui| {
                    for (r, w) in ROLES {
                        ui.selectable_value(&mut form.test_role, r.to_string(), w);
                    }
                });
                let r = ui.add(
                    se_ui_kit::widgets::field(&mut form.test_msg)
                        .hint_text("!discord")
                        .margin(egui::Margin::symmetric(10, 7))
                        .desired_width(ui.available_width() - 110.0),
                );
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let send = widgets::button_ex(ui, t, Some(icon::PLAY), "Send", Kind::Primary, Size::Medium, 0.0, !form.test_msg.trim().is_empty()).clicked();
                if (send || enter) && !form.test_msg.trim().is_empty() {
                    act(app, "sim.chat", Value::map().with("message", form.test_msg.trim()).with("role", form.test_role.clone()).with("user", "tester"));
                    form.test_msg.clear();
                }
            });
            ui.add_space(spacing::M);
            widgets::section(ui, t, "", "Chat and bot answers");
            egui::Frame::new()
                .fill(t.bg)
                .stroke(egui::Stroke::new(1.0, t.border))
                .corner_radius(egui::CornerRadius::same(se_ui_kit::theme::radius::CONTROL))
                .inner_margin(egui::Margin::same(12))
                .show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    egui::ScrollArea::vertical()
                        .id_salt("bot-test-log")
                        .stick_to_bottom(true)
                        .auto_shrink([false, false])
                        .max_height((height - 220.0).max(160.0))
                        .show(ui, |ui| {
                            let mut any = false;
                            for e in app.m.events.iter().filter(|e| e.ty == "twitch.chat" || e.ty == "bot.said") {
                                any = true;
                                ui.horizontal_wrapped(|ui| {
                                    if e.ty == "bot.said" {
                                        ui.label(RichText::new(format!("{}  Bot", icon::BOT)).font(font_semibold(type_scale::BODY)).color(t.green));
                                        ui.label(RichText::new(s(&e.payload, "text")).color(t.fg));
                                    } else {
                                        let who = e.actor.as_ref().map(|a| a.name.as_str()).unwrap_or("someone");
                                        ui.label(RichText::new(who).font(font_semibold(type_scale::BODY)).color(t.accent));
                                        ui.label(RichText::new(s(&e.payload, "message")).color(t.fg));
                                    }
                                });
                            }
                            if !any {
                                widgets::hint(ui, t, "Nothing yet. Send a command above and the answer shows up here.");
                            }
                        });
                });
        },
    );
}
