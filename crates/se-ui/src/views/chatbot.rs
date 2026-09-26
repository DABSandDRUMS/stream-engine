//! Chat bot views (§14.3), both in the list/detail grammar of `docs/ui-model.md`.
//! [`ui`] (Automation → Chat commands): what viewers can type in chat (!discord → a reply and/or
//! steps to run), a list of commands and one command's inspector, with a place to try it.
//! [`extras_ui`] (Community → Chat bot): timed messages, counters, quotes and the bot settings.
//! Data: queries `bot.commands`, `bot.timers`, `bot.counters`, `bot.quotes`, `bot.files`, and
//! `project.read project.toml` for `[bot]`; commands: `bot.command.save|delete`,
//! `bot.timer.save|delete`, `bot.counter.set|delete`, `bot.quote.add|edit|delete`, `sim.chat`,
//! `project.write` (settings). Files are written comment-preserving.

use crate::app::App;
use crate::views::live::nice;
use crate::views::rules::{Words, capitalize, friendly_text, steps_editor};
use egui::RichText;
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_mono, font_semibold, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, LedState, Size, icon};

const EXTRA_TABS: [&str; 4] = ["Timed messages", "Counters", "Quotes", "Settings"];

/// Query key of the `project.toml` read behind the Settings tab.
const PROJECT_KEY: &str = "chatbot.project";

/// Width of the list pane.
const LIST_W: f32 = 300.0;

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
    /// Command lines run after the reply (`do`).
    cmds: Vec<String>,
    /// Engine action of a built-in command and its settings (TOML text); shown, never edited,
    /// but sent back on save so they survive.
    action: String,
    args: String,
    /// `bot.commands` behavior (`reply`, `builtin <name>`, `action <name>`, `do`).
    behavior: String,
    /// What the command does, in words.
    does: String,
    // timers
    every: String,
    min_lines: String,
}

/// State of the Chat commands page.
#[derive(Clone, Default)]
struct Form {
    filter: String,
    cmd: Option<Draft>,
    /// A command just created: select it once it shows up in the list.
    select_name: Option<String>,
    test_msg: String,
    test_role: String,
    last_query: f64,
}

/// State of the Chat bot page (timed messages, counters, quotes, settings).
#[derive(Clone, Default)]
struct Extras {
    tab: usize,
    timer: Option<Draft>,
    /// Selected counter (by name).
    counter: Option<String>,
    /// New counter being set up: (name, starting value).
    counter_new: Option<(String, String)>,
    /// Quote being edited: (id, text); id `None` = new.
    quote: Option<(Option<i64>, String)>,
    quote_filter: String,
    /// `[bot]` as last read, and the edited copy.
    set_base: Option<BotSettings>,
    set_draft: Option<BotSettings>,
    set_seq: u64,
    /// Ignore reads until this time (a save is on its way).
    set_hold: f64,
    set_error: bool,
    last_query: f64,
    last_read: f64,
}

