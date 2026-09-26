//! Alerts, stats, goals, credits, and the chat feed for overlays (§14.1–14.2, §12.1).
//!
//! * **Routing** (`alerts/*.toml`): event → first matching `[[alert]]` → variation by
//!   expression → title/message/sound/TTS/duration/priority.
//! * **Queue**: priorities, minimum spacing, maximum on-screen time, interrupting, veto window
//!   for viewer text not already vetted by the policy (`alerts.veto.<id>` countdown, actions
//!   `alerts.veto|approve`), pause in `ad_break`/`brb` (and on panic) with replay after,
//!   deletion sync (`twitch.chat.delete`, `twitch.user.purge`).
//! * **Gift bombs** combine into one alert.
//! * Overlays are driven by events `alert.show {id,kind,title,message,user,amount,sound,
//!   duration,variation,…}`, `alert.update`, `alert.hide {id, reason}` and state
//!   `alerts.current`; sounds go out as `audio.play {sound}`, speech as `tts.say`.
//! * **Stats** `stats.*`, **goals** `goals.<name>.{current,target,label,progress}` (DB,
//!   persisted across streams; `[goals.*]` in project.toml), **credits** query.
//! * **Chat feed** `chat.message|delete|purge|clear` + query `chat.recent`.

pub mod chat;
pub mod combine;
pub mod config;
pub mod queue;
pub mod route;
pub mod stats;

use anyhow::{Result, anyhow, bail};
use combine::{Combiner, Gift};
use config::{AlertsConfig, GoalDef, QueuePolicy, StatsSettings};
use parking_lot::Mutex;
use queue::{AlertQueue, Effect, HideReason, PauseReason};
use se_bot::edit::{self, Seg};
use se_core::policy::FilterCfg;
use se_core::rng::Rng;
use se_hub::{Bus, EngineCtx, Hub, Snapshot};
use se_proto::{Command, Event, Meta, Op, Origin, PRIORITY_CHAT, Role, Value, ValueType};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

