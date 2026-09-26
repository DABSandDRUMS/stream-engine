//! `commands/*.toml` (commands and timers) and `project.toml [bot]`.
//!
//! ```toml
//! [[command]]
//! name     = "!discord"
//! aliases  = ["!dc"]
//! reply    = "Join the Discord: https://discord.gg/xxxx"
//! role     = "everyone"                    # everyone | follower | sub | vip | mod | owner
//! cooldown = { global = "30s", per_user = "2m" }
//!
//! [[command]]
//! name   = "!hype"
//! role   = "vip"
//! do     = ["preset.fire hype"]            # one-line commands, templated per token
//!
//! [[command]]
//! name   = "!sr"
//! action = "queue.request"                 # one subsystem action with templated args
//! args   = { user = "{user}", text = "{args}" }
//! usage  = "Usage: !sr <song or YouTube link>"
//! min_args = 1
//!
//! [[command]]
//! name = "!queue"
//! reply = "Queue: {queue.url}"
//! sub.open = { role = "mod", action = "queue.open", reply = "Requests are open" }
//!
//! [[timer]]
//! every = "20m"
//! min_chat_lines = 10
//! reply = "Song requests are open: !sr <song or link>"
//! ```

use crate::template;
use se_core::config::Dur;
use se_core::policy::CooldownSpec;
use se_proto::{Op, Role, Value};
use serde::Deserialize;
use std::collections::BTreeMap;

/// Built-in behaviors a command can bind to (renamable, role-gated like any command).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Builtin {
    /// `!addcom !name reply…`
    Addcom,
    /// `!editcom !name reply…`
    Editcom,
    /// `!delcom !name`
    Delcom,
    /// `!quote [n | search words]`
    Quote,
    /// `!addquote text…`
    Addquote,
    /// `!delquote n`
    Delquote,
    /// `!setcounter name value`
    Setcounter,
    /// `!commands`: list what the caller may use
    Commands,
}