/// What a list pane reports: the "+" or a row.
enum Pick {
    New,
    At(usize),
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

/// Command lines as a `do` list (`Null` when there are none).
fn steps(cmds: &[String]) -> Value {
    let v: Vec<Value> = cmds.iter().map(|x| x.trim()).filter(|x| !x.is_empty()).map(|x| Value::Str(x.into())).collect();
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

fn mode_list(app: &App) -> Vec<String> {
    app.m.q_list("modes").iter().filter_map(|v| v.as_str().map(String::from)).collect()
}

impl Draft {
    fn new_command(custom: &str) -> Draft {
        Draft { enabled: true, role: "everyone".into(), file: custom.to_string(), name: "!".into(), behavior: "reply".into(), ..Default::default() }
    }

    /// Timers post only while live unless modes say otherwise, so modes start empty.
    fn new_timer(custom: &str) -> Draft {
        Draft { enabled: true, every: "20m".into(), min_lines: "10".into(), file: custom.to_string(), ..Default::default() }
    }

    fn from_command(c: &Value, words: &Words) -> Draft {
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
            cmds: list(c, "do"),
            action: s(c, "action").into(),
            args: args.unwrap_or_default(),
            behavior: s(c, "behavior").into(),
            does: does(c, words),
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
            cmds: list(t, "do"),
            every: dur(i(t, "every_ms")),
            min_lines: i(t, "min_chat_lines").to_string(),
            ..Default::default()
        }
    }

    /// Built-in and action commands: the bot already knows what to do.
    fn builtin(&self) -> bool {
        self.behavior.starts_with("builtin") || self.behavior.starts_with("action")
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
                _ => return Err("This command's built-in settings can't be read, so it wasn't saved.".into()),
            }
        };
        let min_args = match self.min_args.trim() {
            "" | "0" => Value::Null,
            n => Value::Int(n.parse::<i64>().map_err(|_| "“Words needed” must be a number.")?),
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
            .with("do", steps(&self.cmds))
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
            .with("do", steps(&self.cmds))
            .with("enabled", if self.enabled { Value::Null } else { Value::Bool(false) }))
    }

    /// Where it is saved: a file picker while new, the file name after.
    fn saved_in(&mut self, ui: &mut egui::Ui, t: &Theme, salt: &str, files: &BotFiles) {
        widgets::prop_row(ui, t, "Saved in", |ui| {
            if self.at.is_some() {
                ui.label(RichText::new(&self.file).font(font_mono(type_scale::SMALL + 0.5)).color(t.fg));
            } else {
                file_picker(ui, salt, &mut self.file, &files.files, &files.custom);
            }
        });
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

/// Mode chips over a comma-separated list.
fn mode_chips(ui: &mut egui::Ui, t: &Theme, csv: &mut String, modes: &[String]) {
    let chosen: Vec<String> = csv.split(',').map(str::trim).filter(|x| !x.is_empty()).map(String::from).collect();
    let mut flip = None;
    ui.horizontal_wrapped(|ui| {
        for m in modes.iter().chain(chosen.iter().filter(|c| !modes.contains(c))) {
            if widgets::chip(ui, t, "", &pretty(m), chosen.contains(m)).clicked() {
                flip = Some(m.clone());
            }
        }
    });
    if let Some(m) = flip {
        toggle_csv(csv, &m);
    }
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

/// A hint lined up with the controls of the rows above it.
fn row_hint(ui: &mut egui::Ui, t: &Theme, text: &str) {
    widgets::prop_row(ui, t, "", |ui| {
        widgets::hint(ui, t, text);
    });
}

/// The hold-to-delete button that ends a detail pane.
fn delete_button(ui: &mut egui::Ui, t: &Theme, label: &str) -> bool {
    ui.add_space(spacing::L);
    ui.horizontal(|ui| {
        let done = widgets::hold_button(ui, t, label, t.bright_red, 0.6);
        widgets::hint(ui, t, "Press and hold to delete.");
        done
    })
    .inner
}

/// Save (or Create while new) and Close, for a detail header (right to left).
fn save_close(ui: &mut egui::Ui, t: &Theme, new: bool, can_save: bool, save: &mut bool, close: &mut bool) {
    *save = widgets::button_ex(ui, t, Some(icon::CHECK), if new { "Create" } else { "Save" }, Kind::Primary, Size::Medium, 0.0, can_save).clicked();
    *close = widgets::icon_button(ui, t, icon::CROSS, "Close").clicked();
}

fn poll(app: &mut App, now: f64, last: &mut f64) {
    if now - *last > 1.0 {
        *last = now;
        for q in ["bot.commands", "bot.timers", "bot.counters", "bot.quotes", "bot.files"] {
            app.m.query(q, Value::Null);
        }
    }
}

/// Command files (`bot.files`): where new entries can go, and files that failed to load.
struct BotFiles {
    files: Vec<String>,
    custom: String,
    errors: Vec<String>,
}

impl BotFiles {
    fn read(app: &App) -> BotFiles {
        let v = app.m.q("bot.files").cloned().unwrap_or_default();
        let errors: Vec<String> =
            v.get_path("errors").and_then(Value::as_list).map(|l| l.iter().map(|e| s(e, "msg").to_string()).collect()).unwrap_or_default();
        BotFiles { files: list(&v, "files"), custom: s(&v, "custom_file").to_string(), errors }
    }

    fn callouts(&self, ui: &mut egui::Ui, t: &Theme) {
        for e in &self.errors {
            widgets::callout(
                ui,
                t,
                widgets::Tone::Warn,
                icon::WARN,
                "Some commands didn't load",
                &format!("A command file has a mistake, so the bot skips it until it's fixed: {e}"),
                None,
            );
            ui.add_space(spacing::M);
        }
    }
}

/// Whether the bot can answer in chat (bot health plus the Twitch connection).
struct BotStatus {
    led: LedState,
    text: &'static str,
    tip: String,
}

impl BotStatus {
    fn read(app: &App) -> BotStatus {
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
        let tip = if twitch.connected() { s(&health, "detail").to_string() } else { format!("Connect Twitch so the bot can answer in chat. {}", twitch.body) };
        BotStatus { led, text, tip }
    }

    fn show(&self, ui: &mut egui::Ui, t: &Theme) {
        widgets::pill(ui, t, if self.led == LedState::Healthy { icon::CHECK } else { icon::WARN }, self.text, self.led).on_hover_text(self.tip.as_str());
    }
}

// ---- chat commands -------------------------------------------------------------------------------

/// Automation → Chat commands.
pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("chatbot-form");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    if form.test_role.is_empty() {
        form.test_role = "everyone".into();
    }
    poll(app, ui.input(|i| i.time), &mut form.last_query);
    let files = BotFiles::read(app);
    files.callouts(ui, &t);
    let cmds: Vec<Value> = app.m.q_list("bot.commands").to_vec();
    let words = Words::new(app);
    if form.cmd.is_none()
        && let Some(name) = form.select_name.clone()
        && let Some(c) = cmds.iter().find(|c| s(c, "name").trim_start_matches('!') == name.trim_start_matches('!'))
    {
        form.cmd = Some(Draft::from_command(c, &words));
        form.select_name = None;
    }
    let status = BotStatus::read(app);
    let modes = mode_list(app);
    let selected = form.cmd.as_ref().and_then(|d| d.at.clone());
    let mut filter = std::mem::take(&mut form.filter);
    let (pick, ()) = widgets::split(
        ui,
        LIST_W,
        |ui| command_list(ui, &t, &cmds, &words, &status, &mut filter, selected.as_ref()),
        |ui| {
            egui::ScrollArea::vertical().id_salt("bot-cmd-detail").auto_shrink([false, false]).show(ui, |ui| {
                command_detail(app, ui, &t, &mut form, &files, &modes);
            });
        },
    );
    form.filter = filter;
    match pick {
        Some(Pick::New) => {
            form.cmd = Some(Draft::new_command(&files.custom));
            form.select_name = None;
        }
        Some(Pick::At(n)) => {
            if let Some(c) = cmds.get(n) {
                form.cmd = Some(Draft::from_command(c, &words));
                form.select_name = None;
            }
        }
        None => {}
    }
    ui.data_mut(|d| d.insert_temp(id, form));
}

/// Commands the owner wrote (a reply or steps), as opposed to built-in ones.
fn is_own(c: &Value) -> bool {
    matches!(s(c, "behavior"), "reply" | "do")
}

fn command_list(
    ui: &mut egui::Ui,
    t: &Theme,
    cmds: &[Value],
    words: &Words,
    status: &BotStatus,
    filter: &mut String,
    selected: Option<&(String, i64)>,
) -> Option<Pick> {
    let mut pick = None;
    if widgets::pane_header(ui, t, "Chat commands", Some(cmds.len()), Some("New command")) {
        pick = Some(Pick::New);
    }
    status.show(ui, t);
    ui.add_space(spacing::S);
    ui.add(widgets::field(&mut *filter).hint_text(format!("{}  Find a command", icon::SEARCH)).desired_width(f32::INFINITY));
    ui.add_space(spacing::XS);
    let q = filter.trim().to_lowercase();
    egui::ScrollArea::vertical().id_salt("bot-cmd-list").auto_shrink([false, false]).show(ui, |ui| {
        if cmds.is_empty() {
            widgets::hint(ui, t, "No commands yet.");
            return;
        }
        let mut shown = 0;
        for (group, own) in [("Your commands", true), ("Built in", false)] {
            let rows: Vec<(usize, &Value)> = cmds
                .iter()
                .enumerate()
                .filter(|(_, c)| is_own(c) == own)
                .filter(|(_, c)| q.is_empty() || s(c, "name").to_lowercase().contains(&q) || s(c, "reply").to_lowercase().contains(&q))
                .collect();
            if rows.is_empty() {
                continue;
            }
            widgets::group_label(ui, t, group);
            for (n, c) in rows {
                shown += 1;
                let at = (s(c, "file").to_string(), i(c, "index"));
                let enabled = c.get_path("enabled").is_none_or(Value::truthy);
                let trailing = match (enabled, s(c, "role")) {
                    (false, _) => "Off".to_string(),
                    (true, "" | "everyone") => String::new(),
                    (true, r) => role_words(r).to_string(),
                };
                let sub = clip(&does(c, words), if trailing.is_empty() { 34 } else { 20 });
                let glyph = if s(c, "behavior") == "reply" { icon::CHAT } else { icon::BOLT };
                if widgets::list_row(ui, t, glyph, s(c, "name"), &sub, &trailing, selected == Some(&at)).clicked() {
                    pick = Some(Pick::At(n));
                }
            }
        }
        if shown == 0 {
            widgets::hint(ui, t, "No command matches that.");
        }
    });
    pick
}

fn command_detail(app: &mut App, ui: &mut egui::Ui, t: &Theme, form: &mut Form, files: &BotFiles, modes: &[String]) {
    let Some(mut d) = form.cmd.take() else {
        if widgets::empty_state(
            ui,
            t,
            icon::CHAT,
            "Chat commands",
            "Something viewers type in chat, like !discord, and what the bot does.",
            Some("New command"),
        ) {
            form.cmd = Some(Draft::new_command(&files.custom));
            form.select_name = None;
        }
        return;
    };
    if command_editor(app, ui, t, form, &mut d, files, modes) {
        form.cmd = Some(d);
    }
}

/// One command's inspector. Returns false when the command should be deselected.
fn command_editor(app: &mut App, ui: &mut egui::Ui, t: &Theme, form: &mut Form, d: &mut Draft, files: &BotFiles, modes: &[String]) -> bool {
    let new = d.at.is_none();
    let title = if new {
        "New command".to_string()
    } else if d.name.trim().is_empty() {
        "Command".to_string()
    } else {
        d.name.trim().to_string()
    };
    let can_save = !d.name.trim().trim_start_matches('!').is_empty();
    let (mut save, mut close) = (false, false);
    widgets::detail_header(ui, t, icon::CHAT, &title, "What viewers type and what happens", |ui| {
        save = widgets::button_ex(ui, t, Some(icon::CHECK), if new { "Create" } else { "Save" }, Kind::Primary, Size::Medium, 0.0, can_save).clicked();
        ui.add_space(spacing::S);
        ui.label(RichText::new(if d.enabled { "On" } else { "Off" }).color(t.text_dim));
        widgets::toggle(ui, t, &mut d.enabled).on_hover_text("Off: the bot ignores it");
        close = widgets::icon_button(ui, t, icon::CROSS, "Close").clicked();
    });

    widgets::inspector_section(
        ui,
        t,
        ("bot-cmd", "command"),
        "Command",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Name", |ui| {
                ui.add(widgets::field(&mut d.name).hint_text("!discord").desired_width(f32::INFINITY));
            });
            widgets::prop_row(ui, t, "Also works as", |ui| {
                ui.add(widgets::field(&mut d.aliases).hint_text("!dc, !disc").desired_width(f32::INFINITY));
            });
            widgets::prop_row(ui, t, "Note", |ui| {
                ui.add(widgets::field(&mut d.description).hint_text("Just for you").desired_width(f32::INFINITY));
            });
        },
    );

    widgets::inspector_section(
        ui,
        t,
        ("bot-cmd", "who"),
        "Who & how often",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Who can use it", |ui| {
                egui::ComboBox::from_id_salt("bot-cmd-role").width(200.0).selected_text(role_words(&d.role)).show_ui(ui, |ui| {
                    for (r, w) in ROLES {
                        ui.selectable_value(&mut d.role, r.to_string(), w);
                    }
                });
            });
            widgets::prop_row(ui, t, "Wait between uses", |ui| {
                ui.add(widgets::field(&mut d.cd_global).hint_text("30s").desired_width(90.0));
            });
            widgets::prop_row(ui, t, "Same viewer waits", |ui| {
                ui.add(widgets::field(&mut d.cd_user).hint_text("2m").desired_width(90.0));
            });
            row_hint(ui, t, "Times like 30s or 2m. Empty: no wait.");
            widgets::prop_row(ui, t, "Only during", |ui| mode_chips(ui, t, &mut d.modes, modes));
            row_hint(ui, t, "Show modes. None picked: always.");
            widgets::prop_row(ui, t, "Words needed", |ui| {
                ui.add(widgets::field(&mut d.min_args).hint_text("0").desired_width(60.0));
                widgets::hint(ui, t, "after the command");
            });
            widgets::prop_row(ui, t, "Otherwise answer", |ui| {
                ui.add(widgets::field(&mut d.usage).hint_text("Try: !so @name").desired_width(f32::INFINITY));
            });
        },
    );

    let builtin = d.builtin();
    widgets::inspector_section(
        ui,
        t,
        ("bot-cmd", "reply"),
        "Reply",
        true,
        |_| {},
        |ui| {
            if builtin && d.reply.is_empty() {
                widgets::hint(ui, t, "This command is built in, so the bot already knows what to answer.");
                return;
            }
            ui.add(egui::TextEdit::multiline(&mut d.reply).hint_text("Join the Discord: https://discord.gg/…").desired_rows(3).desired_width(f32::INFINITY));
            ui.add_space(spacing::XS);
            ui.horizontal_wrapped(|ui| {
                widgets::hint(ui, t, "Insert");
                for (ph, what) in PLACEHOLDERS {
                    if widgets::chip(ui, t, "", &capitalize(what), false).on_hover_text(format!("Becomes {what} when it's used. Types {ph}.")).clicked() {
                        d.reply.push_str(ph);
                    }
                }
            });
        },
    );

    widgets::inspector_section(
        ui,
        t,
        ("bot-cmd", "do"),
        "Do",
        true,
        |_| {},
        |ui| {
            if builtin {
                if !d.does.is_empty() {
                    widgets::hint(ui, t, &d.does);
                    ui.add_space(spacing::XS);
                }
                if let Some(name) = d.behavior.strip_prefix("builtin ") {
                    widgets::fact(ui, t, "Built in", name);
                }
                if !d.action.is_empty() {
                    widgets::fact(ui, t, "Action", &d.action);
                }
                if !d.args.is_empty() {
                    widgets::fact(ui, t, "Settings", &d.args);
                }
            } else {
                widgets::hint(ui, t, "Runs at once; timed steps need a saved action.");
                ui.add_space(spacing::XS);
                steps_editor(app, ui, "bot-cmd-do", &mut d.cmds, "twitch.chat");
            }
        },
    );

    widgets::inspector_section(ui, t, ("bot-cmd", "try"), "Try it", false, |_| {}, |ui| try_it(app, ui, t, form, d.name.trim()));

    ui.add_space(spacing::S);
    widgets::details(ui, t, "bot-cmd-details", "Details", |ui| d.saved_in(ui, t, "bot-cmd-file", files));
    let delete = !new && delete_button(ui, t, "Delete command");

    if save {
        match d.command_fields() {
            Ok(fields) => {
                let mut args = Value::map().with("fields", fields).with("file", d.file.clone());
                if let Some((_, idx)) = &d.at {
                    args = args.with("index", *idx);
                }
                act(app, "bot.command.save", args);
                app.m.toast(format!("Saved {}.", d.name.trim()), false);
                if new {
                    form.select_name = Some(d.name.trim().to_string());
                    return false;
                }
            }
            Err(e) => app.m.toast(e, true),
        }
    }
    if delete {
        if let Some((file, idx)) = d.at.clone() {
            act(app, "bot.command.delete", Value::map().with("file", file).with("index", idx));
        }
        return false;
    }
    !close
}