fn now_ms() -> u64 {
    se_clock::now() / 1_000_000
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

struct SnapView(Arc<Snapshot>);

impl route::StateView for SnapView {
    fn get(&self, a: &str) -> Option<Value> {
        self.0.get(a).cloned()
    }
    fn mode(&self) -> String {
        self.0.str("show.mode").unwrap_or("offline").to_string()
    }
}

type ParsedFiles = BTreeMap<String, (Option<QueuePolicy>, Vec<config::AlertDef>)>;

/// Everything the alerts subsystem owns; shared by the event loop and query handlers.
pub struct Alerts {
    hub: Arc<Hub>,
    cfg: AlertsConfig,
    parsed: ParsedFiles,
    errors: Vec<(String, String)>,
    pub queue: AlertQueue,
    combiner: Combiner,
    pub stats: stats::Stats,
    chat: chat::ChatFeed,
    filter: FilterCfg,
    project: Option<se_store::Project>,
    rng: Rng,
    veto_shown: HashMap<u64, u64>,
    /// Last published value per address (queue state is only published on change).
    last: HashMap<String, Value>,
    declared: HashSet<String>,
}

fn declare(hub: &Hub, declared: &mut HashSet<String>, a: &str, meta: Meta) {
    if declared.insert(a.to_string()) {
        hub.declare(a, meta);
    }
}

fn ro(ty: ValueType, default: Value, d: &str) -> Meta {
    Meta { ty, default, ..Default::default() }.readonly().owner("alerts").describe(d)
}

impl Alerts {
    pub fn new(hub: Arc<Hub>, db: se_store::Db, project: Option<se_store::Project>, session: &str) -> Result<Alerts> {
        let stats = stats::Stats::new(db, session, StatsSettings::default())?;
        let policy = QueuePolicy::default();
        Ok(Alerts {
            hub,
            combiner: Combiner::new(policy.gift_window.ms()),
            queue: AlertQueue::new(policy),
            cfg: AlertsConfig::default(),
            parsed: BTreeMap::new(),
            errors: Vec::new(),
            stats,
            chat: chat::ChatFeed::new(100),
            filter: FilterCfg::default(),
            project,
            rng: Rng::new(unix_ms() as u64),
            veto_shown: HashMap::new(),
            last: HashMap::new(),
            declared: HashSet::new(),
        })
    }

    fn log(&self, level: &str, msg: impl Into<String>) {
        self.hub.log(level, "alerts", msg);
    }

    fn publish(&mut self, changes: Vec<(String, Value)>) {
        for (a, v) in changes {
            if !self.declared.contains(&a) {
                let meta = match &v {
                    Value::Int(_) => ro(ValueType::Int, Value::Int(0), ""),
                    Value::Float(_) => ro(ValueType::Float, Value::Float(0.0), ""),
                    Value::Bool(_) => ro(ValueType::Bool, Value::Bool(false), ""),
                    _ => ro(ValueType::String, Value::Str(String::new()), ""),
                };
                let desc = stats::STAT_ADDRS.iter().find(|(x, _)| *x == a).map(|(_, d)| *d).unwrap_or("");
                declare(&self.hub, &mut self.declared, &a, Meta { description: Some(desc.to_string()).filter(|d| !d.is_empty()), ..meta });
            }
            self.hub.publish(&a, v);
        }
    }

    fn declare_static(&mut self) {
        let h = self.hub.clone();
        let d = &mut self.declared;
        declare(&h, d, "alerts.enabled", Meta::boolean(true).owner("alerts").describe("Alerts master switch (off: events are still counted, nothing shows)"));
        declare(&h, d, "alerts.current", ro(ValueType::Map, Value::Null, "Alert on screen (null when none)"));
        declare(&h, d, "alerts.queue", ro(ValueType::List, Value::List(vec![]), "Queued alerts in show order"));
        declare(&h, d, "alerts.length", ro(ValueType::Int, Value::Int(0), "Queued alerts"));
        declare(&h, d, "alerts.veto_pending", ro(ValueType::Int, Value::Int(0), "Alerts waiting in their veto window"));
        declare(
            &h,
            d,
            "alerts.veto",
            ro(ValueType::List, Value::List(vec![]), "Alerts in their veto window: [{id, kind, user, title, text, remaining_ms, window_ms}]"),
        );
        declare(&h, d, "alerts.paused", ro(ValueType::Bool, Value::Bool(false), "Queue held (ad break/BRB, manual, or panic)"));
        declare(&h, d, "alerts.pause_reason", ro(ValueType::String, Value::Str(String::new()), "mode | manual | panic"));
        for (a, desc) in stats::STAT_ADDRS {
            let (ty, def) = if a.ends_with(".user") || a.ends_with(".currency") {
                (ValueType::String, Value::Str(String::new()))
            } else if a.contains(".tip.") || a.ends_with(".tips") {
                (ValueType::Float, Value::Float(0.0))
            } else {
                (ValueType::Int, Value::Int(0))
            };
            declare(&h, d, a, Meta { ty, default: def, ..Default::default() }.readonly().owner("stats").describe(desc));
        }
    }

    fn declare_goal(&mut self, g: &GoalDef) {
        let p = format!("goals.{}", g.name);
        let h = self.hub.clone();
        let d = &mut self.declared;
        let own = |m: Meta| m.readonly().owner("goals");
        declare(
            &h,
            d,
            &format!("{p}.current"),
            own(Meta { ty: ValueType::Float, default: Value::Float(0.0), ..Default::default() }.describe(&format!("{}: progress", g.label))),
        );
        declare(
            &h,
            d,
            &format!("{p}.target"),
            own(Meta { ty: ValueType::Float, default: Value::Float(g.target), ..Default::default() }.describe(&format!("{}: target", g.label))),
        );
        declare(&h, d, &format!("{p}.label"), own(Meta::string(&g.label)));
        declare(&h, d, &format!("{p}.progress"), own(Meta::float(0.0, [0.0, 1.0]).describe(&format!("{}: 0–1", g.label))));
    }

    /// Apply the (re)loaded project config.
    pub fn apply_config(&mut self, c: &se_core::Config) {
        let mut errs = Vec::new();
        let files: BTreeMap<String, (String, toml::Table)> = c
            .other
            .get("alerts")
            .map(|m| {
                m.iter()
                    .map(|(stem, t)| {
                        (stem.clone(), (c.files.get(&format!("alerts/{stem}")).cloned().unwrap_or_else(|| format!("alerts/{stem}.toml")), t.clone()))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let (cfg, parsed, e) = config::build(&files, &self.parsed);
        errs.extend(e);
        self.cfg = cfg;
        self.parsed = parsed;
        self.queue.policy = self.cfg.queue.clone();
        self.combiner.window_ms = self.cfg.queue.gift_window.ms();
        self.filter = FilterCfg::from_config(c);
        match StatsSettings::from_section(c.project.extra.get("stats")) {
            Ok(s) => self.stats.settings = s,
            Err(e) => errs.push(("project.toml".into(), e)),
        }
        match config::parse_goals(c.project.extra.get("goals")) {
            Ok(goals) => self.set_goals(goals),
            Err(e) => errs.push(("project.toml".into(), e)),
        }
        for (f, e) in &errs {
            self.log("error", format!("{f}: {e}"));
        }
        self.errors = errs;
        self.publish_health();
    }

    fn set_goals(&mut self, goals: Vec<GoalDef>) {
        for g in &goals {
            self.declare_goal(g);
        }
        match self.stats.set_goals(goals) {
            Ok(removed) => {
                for p in removed {
                    self.declared.retain(|a| !a.starts_with(&format!("{p}.")));
                    self.hub.submit(se_core::Input::Remove { prefix: p });
                }
            }
            Err(e) => self.log("error", format!("goals: {e:#}")),
        }
        match self.stats.all() {
            Ok(v) => self.publish(v),
            Err(e) => self.log("error", format!("stats: {e:#}")),
        }
    }

    fn publish_health(&self) {
        let (status, detail) = if self.errors.is_empty() {
            ("pass", format!("{} alerts, {} goals", self.cfg.alerts.len(), self.stats.goal_defs().count()))
        } else {
            ("warn", self.errors.iter().map(|(f, e)| format!("{f}: {e}")).collect::<Vec<_>>().join("; "))
        };
        self.hub.publish("health.alerts", Value::map().with("status", status).with("detail", detail));
    }

    fn apply_update(&mut self, up: stats::Update, cause: Option<u64>) {
        self.publish(up.changes);
        for (name, current, target) in up.reached {
            self.hub.emit(
                Event::new("goal.reached", Origin::System, Value::map().with("name", name).with("current", current).with("target", target)).with_causal(cause),
            );
        }
    }

    // ---- effects ------------------------------------------------------------------------

    fn action(&self, name: &str, args: Value, cause: Option<u64>) {
        self.hub.command(Command::new(Origin::System, Op::Action { name: name.into(), args }).caused_by(cause));
    }

    fn apply_effects(&mut self, fx: Vec<Effect>, now: u64) {
        for f in fx {
            match f {
                Effect::Show(a) => {
                    let v = a.to_value();
                    let mut e = Event::new("alert.show", Origin::System, v.clone()).with_causal(a.cause);
                    e.actor = a.actor.clone();
                    self.hub.emit(e);
                    self.hub.publish("alerts.current", v);
                    if let Some(s) = &a.sound {
                        self.action("audio.play", Value::map().with("sound", s.clone()), a.cause);
                    }
                    if let Some(t) = &a.tts {
                        let mut args = Value::map()
                            .with("text", t.text.clone())
                            .with("id", a.id.to_string())
                            .with("kind", a.event_type.clone())
                            .with("user", a.user.clone())
                            .with("amount", a.amount)
                            .with("tier", a.tier.map(Value::Int).unwrap_or_default());
                        if let Some(v) = &t.voice {
                            args = args.with("voice", v.clone());
                        }
                        self.action("tts.say", args, a.cause);
                    }
                    let viewer = a.actor.as_ref().is_some_and(|x| x.top_role() < Role::Owner);
                    for toks in &a.cmds {
                        match Op::from_tokens(toks) {
                            Ok(op) => {
                                let prio = (viewer && !a.sim).then_some(PRIORITY_CHAT);
                                self.hub.command(Command::new(Origin::Rule, op).with_actor(a.actor.clone()).caused_by(a.cause).with_priority(prio));
                            }
                            Err(e) => self.log("warn", format!("alert {}: `{}`: {e}", a.kind, toks.join(" "))),
                        }
                    }
                }
                Effect::Hide { alert, reason } => {
                    self.hub.emit(
                        Event::new("alert.hide", Origin::System, Value::map().with("id", alert.id).with("reason", reason.as_str())).with_causal(alert.cause),
                    );
                    if self.queue.current().is_none() {
                        self.hub.publish("alerts.current", Value::Null);
                    }
                    if alert.tts.is_some() && reason != HideReason::Done {
                        self.action("tts.skip", Value::map().with("id", alert.id.to_string()), alert.cause);
                    }
                }
                Effect::Update(a) => {
                    let v = a.to_value();
                    self.hub.emit(Event::new("alert.update", Origin::System, v.clone()).with_causal(a.cause));
                    self.hub.publish("alerts.current", v);
                }
                Effect::Dropped { alert, reason } => {
                    if reason != "cleared" {
                        self.log("info", format!("alert #{} ({} by {}) {reason}", alert.id, alert.kind, alert.user));
                    }
                }
            }
        }
        self.publish_queue(now);
    }

    fn queue_value(&self, now: u64) -> Value {
        Value::List(
            self.queue
                .queued(now)
                .into_iter()
                .map(|(a, veto)| {
                    Value::map()
                        .with("id", a.id)
                        .with("kind", a.kind.clone())
                        .with("title", a.title.clone())
                        .with("message", a.message.clone())
                        .with("user", a.user.clone())
                        .with("amount", a.amount)
                        .with("priority", a.priority)
                        .with("veto_remaining_ms", veto)
                        .with("vetted", !a.needs_veto)
                        .with("sim", a.sim)
                })
                .collect(),
        )
    }

    fn publish_changed(&mut self, a: &str, v: Value) {
        if self.last.get(a) != Some(&v) {
            self.last.insert(a.to_string(), v.clone());
            self.hub.publish(a, v);
        }
    }

    /// Publish queue state and veto countdowns (changes only).
    fn publish_queue(&mut self, now: u64) {
        let q = self.queue_value(now);
        self.publish_changed("alerts.length", Value::Int(self.queue.len() as i64));
        self.publish_changed("alerts.queue", q);
        let pending = self.queue.veto_pending(now);
        self.publish_changed("alerts.veto_pending", Value::Int(pending.len() as i64));
        let window = self.queue.policy.veto_window.ms();
        let list: Vec<Value> = self
            .queue
            .queued(now)
            .into_iter()
            .filter(|(_, rem)| *rem > 0)
            .map(|(a, rem)| {
                Value::map()
                    .with("id", a.id)
                    .with("kind", a.kind.clone())
                    .with("user", a.user.clone())
                    .with("title", a.title.clone())
                    .with("text", a.message.clone())
                    .with("remaining_ms", rem.div_ceil(100) * 100)
                    .with("window_ms", window)
            })
            .collect();
        self.publish_changed("alerts.veto", Value::List(list));
        for (id, rem) in &pending {
            let tenths = rem.div_ceil(100);
            if self.veto_shown.get(id) != Some(&tenths) {
                let a = format!("alerts.veto.{id}");
                if !self.veto_shown.contains_key(id) {
                    self.hub.declare(
                        &a,
                        Meta { ty: ValueType::Float, default: Value::Float(0.0), unit: Some("s".into()), ..Default::default() }
                            .readonly()
                            .owner("alerts")
                            .describe("Veto window countdown"),
                    );
                }
                self.veto_shown.insert(*id, tenths);
                self.hub.publish(&a, Value::Float(tenths as f64 / 10.0));
            }
        }
        let live: HashSet<u64> = pending.iter().map(|(id, _)| *id).collect();
        let gone: Vec<u64> = self.veto_shown.keys().filter(|id| !live.contains(id)).copied().collect();
        for id in gone {
            self.veto_shown.remove(&id);
            self.hub.submit(se_core::Input::Remove { prefix: format!("alerts.veto.{id}") });
        }
        let pr = self.queue.paused();
        self.publish_changed("alerts.paused", Value::Bool(pr.is_some()));
        self.publish_changed("alerts.pause_reason", Value::Str(pr.map(|r| r.as_str()).unwrap_or("").into()));
    }

    // ---- events -------------------------------------------------------------------------

    /// Handle one bus event.
    pub fn on_event(&mut self, e: &Event, snap: Arc<Snapshot>, now: u64) {
        let ty = e.ty.as_str();
        if ty.starts_with("alert.") || ty.starts_with("chat.") || ty.starts_with("goal.") || ty == "bot.said" {
            return;
        }
        let sim = e.origin == Origin::Sim;
        match ty {
            "mode.changed" => {
                let to = e.payload.get_path("to").and_then(Value::as_str).unwrap_or("");
                let on = self.queue.policy.pause_modes.iter().any(|m| m == to);
                let fx = self.queue.set_pause(PauseReason::Mode, on, now);
                self.apply_effects(fx, now);
                return;
            }
            "panic" | "clean" => {
                let fx = self.queue.set_pause(PauseReason::Panic, ty == "panic", now);
                self.apply_effects(fx, now);
                return;
            }
            "session.closed" => {
                let next = e.payload.get_path("next").and_then(Value::as_str).unwrap_or("").to_string();
                if !next.is_empty() {
                    match self.stats.set_session(&next) {
                        Ok(up) => self.apply_update(up, Some(e.id)),
                        Err(err) => self.log("error", format!("stats: {err:#}")),
                    }
                }
                return;
            }
            "twitch.chat" | "tiktok.chat" => {
                self.stats.chat(e, sim);
                match self.chat.on_chat(e, &self.filter, unix_ms()) {
                    Ok(Some(m)) => {
                        let mut ev = Event::new("chat.message", Origin::System, m).with_causal(Some(e.id));
                        ev.actor = e.actor.clone();
                        self.hub.emit(ev);
                    }
                    Ok(None) => {}
                    Err(reason) => tracing::debug!(target: "alerts", "chat line withheld from overlays: {reason}"),
                }
                return;
            }
            "twitch.chat.delete" => {
                let mid = e.payload.get_path("message_id").and_then(Value::as_str).unwrap_or("").to_string();
                if mid.is_empty() {
                    return;
                }
                self.chat.delete(&mid);
                self.hub.emit(Event::new("chat.delete", Origin::System, Value::map().with("message_id", mid.clone())).with_causal(Some(e.id)));
                let on_screen = self.queue.current().filter(|a| a.message_id.as_deref() == Some(mid.as_str()) && a.tts.is_some()).map(|a| (a.id, a.cause));
                let fx = self.queue.strip_message(&mid);
                if let Some((id, cause)) = on_screen {
                    self.action("tts.skip", Value::map().with("id", id.to_string()), cause);
                }
                self.apply_effects(fx, now);
                return;
            }
            "twitch.user.purge" => {
                let uid = e.payload.get_path("user_id").and_then(Value::as_str).unwrap_or("").to_string();
                let user = e.payload.get_path("user").and_then(Value::as_str).unwrap_or("").to_string();
                if uid.is_empty() && user.is_empty() {
                    return;
                }
                self.chat.purge(&uid, &user);
                self.hub.emit(
                    Event::new("chat.purge", Origin::System, Value::map().with("user_id", uid.clone()).with("user", user.clone())).with_causal(Some(e.id)),
                );
                let fx = self.queue.purge_user(&uid, &user, now);
                self.apply_effects(fx, now);
                return;
            }
            "twitch.chat.clear" => {
                self.chat.clear();
                self.hub.emit(Event::new("chat.clear", Origin::System, Value::Null).with_causal(Some(e.id)));
                return;
            }
            _ => {}
        }
        // counting happens for every event, shown or not
        match self.stats.record(e, sim) {
            Ok(up) => self.apply_update(up, Some(e.id)),
            Err(err) => self.log("error", format!("stats: {err:#}")),
        }
        if !snap.get("alerts.enabled").is_none_or(Value::truthy) {
            return;
        }
        let routable = self.cfg.alerts.iter().any(|d| d.enabled && se_proto::address::matches(&d.when, ty));
        let view = SnapView(snap);
        match self.combiner.on_event(e, now) {
            Gift::Pass => {
                if !routable {
                    return;
                }
                let id = self.queue.alloc_id();
                if let Some(a) = route::build(&self.cfg, e, &view, &self.filter, &mut self.rng, id) {
                    let fx = self.queue.push(a, now);
                    self.apply_effects(fx, now);
                }
            }
            Gift::Announce { recipients } => {
                if !routable {
                    return;
                }
                let id = self.queue.alloc_id();
                if let Some(mut a) = route::build(&self.cfg, e, &view, &self.filter, &mut self.rng, id) {
                    a.recipients = recipients;
                    self.combiner.set_alert(e, id);
                    let fx = self.queue.push(a, now);
                    self.apply_effects(fx, now);
                }
            }
            Gift::Absorb { alert, recipient } => {
                if let Some(id) = alert {
                    let fx = self.queue.update(id, |a| a.recipients.push(recipient));
                    self.apply_effects(fx, now);
                }
            }
            Gift::Held => {}
        }
    }

    /// Periodic work: queue timing, orphaned gift subs, veto countdowns.
    pub fn tick(&mut self, snap: Arc<Snapshot>, now: u64) {
        for o in self.combiner.tick(now) {
            let view = SnapView(snap.clone());
            let ev = if o.subs.len() == 1 {
                o.subs.into_iter().next().unwrap()
            } else {
                // their gift event never came: combine what we have
                let first = &o.subs[0];
                let recipients: Vec<String> = o.subs.iter().filter_map(|s| s.payload.get_path("user").and_then(Value::as_str).map(String::from)).collect();
                let mut payload = Value::map()
                    .with("count", o.subs.len())
                    .with("tier", first.payload.get_path("tier").cloned().unwrap_or(Value::Int(1)))
                    .with("user", o.gifter.clone().unwrap_or_else(|| "Anonymous".into()))
                    .with("recipients", recipients.clone());
                if let Some(g) = &o.gift_id {
                    payload = payload.with("gift_id", g.clone());
                }
                let mut synth = Event::new("twitch.gift", first.origin, payload).with_causal(Some(first.id));
                synth.ts = first.ts;
                if let Err(err) = self.stats.record(&synth, synth.origin == Origin::Sim).map(|up| self.apply_update(up, Some(first.id))) {
                    self.log("error", format!("stats: {err:#}"));
                }
                let id = self.queue.alloc_id();
                if let Some(mut a) = route::build(&self.cfg, &synth, &view, &self.filter, &mut self.rng, id) {
                    a.recipients = recipients;
                    let fx = self.queue.push(a, now);
                    self.apply_effects(fx, now);
                }
                continue;
            };
            let id = self.queue.alloc_id();
            if let Some(a) = route::build(&self.cfg, &ev, &view, &self.filter, &mut self.rng, id) {
                let fx = self.queue.push(a, now);
                self.apply_effects(fx, now);
            }
        }
        let fx = self.queue.tick(now);
        if fx.is_empty() {
            self.publish_queue_countdown(now);
        } else {
            self.apply_effects(fx, now);
        }
    }

    fn publish_queue_countdown(&mut self, now: u64) {
        if !self.queue.veto_pending(now).is_empty() || !self.veto_shown.is_empty() {
            self.publish_queue(now);
        }
    }

    // ---- actions ------------------------------------------------------------------------

    fn id_arg(args: &Value) -> Option<u64> {
        let v = args.get_path("id").or_else(|| args.get_path("args.0"))?;
        match v {
            Value::Str(s) => s.trim_start_matches('#').parse().ok(),
            other => other.as_i64().filter(|i| *i > 0).map(|i| i as u64),
        }
    }

    /// Handle an `alerts.*`, `goals.*`, or `stats.*` action.
    pub fn on_action(&mut self, c: &Command, now: u64) -> Result<String> {
        let Op::Action { name, args } = &c.op else { bail!("not an action") };
        let chat = c.priority() <= PRIORITY_CHAT;
        let is_mod = c.actor.as_ref().is_some_and(|a| a.top_role() >= Role::Mod);
        if chat && !is_mod {
            bail!("{name}: only mods and the operator");
        }
        let edit_only_ui = |what: &str| -> Result<()> {
            if chat {
                bail!("{what} can't be changed from chat");
            }
            Ok(())
        };
        let s = |k: &str| args.get_path(k).and_then(Value::as_str).map(String::from);
        let f = |k: &str| args.get_path(k).and_then(Value::as_f64).or_else(|| args.get_path("args.1").and_then(Value::as_f64));
        let msg = match name.as_str() {
            "alerts.veto" => {
                let id = Self::id_arg(args).ok_or_else(|| anyhow!("which alert id?"))?;
                let fx = self.queue.veto(id, now);
                anyhow::ensure!(!fx.is_empty(), "alert #{id} is not queued or on screen");
                self.apply_effects(fx, now);
                format!("alert #{id} vetoed")
            }
            "alerts.approve" => {
                let id = Self::id_arg(args).ok_or_else(|| anyhow!("which alert id?"))?;
                let (ok, fx) = self.queue.approve(id, now);
                anyhow::ensure!(ok, "alert #{id} is not queued");
                self.apply_effects(fx, now);
                format!("alert #{id} approved")
            }
            "alerts.skip" => {
                let fx = self.queue.skip(now);
                self.apply_effects(fx, now);
                "skipped".into()
            }
            "alerts.pause" | "alerts.resume" => {
                let fx = self.queue.set_pause(PauseReason::Manual, name == "alerts.pause", now);
                self.apply_effects(fx, now);
                format!("{}d", name.trim_start_matches("alerts."))
            }
            "alerts.clear" => {
                let fx = self.queue.clear();
                let n = fx.len();
                self.apply_effects(fx, now);
                format!("{n} queued alerts cleared")
            }
            "alerts.replay" => {
                let id = Self::id_arg(args).ok_or_else(|| anyhow!("which alert id?"))?;
                let fx = self.queue.replay(id, now).ok_or_else(|| anyhow!("alert #{id} is not in the history"))?;
                self.apply_effects(fx, now);
                format!("alert #{id} replayed")
            }
            "alerts.edit" | "alerts.add" | "alerts.remove" | "alerts.policy" => {
                edit_only_ui("alert routing")?;
                self.edit_alerts(name, args)?
            }
            "goals.set" | "goals.add" | "goals.reset" => {
                let g = s("name").or_else(|| args.get_path("args.0").and_then(Value::as_str).map(String::from)).ok_or_else(|| anyhow!("which goal?"))?;
                let up = match name.as_str() {
                    "goals.set" => self.stats.goal_set(&g, f("value").ok_or_else(|| anyhow!("needs value"))?)?,
                    "goals.add" => self.stats.goal_add(&g, f("amount").ok_or_else(|| anyhow!("needs amount"))?)?,
                    _ => self.stats.goal_reset(&g)?,
                };
                self.apply_update(up, Some(c.id));
                format!("goal {g} updated")
            }
            "goals.save" | "goals.delete" => {
                edit_only_ui("goals")?;
                self.edit_goals(name, args)?
            }
            "stats.purge_simulated" => {
                edit_only_ui("stats")?;
                let up = self.stats.purge_simulated()?;
                self.apply_update(up, Some(c.id));
                "simulated stats removed".into()
            }
            other => bail!("unknown action `{other}`"),
        };
        Ok(msg)
    }

    fn check_alerts_path(path: &str) -> Result<()> {
        if !path.starts_with("alerts/") || !path.ends_with(".toml") || path.contains("..") || path.matches('/').count() != 1 {
            bail!("alert files live in alerts/<name>.toml");
        }
        Ok(())
    }

    /// `alerts.edit {file, path, fields}`, `alerts.add {file, path, key, fields}`,
    /// `alerts.remove {file, path, key, index}`, `alerts.policy {fields}`.
    fn edit_alerts(&mut self, name: &str, args: &Value) -> Result<String> {
        let project = self.project.as_ref().ok_or_else(|| anyhow!("project not writable"))?;
        let fields: BTreeMap<String, Value> = args.get_path("fields").and_then(Value::as_map).cloned().unwrap_or_default();
        let path = match args.get_path("path") {
            Some(p) => Seg::parse_path(p)?,
            None => Vec::new(),
        };
        let file = match name {
            "alerts.policy" => self.parsed.iter().find(|(_, (q, _))| q.is_some()).map(|(p, _)| p.clone()).unwrap_or_else(|| "alerts/queue.toml".into()),
            _ => args.get_path("file").and_then(Value::as_str).map(String::from).ok_or_else(|| anyhow!("needs file"))?,
        };
        Self::check_alerts_path(&file)?;
        let stem = file.trim_start_matches("alerts/").trim_end_matches(".toml").to_string();
        let key = args.get_path("key").and_then(Value::as_str).unwrap_or("alert").to_string();
        anyhow::ensure!(key == "alert" || key == "variation", "key must be alert or variation");
        let mut parsed = None;
        project.edit(&file, |doc| {
            match name {
                "alerts.edit" => edit::set_fields(doc, &path, &fields)?,
                "alerts.add" => {
                    edit::append_table(doc, &path, &key, &fields, None)?;
                }
                "alerts.remove" => {
                    let i = args.get_path("index").and_then(Value::as_i64).ok_or_else(|| anyhow!("needs index"))?;
                    edit::remove_table(doc, &path, &key, i.max(0) as usize)?;
                }
                _ => {
                    if doc.get("queue").is_none() {
                        doc.insert("queue", toml_edit::Item::Table(toml_edit::Table::new()));
                    }
                    edit::set_fields(doc, &[Seg::key("queue")], &fields)?;
                }
            }
            let table: toml::Table = doc.to_string().parse()?;
            parsed = Some(config::parse_file(&stem, &file, &table).map_err(|e| anyhow!(e))?);
            Ok(())
        })?;
        if let Some(p) = parsed {
            self.parsed.insert(file.clone(), p);
            let mut cfg = AlertsConfig::default();
            let mut q_set = false;
            for (q, alerts) in self.parsed.values() {
                if let Some(q) = q
                    && !q_set
                {
                    cfg.queue = q.clone();
                    q_set = true;
                }
                cfg.alerts.extend(alerts.iter().cloned());
            }
            self.cfg = cfg;
            self.queue.policy = self.cfg.queue.clone();
            self.combiner.window_ms = self.cfg.queue.gift_window.ms();
        }
        Ok(format!("saved {file}"))
    }

    /// `goals.save {name, fields:{label?, target?, counts?, when?, if?, add?}}`, `goals.delete {name}`
    /// in `project.toml [goals.<name>]`.
    fn edit_goals(&mut self, name: &str, args: &Value) -> Result<String> {
        let project = self.project.as_ref().ok_or_else(|| anyhow!("project not writable"))?;
        let g = args.get_path("name").and_then(Value::as_str).ok_or_else(|| anyhow!("which goal?"))?.to_string();
        anyhow::ensure!(se_proto::address::is_valid(&g, false) && !g.contains('.'), "goal names are one word (letters, digits, _ -)");
        let fields: BTreeMap<String, Value> = args.get_path("fields").and_then(Value::as_map).cloned().unwrap_or_default();
        let mut goals = None;
        project.edit("project.toml", |doc| {
            if doc.get("goals").is_none() {
                let mut t = toml_edit::Table::new();
                t.set_implicit(true);
                doc.insert("goals", toml_edit::Item::Table(t));
            }
            let gt = doc["goals"].as_table_like_mut().ok_or_else(|| anyhow!("[goals] is not a table"))?;
            if name == "goals.delete" {
                anyhow::ensure!(gt.remove(&g).is_some(), "no goal `{g}`");
            } else {
                if gt.get(&g).is_none() {
                    gt.insert(&g, toml_edit::Item::Table(toml_edit::Table::new()));
                }
                edit::set_fields(doc, &[Seg::key("goals"), Seg::key(&g)], &fields)?;
            }
            let t: toml::Table = doc.to_string().parse()?;
            goals = Some(config::parse_goals(t.get("goals")).map_err(|e| anyhow!(e))?);
            Ok(())
        })?;
        if let Some(gs) = goals {
            self.set_goals(gs);
        }
        Ok(format!("goal {g} saved"))
    }

    // ---- queries ------------------------------------------------------------------------

    pub fn query_alerts(&self, now: u64) -> Value {
        let current = self.queue.current().map(|a| a.to_value().with("remaining_ms", self.queue.current_remaining(now).unwrap_or(0))).unwrap_or_default();
        let history: Vec<Value> = self
            .queue
            .history()
            .map(|h| {
                Value::map()
                    .with("id", h.alert.id)
                    .with("kind", h.alert.kind.clone())
                    .with("variation", h.alert.variation.clone())
                    .with("title", h.alert.title.clone())
                    .with("user", h.alert.user.clone())
                    .with("amount", h.alert.amount)
                    .with("ago_ms", now.saturating_sub(h.shown_ms))
                    .with("shown_at_unix", unix_ms() / 1000 - (now.saturating_sub(h.shown_ms) / 1000) as i64)
            })
            .collect();
        Value::map()
            .with("current", current)
            .with("queue", self.queue_value(now))
            .with("paused", self.queue.paused().is_some())
            .with("pause_reason", self.queue.paused().map(|r| r.as_str()).unwrap_or(""))
            .with("history", history)
    }

    pub fn query_config(&self) -> Value {
        let look = |l: &config::Look| {
            Value::map()
                .with("title", l.title.clone().map(Value::Str).unwrap_or_default())
                .with("message", l.message.clone().map(Value::Str).unwrap_or_default())
                .with("priority", l.priority.map(Value::Int).unwrap_or_default())
                .with("duration_ms", l.duration.map(|d| Value::Int(d.ms() as i64)).unwrap_or_default())
                .with("sound", l.sound.clone().map(Value::Str).unwrap_or_default())
                .with("image", l.image.clone().map(Value::Str).unwrap_or_default())
                .with("tts", l.tts.map(Value::Bool).unwrap_or_default())
                .with("tts_text", l.tts_text.clone().map(Value::Str).unwrap_or_default())
                .with("voice", l.voice.clone().map(Value::Str).unwrap_or_default())
                .with("interrupt", l.interrupt.map(Value::Bool).unwrap_or_default())
                .with("do", l.cmds.clone().map(Value::from).unwrap_or_default())
        };
        let alerts: Vec<Value> = self
            .cfg
            .alerts
            .iter()
            .map(|d| {
                Value::map()
                    .with("name", d.name.clone())
                    .with("when", d.when.clone())
                    .with("if", d.cond.as_ref().map(|c| Value::Str(c.source().to_string())).unwrap_or_default())
                    .with("enabled", d.enabled)
                    .with("veto", d.veto)
                    .with("file", d.file.clone())
                    .with("index", d.index)
                    .with("look", look(&d.look))
                    .with(
                        "variations",
                        Value::List(
                            d.variations
                                .iter()
                                .map(|v| Value::map().with("name", v.name.clone()).with("if", v.cond.source()).with("look", look(&v.look)))
                                .collect(),
                        ),
                    )
            })
            .collect();
        let q = &self.cfg.queue;
        let queue = Value::map()
            .with("min_spacing_ms", q.min_spacing.ms() as i64)
            .with("max_on_screen_ms", q.max_on_screen.ms() as i64)
            .with("max_queue", q.max_queue)
            .with("interrupt", q.interrupt)
            .with("interrupt_margin", q.interrupt_margin)
            .with("on_interrupt", if q.on_interrupt == config::OnInterrupt::Drop { "drop" } else { "requeue" })
            .with("pause_modes", q.pause_modes.clone())
            .with("veto_window_ms", q.veto_window.ms() as i64)
            .with("gift_window_ms", q.gift_window.ms() as i64)
            .with("max_message", q.max_message);
        let goals: Vec<Value> = self
            .stats
            .goal_defs()
            .map(|(g, cur)| {
                let counts = match &g.counts {
                    config::Counts::Builtin(k) => k.clone(),
                    config::Counts::Custom { when, .. } => format!("custom: {when}"),
                };
                Value::map().with("name", g.name.clone()).with("label", g.label.clone()).with("target", g.target).with("current", cur).with("counts", counts)
            })
            .collect();
        Value::map()
            .with("queue", queue)
            .with("alerts", alerts)
            .with("files", self.parsed.keys().cloned().collect::<Vec<_>>())
            .with("goals", goals)
            .with("errors", Value::List(self.errors.iter().map(|(f, e)| Value::map().with("file", f.clone()).with("msg", e.clone())).collect()))
    }
}

fn register_queries(hub: &Hub, a: Arc<Mutex<Alerts>>) {
    let x = a.clone();
    hub.register_query(
        "alerts",
        Arc::new(move |name, _| {
            let g = x.lock();
            let r = match name.as_str() {
                "alerts" => Ok(g.query_alerts(now_ms())),
                "alerts.config" => Ok(g.query_config()),
                other => Err(format!("unknown query `{other}`")),
            };
            Box::pin(async move { r })
        }),
    );
    let x = a.clone();
    hub.register_query(
        "credits",
        Arc::new(move |_, args| {
            let n = args.get_path("chatters").and_then(Value::as_i64).unwrap_or(25).clamp(1, 200) as usize;
            let r = x.lock().stats.credits(n).map_err(|e| e.to_string());
            Box::pin(async move { r })
        }),
    );
    let x = a.clone();
    hub.register_query(
        "chat.recent",
        Arc::new(move |_, _| {
            let r = Ok(x.lock().chat.recent());
            Box::pin(async move { r })
        }),
    );
    let x = a.clone();
    hub.register_query(
        "stats",
        Arc::new(move |name, args| {
            let g = x.lock();
            let r = match name.as_str() {
                "stats" => g.stats.all().map(|v| Value::Map(v.into_iter().collect())).map_err(|e| e.to_string()),
                "stats.recent" => g.stats.recent(args.get_path("n").and_then(Value::as_i64).unwrap_or(20).clamp(1, 200) as usize).map_err(|e| e.to_string()),
                other => Err(format!("unknown query `{other}`")),
            };
            Box::pin(async move { r })
        }),
    );
    hub.register_query(
        "goals",
        Arc::new(move |_, _| {
            let r = Ok(a.lock().query_config().get_path("goals").cloned().unwrap_or_default());
            Box::pin(async move { r })
        }),
    );
}

/// Start the alerts subsystem.
pub async fn start(ctx: EngineCtx) -> Result<()> {
    let project = match se_store::Project::open(&ctx.project_root) {
        Ok(p) => Some(p),
        Err(e) => {
            ctx.hub.log("error", "alerts", format!("project not writable for alert/goal edits: {e:#}"));
            None
        }
    };
    let session = ctx.hub.info.read().session.clone();
    let mut a = Alerts::new(ctx.hub.clone(), ctx.db.clone(), project, &session)?;
    a.declare_static();
    a.apply_config(&ctx.config.borrow());
    let mode = ctx.hub.snapshot.load().str("show.mode").unwrap_or("offline").to_string();
    if a.queue.policy.pause_modes.contains(&mode) {
        a.queue.set_pause(PauseReason::Mode, true, now_ms());
    }
    a.hub.publish("alerts.current", Value::Null);
    a.publish_queue(now_ms());
    let a = Arc::new(Mutex::new(a));
    register_queries(&ctx.hub, a.clone());
    let hub = ctx.hub.clone();
    let mut bus = hub.subscribe();
    let mut routes = [hub.route_actions("alerts"), hub.route_actions("goals"), hub.route_actions("stats")];
    let mut cfg = ctx.config.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(50));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut flush = tokio::time::interval(Duration::from_secs(10));
        let [r0, r1, r2] = &mut routes;
        loop {
            tokio::select! {
                r = bus.recv() => match r {
                    Ok(b) => {
                        if let Bus::Event(e) = &*b {
                            a.lock().on_event(e, hub.snapshot.load_full(), now_ms());
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => hub.log("warn", "alerts", format!("alerts lagged {n} bus messages")),
                    Err(_) => break,
                },
                Some(c) = r0.recv() => handle(&a, &hub, c),
                Some(c) = r1.recv() => handle(&a, &hub, c),
                Some(c) = r2.recv() => handle(&a, &hub, c),
                r = cfg.changed() => {
                    if r.is_err() {
                        break;
                    }
                    let c = cfg.borrow_and_update().clone();
                    a.lock().apply_config(&c);
                }
                _ = tick.tick() => a.lock().tick(hub.snapshot.load_full(), now_ms()),
                _ = flush.tick() => {
                    if let Err(e) = a.lock().stats.flush_chatters() {
                        hub.log("error", "alerts", format!("chatters: {e:#}"));
                    }
                }
            }
        }
        tracing::info!("alerts stopped");
    });
    Ok(())
}

fn handle(a: &Arc<Mutex<Alerts>>, hub: &Hub, c: Command) {
    let res = a.lock().on_action(&c, now_ms());
    let name = match &c.op {
        Op::Action { name, .. } => name.clone(),
        _ => String::new(),
    };
    match res {
        Ok(msg) => hub.log("info", "alerts", format!("{name}: {msg}")),
        Err(e) => hub.log("error", "alerts", format!("{name}: {e:#}")),
    }
}