impl Builtin {
    pub fn as_str(self) -> &'static str {
        match self {
            Builtin::Addcom => "addcom",
            Builtin::Editcom => "editcom",
            Builtin::Delcom => "delcom",
            Builtin::Quote => "quote",
            Builtin::Addquote => "addquote",
            Builtin::Delquote => "delquote",
            Builtin::Setcounter => "setcounter",
            Builtin::Commands => "commands",
        }
    }

    /// Reply used when the command defines none.
    pub fn default_reply(self) -> &'static str {
        match self {
            Builtin::Addcom => "@{user} added {command}",
            Builtin::Editcom => "@{user} updated {command}",
            Builtin::Delcom => "@{user} deleted {command}",
            Builtin::Quote => "Quote #{quote_id}: {quote} ({quote_date})",
            Builtin::Addquote => "@{user} added quote #{quote_id}",
            Builtin::Delquote => "@{user} deleted quote #{quote_id}",
            Builtin::Setcounter => "{counter} = {value}",
            Builtin::Commands => "Commands: {commands}",
        }
    }
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawCooldown {
    global: Option<Dur>,
    per_user: Option<Dur>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawCommand {
    name: String,
    aliases: Vec<String>,
    description: Option<String>,
    reply: Option<String>,
    role: Option<String>,
    cooldown: Option<RawCooldown>,
    #[serde(rename = "do")]
    cmds: Vec<String>,
    action: Option<String>,
    args: toml::Table,
    builtin: Option<Builtin>,
    modes: Vec<String>,
    enabled: Option<bool>,
    counter: Option<String>,
    usage: Option<String>,
    min_args: usize,
    deny_reply: Option<String>,
    /// Reply as a thread on the invoking message.
    thread: bool,
    /// Send as `bot` or `broadcaster` (default: `[twitch] chat_as`).
    #[serde(rename = "as")]
    as_account: Option<String>,
    sub: BTreeMap<String, RawCommand>,
}

#[derive(Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
struct RawTimer {
    name: Option<String>,
    every: Option<Dur>,
    min_chat_lines: u32,
    modes: Option<Vec<String>>,
    reply: Option<String>,
    #[serde(rename = "do")]
    cmds: Vec<String>,
    enabled: Option<bool>,
    /// Delay before the first run after entering an allowed mode (default: `every`).
    offset: Option<Dur>,
}

/// One chat command.
#[derive(Clone, Debug, PartialEq)]
pub struct CommandDef {
    /// Trigger word, lowercase (`!discord`).
    pub name: String,
    pub aliases: Vec<String>,
    pub description: String,
    pub reply: Option<String>,
    pub role: Role,
    pub cooldown: CooldownSpec,
    pub cmds: Vec<String>,
    pub action: Option<String>,
    pub args: BTreeMap<String, Value>,
    pub builtin: Option<Builtin>,
    pub modes: Vec<String>,
    pub enabled: bool,
    pub counter: Option<String>,
    pub usage: Option<String>,
    pub min_args: usize,
    pub deny_reply: Option<String>,
    pub thread: bool,
    pub as_account: Option<String>,
    /// Subcommands by first argument (`!queue open`).
    pub sub: BTreeMap<String, CommandDef>,
    /// Source file (relative) and index in its `[[command]]` array.
    pub file: String,
    pub index: usize,
}

impl CommandDef {
    /// A plain reply command (what `!addcom` creates); only these are editable from chat.
    pub fn is_simple(&self) -> bool {
        self.builtin.is_none() && self.action.is_none() && self.cmds.is_empty() && self.sub.is_empty()
    }

    /// Counter used by `{count}` (explicit `counter`, else the name without its prefix when
    /// the reply mentions `{count}`).
    pub fn counter_name(&self) -> Option<String> {
        if let Some(c) = &self.counter {
            return Some(crate::text::segment(c));
        }
        let uses = self.reply.as_deref().is_some_and(|r| template::names(r).contains(&"count"));
        uses.then(|| crate::text::segment(&self.name))
    }
}

/// One timed message.
#[derive(Clone, Debug, PartialEq)]
pub struct TimerDef {
    /// Stable id: `name`, else `<file stem>#<n>`.
    pub name: String,
    pub every_ms: u64,
    pub offset_ms: u64,
    pub min_chat_lines: u32,
    pub modes: Vec<String>,
    pub reply: Option<String>,
    pub cmds: Vec<String>,
    pub enabled: bool,
    pub file: String,
    pub index: usize,
}

/// `project.toml [bot]`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Where `!addcom` (and the UI's "new command") writes.
    pub custom_file: String,
    /// Mods and the broadcaster skip command cooldowns.
    pub mods_bypass_cooldowns: bool,
    /// Longest reply (characters, ≤ 500).
    pub max_reply: usize,
    /// Longest reply a chat-added command may have.
    pub max_custom_reply: usize,
    /// A chat line identical to something the bot sent within this window is its own echo.
    pub echo_window: Dur,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            custom_file: "commands/custom.toml".into(),
            mods_bypass_cooldowns: true,
            max_reply: crate::text::TWITCH_MAX,
            max_custom_reply: 400,
            echo_window: Dur(30_000),
        }
    }
}

impl Settings {
    pub fn from_section(v: Option<&toml::Value>) -> Result<Settings, String> {
        let Some(v) = v else { return Ok(Settings::default()) };
        let s: Settings = v.clone().try_into().map_err(|e: toml::de::Error| format!("[bot]: {}", e.message()))?;
        if !s.custom_file.starts_with("commands/") || !s.custom_file.ends_with(".toml") || s.custom_file.contains("..") {
            return Err("[bot] custom_file must be commands/<name>.toml".into());
        }
        Ok(s)
    }
}

/// Everything one `commands/*.toml` file defines.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileDefs {
    pub commands: Vec<CommandDef>,
    pub timers: Vec<TimerDef>,
}

fn check_line(line: &str) -> Result<(), String> {
    let op = Op::parse(line).map_err(|e| format!("`{line}`: {e}"))?;
    if matches!(op, Op::Wait { .. }) {
        return Err(format!("`{line}`: bot commands run at once; use a preset for timed sequences"));
    }
    Ok(())
}

fn role(s: Option<&str>, default: Role) -> Result<Role, String> {
    match s {
        None => Ok(default),
        Some(r) => Role::parse(r).ok_or_else(|| format!("unknown role `{r}` (everyone, follower, sub, vip, mod, owner)")),
    }
}