/// Send a chat line as a pretend viewer and watch the bot answer.
fn try_it(app: &mut App, ui: &mut egui::Ui, t: &Theme, form: &mut Form, name: &str) {
    widgets::prop_row(ui, t, "Pretend to be", |ui| {
        egui::ComboBox::from_id_salt("bot-test-role").width(200.0).selected_text(role_words(&form.test_role)).show_ui(ui, |ui| {
            for (r, w) in ROLES {
                ui.selectable_value(&mut form.test_role, r.to_string(), w);
            }
        });
    });
    let fallback = if name.trim_start_matches('!').is_empty() { "" } else { name };
    let can_send = !form.test_msg.trim().is_empty() || !fallback.is_empty();
    let mut send = false;
    widgets::prop_row(ui, t, "Message", |ui| {
        let r = ui.add(
            widgets::field(&mut form.test_msg)
                .hint_text(if fallback.is_empty() { "!discord" } else { fallback })
                .desired_width((ui.available_width() - 110.0).max(80.0)),
        );
        let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
        send = widgets::button_ex(ui, t, Some(icon::PLAY), "Send", Kind::Secondary, Size::Medium, 0.0, can_send).clicked() || enter;
    });
    let msg = if form.test_msg.trim().is_empty() { fallback.to_string() } else { form.test_msg.trim().to_string() };
    if send && !msg.is_empty() {
        act(app, "sim.chat", Value::map().with("message", msg).with("role", form.test_role.clone()).with("user", "tester"));
        form.test_msg.clear();
    }
    row_hint(ui, t, "The bot answers in your Twitch chat when it's connected.");
    ui.add_space(spacing::S);
    egui::Frame::new()
        .fill(t.bg)
        .stroke(egui::Stroke::new(1.0, t.border))
        .corner_radius(egui::CornerRadius::same(se_ui_kit::theme::radius::CONTROL))
        .inner_margin(egui::Margin::same(12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt("bot-test-log").stick_to_bottom(true).auto_shrink([false, false]).max_height(240.0).show(ui, |ui| {
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
                    widgets::hint(ui, t, "Nothing yet. Send a message and the answer shows up here.");
                }
            });
        });
}

