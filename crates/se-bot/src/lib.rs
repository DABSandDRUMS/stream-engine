//! Chatbot (§14.3): commands from `commands/*.toml` (templated replies, role gates and
//! cooldowns through the core policy, commands that run engine commands or subsystem
//! actions), mod edits from chat (`!addcom/!editcom/!delcom`, written back comment-preserving),
//! timers, counters and quotes in the DB, and the `bot.*` action router (`bot.say` →
//! `twitch.chat.send`).
//!
//! State: `bot.counter.<name>` (int). Events: `bot.said {text, source, reply_to}`.
//! Queries: `bot.commands`, `bot.timers`, `bot.counters`, `bot.quotes`, `bot.files`.
//! Actions: `bot.say {args:[text]} | {text, reply_to?, as?}`, and (UI/CLI only)
//! `bot.command.save|delete`, `bot.timer.save|delete` (`{file?, index?, fields}`),
//! `bot.counter.set|delete {name, value?}`, `bot.quote.add|edit|delete {id?, text?}`.

pub mod bot;
pub mod config;
pub mod edit;
pub mod store;
pub mod template;
pub mod text;

use bot::{Bot, ChatLine, Env, Out};
use parking_lot::Mutex;
use se_hub::{Bus, EngineCtx, Hub, Snapshot};
use se_proto::{Actor, Command, Event, Meta, Op, Origin, PRIORITY_CHAT, Value, ValueType};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use store::Store;

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn now_ms() -> u64 {
    se_clock::now() / 1_000_000
}

/// [`Env`] over the lock-free state snapshot.
pub struct SnapEnv(pub Arc<Snapshot>);

impl Env for SnapEnv {
    fn state(&self, address: &str) -> Option<Value> {
        self.0.get(address).cloned()
    }
    fn mode(&self) -> String {
        self.0.str("show.mode").unwrap_or("offline").to_string()
    }
    fn now_unix(&self) -> i64 {
        unix_now()
    }
}

/// Normalize a `twitch.chat` (or any `*.chat`) event into a [`ChatLine`]; `None` for the bot's
/// own messages.
pub fn chat_line(e: &Event) -> Option<ChatLine> {
    // our own bot account's messages (the adapter marks them) never trigger commands
    if e.payload.get_path("bot").is_some_and(Value::truthy) {
        return None;
    }
    let text = e.payload.get_path("message").and_then(Value::as_str)?.to_string();
    let actor = e.actor.clone().unwrap_or_else(|| {
        let name = e.payload.get_path("user").and_then(Value::as_str).unwrap_or("").to_string();
        let id = e.payload.get_path("user_id").and_then(Value::as_str).map(String::from).unwrap_or_else(|| name.clone());
        Actor { platform: e.ty.split('.').next().unwrap_or("").to_string(), id, name, roles: Vec::new() }
    });
    Some(ChatLine { text, message_id: e.payload.get_path("message_id").and_then(Value::as_str).map(String::from), actor, event_id: Some(e.id) })
}

struct Driver {
    hub: Arc<Hub>,
    bot: Arc<Mutex<Bot>>,
    declared: HashSet<String>,
}

impl Driver {
    fn env(&self) -> SnapEnv {
        SnapEnv(self.hub.snapshot.load_full())
    }

    fn counter(&mut self, name: &str, value: Option<i64>) {
        let addr = format!("bot.counter.{name}");
        match value {
            Some(v) => {
                if self.declared.insert(addr.clone()) {
                    let meta = Meta { ty: ValueType::Int, default: Value::Int(0), ..Default::default() }.readonly().owner("bot").describe("Chatbot counter");
                    self.hub.declare(&addr, meta);
                }
                self.hub.publish(&addr, Value::Int(v));
            }
            None => {
                self.declared.remove(&addr);
                self.hub.submit(se_core::Input::Remove { prefix: addr });
            }
        }
    }

    fn apply(&mut self, outs: Vec<Out>) {
        for o in outs {
            match o {
                Out::Say { text, reply_to, as_account, cause, source } => {
                    let mut args = Value::map().with("text", text.clone());
                    if let Some(r) = &reply_to {
                        args = args.with("reply_to", r.clone());
                    }
                    if let Some(a) = as_account {
                        args = args.with("as", a);
                    }
                    self.hub.command(Command::new(Origin::System, Op::Action { name: "twitch.chat.send".into(), args }).caused_by(cause));
                    let payload = Value::map().with("text", text).with("source", source).with("reply_to", reply_to.map(Value::Str).unwrap_or_default());
                    self.hub.emit(Event::new("bot.said", Origin::System, payload).with_causal(cause));
                }
                Out::Command(c) => {
                    self.hub.command(c);
                }
                Out::Counter { name, value } => self.counter(&name, value),
                Out::Log { level, msg } => match level {
                    "debug" => tracing::debug!(target: "bot", "{msg}"),
                    _ => self.hub.log(level, "bot", msg),
                },
            }
        }
    }