fn command(raw: RawCommand, file: &str, index: usize, parent_role: Role) -> Result<CommandDef, String> {
    let name = raw.name.trim().to_lowercase();
    let label = name.clone();
    let ctx = move |m: String| format!("command `{label}`: {m}");
    let mut def = CommandDef {
        aliases: raw.aliases.iter().map(|a| a.trim().to_lowercase()).collect(),
        description: raw.description.unwrap_or_default(),
        reply: raw.reply,
        role: role(raw.role.as_deref(), parent_role).map_err(&ctx)?,
        cooldown: CooldownSpec {
            global_ms: raw.cooldown.as_ref().and_then(|c| c.global).map(Dur::ms),
            per_user_ms: raw.cooldown.as_ref().and_then(|c| c.per_user).map(Dur::ms),
        },
        cmds: raw.cmds,
        action: raw.action,
        args: raw.args.into_iter().map(|(k, v)| (k, Value::from(v))).collect(),
        builtin: raw.builtin,
        modes: raw.modes,
        enabled: raw.enabled.unwrap_or(true),
        counter: raw.counter,
        usage: raw.usage,
        min_args: raw.min_args,
        deny_reply: raw.deny_reply,
        thread: raw.thread,
        as_account: raw.as_account,
        sub: BTreeMap::new(),
        file: file.to_string(),
        index,
        name,
    };
    for (k, s) in raw.sub {
        let sd = command(RawCommand { name: format!("{} {}", def.name, k.to_lowercase()), ..s }, file, index, def.role)?;
        def.sub.insert(k.to_lowercase(), sd);
    }
    for c in &def.cmds {
        check_line(c).map_err(&ctx)?;
    }
    if let Some(a) = &def.action
        && !se_proto::address::is_valid(a, false)
    {
        return Err(ctx(format!("bad action name `{a}`")));
    }
    if let Some(a) = &def.as_account
        && a != "bot"
        && a != "broadcaster"
    {
        return Err(ctx(format!("`as` must be \"bot\" or \"broadcaster\", not `{a}`")));
    }
    let behaviors = def.builtin.is_some() as u8 + def.action.is_some() as u8 + (!def.cmds.is_empty()) as u8;
    if behaviors > 1 {
        return Err(ctx("use only one of `builtin`, `action`, `do`".into()));
    }
    if behaviors == 0 && def.reply.is_none() && def.sub.is_empty() {
        return Err(ctx("needs a `reply`, `do`, `action`, `builtin`, or `sub`".into()));
    }
    Ok(def)
}