// ---- chat bot extras -----------------------------------------------------------------------------

/// Community → Chat bot: timed messages, counters, quotes and settings.
pub fn extras_ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("chatbot-extras");
    let mut ex: Extras = ui.data_mut(|d| d.get_temp::<Extras>(id)).unwrap_or_default();
    let now = ui.input(|i| i.time);
    poll(app, now, &mut ex.last_query);
    let files = BotFiles::read(app);
    widgets::segmented(ui, &t, &mut ex.tab, &EXTRA_TABS);
    ui.add_space(spacing::M);
    files.callouts(ui, &t);
    match ex.tab {
        0 => timers(app, ui, &t, &mut ex, &files),
        1 => counters(app, ui, &t, &mut ex),
        2 => quotes(app, ui, &t, &mut ex),
        _ => settings(app, ui, &t, &mut ex, &files, now),
    }
    ui.data_mut(|d| d.insert_temp(id, ex));
}

// ---- timed messages ------------------------------------------------------------------------------

fn timers(app: &mut App, ui: &mut egui::Ui, t: &Theme, ex: &mut Extras, files: &BotFiles) {
    let timers: Vec<Value> = app.m.q_list("bot.timers").to_vec();
    let modes = mode_list(app);
    let mode = app.m.str("show.mode").to_string();
    let selected = ex.timer.as_ref().and_then(|d| d.at.clone());
    let (pick, ()) = widgets::split(
        ui,
        LIST_W,
        |ui| timer_list(ui, t, &timers, &mode, selected.as_ref()),
        |ui| {
            egui::ScrollArea::vertical().id_salt("bot-timer-detail").auto_shrink([false, false]).show(ui, |ui| {
                let Some(mut d) = ex.timer.take() else {
                    if widgets::empty_state(
                        ui,
                        t,
                        icon::CLOCK,
                        "Timed messages",
                        "The bot posts these every so often while chat is active.",
                        Some("New timed message"),
                    ) {
                        ex.timer = Some(Draft::new_timer(&files.custom));
                    }
                    return;
                };
                if timer_editor(app, ui, t, &mut d, files, &modes) {
                    ex.timer = Some(d);
                }
            });
        },
    );
    match pick {
        Some(Pick::New) => ex.timer = Some(Draft::new_timer(&files.custom)),
        Some(Pick::At(n)) => {
            if let Some(tm) = timers.get(n) {
                ex.timer = Some(Draft::from_timer(tm));
            }
        }
        None => {}
    }
}

