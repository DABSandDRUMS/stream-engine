//! The chatbot itself: command dispatch (role gate → cooldown → usage → behavior → reply),
//! built-ins (mod edits, quotes, counters), timers, and write-back of edits.
//!
//! Pure with respect to the engine: it reads state through [`Env`] and returns [`Out`]
//! effects, which the driver in `lib.rs` turns into hub commands. The DB (counters, quotes)
//! and project files are written directly.

use crate::config::{Builtin, CommandDef, FileDefs, Settings, TimerDef, parse_file};
use crate::edit::{self, Seg};
use crate::store::{Quote, Store};
use crate::template::{self, format_duration};
use crate::text;
use anyhow::{Result, anyhow, bail};
use se_core::policy::{self, Cooldowns, FilterCfg};
use se_core::rng::Rng;
use se_proto::{Actor, Command, Id, Op, Origin, PRIORITY_PRESET, Role, Value};
use se_store::Project;
use std::collections::{BTreeMap, HashMap, VecDeque};

/// A chat line addressed to the bot.
#[derive(Clone, Debug)]
pub struct ChatLine {
    pub text: String,
    pub message_id: Option<String>,
    pub actor: Actor,
    pub event_id: Option<Id>,
}

/// Effects for the driver.
#[derive(Clone, Debug, PartialEq)]
pub enum Out {
    /// Send a chat message (already sanitized).
    Say {
        text: String,
        reply_to: Option<String>,
        as_account: Option<String>,
        cause: Option<Id>,
        source: String,
    },
    /// Submit a command to the core.
    Command(Command),
    /// Counter changed (`None` = deleted): publish `bot.counter.<name>`.
    Counter {
        name: String,
        value: Option<i64>,
    },
    Log {
        level: &'static str,
        msg: String,
    },
}

/// Engine state the bot reads.
pub trait Env {
    fn state(&self, address: &str) -> Option<Value>;
    fn mode(&self) -> String;
    fn now_unix(&self) -> i64;
}

struct TimerRt {
    def: TimerDef,
    last_ms: Option<u64>,
    lines: u32,
    scope_since: Option<u64>,
}

pub struct Bot {
    pub settings: Settings,
    files: BTreeMap<String, FileDefs>,
    errors: BTreeMap<String, String>,
    commands: Vec<CommandDef>,
    lookup: HashMap<String, usize>,
    timers: Vec<TimerRt>,
    cooldowns: Cooldowns,
    filter: FilterCfg,
    store: Store,
    project: Option<Project>,
    rng: Rng,
    sent: VecDeque<(u64, String)>,
    live_since: Option<i64>,
}

/// Values rendered into replies.
#[derive(Default)]
struct Vars<'a> {
    user: &'a str,
    args: &'a [String],
    count: Option<i64>,
    extra: Vec<(&'static str, String)>,
}