    fn publish_health(&self) {
        let b = self.bot.lock();
        let errs = b.errors();
        let (status, detail) = if errs.is_empty() {
            ("pass", format!("{} commands", b.commands().len()))
        } else {
            ("warn", errs.iter().map(|(f, e)| format!("{f}: {e}")).collect::<Vec<_>>().join("; "))
        };
        self.hub.publish("health.bot", Value::map().with("status", status).with("detail", detail));
    }

    fn on_action(&mut self, c: Command) {
        let Op::Action { name, args } = &c.op else { return };
        let str_arg = |k: &str| args.get_path(k).and_then(Value::as_str).map(String::from);
        let int_arg = |k: &str| args.get_path(k).and_then(Value::as_i64);
        if name == "bot.say" {
            let text = match str_arg("text") {
                Some(t) => t,
                None => args.get_path("args").and_then(Value::as_list).map(|l| l.iter().map(bot::fmt_value).collect::<Vec<_>>().join(" ")).unwrap_or_default(),
            };
            let out = self.bot.lock().outgoing(&text, now_ms());
            match out {
                Some(text) => {
                    let o = Out::Say {
                        text,
                        reply_to: str_arg("reply_to"),
                        as_account: str_arg("as").filter(|a| a == "bot" || a == "broadcaster"),
                        cause: Some(c.id),
                        source: format!("bot.say ({})", c.origin.as_str()),
                    };
                    self.apply(vec![o]);
                }
                None => self.hub.log("warn", "bot", "bot.say with empty text"),
            }
            return;
        }
        if c.priority() <= PRIORITY_CHAT || matches!(c.origin, Origin::Chat | Origin::Twitch) {
            self.hub.log("warn", "bot", format!("{name}: chat can't edit the bot directly (use !addcom/!editcom/!delcom)"));
            return;
        }
        let res: anyhow::Result<String> = (|| {
            let mut b = self.bot.lock();
            match name.as_str() {
                "bot.command.save" | "bot.timer.save" => {
                    let kind = if name == "bot.command.save" { "command" } else { "timer" };
                    let fields = args.get_path("fields").cloned().unwrap_or_default();
                    let index = int_arg("index").map(|i| i.max(0) as usize);
                    let file = b.save_entry(kind, str_arg("file").as_deref(), index, &fields)?;
                    Ok(format!("saved {kind} in {file}"))
                }
                "bot.command.delete" | "bot.timer.delete" => {
                    let kind = if name == "bot.command.delete" { "command" } else { "timer" };
                    let file = str_arg("file").ok_or_else(|| anyhow::anyhow!("needs file"))?;
                    let index = int_arg("index").ok_or_else(|| anyhow::anyhow!("needs index"))?.max(0) as usize;
                    b.delete_entry(kind, &file, index)?;
                    Ok(format!("deleted {kind} #{index} from {file}"))
                }
                "bot.counter.set" => {
                    let n = text::segment(&str_arg("name").unwrap_or_default());
                    anyhow::ensure!(!n.is_empty(), "needs a counter name");
                    let v = int_arg("value").unwrap_or(0);
                    b.store().counter_set(&n, v)?;
                    drop(b);
                    self.counter(&n, Some(v));
                    Ok(format!("counter {n} = {v}"))
                }
                "bot.counter.delete" => {
                    let n = text::segment(&str_arg("name").unwrap_or_default());
                    b.store().counter_delete(&n)?;
                    drop(b);
                    self.counter(&n, None);
                    Ok(format!("counter {n} deleted"))
                }
                "bot.quote.add" => {
                    let t = str_arg("text").filter(|t| !t.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("needs text"))?;
                    let id = b.store().quote_add(t.trim(), &str_arg("by").unwrap_or_else(|| c.origin.as_str().to_string()))?;
                    Ok(format!("quote #{id} added"))
                }
                "bot.quote.edit" => {
                    let id = int_arg("id").ok_or_else(|| anyhow::anyhow!("needs id"))?;
                    let t = str_arg("text").filter(|t| !t.trim().is_empty()).ok_or_else(|| anyhow::anyhow!("needs text"))?;
                    anyhow::ensure!(b.store().quote_edit(id, t.trim())?, "no quote #{id}");
                    Ok(format!("quote #{id} updated"))
                }
                "bot.quote.delete" => {
                    let id = int_arg("id").ok_or_else(|| anyhow::anyhow!("needs id"))?;
                    anyhow::ensure!(b.store().quote_delete(id)?, "no quote #{id}");
                    Ok(format!("quote #{id} deleted"))
                }
                other => anyhow::bail!("unknown action `{other}`"),
            }
        })();
        match res {
            Ok(msg) => self.hub.log("info", "bot", msg),
            Err(e) => self.hub.log("error", "bot", format!("{name}: {e:#}")),
        }
        self.publish_health();
    }
}