fn timer_list(ui: &mut egui::Ui, t: &Theme, timers: &[Value], mode: &str, selected: Option<&(String, i64)>) -> Option<Pick> {
    let mut pick = None;
    if widgets::pane_header(ui, t, "Timed messages", Some(timers.len()), Some("New timed message")) {
        pick = Some(Pick::New);
    }
    egui::ScrollArea::vertical().id_salt("bot-timer-list").auto_shrink([false, false]).show(ui, |ui| {
        if timers.is_empty() {
            widgets::hint(ui, t, "No timed messages yet.");
        }
        for (n, tm) in timers.iter().enumerate() {
            let at = (s(tm, "file").to_string(), i(tm, "index"));
            let enabled = tm.get_path("enabled").is_none_or(Value::truthy);
            let tm_modes = list(tm, "modes");
            let in_mode = tm_modes.is_empty() || tm_modes.iter().any(|m| m == mode);
            let title = if s(tm, "name").is_empty() { clip(s(tm, "reply"), 24) } else { nice(s(tm, "name")) };
            let sub = format!("Every {} · {} messages", dur_words(i(tm, "every_ms")), i(tm, "min_chat_lines"));
            let trailing = match (enabled, in_mode, tm.get_path("next_in_ms").and_then(Value::as_i64)) {
                (false, ..) => "Off".to_string(),
                (true, false, _) | (true, _, None) => format!("waits for {}", tm_modes.iter().map(|m| pretty(m)).collect::<Vec<_>>().join(" / ")),
                (true, true, Some(ms)) => format!("next in {}:{:02}", ms / 60_000, ms / 1000 % 60),
            };
            if widgets::list_row(ui, t, icon::CLOCK, &title, &sub, &trailing, selected == Some(&at)).clicked() {
                pick = Some(Pick::At(n));
            }
        }
    });
    pick
}

/// One timed message's inspector. Returns false when it should be deselected.
fn timer_editor(app: &mut App, ui: &mut egui::Ui, t: &Theme, d: &mut Draft, files: &BotFiles, modes: &[String]) -> bool {
    let new = d.at.is_none();
    let title = if new {
        "New timed message".to_string()
    } else if d.name.trim().is_empty() {
        "Timed message".to_string()
    } else {
        nice(d.name.trim())
    };
    let can_save = !d.every.trim().is_empty();
    let (mut save, mut close) = (false, false);
    widgets::detail_header(ui, t, icon::CLOCK, &title, "Posted in chat every so often", |ui| {
        save_close(ui, t, new, can_save, &mut save, &mut close);
    });

    widgets::inspector_section(
        ui,
        t,
        ("bot-timer", "message"),
        "Message",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Name", |ui| {
                ui.add(widgets::field(&mut d.name).hint_text("Follow reminder").desired_width(f32::INFINITY));
            });
            widgets::prop_row(ui, t, "Message", |ui| {
                ui.add(egui::TextEdit::multiline(&mut d.reply).hint_text("Enjoying the stream? Hit follow!").desired_rows(3).desired_width(f32::INFINITY));
            });
            widgets::prop_row(ui, t, "On", |ui| {
                widgets::toggle(ui, t, &mut d.enabled);
                widgets::hint(ui, t, if d.enabled { "The bot posts it." } else { "Off: the bot skips it." });
            });
        },
    );

    widgets::inspector_section(
        ui,
        t,
        ("bot-timer", "when"),
        "When",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Every", |ui| {
                ui.add(widgets::field(&mut d.every).hint_text("20m").desired_width(90.0));
                widgets::hint(ui, t, "at least 1 minute");
            });
            widgets::prop_row(ui, t, "After", |ui| {
                ui.add(widgets::field(&mut d.min_lines).hint_text("10").desired_width(60.0));
                widgets::hint(ui, t, "chat messages, so it never posts into a quiet chat");
            });
            widgets::prop_row(ui, t, "Only during", |ui| mode_chips(ui, t, &mut d.modes, modes));
            row_hint(ui, t, "Show modes. None picked: only while live.");
        },
    );

    widgets::inspector_section(
        ui,
        t,
        ("bot-timer", "do"),
        "Do",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, t, "Also runs these each time it posts.");
            ui.add_space(spacing::XS);
            steps_editor(app, ui, "bot-timer-do", &mut d.cmds, "");
        },
    );

    ui.add_space(spacing::S);
    widgets::details(ui, t, "bot-timer-details", "Details", |ui| d.saved_in(ui, t, "bot-timer-file", files));
    let delete = !new && delete_button(ui, t, "Delete timed message");

    if save {
        match d.timer_fields() {
            Ok(fields) => {
                let mut args = Value::map().with("fields", fields).with("file", d.file.clone());
                if let Some((_, idx)) = &d.at {
                    args = args.with("index", *idx);
                }
                act(app, "bot.timer.save", args);
                app.m.toast("Timed message saved.", false);
                if new {
                    return false;
                }
            }
            Err(e) => app.m.toast(e, true),
        }
    }
    if delete {
        if let Some((file, idx)) = d.at.clone() {
            act(app, "bot.timer.delete", Value::map().with("file", file).with("index", idx));
        }
        return false;
    }
    !close
}

