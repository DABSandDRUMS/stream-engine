//! Engine side of remote mod access (§12.3, §19).
//!
//! Moderators log in on the relay's `/mod` page with Twitch; the relay verifies the login
//! against the channel's moderator list and forwards requests over the engine link as
//! `mod.req` frames, which the link turns into the query `remote_mod.request`. This module
//! is the engine's authority:
//!
//! * `[remote_mod] enabled` must be true (off by default).
//! * Optional `allow_users` / `deny_users` (logins or user ids) narrow who gets in.
//! * Commands must match `[remote_mod] actions` AND the API's restricted `Scope::Mod`
//!   (queue, moderation, alerts, TTS skip, giveaways, clean — never mixer/lights/scenes).
//! * Commands run with `Origin::Relay` and the moderator as actor (the core audits them and
//!   checks `mod.*` actors are Mod+), rate-limited per moderator.
//!
//! Request bodies (`kind`): `hello`, `authorize {user}`, `state {user}`, `cmd {user, action,
//! args}`, `query {user, name, args}`; `user` = `{id, login, broadcaster?}`.

use crate::util::{self, Section};
use parking_lot::Mutex;
use se_api::Scope;
use se_hub::EngineCtx;
use se_proto::{Actor, Command, Meta, Op, Origin, Role, Value, ValueType, address};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::{Duration, Instant};

const TARGET: &str = "remote_mod";
/// Queries aggregated into the mod console's `state`.
const STATE_QUERIES: &[&str] = &["queue", "alerts", "tts", "giveaway"];
/// State lists shown in the console (AutoMod holds, policy approvals).
const STATE_ADDRS: &[&str] = &["show.mode", "policy.pending", "twitch.automod.queue", "queue.open"];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub enabled: bool,
    /// Logins or user ids; empty = every moderator of the channel.
    pub allow_users: Vec<String>,
    pub deny_users: Vec<String>,
    /// Allowed command patterns (`queue.**`, `tts.skip`, `clean`), further limited by the
    /// API's mod scope.
    pub actions: Vec<String>,
    /// Commands per moderator per minute.
    pub rate_per_min: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            enabled: false,
            allow_users: vec![],
            deny_users: vec![],
            actions: ["queue.**", "alerts.**", "mod.**", "tts.skip", "tts.clear", "giveaway.**", "clean", "bot.say"].map(String::from).to_vec(),
            rate_per_min: 60,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ModUser {
    pub id: String,
    pub login: String,
    pub broadcaster: bool,
}

impl ModUser {
    fn from_body(body: &Value) -> Result<ModUser, String> {
        let u = body.get_path("user").ok_or("missing user")?;
        let id = u.get_path("id").and_then(Value::as_str).unwrap_or("").trim().to_string();
        let login = u.get_path("login").and_then(Value::as_str).unwrap_or("").trim().to_ascii_lowercase();
        if id.is_empty() || login.is_empty() || !login.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') || login.len() > 25 {
            return Err("invalid user".into());
        }
        Ok(ModUser { id, login, broadcaster: u.get_path("broadcaster").is_some_and(Value::truthy) })
    }

    fn actor(&self) -> Actor {
        Actor { platform: "twitch".into(), id: self.id.clone(), name: self.login.clone(), roles: vec![if self.broadcaster { Role::Owner } else { Role::Mod }] }
    }
}

impl Settings {
    pub fn admits(&self, u: &ModUser) -> Result<(), String> {
        if !self.enabled {
            return Err("remote mod access is disabled".into());
        }
        let is = |list: &[String]| list.iter().any(|x| x.eq_ignore_ascii_case(&u.login) || *x == u.id);
        if is(&self.deny_users) {
            return Err("not allowed".into());
        }
        if !u.broadcaster && !self.allow_users.is_empty() && !is(&self.allow_users) {
            return Err("not on the allow list".into());
        }
        Ok(())
    }

    /// The op for a console command if it is allowed.
    pub fn command(&self, action: &str, args: Value) -> Result<Op, String> {
        let op = if action == "clean" {
            Op::Clean
        } else if address::is_valid(action, false) {
            Op::Action { name: action.to_string(), args }
        } else {
            return Err(format!("invalid command `{action}`"));
        };
        let listed = self.actions.iter().any(|p| address::matches(p, action));
        if !listed || !Scope::Mod.allows(&op) {
            return Err(format!("`{action}` is not available to moderators"));
        }
        Ok(op)
    }
}

/// Sliding one-minute window per moderator.
#[derive(Default)]
struct RateLimiter {
    hits: HashMap<String, VecDeque<Instant>>,
}