fn register_queries(hub: &Hub, bot: Arc<Mutex<Bot>>) {
    hub.register_query(
        "bot",
        Arc::new(move |name, _args| {
            let b = bot.lock();
            let r = match name.as_str() {
                "bot.commands" => Ok(b.query_commands()),
                "bot.timers" => Ok(b.query_timers(now_ms())),
                "bot.files" => Ok(b.query_files()),
                "bot.counters" => b
                    .store()
                    .counters()
                    .map(|l| Value::List(l.into_iter().map(|(n, v)| Value::map().with("name", n).with("value", v)).collect()))
                    .map_err(|e| e.to_string()),
                "bot.quotes" => b.store().quotes().map(|l| Value::List(l.iter().map(|q| q.to_value()).collect())).map_err(|e| e.to_string()),
                other => Err(format!("unknown query `{other}`")),
            };
            Box::pin(async move { r })
        }),
    );
}

/// Start the chatbot.
pub async fn start(ctx: EngineCtx) -> anyhow::Result<()> {
    let store = Store::new(ctx.db.clone())?;
    let project = match se_store::Project::open(&ctx.project_root) {
        Ok(p) => Some(p),
        Err(e) => {
            ctx.hub.log("error", "bot", format!("project not writable for chat edits: {e:#}"));
            None
        }
    };
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
    let mut bot = Bot::new(store.clone(), project, seed);
    for (f, e) in bot.apply_config(&ctx.config.borrow()) {
        ctx.hub.log("error", "bot", format!("{f}: {e}"));
    }
    bot.on_mode(ctx.hub.snapshot.load().str("show.mode").unwrap_or("offline"), unix_now());
    let bot = Arc::new(Mutex::new(bot));
    register_queries(&ctx.hub, bot.clone());
    let mut d = Driver { hub: ctx.hub.clone(), bot, declared: HashSet::new() };
    for (n, v) in store.counters()? {
        d.counter(&n, Some(v));
    }
    d.publish_health();
    let mut bus = ctx.hub.subscribe();
    let mut actions = ctx.hub.route_actions("bot");
    let mut cfg = ctx.config.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(1));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                r = bus.recv() => match r {
                    Ok(b) => {
                        let Bus::Event(e) = &*b else { continue };
                        match e.ty.as_str() {
                            "twitch.chat" => {
                                if let Some(line) = chat_line(e) {
                                    let env = d.env();
                                    let outs = d.bot.lock().handle_chat(&line, &env, now_ms());
                                    d.apply(outs);
                                }
                            }
                            "mode.changed" => {
                                let to = e.payload.get_path("to").and_then(Value::as_str).unwrap_or("");
                                d.bot.lock().on_mode(to, unix_now());
                            }
                            "project.reloaded" => d.publish_health(),
                            _ => {}
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => d.hub.log("warn", "bot", format!("bot lagged {n} bus messages")),
                    Err(_) => break,
                },
                Some(c) = actions.recv() => d.on_action(c),
                r = cfg.changed() => {
                    if r.is_err() {
                        break;
                    }
                    let c = cfg.borrow_and_update().clone();
                    let errs = d.bot.lock().apply_config(&c);
                    for (f, e) in errs {
                        d.hub.log("error", "bot", format!("{f}: {e}"));
                    }
                    d.publish_health();
                }
                _ = tick.tick() => {
                    let env = d.env();
                    let outs = d.bot.lock().tick(&env, now_ms());
                    d.apply(outs);
                }
            }
        }
        tracing::info!("bot stopped");
    });
    Ok(())
}