// ---- counters ------------------------------------------------------------------------------------

fn counters(app: &mut App, ui: &mut egui::Ui, t: &Theme, ex: &mut Extras) {
    let counters: Vec<Value> = app.m.q_list("bot.counters").to_vec();
    let selected = if ex.counter_new.is_some() { None } else { ex.counter.clone() };
    let (pick, ()) = widgets::split(
        ui,
        LIST_W,
        |ui| {
            let mut pick = None;
            if widgets::pane_header(ui, t, "Counters", Some(counters.len()), Some("New counter")) {
                pick = Some(Pick::New);
            }
            egui::ScrollArea::vertical().id_salt("bot-counter-list").auto_shrink([false, false]).show(ui, |ui| {
                if counters.is_empty() {
                    widgets::hint(ui, t, "No counters yet.");
                }
                for (n, c) in counters.iter().enumerate() {
                    let name = s(c, "name");
                    if widgets::list_row(ui, t, icon::LIST, &nice(name), "", &i(c, "value").to_string(), selected.as_deref() == Some(name)).clicked() {
                        pick = Some(Pick::At(n));
                    }
                }
            });
            pick
        },
        |ui| {
            egui::ScrollArea::vertical().id_salt("bot-counter-detail").auto_shrink([false, false]).show(ui, |ui| {
                counter_detail(app, ui, t, ex, &counters);
            });
        },
    );
    match pick {
        Some(Pick::New) => {
            ex.counter = None;
            ex.counter_new = Some(Default::default());
        }
        Some(Pick::At(n)) => {
            if let Some(c) = counters.get(n) {
                ex.counter = Some(s(c, "name").to_string());
                ex.counter_new = None;
            }
        }
        None => {}
    }
}

fn counter_detail(app: &mut App, ui: &mut egui::Ui, t: &Theme, ex: &mut Extras, counters: &[Value]) {
    if let Some((name, value)) = ex.counter_new.as_mut() {
        let start = if value.trim().is_empty() { Ok(0) } else { value.trim().parse::<i64>() };
        let can_create = !name.trim().is_empty() && start.is_ok();
        let (mut create, mut close) = (false, false);
        widgets::detail_header(ui, t, icon::LIST, "New counter", "A number your commands can show", |ui| {
            save_close(ui, t, true, can_create, &mut create, &mut close);
        });
        widgets::inspector_section(
            ui,
            t,
            ("bot-counter", "new"),
            "Counter",
            true,
            |_| {},
            |ui| {
                widgets::prop_row(ui, t, "Name", |ui| {
                    ui.add(widgets::field(&mut *name).hint_text("drumsticks").desired_width(f32::INFINITY));
                });
                widgets::prop_row(ui, t, "Starts at", |ui| {
                    ui.add(widgets::field(&mut *value).hint_text("0").desired_width(90.0));
                });
            },
        );
        if create {
            let name = name.trim().to_string();
            act(app, "bot.counter.set", Value::map().with("name", name.clone()).with("value", start.unwrap_or(0)));
            ex.counter = Some(name);
            ex.counter_new = None;
        } else if close {
            ex.counter_new = None;
        }
        return;
    }
    let Some(c) = ex.counter.as_ref().and_then(|n| counters.iter().find(|c| s(c, "name") == n)) else {
        if widgets::empty_state(ui, t, icon::LIST, "Counters", "Numbers your commands can show, like drumsticks thrown.", Some("New counter")) {
            ex.counter = None;
            ex.counter_new = Some(Default::default());
        }
        return;
    };
    let name = s(c, "name").to_string();
    let v = i(c, "value");
    let mut close = false;
    widgets::detail_header(ui, t, icon::LIST, &nice(&name), "A number your commands can show", |ui| {
        close = widgets::icon_button(ui, t, icon::CROSS, "Close").clicked();
    });
    let mut set = None;
    widgets::inspector_section(
        ui,
        t,
        ("bot-counter", "count"),
        "Count",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Now", |ui| {
                ui.label(RichText::new(v.to_string()).font(font_mono(type_scale::HEADING)).color(t.fg));
                ui.add_space(spacing::M);
                if widgets::button(ui, t, "−1", Kind::Secondary).clicked() {
                    set = Some(v - 1);
                }
                if widgets::button(ui, t, "+1", Kind::Secondary).clicked() {
                    set = Some(v + 1);
                }
                if widgets::button(ui, t, "Reset", Kind::Ghost).clicked() {
                    set = Some(0);
                }
            });
        },
    );
    if let Some(to) = set {
        act(app, "bot.counter.set", Value::map().with("name", name.clone()).with("value", to));
    }
    widgets::inspector_section(
        ui,
        t,
        ("bot-counter", "use"),
        "Use it",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, t, "Show it in a command's reply with Insert → A counter. Mods can set it from chat with !setcounter.");
        },
    );
    ui.add_space(spacing::S);
    widgets::details(ui, t, "bot-counter-details", "Details", |ui| {
        widgets::fact(ui, t, "Setting", &format!("bot.counter.{name}"));
        widgets::hint(ui, t, "Triggers and sources can use this setting.");
    });
    if delete_button(ui, t, "Delete counter") {
        act(app, "bot.counter.delete", Value::map().with("name", name));
        ex.counter = None;
    } else if close {
        ex.counter = None;
    }
}