impl RateLimiter {
    fn allow(&mut self, who: &str, per_min: u32, now: Instant) -> bool {
        let q = self.hits.entry(who.to_string()).or_default();
        while q.front().is_some_and(|t| now.duration_since(*t) >= Duration::from_secs(60)) {
            q.pop_front();
        }
        if q.len() >= per_min as usize {
            return false;
        }
        q.push_back(now);
        true
    }
}

struct Inner {
    settings: Settings,
    rate: RateLimiter,
    /// login → last seen (console activity, shown in the UI).
    seen: HashMap<String, Instant>,
}

pub fn start(ctx: EngineCtx) {
    ctx.hub.declare("remote_mod.enabled", Meta::boolean(false).readonly().owner(TARGET).describe("Remote mod console accepted by the engine"));
    ctx.hub.declare(
        "remote_mod.active",
        Meta {
            ty: ValueType::List,
            default: Value::List(vec![]),
            readonly: true,
            owner: Some(TARGET.into()),
            description: Some("Moderators active in the last 30 minutes".into()),
            ..Default::default()
        },
    );
    let inner = Arc::new(Mutex::new(Inner { settings: Settings::default(), rate: RateLimiter::default(), seen: HashMap::new() }));
    {
        let (inner, ctx2) = (inner.clone(), ctx.clone());
        ctx.hub.register_query(
            "remote_mod",
            Arc::new(move |name, body| {
                let (inner, ctx) = (inner.clone(), ctx2.clone());
                Box::pin(async move {
                    if name != "remote_mod.request" {
                        return Err(format!("unknown query {name}"));
                    }
                    handle(&ctx, &inner, &body).await
                })
            }),
        );
    }
    tokio::spawn(async move {
        let mut ctx = ctx;
        let mut section: Section<Settings> = Section::new("remote_mod", TARGET);
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        let mut last_active: Vec<String> = vec![];
        loop {
            if section.reload(&ctx) {
                inner.lock().settings = section.value.clone();
                ctx.hub.publish("remote_mod.enabled", Value::Bool(section.value.enabled));
            }
            tokio::select! {
                r = ctx.config.changed() => if r.is_err() { break },
                _ = tick.tick() => {
                    let active = active_list(&inner);
                    if active != last_active {
                        ctx.hub.publish("remote_mod.active", Value::List(active.iter().cloned().map(Value::Str).collect()));
                        last_active = active;
                    }
                }
            }
        }
    });
}

fn active_list(inner: &Mutex<Inner>) -> Vec<String> {
    let mut g = inner.lock();
    g.seen.retain(|_, t| t.elapsed() < Duration::from_secs(1800));
    let mut v: Vec<String> = g.seen.keys().cloned().collect();
    v.sort();
    v
}

async fn handle(ctx: &EngineCtx, inner: &Mutex<Inner>, body: &Value) -> Result<Value, String> {
    let kind = body.get_path("kind").and_then(Value::as_str).unwrap_or("");
    let settings = inner.lock().settings.clone();
    if kind == "hello" {
        let snap = ctx.hub.snapshot.load();
        let s = |a: &str| snap.str(a).unwrap_or("").to_string();
        return Ok(Value::map()
            .with("enabled", settings.enabled)
            .with("client_id", s("twitch.client_id"))
            .with("broadcaster", Value::map().with("id", s("twitch.broadcaster.id")).with("login", s("twitch.broadcaster.login")))
            .with("actions", settings.actions.clone())
            .with("version", env!("CARGO_PKG_VERSION")));
    }
    let user = ModUser::from_body(body)?;
    settings.admits(&user)?;
    if inner.lock().seen.insert(user.login.clone(), Instant::now()).is_none() {
        let active = active_list(inner);
        ctx.hub.publish("remote_mod.active", Value::List(active.into_iter().map(Value::Str).collect()));
    }
    match kind {
        "authorize" => {
            ctx.hub.log("info", TARGET, format!("{} signed in to the remote mod console", user.login));
            Ok(Value::map().with("allowed", true).with("role", if user.broadcaster { "owner" } else { "mod" }).with("actions", settings.actions.clone()))
        }
        "state" => Ok(state(ctx).await.with("actions", settings.actions.clone())),
        "query" => {
            let name = body.get_path("name").and_then(Value::as_str).ok_or("missing name")?;
            if !STATE_QUERIES.contains(&name) || !Scope::Mod.may_query(name) {
                return Err(format!("query `{name}` is not available to moderators"));
            }
            ctx.hub.query(name, body.get_path("args").cloned().unwrap_or_default()).await
        }
        "cmd" => {
            let action = body.get_path("action").and_then(Value::as_str).ok_or("missing action")?;
            let op = settings.command(action, body.get_path("args").cloned().unwrap_or_default())?;
            if !inner.lock().rate.allow(&user.login, settings.rate_per_min.max(1), Instant::now()) {
                return Err("slow down: too many commands".into());
            }
            let label = op.describe();
            let mut cmd = Command::new(Origin::Relay, op);
            cmd.actor = Some(user.actor());
            let res = ctx.hub.exec(cmd).await;
            ctx.hub.log(
                if res.is_ok() { "info" } else { "warn" },
                TARGET,
                format!("{} (remote mod): {label}{}", user.login, res.as_ref().err().map(|e| format!(" — {e}")).unwrap_or_default()),
            );
            res.map(|()| Value::map().with("ok", true))
        }
        other => Err(format!("unknown request kind `{other}`")),
    }
}