/// Render floats without a trailing `.0`, strings raw.
pub fn fmt_value(v: &Value) -> String {
    match v {
        Value::Float(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", *f as i64),
        Value::Float(f) => {
            let s = format!("{f:.2}");
            s.trim_end_matches('0').trim_end_matches('.').to_string()
        }
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn is_positional(tpl: &str) -> bool {
    tpl.len() == 3 && tpl.starts_with('{') && tpl.ends_with('}') && tpl.as_bytes()[1].is_ascii_digit()
}

fn valid_new_name(name: &str, prefix: char) -> bool {
    let mut cs = name.chars();
    cs.next() == Some(prefix) && (2..=31).contains(&name.chars().count()) && cs.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Bot {
    pub fn new(store: Store, project: Option<Project>, seed: u64) -> Bot {
        Bot {
            settings: Settings::default(),
            files: BTreeMap::new(),
            errors: BTreeMap::new(),
            commands: Vec::new(),
            lookup: HashMap::new(),
            timers: Vec::new(),
            cooldowns: Cooldowns::default(),
            filter: FilterCfg::default(),
            store,
            project,
            rng: Rng::new(seed),
            sent: VecDeque::new(),
            live_since: None,
        }
    }

    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Apply a (re)loaded project config. Files that fail validation keep their last good
    /// version; returns `(file, error)` for each failure.
    pub fn apply_config(&mut self, cfg: &se_core::Config) -> Vec<(String, String)> {
        let mut errs = Vec::new();
        match Settings::from_section(cfg.project.extra.get("bot")) {
            Ok(s) => self.settings = s,
            Err(e) => errs.push(("project.toml".to_string(), e)),
        }
        self.filter = FilterCfg::from_config(cfg);
        let mut next = BTreeMap::new();
        if let Some(files) = cfg.other.get("commands") {
            for (stem, table) in files {
                let path = cfg.files.get(&format!("commands/{stem}")).cloned().unwrap_or_else(|| format!("commands/{stem}.toml"));
                match parse_file(stem, &path, table) {
                    Ok(defs) => {
                        next.insert(path, defs);
                    }
                    Err(e) => {
                        if let Some(old) = self.files.get(&path) {
                            next.insert(path.clone(), old.clone());
                        }
                        errs.push((path, e));
                    }
                }
            }
        }
        self.files = next;
        self.errors = errs.iter().cloned().collect();
        errs.extend(self.rebuild());
        errs
    }

    /// Recompute the flat command table and timers from `files`.
    fn rebuild(&mut self) -> Vec<(String, String)> {
        let mut errs = Vec::new();
        self.commands.clear();
        self.lookup.clear();
        for defs in self.files.values() {
            for c in &defs.commands {
                let idx = self.commands.len();
                let mut clash = None;
                for w in std::iter::once(&c.name).chain(&c.aliases) {
                    if let Some(&j) = self.lookup.get(w) {
                        clash = Some((w.clone(), self.commands[j].file.clone()));
                        break;
                    }
                }
                if let Some((w, other)) = clash {
                    errs.push((c.file.clone(), format!("`{w}` is already defined in {other}; ignoring this one")));
                    continue;
                }
                for w in std::iter::once(&c.name).chain(&c.aliases) {
                    self.lookup.insert(w.clone(), idx);
                }
                self.commands.push(c.clone());
            }
        }
        let mut old: HashMap<String, TimerRt> = self.timers.drain(..).map(|t| (t.def.name.clone(), t)).collect();
        for defs in self.files.values() {
            for t in &defs.timers {
                let rt = match old.remove(&t.name) {
                    Some(mut rt) => {
                        rt.def = t.clone();
                        rt
                    }
                    None => TimerRt { def: t.clone(), last_ms: None, lines: 0, scope_since: None },
                };
                self.timers.push(rt);
            }
        }
        for (f, e) in &errs {
            self.errors.insert(f.clone(), e.clone());
        }
        errs
    }

    pub fn commands(&self) -> &[CommandDef] {
        &self.commands
    }

    pub fn errors(&self) -> &BTreeMap<String, String> {
        &self.errors
    }

    /// Track the stream's start for `{uptime}` when Twitch doesn't report it.
    pub fn on_mode(&mut self, mode: &str, now_unix: i64) {
        if mode == "live" {
            self.live_since.get_or_insert(now_unix);
        } else {
            self.live_since = None;
        }
    }

    fn uptime(&self, env: &dyn Env) -> Option<i64> {
        let live = env.state("twitch.stream.live").is_some_and(|v| v.truthy());
        let started = env.state("twitch.stream.started_at").and_then(|v| v.as_i64()).unwrap_or(0);
        if live && started > 0 {
            return Some(env.now_unix() - started);
        }
        self.live_since.map(|s| env.now_unix() - s)
    }

    // ---- echo guard ---------------------------------------------------------------------

    fn is_echo(&mut self, key: &str, now_ms: u64) -> bool {
        let win = self.settings.echo_window.ms();
        while self.sent.front().is_some_and(|(t, _)| now_ms.saturating_sub(*t) > win) {
            self.sent.pop_front();
        }
        self.sent.iter().any(|(_, s)| s == key)
    }

    /// Sanitize an outgoing message and remember it for echo detection. `None` = nothing to send.
    pub fn outgoing(&mut self, raw: &str, now_ms: u64) -> Option<String> {
        let t = text::sanitize_chat(raw, self.settings.max_reply);
        if t.trim_start_matches(text::ZWSP).is_empty() {
            return None;
        }
        self.sent.push_back((now_ms, text::echo_key(&t)));
        if self.sent.len() > 64 {
            self.sent.pop_front();
        }
        Some(t)
    }

    // ---- templating ---------------------------------------------------------------------

    /// Render a template. Returns `Err(reason)` when a viewer-supplied value fails the content filter.
    fn render(&mut self, tpl: &str, vars: &Vars<'_>, env: &dyn Env) -> Result<String, String> {
        let mut blocked: Option<String> = None;
        let uptime = self.uptime(env);
        let filter = &self.filter;
        let mut check = |s: String| -> String {
            if s.is_empty() {
                return s;
            }
            match policy::filter_text(&s, filter) {
                Ok(clean) => clean,
                Err(reason) => {
                    blocked.get_or_insert_with(|| reason.to_string());
                    String::new()
                }
            }
        };
        let mut lookup = |name: &str| -> Option<String> {
            if let Some((_, v)) = vars.extra.iter().find(|(k, _)| *k == name) {
                return Some(v.clone());
            }
            match name {
                "user" => Some(vars.user.to_string()),
                "args" => Some(check(vars.args.join(" "))),
                "touser" => Some(match vars.args.first() {
                    Some(a) => check(a.trim_start_matches('@').to_string()),
                    None => vars.user.to_string(),
                }),
                "count" => vars.count.map(|c| c.to_string()),
                "uptime" => Some(uptime.map(format_duration).unwrap_or_else(|| "offline".into())),
                "song" => Some(env.state("queue.now.title").map(|v| fmt_value(&v)).filter(|s| !s.is_empty()).unwrap_or_else(|| "nothing playing".into())),
                n if n.len() == 1 && n.as_bytes()[0].is_ascii_digit() => {
                    let i = (n.as_bytes()[0] - b'0') as usize;
                    Some(if i == 0 { String::new() } else { check(vars.args.get(i - 1).cloned().unwrap_or_default()) })
                }
                n if n.contains('.') => env.state(n).map(|v| fmt_value(&v)),
                _ => None,
            }
        };
        let out = template::render(tpl, &mut lookup, &mut self.rng);
        match blocked {
            Some(r) => Err(r),
            None => Ok(out),
        }
    }

    fn say(&mut self, tpl: &str, def: &CommandDef, line: &ChatLine, vars: &Vars<'_>, env: &dyn Env, now_ms: u64, out: &mut Vec<Out>) {
        match self.render(tpl, vars, env) {
            Ok(t) => {
                if let Some(text) = self.outgoing(&t, now_ms) {
                    out.push(Out::Say {
                        text,
                        reply_to: if def.thread { line.message_id.clone() } else { None },
                        as_account: def.as_account.clone(),
                        cause: line.event_id,
                        source: def.name.clone(),
                    });
                }
            }
            Err(reason) => {
                out.push(Out::Log { level: "info", msg: format!("{}: reply to {} withheld by the content filter ({reason})", def.name, line.actor.name) })
            }
        }
    }

    fn chat_command(line: &ChatLine, op: Op) -> Command {
        let prio = (line.actor.top_role() >= Role::Owner).then_some(PRIORITY_PRESET);
        Command::new(Origin::Chat, op).with_actor(Some(line.actor.clone())).caused_by(line.event_id).with_priority(prio)
    }

    // ---- chat ---------------------------------------------------------------------------

    /// Handle one chat line.
    pub fn handle_chat(&mut self, line: &ChatLine, env: &dyn Env, now_ms: u64) -> Vec<Out> {
        let mut out = Vec::new();
        if self.is_echo(&text::echo_key(&line.text), now_ms) {
            return out;
        }
        for t in &mut self.timers {
            t.lines = t.lines.saturating_add(1);
        }
        let mut words = line.text.split_whitespace();
        let Some(first) = words.next() else { return out };
        let Some(&ci) = self.lookup.get(&first.to_lowercase()) else { return out };
        let mut args: Vec<String> = words.map(String::from).collect();
        let mut def = self.commands[ci].clone();
        if let Some(a0) = args.first().map(|a| a.to_lowercase())
            && let Some(sd) = def.sub.get(&a0)
        {
            def = sd.clone();
            args.remove(0);
        }
        self.run(&def, &args, line, env, now_ms, &mut out);
        out
    }

    fn run(&mut self, def: &CommandDef, args: &[String], line: &ChatLine, env: &dyn Env, now_ms: u64, out: &mut Vec<Out>) {
        if !def.enabled || (!def.modes.is_empty() && !def.modes.contains(&env.mode())) {
            return;
        }
        let user = line.actor.name.as_str();
        if !policy::role_allows(Some(&line.actor), def.role) {
            if let Some(d) = def.deny_reply.clone() {
                self.say(&d, def, line, &Vars { user, args, ..Default::default() }, env, now_ms, out);
            }
            return;
        }
        let bypass = self.settings.mods_bypass_cooldowns && line.actor.top_role() >= Role::Mod;
        if !bypass && let Err(remaining) = self.cooldowns.check(&def.name, &line.actor.id, &def.cooldown, now_ms) {
            out.push(Out::Log { level: "debug", msg: format!("{} on cooldown for {user} ({remaining} ms)", def.name) });
            return;
        }
        if args.len() < def.min_args {
            if let Some(u) = def.usage.clone() {
                self.say(&u, def, line, &Vars { user, args, ..Default::default() }, env, now_ms, out);
            }
            return;
        }
        self.cooldowns.commit(&def.name, &line.actor.id, now_ms);
        let count = match def.counter_name() {
            Some(c) => match self.store.counter_add(&c, 1) {
                Ok(v) => {
                    out.push(Out::Counter { name: c, value: Some(v) });
                    Some(v)
                }
                Err(e) => {
                    out.push(Out::Log { level: "error", msg: format!("counter {c}: {e:#}") });
                    None
                }
            },
            None => None,
        };
        let mut vars = Vars { user, args, count, extra: Vec::new() };
        if let Some(b) = def.builtin {
            match self.builtin(b, def, args, line, &mut vars, env, out) {
                Ok(true) => {}
                Ok(false) => return,
                Err(msg) => {
                    let m = format!("@{user} {msg}");
                    if let Some(text) = self.outgoing(&m, now_ms) {
                        out.push(Out::Say { text, reply_to: None, as_account: def.as_account.clone(), cause: line.event_id, source: def.name.clone() });
                    }
                    return;
                }
            }
        } else if let Some(action) = &def.action {
            let mut m = BTreeMap::new();
            for (k, v) in &def.args {
                let rv = match v {
                    Value::Str(s) => match self.render(s, &vars, env) {
                        Ok(r) if r.is_empty() => continue,
                        Ok(r) if is_positional(s) => r.trim_start_matches('#').parse::<i64>().map(Value::Int).unwrap_or(Value::Str(r)),
                        Ok(r) => Value::Str(r),
                        Err(reason) => {
                            out.push(Out::Log { level: "info", msg: format!("{}: {user}'s input withheld by the content filter ({reason})", def.name) });
                            return;
                        }
                    },
                    other => other.clone(),
                };
                m.insert(k.clone(), rv);
            }
            out.push(Out::Command(Self::chat_command(line, Op::Action { name: action.clone(), args: Value::Map(m) })));
        } else if !def.cmds.is_empty() {
            for c in &def.cmds {
                let toks = match se_proto::command::tokenize(c) {
                    Ok(t) => t,
                    Err(e) => {
                        out.push(Out::Log { level: "error", msg: format!("{}: `{c}`: {e}", def.name) });
                        continue;
                    }
                };
                let mut rendered = Vec::with_capacity(toks.len());
                for t in &toks {
                    match self.render(t, &vars, env) {
                        Ok(r) => rendered.push(r),
                        Err(reason) => {
                            out.push(Out::Log { level: "info", msg: format!("{}: {user}'s input withheld by the content filter ({reason})", def.name) });
                            return;
                        }
                    }
                }
                match Op::from_tokens(&rendered) {
                    Ok(op) => out.push(Out::Command(Self::chat_command(line, op))),
                    Err(e) => out.push(Out::Log { level: "warn", msg: format!("{}: `{c}`: {e}", def.name) }),
                }
            }
        }
        if let Some(r) = def.reply.clone().or_else(|| def.builtin.map(|b| b.default_reply().to_string())) {
            self.say(&r, def, line, &vars, env, now_ms, out);
        }
    }

    /// Run a built-in. `Ok(true)` = continue to the reply, `Ok(false)` = done (already replied),
    /// `Err(msg)` = tell the caller.
    #[allow(clippy::too_many_arguments)]
    fn builtin(
        &mut self,
        b: Builtin,
        def: &CommandDef,
        args: &[String],
        line: &ChatLine,
        vars: &mut Vars<'_>,
        env: &dyn Env,
        out: &mut Vec<Out>,
    ) -> Result<bool, String> {
        let user = line.actor.name.clone();
        let prefix = def.name.chars().next().unwrap_or('!');
        match b {
            Builtin::Addcom | Builtin::Editcom => {
                let name = args.first().map(|a| a.to_lowercase()).ok_or("which command?")?;
                let reply: String = args[1..].join(" ").chars().filter(|c| !c.is_control()).collect();
                if reply.trim().is_empty() {
                    return Err(format!("{name} needs reply text"));
                }
                let reply = text::truncate_chars(reply.trim(), self.settings.max_custom_reply);
                vars.extra.push(("command", name.clone()));
                if b == Builtin::Addcom {
                    if self.lookup.contains_key(&name) {
                        return Err(format!("{name} already exists"));
                    }
                    if !valid_new_name(&name, prefix) {
                        return Err(format!("command names look like {prefix}name (letters, digits, _)"));
                    }
                    let fields = BTreeMap::from([("name".to_string(), Value::Str(name.clone())), ("reply".to_string(), Value::Str(reply))]);
                    let file = self.settings.custom_file.clone();
                    self.edit_file(&file, |doc| edit::append_table(doc, &[], "command", &fields, Some(&format!("added from chat by {user}"))).map(|_| ()))
                        .map_err(|e| format!("could not save {name}: {e:#}"))?;
                } else {
                    let target = self.lookup.get(&name).map(|&i| self.commands[i].clone()).ok_or_else(|| format!("{name} doesn't exist"))?;
                    if !target.is_simple() {
                        return Err(format!("{name} isn't a plain reply command; edit it in the UI"));
                    }
                    let fields = BTreeMap::from([("reply".to_string(), Value::Str(reply))]);
                    self.edit_file(&target.file, |doc| edit::set_fields(doc, &[Seg::key("command"), Seg::Index(target.index)], &fields))
                        .map_err(|e| format!("could not save {name}: {e:#}"))?;
                }
                out.push(Out::Log { level: "info", msg: format!("{user} {} {name}", if b == Builtin::Addcom { "added" } else { "edited" }) });
                Ok(true)
            }
            Builtin::Delcom => {
                let name = args.first().map(|a| a.to_lowercase()).ok_or("which command?")?;
                let target = self.lookup.get(&name).map(|&i| self.commands[i].clone()).ok_or_else(|| format!("{name} doesn't exist"))?;
                if !target.is_simple() {
                    return Err(format!("{name} isn't a plain reply command; remove it in the UI"));
                }
                vars.extra.push(("command", target.name.clone()));
                self.edit_file(&target.file, |doc| edit::remove_table(doc, &[], "command", target.index))
                    .map_err(|e| format!("could not delete {name}: {e:#}"))?;
                out.push(Out::Log { level: "info", msg: format!("{user} deleted {name}") });
                Ok(true)
            }
            Builtin::Quote => {
                let q = match args.first() {
                    None => self.store.quote_pick(&[], &mut self.rng),
                    Some(a) => match a.trim_start_matches('#').parse::<i64>() {
                        Ok(id) => self.store.quote_get(id),
                        Err(_) => self.store.quote_pick(args, &mut self.rng),
                    },
                }
                .map_err(|e| format!("quotes unavailable: {e:#}"))?;
                let q = q.ok_or("no quote found")?;
                quote_vars(vars, &q);
                Ok(true)
            }
            Builtin::Addquote => {
                let t: String = args.join(" ").chars().filter(|c| !c.is_control()).collect();
                let t = text::truncate_chars(t.trim(), self.settings.max_custom_reply);
                if t.is_empty() {
                    return Err("quote text?".into());
                }
                let id = self.store.quote_add(&t, &user).map_err(|e| format!("could not save: {e:#}"))?;
                let q = self.store.quote_get(id).ok().flatten().ok_or("could not save")?;
                quote_vars(vars, &q);
                Ok(true)
            }
            Builtin::Delquote => {
                let id: i64 = args.first().and_then(|a| a.trim_start_matches('#').parse().ok()).ok_or("which quote number?")?;
                if !self.store.quote_delete(id).map_err(|e| format!("{e:#}"))? {
                    return Err(format!("no quote #{id}"));
                }
                vars.extra.push(("quote_id", id.to_string()));
                Ok(true)
            }
            Builtin::Setcounter => {
                let name = args.first().map(|a| text::segment(a)).filter(|s| !s.is_empty()).ok_or("which counter?")?;
                let raw = args.get(1).ok_or("value? (e.g. 5, +1, -1)")?;
                let n: i64 = raw.trim_start_matches('+').parse().map_err(|_| format!("`{raw}` is not a number"))?;
                let value = if raw.starts_with(['+', '-']) {
                    self.store.counter_add(&name, n).map_err(|e| format!("{e:#}"))?
                } else {
                    self.store.counter_set(&name, n).map_err(|e| format!("{e:#}"))?;
                    n
                };
                out.push(Out::Counter { name: name.clone(), value: Some(value) });
                vars.extra.push(("counter", name));
                vars.extra.push(("value", value.to_string()));
                Ok(true)
            }
            Builtin::Commands => {
                let mut names: Vec<&str> = self
                    .commands
                    .iter()
                    .filter(|c| c.enabled && policy::role_allows(Some(&line.actor), c.role) && (c.modes.is_empty() || c.modes.contains(&env.mode())))
                    .map(|c| c.name.as_str())
                    .collect();
                names.sort_unstable();
                vars.extra.push(("commands", names.join(" ")));
                Ok(true)
            }
        }
    }

    // ---- timers -------------------------------------------------------------------------

    /// Fire due timers (call about once a second).
    pub fn tick(&mut self, env: &dyn Env, now_ms: u64) -> Vec<Out> {
        let mode = env.mode();
        let mut due = Vec::new();
        for (i, t) in self.timers.iter_mut().enumerate() {
            if !t.def.enabled || !(t.def.modes.is_empty() || t.def.modes.contains(&mode)) {
                t.scope_since = None;
                continue;
            }
            let since = *t.scope_since.get_or_insert(now_ms);
            let next = match t.last_ms {
                Some(l) if l >= since => l + t.def.every_ms,
                _ => since + t.def.offset_ms,
            };
            if now_ms >= next && t.lines >= t.def.min_chat_lines {
                t.last_ms = Some(now_ms);
                t.lines = 0;
                due.push(i);
            }
        }
        let mut out = Vec::new();
        for i in due {
            let def = self.timers[i].def.clone();
            let vars = Vars::default();
            if let Some(r) = &def.reply {
                match self.render(r, &vars, env) {
                    Ok(t) => {
                        if let Some(text) = self.outgoing(&t, now_ms) {
                            out.push(Out::Say { text, reply_to: None, as_account: None, cause: None, source: format!("timer {}", def.name) });
                        }
                    }
                    Err(reason) => out.push(Out::Log { level: "warn", msg: format!("timer {}: withheld ({reason})", def.name) }),
                }
            }
            for c in &def.cmds {
                let toks = se_proto::command::tokenize(c).unwrap_or_default();
                let rendered: Vec<String> = toks.iter().map(|t| self.render(t, &vars, env).unwrap_or_default()).collect();
                match Op::from_tokens(&rendered) {
                    Ok(op) => out.push(Out::Command(Command::new(Origin::Rule, op))),
                    Err(e) => out.push(Out::Log { level: "warn", msg: format!("timer {}: `{c}`: {e}", def.name) }),
                }
            }
        }
        out
    }

    // ---- file edits ---------------------------------------------------------------------

    fn check_path(path: &str) -> Result<()> {
        if !path.starts_with("commands/") || !path.ends_with(".toml") || path.contains("..") || path.matches('/').count() != 1 {
            bail!("commands live in commands/<name>.toml");
        }
        Ok(())
    }

    /// Edit a commands file (comment-preserving), validating the result before it's written;
    /// the in-memory commands update at once (the watcher reload then agrees).
    fn edit_file(&mut self, path: &str, f: impl FnOnce(&mut toml_edit::DocumentMut) -> Result<()>) -> Result<()> {
        Self::check_path(path)?;
        let project = self.project.as_ref().ok_or_else(|| anyhow!("no project to write to"))?;
        let stem = path.trim_start_matches("commands/").trim_end_matches(".toml").to_string();
        let mut parsed = None;
        let files = &self.files;
        project.edit(path, |doc| {
            f(doc)?;
            let table: toml::Table = doc.to_string().parse()?;
            let defs = parse_file(&stem, path, &table).map_err(|e| anyhow!(e))?;
            for c in &defs.commands {
                for w in std::iter::once(&c.name).chain(&c.aliases) {
                    if let Some(other) =
                        files.iter().filter(|(p, _)| p.as_str() != path).find(|(_, d)| d.commands.iter().any(|o| &o.name == w || o.aliases.contains(w)))
                    {
                        bail!("`{w}` already exists in {}", other.0);
                    }
                }
            }
            parsed = Some(defs);
            Ok(())
        })?;
        if let Some(defs) = parsed {
            self.files.insert(path.to_string(), defs);
            self.errors.remove(path);
            self.rebuild();
        }
        Ok(())
    }

    fn fields_arg(fields: &Value) -> Result<BTreeMap<String, Value>> {
        let m = fields.as_map().ok_or_else(|| anyhow!("`fields` must be a map"))?;
        for k in m.keys() {
            if k.is_empty() || k.contains(['.', '[', ']', '"']) {
                bail!("bad field name `{k}`");
            }
        }
        Ok(m.clone())
    }

    /// Create (`index` = None) or update a `[[command]]`/`[[timer]]` entry.
    pub fn save_entry(&mut self, kind: &str, file: Option<&str>, index: Option<usize>, fields: &Value) -> Result<String> {
        if kind != "command" && kind != "timer" {
            bail!("kind must be command or timer");
        }
        let fields = Self::fields_arg(fields)?;
        let file = file.map(String::from).unwrap_or_else(|| self.settings.custom_file.clone());
        match index {
            Some(i) => self.edit_file(&file, |doc| edit::set_fields(doc, &[Seg::key(kind), Seg::Index(i)], &fields))?,
            None => self.edit_file(&file, |doc| edit::append_table(doc, &[], kind, &fields, None).map(|_| ()))?,
        }
        Ok(file)
    }

    pub fn delete_entry(&mut self, kind: &str, file: &str, index: usize) -> Result<()> {
        if kind != "command" && kind != "timer" {
            bail!("kind must be command or timer");
        }
        self.edit_file(file, |doc| edit::remove_table(doc, &[], kind, index))
    }

    // ---- queries ------------------------------------------------------------------------

    fn command_value(c: &CommandDef) -> Value {
        let behavior = match (&c.builtin, &c.action, c.cmds.is_empty()) {
            (Some(b), _, _) => format!("builtin {}", b.as_str()),
            (_, Some(a), _) => format!("action {a}"),
            (_, _, false) => "do".to_string(),
            _ => "reply".to_string(),
        };
        Value::map()
            .with("name", c.name.clone())
            .with("aliases", c.aliases.clone())
            .with("description", c.description.clone())
            .with("reply", c.reply.clone().map(Value::Str).unwrap_or_default())
            .with("role", format!("{:?}", c.role).to_lowercase())
            .with("cooldown_global_ms", c.cooldown.global_ms.map(|v| Value::Int(v as i64)).unwrap_or_default())
            .with("cooldown_per_user_ms", c.cooldown.per_user_ms.map(|v| Value::Int(v as i64)).unwrap_or_default())
            .with("do", c.cmds.clone())
            .with("action", c.action.clone().map(Value::Str).unwrap_or_default())
            .with("args", Value::Map(c.args.clone()))
            .with("builtin", c.builtin.map(|b| Value::Str(b.as_str().into())).unwrap_or_default())
            .with("modes", c.modes.clone())
            .with("enabled", c.enabled)
            .with("usage", c.usage.clone().map(Value::Str).unwrap_or_default())
            .with("min_args", c.min_args)
            .with("counter", c.counter_name().map(Value::Str).unwrap_or_default())
            .with("sub", c.sub.keys().cloned().collect::<Vec<_>>())
            .with("behavior", behavior)
            .with("simple", c.is_simple())
            .with("file", c.file.clone())
            .with("index", c.index)
    }

    /// Query `bot.commands`: every command, in file order.
    pub fn query_commands(&self) -> Value {
        Value::List(self.files.values().flat_map(|d| d.commands.iter().map(Self::command_value)).collect())
    }

    /// Query `bot.timers`.
    pub fn query_timers(&self, now_ms: u64) -> Value {
        Value::List(
            self.timers
                .iter()
                .map(|t| {
                    let next = match (t.scope_since, t.last_ms) {
                        (Some(s), Some(l)) if l >= s => Some(l + t.def.every_ms),
                        (Some(s), _) => Some(s + t.def.offset_ms),
                        _ => None,
                    };
                    Value::map()
                        .with("name", t.def.name.clone())
                        .with("every_ms", t.def.every_ms as i64)
                        .with("min_chat_lines", t.def.min_chat_lines as i64)
                        .with("modes", t.def.modes.clone())
                        .with("reply", t.def.reply.clone().map(Value::Str).unwrap_or_default())
                        .with("do", t.def.cmds.clone())
                        .with("enabled", t.def.enabled)
                        .with("lines", t.lines as i64)
                        .with("next_in_ms", next.map(|n| Value::Int(n.saturating_sub(now_ms) as i64)).unwrap_or_default())
                        .with("file", t.def.file.clone())
                        .with("index", t.def.index)
                })
                .collect(),
        )
    }

    /// Query `bot.files`: command files (for "save to …" pickers) and their errors.
    pub fn query_files(&self) -> Value {
        let mut names: Vec<String> = self.files.keys().cloned().collect();
        if !names.contains(&self.settings.custom_file) {
            names.push(self.settings.custom_file.clone());
        }
        Value::map()
            .with("files", names)
            .with("custom_file", self.settings.custom_file.clone())
            .with("errors", Value::List(self.errors.iter().map(|(f, e)| Value::map().with("file", f.clone()).with("msg", e.clone())).collect()))
    }
}

fn quote_vars(vars: &mut Vars<'_>, q: &Quote) {
    vars.extra.push(("quote_id", q.id.to_string()));
    vars.extra.push(("quote", q.text.clone()));
    vars.extra.push(("quote_by", q.added_by.clone()));
    vars.extra.push(("quote_date", q.date()));
}

#[cfg(test)]
mod tests;