// ---- quotes --------------------------------------------------------------------------------------

fn quote_by(q: &Value) -> String {
    match s(q, "added_by") {
        "" | "ui" => "you".to_string(),
        who => who.to_string(),
    }
}

fn quotes(app: &mut App, ui: &mut egui::Ui, t: &Theme, ex: &mut Extras) {
    let quotes: Vec<Value> = app.m.q_list("bot.quotes").to_vec();
    let selected = ex.quote.as_ref().and_then(|(id, _)| *id);
    let mut filter = std::mem::take(&mut ex.quote_filter);
    let (pick, ()) = widgets::split(
        ui,
        LIST_W,
        |ui| quote_list(ui, t, &quotes, &mut filter, selected),
        |ui| {
            egui::ScrollArea::vertical().id_salt("bot-quote-detail").auto_shrink([false, false]).show(ui, |ui| {
                quote_detail(app, ui, t, ex, &quotes);
            });
        },
    );
    ex.quote_filter = filter;
    match pick {
        Some(Pick::New) => ex.quote = Some((None, String::new())),
        Some(Pick::At(n)) => {
            if let Some(q) = quotes.get(n) {
                ex.quote = Some((Some(i(q, "id")), s(q, "text").to_string()));
            }
        }
        None => {}
    }
}

fn quote_list(ui: &mut egui::Ui, t: &Theme, quotes: &[Value], filter: &mut String, selected: Option<i64>) -> Option<Pick> {
    let mut pick = None;
    if widgets::pane_header(ui, t, "Quotes", Some(quotes.len()), Some("New quote")) {
        pick = Some(Pick::New);
    }
    ui.add(widgets::field(&mut *filter).hint_text(format!("{}  Find a quote", icon::SEARCH)).desired_width(f32::INFINITY));
    ui.add_space(spacing::XS);
    let q = filter.trim().to_lowercase();
    egui::ScrollArea::vertical().id_salt("bot-quote-list").auto_shrink([false, false]).show(ui, |ui| {
        if quotes.is_empty() {
            widgets::hint(ui, t, "No quotes yet.");
            return;
        }
        let mut shown = 0;
        // newest first
        for (n, quote) in quotes.iter().enumerate().rev() {
            if !q.is_empty() && !s(quote, "text").to_lowercase().contains(&q) {
                continue;
            }
            shown += 1;
            let qid = i(quote, "id");
            let sub = format!("{} · {}", quote_by(quote), crate_date(i(quote, "created_at")));
            if widgets::list_row(ui, t, icon::CHAT, &clip(s(quote, "text"), 26), &sub, &format!("#{qid}"), selected == Some(qid)).clicked() {
                pick = Some(Pick::At(n));
            }
        }
        if shown == 0 {
            widgets::hint(ui, t, "No quote matches that.");
        }
    });
    pick
}