/// Parse one `commands/<stem>.toml` table.
pub fn parse_file(stem: &str, path: &str, table: &toml::Table) -> Result<FileDefs, String> {
    let mut out = FileDefs::default();
    for key in table.keys() {
        if key != "command" && key != "timer" {
            return Err(format!("unknown key `{key}` (expected [[command]] / [[timer]])"));
        }
    }
    let arr = |k: &str| -> Result<Vec<toml::Table>, String> {
        match table.get(k) {
            None => Ok(Vec::new()),
            Some(toml::Value::Array(a)) => a.iter().map(|v| v.as_table().cloned().ok_or_else(|| format!("`{k}` entries must be tables"))).collect(),
            Some(_) => Err(format!("`{k}` must be an array of tables ([[{k}]])")),
        }
    };
    for (i, t) in arr("command")?.into_iter().enumerate() {
        let raw: RawCommand = toml::Value::Table(t).try_into().map_err(|e: toml::de::Error| format!("command #{}: {}", i + 1, e.message()))?;
        if raw.name.trim().is_empty() {
            return Err(format!("command #{} has no `name`", i + 1));
        }
        if raw.name.trim().contains(char::is_whitespace) {
            return Err(format!("command `{}`: name must be one word", raw.name.trim()));
        }
        out.commands.push(command(raw, path, i, Role::Everyone)?);
    }
    for (i, t) in arr("timer")?.into_iter().enumerate() {
        let raw: RawTimer = toml::Value::Table(t).try_into().map_err(|e: toml::de::Error| format!("timer #{}: {}", i + 1, e.message()))?;
        let name = raw.name.clone().unwrap_or_else(|| format!("{stem}#{}", i + 1));
        let every = raw.every.ok_or_else(|| format!("timer `{name}` needs `every`"))?.ms();
        if every < 60_000 {
            return Err(format!("timer `{name}`: `every` must be at least 1m (chat spam)"));
        }
        if raw.reply.is_none() && raw.cmds.is_empty() {
            return Err(format!("timer `{name}` needs a `reply` or `do`"));
        }
        for c in &raw.cmds {
            check_line(c).map_err(|e| format!("timer `{name}`: {e}"))?;
        }
        out.timers.push(TimerDef {
            every_ms: every,
            offset_ms: raw.offset.map(Dur::ms).unwrap_or(every),
            min_chat_lines: raw.min_chat_lines,
            modes: raw.modes.unwrap_or_else(|| vec!["live".into()]),
            reply: raw.reply,
            cmds: raw.cmds,
            enabled: raw.enabled.unwrap_or(true),
            file: path.to_string(),
            index: i,
            name,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(src: &str) -> Result<FileDefs, String> {
        parse_file("t", "commands/t.toml", &src.parse::<toml::Table>().unwrap())
    }

    #[test]
    fn parses_commands_timers_and_subs() {
        let f = parse(
            r#"
[[command]]
name = "!Discord"
aliases = ["!DC"]
reply = "join"
cooldown = { global = "30s", per_user = "2m" }

[[command]]
name = "!queue"
role = "follower"
reply = "{queue.url}"
sub.open = { role = "mod", action = "queue.open" }
sub.list = { reply = "list" }

[[timer]]
every = "20m"
min_chat_lines = 10
reply = "hi"
"#,
        )
        .unwrap();
        let d = &f.commands[0];
        assert_eq!(d.name, "!discord");
        assert_eq!(d.aliases, vec!["!dc"]);
        assert_eq!(d.cooldown.global_ms, Some(30_000));
        assert_eq!(d.cooldown.per_user_ms, Some(120_000));
        let q = &f.commands[1];
        assert_eq!(q.sub["open"].role, Role::Mod);
        assert_eq!(q.sub["list"].role, Role::Follower, "subcommands inherit the parent's role");
        assert_eq!(q.sub["open"].name, "!queue open");
        let t = &f.timers[0];
        assert_eq!((t.every_ms, t.min_chat_lines, t.name.as_str()), (1_200_000, 10, "t#1"));
        assert_eq!(t.modes, vec!["live"]);
    }

    #[test]
    fn rejects_bad_definitions() {
        assert!(parse("[[command]]\nname = \"!x\"").unwrap_err().contains("needs a `reply`"));
        assert!(parse("[[command]]\nname = \"!x\"\nreply = \"a\"\nrole = \"king\"").unwrap_err().contains("unknown role"));
        assert!(parse("[[command]]\nname = \"!x\"\ndo = [\"wait 1s\"]").unwrap_err().contains("preset"));
        assert!(parse("[[command]]\nname = \"!x\"\nreply = \"a\"\nrepyl = \"typo\"").is_err());
        assert!(parse("[[command]]\nname = \"!x y\"\nreply = \"a\"").is_err());
        assert!(parse("[[command]]\nname = \"!x\"\naction = \"a.b\"\ndo = [\"clean\"]").unwrap_err().contains("only one"));
        assert!(parse("[[timer]]\nevery = \"10s\"\nreply = \"spam\"").unwrap_err().contains("1m"));
        assert!(parse("other = 1").is_err());
    }

    #[test]
    fn counter_names() {
        let f = parse("[[command]]\nname = \"!death\"\nreply = \"Deaths: {count}\"\n[[command]]\nname = \"!x\"\nreply = \"no count\"").unwrap();
        assert_eq!(f.commands[0].counter_name().as_deref(), Some("death"));
        assert_eq!(f.commands[1].counter_name(), None);
    }

    #[test]
    fn settings_validate_custom_file() {
        let v: toml::Value = toml::from_str::<toml::Table>("custom_file = \"../evil.toml\"").map(toml::Value::Table).unwrap();
        assert!(Settings::from_section(Some(&v)).is_err());
        assert_eq!(Settings::from_section(None).unwrap().custom_file, "commands/custom.toml");
    }
}