async fn state(ctx: &EngineCtx) -> Value {
    let mut out = Value::map();
    for q in STATE_QUERIES {
        let v = ctx.hub.query(q, Value::Null).await.unwrap_or(Value::Null);
        out = out.with(*q, v);
    }
    let snap = ctx.hub.snapshot.load();
    let mut st = Value::map();
    for a in STATE_ADDRS {
        st = st.with(*a, snap.get(a).cloned().unwrap_or_default());
    }
    out.with("state", st).with("ts", util::unix_now())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(login: &str) -> ModUser {
        ModUser { id: format!("{login}-id"), login: login.into(), broadcaster: false }
    }

    fn on() -> Settings {
        Settings { enabled: true, ..Default::default() }
    }

    #[test]
    fn disabled_by_default() {
        assert!(Settings::default().admits(&user("m")).is_err());
        assert!(on().admits(&user("m")).is_ok());
    }

    #[test]
    fn allow_and_deny_lists() {
        let s = Settings { allow_users: vec!["Alice".into()], deny_users: vec!["mallory-id".into()], ..on() };
        assert!(s.admits(&user("alice")).is_ok());
        assert!(s.admits(&user("bob")).is_err());
        assert!(s.admits(&user("mallory")).is_err(), "deny by id");
        let owner = ModUser { broadcaster: true, ..user("owner") };
        assert!(s.admits(&owner).is_ok(), "the broadcaster bypasses the allow list");
        let s = Settings { deny_users: vec!["owner".into()], ..on() };
        assert!(s.admits(&owner).is_err(), "deny still applies");
    }

    #[test]
    fn restricted_command_set() {
        let s = on();
        assert!(s.command("queue.skip", Value::Null).is_ok());
        assert!(s.command("queue.approve", Value::map().with("id", 3)).is_ok());
        assert!(s.command("alerts.veto", Value::Null).is_ok());
        assert!(s.command("mod.timeout", Value::Null).is_ok());
        assert!(s.command("tts.skip", Value::Null).is_ok());
        assert!(s.command("giveaway.draw", Value::Null).is_ok());
        assert_eq!(s.command("clean", Value::Null).unwrap(), Op::Clean);
        for bad in ["lights.cue", "mixer.snapshot.recall", "scene.go", "obs.stream.stop", "tts.say", "audio.play", "preset.fire", "project.persist_base"] {
            assert!(s.command(bad, Value::Null).is_err(), "{bad} must be refused");
        }
        assert!(s.command("queue skip", Value::Null).is_err());
        assert!(s.command("", Value::Null).is_err());
        // the owner can narrow the list but never widen it past the API mod scope
        let narrow = Settings { actions: vec!["queue.skip".into()], ..on() };
        assert!(narrow.command("queue.skip", Value::Null).is_ok());
        assert!(narrow.command("queue.remove", Value::Null).is_err());
        let wide = Settings { actions: vec!["**".into()], ..on() };
        assert!(wide.command("lights.cue", Value::Null).is_err());
    }

    #[test]
    fn user_validation() {
        let body = |id: &str, login: &str| Value::map().with("user", Value::map().with("id", id).with("login", login));
        assert_eq!(ModUser::from_body(&body("1", "Alice")).unwrap().login, "alice");
        assert!(ModUser::from_body(&body("", "alice")).is_err());
        assert!(ModUser::from_body(&body("1", "")).is_err());
        assert!(ModUser::from_body(&body("1", "a b")).is_err());
        assert!(ModUser::from_body(&Value::map()).is_err());
        assert_eq!(ModUser::from_body(&body("1", "m")).unwrap().actor().roles, vec![Role::Mod]);
    }

    #[test]
    fn rate_limit_window() {
        let mut r = RateLimiter::default();
        let t0 = Instant::now();
        assert!(r.allow("m", 2, t0));
        assert!(r.allow("m", 2, t0 + Duration::from_secs(1)));
        assert!(!r.allow("m", 2, t0 + Duration::from_secs(2)));
        assert!(r.allow("other", 2, t0 + Duration::from_secs(2)));
        assert!(r.allow("m", 2, t0 + Duration::from_secs(61)));
    }
}