fn quote_detail(app: &mut App, ui: &mut egui::Ui, t: &Theme, ex: &mut Extras, quotes: &[Value]) {
    let Some((qid, mut text)) = ex.quote.take() else {
        if widgets::empty_state(ui, t, icon::CHAT, "Quotes", "Funny moments to bring back. Viewers show one with !quote.", Some("New quote")) {
            ex.quote = Some((None, String::new()));
        }
        return;
    };
    let q = qid.and_then(|id| quotes.iter().find(|q| i(q, "id") == id));
    if qid.is_some() && q.is_none() {
        // it was deleted meanwhile
        return;
    }
    let id = qid;
    let (title, sub) = match q {
        Some(q) => (format!("Quote #{}", i(q, "id")), format!("Added by {} on {}", quote_by(q), crate_date(i(q, "created_at")))),
        None => ("New quote".to_string(), "Viewers show one with !quote".to_string()),
    };
    let changed = q.is_none_or(|q| s(q, "text") != text.trim());
    let can_save = changed && !text.trim().is_empty();
    let (mut save, mut close) = (false, false);
    widgets::detail_header(ui, t, icon::CHAT, &title, &sub, |ui| {
        save_close(ui, t, id.is_none(), can_save, &mut save, &mut close);
    });
    widgets::inspector_section(
        ui,
        t,
        ("bot-quote", "text"),
        "Quote",
        true,
        |_| {},
        |ui| {
            ui.add(egui::TextEdit::multiline(&mut text).hint_text("Something worth remembering…").desired_rows(3).desired_width(f32::INFINITY));
        },
    );
    let delete = id.is_some() && delete_button(ui, t, "Delete quote");
    if save {
        match id {
            Some(id) => act(app, "bot.quote.edit", Value::map().with("id", id).with("text", text.trim())),
            None => {
                act(app, "bot.quote.add", Value::map().with("text", text.trim()).with("by", "ui"));
                return;
            }
        }
    }
    if delete {
        if let Some(id) = id {
            act(app, "bot.quote.delete", Value::map().with("id", id));
        }
        return;
    }
    if !close {
        ex.quote = Some((qid, text));
    }
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

// ---- settings ------------------------------------------------------------------------------------

/// The editable part of `[bot]` in `project.toml` (defaults as in `se-bot` `Settings`).
#[derive(Clone, PartialEq)]
struct BotSettings {
    custom_file: String,
    mods_skip: bool,
    max_reply: i64,
    max_custom: i64,
}

impl BotSettings {
    /// `None` when the file can't be parsed.
    fn parse(text: &str) -> Option<BotSettings> {
        let table = toml::from_str::<toml::Table>(text).ok()?;
        let bot = table.get("bot").and_then(toml::Value::as_table);
        let get = |k: &str| bot.and_then(|b| b.get(k));
        Some(BotSettings {
            custom_file: get("custom_file").and_then(toml::Value::as_str).unwrap_or("commands/custom.toml").to_string(),
            mods_skip: get("mods_bypass_cooldowns").and_then(toml::Value::as_bool).unwrap_or(true),
            max_reply: get("max_reply").and_then(toml::Value::as_integer).unwrap_or(500),
            max_custom: get("max_custom_reply").and_then(toml::Value::as_integer).unwrap_or(400),
        })
    }

    /// `project.write` `set` keys for what differs from `base`.
    fn changes(&self, base: &BotSettings) -> Value {
        let mut set = Value::map();
        if self.mods_skip != base.mods_skip {
            set = set.with("bot.mods_bypass_cooldowns", self.mods_skip);
        }
        if self.max_reply != base.max_reply {
            set = set.with("bot.max_reply", self.max_reply);
        }
        if self.max_custom != base.max_custom {
            set = set.with("bot.max_custom_reply", self.max_custom);
        }
        if self.custom_file != base.custom_file {
            set = set.with("bot.custom_file", self.custom_file.clone());
        }
        set
    }
}

fn settings(app: &mut App, ui: &mut egui::Ui, t: &Theme, ex: &mut Extras, files: &BotFiles, now: f64) {
    if now >= ex.set_hold && now - ex.last_read > 2.0 {
        ex.last_read = now;
        app.m.query_as(PROJECT_KEY, "project.read", Value::map().with("path", "project.toml"));
    }
    let seq = app.m.q_seq(PROJECT_KEY);
    if seq != ex.set_seq {
        ex.set_seq = seq;
        // while a save is on its way, a read may still show the old file
        if now >= ex.set_hold
            && let Some(reply) = app.m.q(PROJECT_KEY)
        {
            let loaded = BotSettings::parse(s(reply, "text"));
            ex.set_error = loaded.is_none();
            if let Some(loaded) = loaded {
                let dirty = ex.set_draft.is_some() && ex.set_draft != ex.set_base;
                if !dirty {
                    ex.set_draft = Some(loaded.clone());
                }
                ex.set_base = Some(loaded);
            }
        }
    }
    let status = BotStatus::read(app);
    egui::ScrollArea::vertical().id_salt("bot-settings").auto_shrink([false, false]).show(ui, |ui| {
        ui.set_max_width(760.0);
        let dirty = ex.set_draft.is_some() && ex.set_draft != ex.set_base;
        let (mut save, mut discard) = (false, false);
        widgets::detail_header(ui, t, icon::BOT, "Chat bot", "How the bot answers in chat", |ui| {
            save = widgets::button_ex(ui, t, Some(icon::CHECK), "Save", Kind::Primary, Size::Medium, 0.0, dirty).clicked();
            discard = widgets::button_ex(ui, t, None, "Discard", Kind::Ghost, Size::Medium, 0.0, dirty).clicked();
        });
        status.show(ui, t);
        ui.add_space(spacing::M);
        if ex.set_error {
            widgets::callout(
                ui,
                t,
                widgets::Tone::Warn,
                icon::WARN,
                "Can't read the bot settings",
                "project.toml has a mistake. Fix it and they show up here.",
                None,
            );
            ui.add_space(spacing::M);
        }
        let Some(d) = ex.set_draft.as_mut() else {
            if !ex.set_error {
                widgets::hint(ui, t, "Loading…");
            }
            return;
        };
        widgets::inspector_section(
            ui,
            t,
            ("bot-settings", "replies"),
            "Replies",
            true,
            |_| {},
            |ui| {
                widgets::prop_row(ui, t, "Longest reply", |ui| {
                    ui.add(egui::DragValue::new(&mut d.max_reply).range(1..=500).suffix(" characters"));
                });
                widgets::prop_row(ui, t, "Added in chat", |ui| {
                    ui.add(egui::DragValue::new(&mut d.max_custom).range(1..=500).suffix(" characters"));
                    widgets::hint(ui, t, "longest reply of a command added with !addcom");
                });
            },
        );
        widgets::inspector_section(
            ui,
            t,
            ("bot-settings", "commands"),
            "Commands",
            true,
            |_| {},
            |ui| {
                widgets::prop_row(ui, t, "Mods skip waits", |ui| {
                    widgets::toggle(ui, t, &mut d.mods_skip);
                    widgets::hint(ui, t, "Mods and you skip the wait between uses.");
                });
                widgets::prop_row(ui, t, "New ones go to", |ui| {
                    egui::ComboBox::from_id_salt("bot-settings-file").width(240.0).selected_text(d.custom_file.clone()).show_ui(ui, |ui| {
                        let current = d.custom_file.clone();
                        for f in files.files.iter().chain((!files.files.contains(&current)).then_some(&current)) {
                            ui.selectable_value(&mut d.custom_file, f.clone(), f);
                        }
                    });
                });
                row_hint(ui, t, "Where new commands are saved, from here or from chat with !addcom.");
            },
        );
        let draft = d.clone();
        if save && let Some(base) = ex.set_base.clone() {
            app.m.action("project.write", Value::map().with("path", "project.toml").with("set", draft.changes(&base)));
            app.m.toast("Bot settings saved.", false);
            ex.set_base = Some(draft);
            ex.set_hold = now + 0.6;
            ex.last_read = 0.0;
        } else if discard {
            ex.set_draft = ex.set_base.clone();
        }
    });
}
