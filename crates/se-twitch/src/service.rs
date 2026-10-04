//! The Twitch subsystem: state/actions/queries wiring, the auth lifecycle, the EventSub
//! processor, and the periodic pollers (viewers, ads, chat rate, emotes, stream delay).

use crate::auth::{Account, Auth, AuthError, DeviceCode, Poll, SecretStore};
use crate::chat::{RateWindow, cap};
use crate::config::{ChatAs, TwitchCfg};
use crate::emotes::{self, EmoteSet, Sources};
use crate::eventsub::{self, Notification, Runner, Status};
use crate::followers::{self, Entry};
use crate::helix::{Helix, HelixError, Method, first};
use crate::normalize::{Emote, Lookup, Normalizer};
use crate::rewards::{self, RewardMap};
use crate::time::{parse_rfc3339, unix_ms, unix_s};
use crate::users::{self, List, Users};
use parking_lot::{Mutex, RwLock};
use se_core::policy::{PolicyCfg, RewardDef};
use se_hub::{Bus, EngineCtx, Hub};
use se_proto::{Command, Event, Meta, Op, Origin, Role, Value, ValueType};
use serde_json::{Value as J, json};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

const OWNER: &str = "twitch";
/// How often the moderator and VIP lists are re-read.
const ROLES_EVERY: Duration = Duration::from_secs(600);
/// `fx.enabled` must hold this long before rewards are re-synced (an operator flicking the
/// toggle costs one Helix round, not one per flick).
const FX_DEBOUNCE: Duration = Duration::from_millis(500);

/// `fx.enabled` as published by the core; missing (older core) or null means effects are on.
fn fx_on(v: Option<&Value>) -> bool {
    v.is_none_or(|v| v.is_null() || v.truthy())
}

fn list_meta() -> Meta {
    Meta { ty: ValueType::List, default: Value::List(Vec::new()), ..Default::default() }
}

fn map_meta() -> Meta {
    Meta { ty: ValueType::Map, default: Value::map(), ..Default::default() }
}

fn auth_prefix(a: Account) -> &'static str {
    match a {
        Account::Broadcaster => "twitch.auth",
        Account::Bot => "twitch.auth.bot",
    }
}

fn arg(args: &Value, key: &str, pos: usize) -> Option<Value> {
    args.get_path(key).cloned().or_else(|| args.get_path("args").and_then(Value::as_list).and_then(|l| l.get(pos)).cloned())
}

fn arg_str(args: &Value, key: &str, pos: usize) -> Option<String> {
    arg(args, key, pos).filter(|v| !v.is_null()).map(|v| v.to_string()).filter(|s| !s.is_empty())
}

/// Seconds from `"2m"`, `"90s"`, or a number (seconds).
fn arg_secs(args: &Value, key: &str, pos: usize) -> Option<i64> {
    match arg(args, key, pos)? {
        Value::Str(s) => s.parse::<i64>().ok().or_else(|| se_proto::parse_duration_ms(&s).map(|ms| (ms / 1000) as i64)),
        v => v.as_i64(),
    }
}

/// `["a","b"]`, `"a|b|c"`, or `"a, b"`.
fn arg_list(args: &Value, key: &str, pos: usize) -> Vec<String> {
    match arg(args, key, pos) {
        Some(Value::List(l)) => l.iter().map(|v| v.to_string()).filter(|s| !s.trim().is_empty()).collect(),
        Some(Value::Str(s)) => s.split(['|', ',']).map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect(),
        _ => Vec::new(),
    }
}

/// Caches the normalizer reads.
#[derive(Default)]
struct Directory {
    broadcaster_id: String,
    bot_id: Option<String>,
    follows: HashMap<String, Entry>,
    /// Users/roles cache (chat badges + moderator/VIP lists), for events without badges.
    users: Users,
    emotes: EmoteSet,
    rewards: RewardMap,
    min_follow_s: i64,
}

impl Lookup for Directory {
    fn broadcaster_id(&self) -> &str {
        &self.broadcaster_id
    }
    fn bot_user_id(&self) -> Option<&str> {
        self.bot_id.as_deref()
    }
    fn follow_age_s(&self, user_id: &str) -> Option<i64> {
        self.follows.get(user_id).and_then(|e| e.age_s(unix_s()))
    }
    fn follower_min_age_s(&self) -> i64 {
        self.min_follow_s
    }
    fn cached_roles(&self, user_id: &str) -> Option<Vec<Role>> {
        self.users.roles(user_id)
    }
    fn badge_url(&self, set_id: &str, id: &str) -> Option<String> {
        self.emotes.badges.get(&format!("{set_id}/{id}")).cloned()
    }
    fn third_party(&self, code: &str) -> Option<Emote> {
        self.emotes.third_party.get(code).cloned()
    }
    fn reward(&self, reward_id: &str) -> (Option<String>, bool) {
        (self.rewards.by_id.get(reward_id).cloned(), self.rewards.manageable.contains(reward_id))
    }
}

#[derive(Clone, Debug, Default)]
struct Identity {
    id: String,
    login: String,
}

#[derive(Default)]
struct AdState {
    next_at: i64,
    duration: i64,
    warned_for: i64,
    last_published: Option<i64>,
}

#[derive(Default)]
struct Delay {
    stream_s: i64,
    chat_ms: Option<f64>,
    manual_ms: Option<i64>,
}

struct ChatOut {
    text: String,
    reply_to: Option<String>,
    account: Account,
}

#[derive(Default, Clone)]
struct HealthParts {
    auth: [Option<Result<(), String>>; 2],
    auth_pending: [bool; 2],
    eventsub: Status,
    eventsub_started: bool,
    rewards_error: Option<String>,
}

pub struct Service {
    ctx: EngineCtx,
    hub: Arc<Hub>,
    cfg: RwLock<TwitchCfg>,
    policy: RwLock<PolicyCfg>,
    rewards_defs: RwLock<Vec<RewardDef>>,
    http: reqwest::Client,
    pub auth: Arc<Auth>,
    pub helix: Arc<Helix>,
    dir: RwLock<Directory>,
    normalizer: Mutex<Normalizer>,
    broadcaster: RwLock<Option<Identity>>,
    bot: RwLock<Option<Identity>>,
    eventsub_stop: Mutex<Option<watch::Sender<bool>>>,
    device_cancel: Mutex<[Option<watch::Sender<bool>>; 2]>,
    chat_tx: mpsc::Sender<ChatOut>,
    chat_rx: Mutex<Option<mpsc::Receiver<ChatOut>>>,
    rate: Mutex<RateWindow>,
    health: Mutex<HealthParts>,
    chat_times: Mutex<VecDeque<Instant>>,
    purged: Mutex<HashMap<String, Instant>>,
    redemptions: Mutex<HashMap<String, String>>,
    echo: Mutex<HashMap<String, Instant>>,
    delay: Mutex<Delay>,
    ads: Mutex<AdState>,
    automod: Mutex<Vec<Value>>,
    users: Mutex<HashMap<String, String>>,
    sources: Mutex<Sources>,
    /// Last error reading each moderator/VIP list (warned once until it works again).
    roles_errors: Mutex<[Option<String>; 2]>,
    /// Last seen `fx.enabled`; rewards with `fx = true` are paused on Twitch while false.
    fx_enabled: AtomicBool,
    fx_changed: tokio::sync::Notify,
    /// `fx.enabled` the rewards on Twitch were last synced with (None: not synced yet).
    rewards_fx: Mutex<Option<bool>>,
    /// One reward sync at a time (config reload, fx toggle and `twitch.rewards.sync` overlap).
    rewards_lock: tokio::sync::Mutex<()>,
}

impl Service {
    pub fn new(ctx: EngineCtx, store: Arc<dyn SecretStore>) -> Arc<Service> {
        let cfg = match TwitchCfg::parse(ctx.project_section("twitch").as_ref()) {
            Ok(c) => c,
            Err(e) => {
                ctx.hub.log("error", OWNER, e);
                TwitchCfg::default()
            }
        };
        let (policy, perr) = PolicyCfg::from_config(&ctx.config.borrow());
        if let Some(e) = perr {
            ctx.hub.log("error", OWNER, e);
        }
        let (defs, errs) = RewardDef::all(&ctx.config.borrow());
        for e in errs {
            ctx.hub.log("error", OWNER, e);
        }
        let http = reqwest::Client::builder()
            .user_agent(concat!("stream-engine/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(20))
            .build()
            .unwrap_or_default();
        let auth = Arc::new(Auth::new(http.clone(), cfg.clone(), store));
        let helix = Arc::new(Helix::new(http.clone(), auth.clone(), cfg.helix_url.clone()));
        let (chat_tx, chat_rx) = mpsc::channel(64);
        let dir = Directory { min_follow_s: (policy.follower_min_age_ms / 1000) as i64, ..Default::default() };
        let rate = RateWindow::new(cfg.chat_rate);
        Arc::new(Service {
            hub: ctx.hub.clone(),
            ctx,
            cfg: RwLock::new(cfg),
            policy: RwLock::new(policy),
            rewards_defs: RwLock::new(defs),
            http,
            auth,
            helix,
            dir: RwLock::new(dir),
            normalizer: Mutex::new(Normalizer::default()),
            broadcaster: RwLock::new(None),
            bot: RwLock::new(None),
            eventsub_stop: Mutex::new(None),
            device_cancel: Mutex::new([None, None]),
            chat_tx,
            chat_rx: Mutex::new(Some(chat_rx)),
            rate: Mutex::new(rate),
            health: Mutex::new(HealthParts::default()),
            chat_times: Mutex::new(VecDeque::new()),
            purged: Mutex::new(HashMap::new()),
            redemptions: Mutex::new(HashMap::new()),
            echo: Mutex::new(HashMap::new()),
            delay: Mutex::new(Delay::default()),
            ads: Mutex::new(AdState::default()),
            automod: Mutex::new(Vec::new()),
            users: Mutex::new(HashMap::new()),
            sources: Mutex::new(Sources::default()),
            roles_errors: Mutex::new([None, None]),
            fx_enabled: AtomicBool::new(true),
            fx_changed: tokio::sync::Notify::new(),
            rewards_fx: Mutex::new(None),
            rewards_lock: tokio::sync::Mutex::new(()),
        })
    }

    fn cfg(&self) -> TwitchCfg {
        self.cfg.read().clone()
    }

    fn broadcaster_id(&self) -> Option<String> {
        self.broadcaster.read().as_ref().map(|b| b.id.clone())
    }

    fn set(&self, a: &str, v: impl Into<Value>) {
        self.hub.publish(a, v.into());
    }

    fn declare(&self) {
        let d = |a: &str, m: Meta, desc: &str| self.hub.declare(a, m.readonly().owner(OWNER).describe(desc));
        d("twitch.client_id", Meta::string(""), "Twitch app Client ID ([twitch] client_id)");
        for a in [Account::Broadcaster, Account::Bot] {
            let p = auth_prefix(a);
            d(&format!("{p}.status"), Meta::enumeration("none", &["none", "pending", "authorized", "expired", "error"]), "Authorization state");
            d(&format!("{p}.user_code"), Meta::string(""), "Device code to enter at the verification page");
            d(&format!("{p}.verification_uri"), Meta::string(""), "Where to enter the device code");
            d(&format!("{p}.expires_in_s"), Meta::int(0, [0.0, 1e9]).unit("s"), "Device code or token lifetime left");
            d(&format!("{p}.login"), Meta::string(""), "Authorized account");
            d(&format!("{p}.error"), Meta::string(""), "Last authorization error");
            d(&format!("{p}.missing_scopes"), list_meta(), "Scopes the stored grant lacks (re-authorize)");
        }
        d("twitch.broadcaster.id", Meta::string(""), "Broadcaster user id");
        d("twitch.broadcaster.login", Meta::string(""), "Broadcaster login");
        d("twitch.broadcaster.name", Meta::string(""), "Broadcaster display name");
        d("twitch.eventsub.connected", Meta::boolean(false), "EventSub WebSocket connected");
        d("twitch.eventsub.session", Meta::string(""), "EventSub session id");
        d("twitch.eventsub.subscriptions", Meta::int(0, [0.0, 1000.0]), "Enabled EventSub subscriptions");
        d("twitch.eventsub.failed", list_meta(), "Subscriptions Twitch refused ({type, error})");
        d("twitch.eventsub.reconnects", Meta::int(0, [0.0, 1e9]), "Reconnects this run");
        d("twitch.stream.live", Meta::boolean(false), "Stream online");
        d("twitch.stream.started_at", Meta::int(0, [0.0, 1e12]).unit("unix s"), "Stream start");
        d("twitch.stream.title", Meta::string(""), "Stream title");
        d("twitch.stream.category", Meta::string(""), "Stream category");
        d("twitch.stream.viewers", Meta::int(0, [0.0, 1e9]), "Concurrent viewers");
        d("twitch.ad.next_in_s", Meta::int(-1, [-1.0, 1e7]).unit("s"), "Seconds until the next scheduled ad (−1 unknown)");
        d("twitch.ad.duration_s", Meta::int(0, [0.0, 1e4]).unit("s"), "Length of the next ad break");
        d("twitch.ad.snoozes", Meta::int(0, [0.0, 10.0]), "Ad snoozes left");
        d("twitch.ad.snooze_refresh_at", Meta::int(0, [0.0, 1e12]).unit("unix s"), "When a snooze is regained");
        d("twitch.ad.last_at", Meta::int(0, [0.0, 1e12]).unit("unix s"), "Last ad break");
        d("twitch.ad.preroll_free_s", Meta::int(0, [0.0, 1e7]).unit("s"), "Pre-roll free time left");
        d("twitch.poll", map_meta(), "Current poll {id, title, choices, total_votes, ends_at, status, winner}");
        d("twitch.poll.active", Meta::boolean(false), "A poll is running");
        d("twitch.prediction", map_meta(), "Current prediction {id, title, outcomes, locks_at, status, winner}");
        d("twitch.prediction.active", Meta::boolean(false), "A prediction is open or locked");
        d("twitch.hype_train", map_meta(), "Current hype train {level, progress, goal, fraction, expires_at}");
        d("twitch.hype_train.active", Meta::boolean(false), "A hype train is running");
        d("twitch.hype_train.level", Meta::int(0, [0.0, 100.0]), "Hype train level");
        d("twitch.hype_train.fraction", Meta::float(0.0, [0.0, 1.0]), "Progress to the next level");
        d("twitch.automod.queue", list_meta(), "AutoMod-held messages {message_id, user, user_id, text, category, level, held_at}");
        d("twitch.automod.held", Meta::int(0, [0.0, 1e6]), "AutoMod-held messages waiting");
        d("twitch.rewards", list_meta(), "Rewards owned by this app {key, title, id, cost, enabled, status}");
        d("twitch.delay_ms", Meta::int(0, [0.0, 600_000.0]).unit("ms"), "Chat → screen time shift (stream delay + latency + chat transit)");
        d("twitch.delay.stream_s", Meta::int(0, [0.0, 900.0]).unit("s"), "Broadcaster stream delay setting on Twitch");
        d("twitch.delay.chat_ms", Meta::int(0, [0.0, 60_000.0]).unit("ms"), "Measured chat transit (send → EventSub echo)");
        d("twitch.helix.remaining", Meta::int(-1, [-1.0, 1e6]), "Helix rate-limit points left");
        d("health.twitch", map_meta(), "Preflight: token valid, EventSub connected");
    }

    // ---- lifecycle ------------------------------------------------------------------------

    pub async fn run(self: Arc<Self>) {
        self.declare();
        if let Err(e) = followers::migrate(&self.ctx.db) {
            self.hub.log("error", OWNER, format!("follower cache: {e:#}"));
        }
        if let Err(e) = users::migrate(&self.ctx.db) {
            self.hub.log("error", OWNER, format!("users/roles cache: {e:#}"));
        }
        {
            let mut d = self.dir.write();
            d.follows = followers::load(&self.ctx.db);
            d.users = users::load(&self.ctx.db, unix_s());
        }
        if let Ok(Some(v)) = self.ctx.db.kv_get("twitch", "delay.manual_ms") {
            self.delay.lock().manual_ms = v.as_i64();
        }
        self.load_cached_emotes();
        self.set("twitch.client_id", self.cfg().client_id);
        self.update_delay();
        self.fx_enabled.store(fx_on(self.hub.snapshot.load().get("fx.enabled")), Ordering::Relaxed);
        self.update_health();

        // routes are registered before `start` returns so no action goes unrouted
        for prefix in ["twitch", "mod"] {
            let rx = self.hub.route_actions(prefix);
            let me = self.clone();
            tokio::spawn(async move { me.actions(rx).await });
        }
        self.register_queries();
        let me = self.clone();
        tokio::spawn(async move { me.chat_sender().await });
        let me = self.clone();
        tokio::spawn(async move { me.bus_listener().await });
        let me = self.clone();
        tokio::spawn(async move { me.config_watcher().await });
        let me = self.clone();
        tokio::spawn(async move { me.fx_watcher().await });
        let me = self.clone();
        tokio::spawn(async move { me.tickers().await });
        let me = self.clone();
        tokio::spawn(async move { me.bootstrap().await });
    }

    async fn bootstrap(self: Arc<Self>) {
        if self.cfg().client_id.is_empty() {
            self.hub.log("warn", OWNER, "Twitch is not configured: set [twitch] client_id in project.toml (see docs/twitch.md)");
            return;
        }
        for a in [Account::Broadcaster, Account::Bot] {
            if a == Account::Bot && !self.cfg().bot {
                continue;
            }
            // Network/DNS/Twitch outages at boot are transient: keep retrying the stored
            // grant with backoff. Only a revoked grant needs the owner to authorize again.
            let mut wait = Duration::from_secs(2);
            loop {
                match self.auth.load(a).await {
                    Ok(true) => self.authorized(a).await,
                    Ok(false) => {
                        self.set(&format!("{}.status", auth_prefix(a)), "none");
                        self.health.lock().auth[a as usize] = None;
                        self.update_health();
                    }
                    Err(e @ AuthError::Other(_)) => {
                        self.auth_failed(a, &e);
                        tokio::time::sleep(wait).await;
                        wait = (wait * 2).min(Duration::from_secs(60));
                        continue;
                    }
                    Err(e) => self.auth_failed(a, &e),
                }
                break;
            }
        }
    }

    fn auth_failed(&self, a: Account, e: &AuthError) {
        let p = auth_prefix(a);
        self.set(&format!("{p}.status"), if matches!(e, AuthError::Revoked(_)) { "expired" } else { "error" });
        self.set(&format!("{p}.error"), e.to_string());
        self.hub.log("error", OWNER, format!("{} account: {e}", a.as_str()));
        self.health.lock().auth[a as usize] = Some(Err(e.to_string()));
        self.update_health();
    }

    /// An account has a valid token: publish identity, start what depends on it.
    async fn authorized(self: &Arc<Self>, a: Account) {
        let Some(t) = self.auth.tokens(a) else { return };
        let p = auth_prefix(a);
        self.set(&format!("{p}.status"), "authorized");
        self.set(&format!("{p}.login"), t.login.clone());
        self.set(&format!("{p}.error"), "");
        self.set(&format!("{p}.user_code"), "");
        self.set(&format!("{p}.verification_uri"), "");
        self.set(&format!("{p}.expires_in_s"), (t.expires_at - unix_s()).max(0));
        self.set(&format!("{p}.missing_scopes"), Value::from(self.auth.missing_scopes(a).iter().map(|s| s.to_string()).collect::<Vec<_>>()));
        self.health.lock().auth[a as usize] = Some(Ok(()));
        match a {
            Account::Bot => {
                *self.bot.write() = Some(Identity { id: t.user_id.clone(), login: t.login.clone() });
                self.dir.write().bot_id = Some(t.user_id);
            }
            Account::Broadcaster => {
                let name = match self.helix.get(a, "/users", &[("id", t.user_id.clone())]).await {
                    Ok(v) => first(&v).and_then(|u| u.get("display_name")).and_then(J::as_str).unwrap_or(&t.login).to_string(),
                    Err(_) => t.login.clone(),
                };
                let id = Identity { id: t.user_id.clone(), login: t.login.clone() };
                self.set("twitch.broadcaster.id", id.id.clone());
                self.set("twitch.broadcaster.login", id.login.clone());
                self.set("twitch.broadcaster.name", name);
                self.dir.write().broadcaster_id = id.id.clone();
                *self.broadcaster.write() = Some(id);
                self.start_eventsub();
                let me = self.clone();
                tokio::spawn(async move {
                    me.sync_rewards().await;
                    me.refresh_emotes().await;
                    me.preload_followers().await;
                    me.sync_roles().await;
                    me.poll_stream().await;
                });
            }
        }
        self.update_health();
    }

    fn start_eventsub(self: &Arc<Self>) {
        let Some(bid) = self.broadcaster_id() else { return };
        let cfg = self.cfg();
        let (stop_tx, stop_rx) = watch::channel(false);
        if let Some(old) = self.eventsub_stop.lock().replace(stop_tx) {
            let _ = old.send(true);
        }
        let (tx, mut rx) = mpsc::channel::<Notification>(2048);
        let runner = Runner {
            url: cfg.eventsub_url.clone(),
            subscriptions_url: cfg.subscriptions_url.clone(),
            specs: eventsub::subscriptions(&bid, cfg.sub_source),
            helix: self.helix.clone(),
        };
        self.health.lock().eventsub_started = true;
        let me = self.clone();
        tokio::spawn(async move {
            runner
                .run(
                    tx,
                    move |s: &Status| {
                        me.set("twitch.eventsub.connected", s.connected);
                        me.set("twitch.eventsub.session", s.session_id.clone());
                        me.set("twitch.eventsub.subscriptions", s.subscriptions as i64);
                        me.set(
                            "twitch.eventsub.failed",
                            Value::List(
                                s.failed.iter().chain(&s.revoked).map(|(t, e)| Value::map().with("type", t.clone()).with("error", e.clone())).collect(),
                            ),
                        );
                        me.set("twitch.eventsub.reconnects", s.reconnects as i64);
                        me.health.lock().eventsub = s.clone();
                        me.update_health();
                    },
                    stop_rx,
                )
                .await;
        });
        let me = self.clone();
        tokio::spawn(async move {
            while let Some(n) = rx.recv().await {
                me.handle(n).await;
            }
        });
    }

    // ---- EventSub processing ----------------------------------------------------------------

    /// Normalize one notification and hand the events to the core.
    async fn handle(self: &Arc<Self>, n: Notification) {
        let user_field = match n.ty.as_str() {
            "channel.chat.message" => "chatter_user_id",
            "channel.cheer" | "channel.channel_points_custom_reward_redemption.add" | "channel.subscribe" | "channel.subscription.message" => "user_id",
            _ => "",
        };
        if !user_field.is_empty()
            && let Some(uid) = n.event.get(user_field).and_then(J::as_str)
        {
            self.ensure_follow(uid).await;
        }
        if n.ty == "channel.follow"
            && let Some(uid) = n.event.get("user_id").and_then(J::as_str)
        {
            let at = n.event.get("followed_at").and_then(J::as_str).and_then(parse_rfc3339).map(|ms| ms / 1000).unwrap_or_else(unix_s);
            let e = Entry { followed_at: at, checked_at: unix_s() };
            self.dir.write().follows.insert(uid.to_string(), e);
            followers::store(&self.ctx.db, &[(uid.to_string(), e)]);
        }
        let events = {
            let dir = self.dir.read();
            self.normalizer.lock().notification(&n.ty, &n.version, &n.event, &*dir, unix_ms())
        };
        for e in events {
            if self.observe(&e) {
                self.hub.emit(e);
            }
        }
    }

    /// Follow status for a chatter: cached, else a quick lookup (bounded wait).
    async fn ensure_follow(&self, uid: &str) {
        if uid.is_empty() || uid == "anonymous" {
            return;
        }
        let fresh = self.dir.read().follows.get(uid).is_some_and(|e| e.fresh(unix_s()));
        let Some(bid) = self.broadcaster_id() else { return };
        if fresh || uid == bid {
            return;
        }
        match tokio::time::timeout(Duration::from_millis(800), followers::lookup(&self.helix, &bid, uid)).await {
            Ok(Ok(e)) => {
                self.dir.write().follows.insert(uid.to_string(), e);
                followers::store(&self.ctx.db, &[(uid.to_string(), e)]);
            }
            Ok(Err(err)) => tracing::debug!(target: "twitch", "follower lookup for {uid}: {err}"),
            Err(_) => tracing::debug!(target: "twitch", "follower lookup for {uid} timed out"),
        }
    }

    /// Side effects of a normalized event on our own state; `false` drops it (duplicate purge).
    fn observe(&self, e: &Event) -> bool {
        let p = &e.payload;
        let s = |k: &str| p.get_path(k).map(|v| v.to_string()).unwrap_or_default();
        match e.ty.as_str() {
            "twitch.chat" => {
                if let Some(a) = e.actor.as_ref().filter(|a| a.id != "anonymous") {
                    let row = self.dir.write().users.saw(&a.id, &s("login"), &a.roles, unix_s());
                    if let Some(row) = row {
                        users::store(&self.ctx.db, &[row]);
                    }
                }
                let now = Instant::now();
                self.chat_times.lock().push_back(now);
                if let Some(sent) = self.echo.lock().remove(&s("message_id")) {
                    let ms = now.duration_since(sent).as_secs_f64() * 1000.0;
                    let mut d = self.delay.lock();
                    d.chat_ms = Some(d.chat_ms.map_or(ms, |old| old * 0.8 + ms * 0.2));
                    drop(d);
                    self.update_delay();
                }
            }
            "twitch.redeem" => {
                let mut r = self.redemptions.lock();
                if r.len() > 5000 {
                    r.clear();
                }
                r.insert(s("redemption_id"), s("reward_id"));
            }
            "twitch.user.purge" => {
                let uid = s("user_id");
                let mut seen = self.purged.lock();
                let now = Instant::now();
                seen.retain(|_, t| now.duration_since(*t) < Duration::from_secs(10));
                if seen.insert(uid, now).is_some() {
                    return false;
                }
            }
            "twitch.automod.hold" => {
                let mut q = self.automod.lock();
                q.retain(|m| m.get_path("message_id") != p.get_path("message_id"));
                q.push(p.clone());
                if q.len() > 200 {
                    q.remove(0);
                }
                self.publish_automod(&q);
            }
            "twitch.automod.update" => {
                let mut q = self.automod.lock();
                q.retain(|m| m.get_path("message_id") != p.get_path("message_id"));
                self.publish_automod(&q);
            }
            "twitch.stream.online" => {
                self.set("twitch.stream.live", true);
                self.set("twitch.stream.started_at", p.get_path("started_at").and_then(Value::as_i64).unwrap_or_else(unix_s));
            }
            "twitch.stream.offline" => {
                self.set("twitch.stream.live", false);
                self.hub.signal("twitch.viewers", 0.0);
                self.set("twitch.stream.viewers", 0);
            }
            "twitch.channel.update" => {
                self.set("twitch.stream.title", s("title"));
                self.set("twitch.stream.category", s("category"));
            }
            "twitch.ad_break" => {
                self.set("twitch.ad.last_at", unix_s());
                let mut a = self.ads.lock();
                a.next_at = 0;
            }
            t if t.starts_with("twitch.poll.") => {
                let active = t != "twitch.poll.end";
                self.set("twitch.poll", p.clone().with("active", active));
                self.set("twitch.poll.active", active);
            }
            t if t.starts_with("twitch.prediction.") => {
                let active = t != "twitch.prediction.end";
                let status = if t == "twitch.prediction.lock" { "locked".to_string() } else { s("status") };
                self.set("twitch.prediction", p.clone().with("active", active).with("status", status));
                self.set("twitch.prediction.active", active);
            }
            t if t.starts_with("twitch.hype_train.") => {
                let active = t != "twitch.hype_train.end";
                self.set("twitch.hype_train", p.clone().with("active", active));
                self.set("twitch.hype_train.active", active);
                self.set("twitch.hype_train.level", p.get_path("level").and_then(Value::as_i64).unwrap_or(0));
                self.set("twitch.hype_train.fraction", if active { p.get_path("fraction").and_then(Value::as_f64).unwrap_or(0.0) } else { 0.0 });
            }
            _ => {}
        }
        true
    }

    fn publish_automod(&self, q: &[Value]) {
        self.set("twitch.automod.queue", Value::List(q.to_vec()));
        self.set("twitch.automod.held", q.len() as i64);
    }

    // ---- periodic work --------------------------------------------------------------------

    async fn tickers(self: Arc<Self>) {
        let mut fast = tokio::time::interval(Duration::from_millis(500));
        let mut second = tokio::time::interval(Duration::from_secs(1));
        let mut minute = tokio::time::interval(Duration::from_secs(60));
        let mut last_stream = Instant::now();
        let mut last_ads = Instant::now() - Duration::from_secs(3600);
        let mut last_validate = Instant::now();
        let mut last_emotes = Instant::now();
        let mut last_roles = Instant::now();
        let mut last_rate = -1.0f32;
        loop {
            tokio::select! {
                _ = fast.tick() => {
                    let now = Instant::now();
                    let n = {
                        let mut q = self.chat_times.lock();
                        while q.front().is_some_and(|t| now.duration_since(*t) > Duration::from_secs(60)) {
                            q.pop_front();
                        }
                        q.len()
                    };
                    let rate = n as f32;
                    if rate != last_rate {
                        last_rate = rate;
                        self.hub.signal("twitch.chat_rate", rate);
                    }
                }
                _ = second.tick() => self.tick_ads(),
                _ = minute.tick() => {
                    let cfg = self.cfg();
                    if self.broadcaster_id().is_none() {
                        continue;
                    }
                    if last_stream.elapsed() >= Duration::from_millis(cfg.viewer_poll_ms.saturating_sub(1000)) {
                        last_stream = Instant::now();
                        self.poll_stream().await;
                    }
                    let live = self.hub.snapshot.load().bool("twitch.stream.live");
                    if live && last_ads.elapsed() >= Duration::from_millis(cfg.ads_poll_ms) {
                        last_ads = Instant::now();
                        self.poll_ads().await;
                    }
                    for a in [Account::Broadcaster, Account::Bot] {
                        let Some(t) = self.auth.tokens(a) else { continue };
                        let left = t.expires_at - unix_s();
                        self.set(&format!("{}.expires_in_s", auth_prefix(a)), left.max(0));
                        if left < 300 {
                            match self.auth.refresh(a).await {
                                Ok(_) => self.set(&format!("{}.expires_in_s", auth_prefix(a)), (self.auth.tokens(a).map(|t| t.expires_at).unwrap_or(0) - unix_s()).max(0)),
                                Err(e) => self.auth_failed(a, &e),
                            }
                        }
                    }
                    if last_validate.elapsed() >= Duration::from_secs(3600) {
                        last_validate = Instant::now();
                        for a in [Account::Broadcaster, Account::Bot] {
                            if self.auth.tokens(a).is_some()
                                && let Err(e) = self.auth.validate(a).await
                            {
                                self.auth_failed(a, &e);
                            }
                        }
                    }
                    if last_emotes.elapsed() >= Duration::from_secs(1800) {
                        last_emotes = Instant::now();
                        self.refresh_emotes().await;
                    }
                    if last_roles.elapsed() >= ROLES_EVERY {
                        last_roles = Instant::now();
                        self.sync_roles().await;
                    }
                    self.set("twitch.helix.remaining", self.helix.remaining.load(std::sync::atomic::Ordering::Relaxed));
                }
            }
        }
    }

    /// Viewers (→ `twitch.viewers` signal), live state, title; the stream delay setting.
    async fn poll_stream(&self) {
        let Some(bid) = self.broadcaster_id() else { return };
        match self.helix.get(Account::Broadcaster, "/streams", &[("user_id", bid.clone())]).await {
            Ok(v) => match first(&v) {
                Some(s) => {
                    let viewers = s.get("viewer_count").and_then(J::as_i64).unwrap_or(0);
                    self.hub.signal("twitch.viewers", viewers as f32);
                    self.set("twitch.stream.viewers", viewers);
                    self.set("twitch.stream.live", true);
                    let started = s.get("started_at").and_then(J::as_str).and_then(parse_rfc3339).map(|ms| ms / 1000).unwrap_or(0);
                    self.set("twitch.stream.started_at", started);
                    self.set("twitch.stream.title", s.get("title").and_then(J::as_str).unwrap_or("").to_string());
                    self.set("twitch.stream.category", s.get("game_name").and_then(J::as_str).unwrap_or("").to_string());
                }
                None => {
                    self.hub.signal("twitch.viewers", 0.0);
                    self.set("twitch.stream.viewers", 0);
                    self.set("twitch.stream.live", false);
                }
            },
            Err(e) => tracing::debug!(target: "twitch", "streams poll: {e}"),
        }
        match self.helix.get(Account::Broadcaster, "/channels", &[("broadcaster_id", bid)]).await {
            Ok(v) => {
                if let Some(c) = first(&v) {
                    self.delay.lock().stream_s = c.get("delay").and_then(J::as_i64).unwrap_or(0);
                    if self.hub.snapshot.load().str("twitch.stream.title").unwrap_or("").is_empty() {
                        self.set("twitch.stream.title", c.get("title").and_then(J::as_str).unwrap_or("").to_string());
                        self.set("twitch.stream.category", c.get("game_name").and_then(J::as_str).unwrap_or("").to_string());
                    }
                    self.update_delay();
                }
            }
            Err(e) => tracing::debug!(target: "twitch", "channel info: {e}"),
        }
    }

    async fn poll_ads(&self) {
        let Some(bid) = self.broadcaster_id() else { return };
        match self.helix.get(Account::Broadcaster, "/channels/ads", &[("broadcaster_id", bid)]).await {
            Ok(v) => {
                if let Some(a) = first(&v) {
                    self.apply_ad_schedule(a);
                }
            }
            Err(e) => tracing::debug!(target: "twitch", "ad schedule: {e}"),
        }
    }

    /// Ad schedule fields are RFC 3339 strings (or unix numbers in some responses).
    fn apply_ad_schedule(&self, a: &J) {
        let t = |k: &str| match a.get(k) {
            Some(J::String(s)) => parse_rfc3339(s).map(|ms| ms / 1000).or_else(|| s.parse().ok()).unwrap_or(0),
            Some(J::Number(n)) => n.as_i64().unwrap_or(0),
            _ => 0,
        };
        let n = |k: &str| a.get(k).and_then(J::as_i64).unwrap_or(0);
        {
            let mut s = self.ads.lock();
            s.next_at = t("next_ad_at");
            s.duration = n("duration");
        }
        self.set("twitch.ad.duration_s", n("duration"));
        self.set("twitch.ad.snoozes", n("snooze_count"));
        self.set("twitch.ad.snooze_refresh_at", t("snooze_refresh_at"));
        self.set("twitch.ad.last_at", t("last_ad_at"));
        self.set("twitch.ad.preroll_free_s", n("preroll_free_time"));
        self.tick_ads();
    }

    /// 1 Hz: `twitch.ad.next_in_s` countdown and the `twitch.ad.upcoming` warning event.
    fn tick_ads(&self) {
        let warn = self.cfg().ad_warning_s;
        let now = unix_s();
        let (value, fire) = {
            let mut s = self.ads.lock();
            let value = if s.next_at > 0 { (s.next_at - now).max(0) } else { -1 };
            let fire = s.next_at > 0 && value <= warn && value > 0 && s.warned_for != s.next_at;
            if fire {
                s.warned_for = s.next_at;
            }
            if s.last_published == Some(value) {
                return;
            }
            s.last_published = Some(value);
            (value, fire.then_some(s.duration))
        };
        self.set("twitch.ad.next_in_s", value);
        if let Some(duration) = fire {
            self.hub.emit(Event::new("twitch.ad.upcoming", Origin::Twitch, Value::map().with("in_s", value).with("duration", duration)));
        }
    }

    /// Chat → screen shift: stream delay setting + viewer latency + measured chat transit,
    /// unless set manually (`twitch.delay.set`). Updates the master clock mapping (§3.2).
    fn update_delay(&self) {
        let (stream_s, chat_ms, manual) = {
            let d = self.delay.lock();
            (d.stream_s, d.chat_ms, d.manual_ms)
        };
        let total = manual.unwrap_or_else(|| stream_s * 1000 + self.cfg().latency_ms as i64 + chat_ms.unwrap_or(0.0).round() as i64).max(0);
        self.hub.clock.update(|m| m.twitch_delay_ms = total.min(u32::MAX as i64) as u32);
        self.set("twitch.delay_ms", total);
        self.set("twitch.delay.stream_s", stream_s);
        self.set("twitch.delay.chat_ms", chat_ms.unwrap_or(0.0).round() as i64);
    }

    async fn preload_followers(&self) {
        let Some(bid) = self.broadcaster_id() else { return };
        match followers::preload(&self.helix, &bid).await {
            Ok(list) => {
                let n = list.len();
                followers::store(&self.ctx.db, &list);
                let mut d = self.dir.write();
                d.follows.extend(list);
                tracing::info!(target: "twitch", "follower cache: {n} followers loaded");
            }
            Err(e) => self.hub.log("warn", OWNER, format!("follower preload failed: {e}")),
        }
    }

    /// Moderator and VIP lists → users/roles cache. A list that can't be read (e.g. a grant
    /// from before its scope was added) keeps its last good copy.
    async fn sync_roles(&self) {
        let Some(bid) = self.broadcaster_id() else { return };
        for (i, l) in List::ALL.into_iter().enumerate() {
            match users::fetch(&self.helix, &bid, l).await {
                Ok(members) => {
                    let now = unix_s();
                    let changed = self.dir.write().users.apply_list(l, &members, now);
                    users::store(&self.ctx.db, &changed);
                    users::store_synced(&self.ctx.db, l, now);
                    self.roles_errors.lock()[i] = None;
                    tracing::info!(target: "twitch", "roles: {} {}", members.len(), l.as_str());
                }
                Err(e) => {
                    let msg = format!("can't read the channel's {} ({e}); roles for events without chat badges use what chat showed", l.as_str());
                    let mut errs = self.roles_errors.lock();
                    if errs[i].as_deref() != Some(msg.as_str()) {
                        self.hub.log("warn", OWNER, msg.clone());
                        errs[i] = Some(msg);
                    }
                }
            }
        }
    }

    // ---- emotes ---------------------------------------------------------------------------

    fn load_cached_emotes(&self) {
        let mut src = Sources::default();
        for (k, v) in self.ctx.db.kv_list("twitch.emotes").unwrap_or_default() {
            src.responses.insert(k, serde_json::Value::from(&v));
        }
        let set = EmoteSet::build(&src);
        *self.sources.lock() = src;
        self.dir.write().emotes = set;
    }

    async fn fetch_json(&self, url: &str) -> Result<J, String> {
        let r = self.http.get(url).timeout(Duration::from_secs(15)).send().await.map_err(|e| e.to_string())?;
        if r.status().as_u16() == 404 {
            // no emotes configured on that provider for this channel
            return Ok(J::Null);
        }
        if !r.status().is_success() {
            return Err(format!("{url}: HTTP {}", r.status()));
        }
        r.json::<J>().await.map_err(|e| format!("{url}: {e}"))
    }

    /// Twitch badges/emotes + 7TV/BTTV/FFZ; failures keep the last good copy.
    async fn refresh_emotes(&self) {
        let Some(bid) = self.broadcaster_id() else { return };
        let providers = self.cfg().emote_providers;
        let mut got: Vec<(String, J)> = Vec::new();
        let helix_sources = [
            ("badges.global", "/chat/badges/global", vec![]),
            ("badges.channel", "/chat/badges", vec![("broadcaster_id", bid.clone())]),
            ("twitch.global", "/chat/emotes/global", vec![]),
            ("twitch.channel", "/chat/emotes", vec![("broadcaster_id", bid.clone())]),
        ];
        for (key, path, q) in helix_sources {
            match self.helix.get(Account::Broadcaster, path, &q).await {
                Ok(v) => got.push((key.into(), v)),
                Err(e) => tracing::warn!(target: "twitch", "{path}: {e}"),
            }
        }
        let third: Vec<(&str, &str, String)> = vec![
            ("7tv", "7tv.global", emotes::SEVENTV_GLOBAL.to_string()),
            ("7tv", "7tv.channel", emotes::seventv_user(&bid)),
            ("bttv", "bttv.global", emotes::BTTV_GLOBAL.to_string()),
            ("bttv", "bttv.channel", emotes::bttv_user(&bid)),
            ("ffz", "ffz.global", emotes::FFZ_GLOBAL.to_string()),
            ("ffz", "ffz.channel", emotes::ffz_room(&bid)),
        ];
        for (provider, key, url) in third {
            if !providers.iter().any(|p| p == provider) {
                continue;
            }
            match self.fetch_json(&url).await {
                Ok(v) => got.push((key.into(), v)),
                Err(e) => tracing::warn!(target: "twitch", "emotes: {e}"),
            }
        }
        let set = {
            let mut src = self.sources.lock();
            src.responses.retain(|k, _| {
                let p = k.split('.').next().unwrap_or("");
                matches!(p, "badges" | "twitch") || providers.iter().any(|x| x == p)
            });
            for (k, v) in got {
                let _ = self.ctx.db.kv_set("twitch.emotes", &k, &Value::from(v.clone()));
                src.responses.insert(k, v);
            }
            EmoteSet::build(&src)
        };
        let (n3, nb) = (set.third_party.len(), set.badges.len());
        self.dir.write().emotes = set;
        tracing::info!(target: "twitch", "emotes: {n3} third-party, {nb} badges");
        self.hub.emit(Event::new("twitch.emotes.updated", Origin::Twitch, Value::map().with("third_party", n3 as i64).with("badges", nb as i64)));
    }

    // ---- rewards --------------------------------------------------------------------------

    async fn sync_rewards(&self) {
        if !self.cfg().sync_rewards {
            return;
        }
        let Some(bid) = self.broadcaster_id() else { return };
        let _one = self.rewards_lock.lock().await;
        let defs = self.rewards_defs.read().clone();
        let fx = self.fx_enabled.load(Ordering::Relaxed);
        match rewards::sync(&self.helix, &self.ctx.db, &bid, &defs, fx).await {
            Ok((list, map)) => {
                *self.rewards_fx.lock() = Some(fx);
                let errors: Vec<String> = list.iter().filter(|s| s.status.starts_with("error")).map(|s| format!("{}: {}", s.key, s.status)).collect();
                for e in &errors {
                    self.hub.log("error", OWNER, format!("reward {e}"));
                }
                self.health.lock().rewards_error = (!errors.is_empty()).then(|| errors.join("; "));
                self.set("twitch.rewards", Value::List(list.iter().map(|s| s.value()).collect()));
                self.dir.write().rewards = map;
                tracing::info!(target: "twitch", "rewards synced: {}", list.iter().map(|s| format!("{}={}", s.key, s.status)).collect::<Vec<_>>().join(", "));
            }
            Err(e) => {
                self.hub.log("error", OWNER, format!("reward sync failed: {e}"));
                self.health.lock().rewards_error = Some(e);
            }
        }
        self.update_health();
    }

    /// Pause/unpause `fx = true` rewards when the operator turns effects off/on, once
    /// `fx.enabled` has settled for [`FX_DEBOUNCE`].
    async fn fx_watcher(self: Arc<Self>) {
        loop {
            self.fx_changed.notified().await;
            while tokio::time::timeout(FX_DEBOUNCE, self.fx_changed.notified()).await.is_ok() {}
            let on = self.fx_enabled.load(Ordering::Relaxed);
            if *self.rewards_fx.lock() == Some(on) || !self.rewards_defs.read().iter().any(|d| d.fx) {
                continue;
            }
            self.hub.log("info", OWNER, if on { "effects on: resuming effect rewards" } else { "effects off: pausing effect rewards" });
            self.sync_rewards().await;
        }
    }

    fn fx_seen(&self, on: bool) {
        if self.fx_enabled.swap(on, Ordering::Relaxed) != on {
            self.fx_changed.notify_one();
        }
    }

    // ---- config ---------------------------------------------------------------------------

    async fn config_watcher(self: Arc<Self>) {
        let mut rx = self.ctx.config.clone();
        while rx.changed().await.is_ok() {
            let config = rx.borrow_and_update().clone();
            let next = match TwitchCfg::parse(config.project.extra.get("twitch")) {
                Ok(c) => c,
                Err(e) => {
                    // keep the last good configuration
                    self.hub.log("error", OWNER, e);
                    continue;
                }
            };
            let (policy, perr) = PolicyCfg::from_config(&config);
            if perr.is_none() {
                self.dir.write().min_follow_s = (policy.follower_min_age_ms / 1000) as i64;
                *self.policy.write() = policy;
            }
            let (defs, _) = RewardDef::all(&config);
            let rewards_changed = *self.rewards_defs.read() != defs;
            *self.rewards_defs.write() = defs;
            let prev = self.cfg();
            let restart = prev.client_id != next.client_id
                || prev.secrets != next.secrets
                || prev.auth_url != next.auth_url
                || prev.eventsub_url != next.eventsub_url
                || prev.subscriptions_url != next.subscriptions_url
                || prev.sub_source != next.sub_source
                || prev.bot != next.bot;
            self.rate.lock().set_limit(next.chat_rate);
            self.helix.set_base(next.helix_url.clone());
            self.auth.set_cfg(next.clone());
            let providers_changed = prev.emote_providers != next.emote_providers;
            *self.cfg.write() = next.clone();
            self.set("twitch.client_id", next.client_id.clone());
            self.update_delay();
            if restart {
                self.hub.log("info", OWNER, "Twitch settings changed: reconnecting");
                if let Some(stop) = self.eventsub_stop.lock().take() {
                    let _ = stop.send(true);
                }
                *self.broadcaster.write() = None;
                let me = self.clone();
                tokio::spawn(async move { me.bootstrap().await });
            } else {
                if rewards_changed {
                    self.sync_rewards().await;
                }
                if providers_changed {
                    self.refresh_emotes().await;
                }
            }
            self.update_health();
        }
    }

    // ---- bus: audit, fx.enabled ----------------------------------------------------------

    /// Audit every policy decision (§12.1 "audit everything"); follow `fx.enabled` for rewards.
    async fn bus_listener(self: Arc<Self>) {
        let mut bus = self.hub.subscribe();
        loop {
            match bus.recv().await {
                Ok(b) => {
                    if let Bus::Changes(changes) = &*b
                        && let Some((_, v)) = changes.iter().rev().find(|(a, _)| a == "fx.enabled")
                    {
                        self.fx_seen(fx_on(Some(v)));
                    }
                    if let Bus::Event(e) = &*b
                        && e.ty == "fx.changed"
                    {
                        self.fx_seen(fx_on(e.payload.get_path("enabled")));
                    }
                    if let Bus::Event(e) = &*b
                        && let Some(kind) = e.ty.strip_prefix("policy.")
                    {
                        let p = &e.payload;
                        let g = |k: &str| p.get_path(k).map(|v| v.to_string()).filter(|s| !s.is_empty() && s != "null");
                        let what = [g("type"), g("reward"), g("id")].into_iter().flatten().collect::<Vec<_>>().join(" ");
                        let ok = matches!(kind, "accepted" | "approved" | "pending");
                        let actor = e.actor.as_ref().map(|a| a.name.clone());
                        let err = g("reason");
                        let _ = self.ctx.db.audit("policy", actor.as_deref(), &format!("{kind} {what}"), ok, err.as_deref());
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    tracing::warn!(target: "twitch", "audit listener lagged by {n} bus messages");
                    self.fx_seen(fx_on(self.hub.snapshot.load().get("fx.enabled")));
                }
                Err(_) => return,
            }
        }
    }

    // ---- health ---------------------------------------------------------------------------

    fn update_health(&self) {
        let h = self.health.lock().clone();
        let cfg = self.cfg();
        let (status, detail) = if cfg.client_id.is_empty() {
            ("fail", "no Client ID: set [twitch] client_id in project.toml, then run twitch.auth.start".to_string())
        } else {
            match &h.auth[0] {
                None if h.auth_pending[0] => ("warn", "authorization pending: enter the device code shown in the Twitch panel".to_string()),
                None => ("fail", "not authorized: run twitch.auth.start and enter the device code".to_string()),
                Some(Err(e)) => ("fail", format!("token invalid ({e}); run twitch.auth.start")),
                Some(Ok(())) => {
                    let t = self.auth.tokens(Account::Broadcaster);
                    let left = t.as_ref().map(|t| t.expires_at - unix_s()).unwrap_or(0);
                    let login = t.map(|t| t.login).unwrap_or_default();
                    let missing = self.auth.missing_scopes(Account::Broadcaster);
                    let mut warns: Vec<String> = Vec::new();
                    if !missing.is_empty() {
                        warns.push(format!("missing scopes {}: re-authorize", missing.join(" ")));
                    }
                    if !h.eventsub.failed.is_empty() {
                        warns.push(format!(
                            "{} EventSub subscription(s) refused ({})",
                            h.eventsub.failed.len(),
                            h.eventsub.failed.iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(", ")
                        ));
                    }
                    if !h.eventsub.revoked.is_empty() {
                        warns.push(format!("revoked: {}", h.eventsub.revoked.iter().map(|(t, r)| format!("{t} ({r})")).collect::<Vec<_>>().join(", ")));
                    }
                    if cfg.bot && !matches!(h.auth[1], Some(Ok(()))) {
                        warns.push("bot account not authorized (twitch.auth.start account=bot)".into());
                    }
                    if let Some(e) = &h.rewards_error {
                        warns.push(format!("rewards: {e}"));
                    }
                    if left < 600 {
                        warns.push(format!("token expires in {}s", left.max(0)));
                    }
                    if !h.eventsub.connected {
                        let why = if h.eventsub.last_error.is_empty() { "connecting".to_string() } else { h.eventsub.last_error.clone() };
                        ("fail", format!("authorized as {login}; EventSub not connected ({why})"))
                    } else if warns.is_empty() {
                        ("pass", format!("authorized as {login} (token {}m left); EventSub connected ({} subscriptions)", left / 60, h.eventsub.subscriptions))
                    } else {
                        ("warn", format!("authorized as {login}; EventSub connected; {}", warns.join("; ")))
                    }
                }
            }
        };
        self.set("health.twitch", Value::map().with("status", status).with("detail", detail));
    }

    // ---- queries --------------------------------------------------------------------------

    fn register_queries(self: &Arc<Self>) {
        let me = self.clone();
        self.hub.register_query(
            "emotes",
            Arc::new(move |_, _| {
                let v = me.dir.read().emotes.value();
                Box::pin(async move { Ok(v) })
            }),
        );
        let me = self.clone();
        self.hub.register_query(
            "twitch",
            Arc::new(move |name, args| {
                let me = me.clone();
                Box::pin(async move {
                    match name.as_str() {
                        "twitch.follow_age" => {
                            let uid = args.get_path("user_id").map(|v| v.to_string()).ok_or("needs user_id")?;
                            me.ensure_follow(&uid).await;
                            let e = me.dir.read().follows.get(&uid).copied();
                            let now = unix_s();
                            Ok(Value::map()
                                .with("following", e.is_some_and(|e| e.followed_at > 0))
                                .with("followed_at", e.filter(|e| e.followed_at > 0).map(|e| Value::Int(e.followed_at)).unwrap_or_default())
                                .with("age_s", e.and_then(|e| e.age_s(now)).unwrap_or(-1)))
                        }
                        // `{user_id}` → that viewer's cached roles; else the lists
                        "twitch.users" => {
                            let d = me.dir.read();
                            if let Some(uid) = args.get_path("user_id").map(|v| v.to_string()) {
                                let roles = d.users.roles(&uid);
                                return Ok(Value::map()
                                    .with("known", roles.is_some())
                                    .with("roles", Value::from(roles.unwrap_or_default().into_iter().map(se_core::policy::role_name).collect::<Vec<_>>())));
                            }
                            let list = |l: List| {
                                Value::List(d.users.listed(l).into_iter().map(|(id, login)| Value::map().with("id", id).with("login", login)).collect())
                            };
                            Ok(Value::map()
                                .with("moderators", list(List::Mods))
                                .with("vips", list(List::Vips))
                                .with("moderators_synced_at", d.users.synced_at(List::Mods))
                                .with("vips_synced_at", d.users.synced_at(List::Vips))
                                .with("known", d.users.len() as i64))
                        }
                        "twitch.scopes" => Ok(Value::map()
                            .with("broadcaster", Value::from(crate::config::SCOPES.iter().map(|s| s.to_string()).collect::<Vec<_>>()))
                            .with("bot", Value::from(crate::config::BOT_SCOPES.iter().map(|s| s.to_string()).collect::<Vec<_>>()))),
                        other => Err(format!("unknown query `{other}`")),
                    }
                })
            }),
        );
    }

    // ---- actions --------------------------------------------------------------------------

    async fn actions(self: Arc<Self>, mut rx: mpsc::UnboundedReceiver<Command>) {
        while let Some(c) = rx.recv().await {
            let me = self.clone();
            tokio::spawn(async move { me.action(c).await });
        }
    }

    fn dry_run(&self) -> bool {
        self.hub.snapshot.load().str("show.mode") == Some("rehearsal")
    }

    async fn action(self: Arc<Self>, c: Command) {
        let Op::Action { name, args } = &c.op else { return };
        let who = c.actor.as_ref().map(|a| a.name.clone());
        let mutating = !matches!(
            name.as_str(),
            "twitch.auth.start"
                | "twitch.auth.cancel"
                | "twitch.auth.logout"
                | "twitch.rewards.sync"
                | "twitch.emotes.refresh"
                | "twitch.users.refresh"
                | "twitch.delay.set"
        );
        if mutating && self.dry_run() {
            self.hub.log("info", OWNER, format!("[rehearsal dry-run] {}", c.op.describe()));
            self.hub.emit(Event::new("twitch.dry_run", Origin::Twitch, Value::map().with("action", name.clone()).with("args", args.clone())));
            let _ = self.ctx.db.audit("twitch", who.as_deref(), &format!("dry-run {}", c.op.describe()), true, None);
            return;
        }
        let res = self.clone().run_action(name, args).await;
        if let Err(e) = &res {
            self.hub.log("error", OWNER, format!("{name}: {e}"));
            self.hub.emit(Event::new("twitch.action.failed", Origin::Twitch, Value::map().with("action", name.clone()).with("error", e.clone())));
        }
        if mutating {
            let _ = self.ctx.db.audit("twitch", who.as_deref(), &c.op.describe(), res.is_ok(), res.as_ref().err().map(String::as_str));
        }
    }

    fn need_broadcaster(&self) -> Result<String, String> {
        self.broadcaster_id().ok_or_else(|| "Twitch is not connected (authorize first: twitch.auth.start)".into())
    }

    /// Resolve `user_id` or `user` (login or display name) to a user id.
    async fn user_id(&self, args: &Value) -> Result<String, String> {
        if let Some(id) = arg_str(args, "user_id", usize::MAX) {
            return Ok(id);
        }
        let login = arg_str(args, "user", 0).ok_or("needs `user` or `user_id`")?.trim_start_matches('@').to_lowercase();
        if let Some(id) = self.users.lock().get(&login) {
            return Ok(id.clone());
        }
        let v = self.helix.get(Account::Broadcaster, "/users", &[("login", login.clone())]).await.map_err(|e| e.to_string())?;
        let id = first(&v).and_then(|u| u.get("id")).and_then(J::as_str).ok_or_else(|| format!("no Twitch user `{login}`"))?.to_string();
        self.users.lock().insert(login, id.clone());
        Ok(id)
    }

    async fn helix(&self, m: Method, path: &str, q: &[(&str, String)], body: Option<J>) -> Result<J, String> {
        self.helix.call(Account::Broadcaster, m, path, q, body.as_ref()).await.map_err(|e: HelixError| e.to_string())
    }

    async fn run_action(self: Arc<Self>, name: &str, args: &Value) -> Result<(), String> {
        match name {
            "twitch.auth.start" => {
                let a = Account::parse(&arg_str(args, "account", 0).unwrap_or_default()).ok_or("account must be `broadcaster` or `bot`")?;
                self.device_flow(a).await
            }
            "twitch.auth.cancel" => {
                let a = Account::parse(&arg_str(args, "account", 0).unwrap_or_default()).ok_or("account must be `broadcaster` or `bot`")?;
                if let Some(tx) = self.device_cancel.lock()[a as usize].take() {
                    let _ = tx.send(true);
                }
                Ok(())
            }
            "twitch.auth.logout" => {
                let a = Account::parse(&arg_str(args, "account", 0).unwrap_or_default()).ok_or("account must be `broadcaster` or `bot`")?;
                self.auth.logout(a).map_err(|e| format!("{e:#}"))?;
                let p = auth_prefix(a);
                self.set(&format!("{p}.status"), "none");
                self.set(&format!("{p}.login"), "");
                self.health.lock().auth[a as usize] = None;
                if a == Account::Broadcaster {
                    if let Some(stop) = self.eventsub_stop.lock().take() {
                        let _ = stop.send(true);
                    }
                    *self.broadcaster.write() = None;
                    self.set("twitch.eventsub.connected", false);
                    self.health.lock().eventsub = Status::default();
                }
                self.update_health();
                Ok(())
            }
            "twitch.chat.send" => {
                let text = arg_str(args, "text", 0).ok_or("needs `text`")?;
                let account = match arg_str(args, "as", usize::MAX).as_deref() {
                    Some("bot") => Account::Bot,
                    Some("broadcaster") => Account::Broadcaster,
                    _ => match self.cfg().chat_as {
                        ChatAs::Bot if self.auth.tokens(Account::Bot).is_some() => Account::Bot,
                        _ => Account::Broadcaster,
                    },
                };
                self.chat_tx
                    .try_send(ChatOut { text, reply_to: arg_str(args, "reply_to", usize::MAX), account })
                    .map_err(|_| "chat queue is full (rate limit); message dropped".to_string())
            }
            "twitch.refund" | "twitch.fulfill" => {
                let rid = arg_str(args, "redemption_id", 0).ok_or("needs `redemption_id`")?;
                if rid.starts_with("sim-") {
                    self.hub.log("info", OWNER, format!("{name}: simulated redemption {rid}, nothing to do on Twitch"));
                    return Ok(());
                }
                let reward = arg_str(args, "reward_id", 1).or_else(|| self.redemptions.lock().get(&rid).cloned()).ok_or("needs `reward_id`")?;
                let bid = self.need_broadcaster()?;
                let status = if name == "twitch.refund" { "CANCELED" } else { "FULFILLED" };
                let q = [("id", rid.clone()), ("broadcaster_id", bid), ("reward_id", reward)];
                self.helix(Method::Patch, "/channel_points/custom_rewards/redemptions", &q, Some(json!({ "status": status }))).await?;
                self.redemptions.lock().remove(&rid);
                Ok(())
            }
            "twitch.marker" => {
                let bid = self.need_broadcaster()?;
                let desc: String = arg_str(args, "description", 0).unwrap_or_default().chars().take(140).collect();
                let v = self.helix(Method::Post, "/streams/markers", &[], Some(json!({ "user_id": bid, "description": desc }))).await?;
                let m = first(&v).cloned().unwrap_or(J::Null);
                self.hub.emit(Event::new(
                    "twitch.marker.created",
                    Origin::Twitch,
                    Value::map()
                        .with("id", m.get("id").and_then(J::as_str).unwrap_or("").to_string())
                        .with("position_s", m.get("position_seconds").and_then(J::as_i64).unwrap_or(0))
                        .with("description", desc),
                ));
                Ok(())
            }
            "twitch.poll.start" => {
                let bid = self.need_broadcaster()?;
                let title: String = arg_str(args, "title", 0).ok_or("needs `title`")?.chars().take(60).collect();
                let choices = arg_list(args, "choices", 1);
                if !(2..=5).contains(&choices.len()) {
                    return Err("a poll needs 2–5 choices".into());
                }
                let duration = arg_secs(args, "duration", 2).unwrap_or(120).clamp(15, 1800);
                let mut body = json!({
                    "broadcaster_id": bid, "title": title, "duration": duration,
                    "choices": choices.iter().map(|c| json!({ "title": c.chars().take(25).collect::<String>() })).collect::<Vec<_>>(),
                });
                if let Some(p) = arg(args, "points", usize::MAX).and_then(|v| v.as_i64()).filter(|p| *p > 0) {
                    body["channel_points_voting_enabled"] = json!(true);
                    body["channel_points_per_vote"] = json!(p.min(1_000_000));
                }
                self.helix(Method::Post, "/polls", &[], Some(body)).await.map(|_| ())
            }
            "twitch.poll.end" => {
                let bid = self.need_broadcaster()?;
                let id = arg_str(args, "id", 0)
                    .or_else(|| self.hub.snapshot.load().get("twitch.poll").and_then(|p| p.get_path("id")).map(|v| v.to_string()))
                    .ok_or("no poll running")?;
                let status = if arg(args, "archive", usize::MAX).is_some_and(|v| v.truthy()) { "ARCHIVED" } else { "TERMINATED" };
                self.helix(Method::Patch, "/polls", &[], Some(json!({ "broadcaster_id": bid, "id": id, "status": status }))).await.map(|_| ())
            }
            "twitch.prediction.start" => {
                let bid = self.need_broadcaster()?;
                let title: String = arg_str(args, "title", 0).ok_or("needs `title`")?.chars().take(45).collect();
                let outcomes = arg_list(args, "outcomes", 1);
                if !(2..=10).contains(&outcomes.len()) {
                    return Err("a prediction needs 2–10 outcomes".into());
                }
                let window = arg_secs(args, "window", 2).unwrap_or(120).clamp(30, 1800);
                let body = json!({
                    "broadcaster_id": bid, "title": title, "prediction_window": window,
                    "outcomes": outcomes.iter().map(|o| json!({ "title": o.chars().take(25).collect::<String>() })).collect::<Vec<_>>(),
                });
                self.helix(Method::Post, "/predictions", &[], Some(body)).await.map(|_| ())
            }
            "twitch.prediction.lock" | "twitch.prediction.cancel" | "twitch.prediction.resolve" => {
                let bid = self.need_broadcaster()?;
                let current = self.hub.snapshot.load().get("twitch.prediction").cloned().unwrap_or_default();
                let id = arg_str(args, "id", usize::MAX)
                    .or_else(|| current.get_path("id").map(|v| v.to_string()))
                    .filter(|s| !s.is_empty())
                    .ok_or("no prediction open")?;
                let mut body = json!({ "broadcaster_id": bid, "id": id });
                match name {
                    "twitch.prediction.lock" => body["status"] = json!("LOCKED"),
                    "twitch.prediction.cancel" => body["status"] = json!("CANCELED"),
                    _ => {
                        let w = arg(args, "winner", 0).ok_or("needs `winner` (outcome id, title, or 1-based number)")?;
                        let outcomes = current.get_path("outcomes").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
                        let pick = |o: &Value| o.get_path("id").map(|v| v.to_string());
                        let winner = match &w {
                            Value::Int(n) => outcomes.get((*n as usize).saturating_sub(1)).and_then(pick),
                            v => {
                                let s = v.to_string();
                                outcomes
                                    .iter()
                                    .find(|o| {
                                        pick(o).as_deref() == Some(s.as_str()) || o.get_path("title").is_some_and(|t| t.to_string().eq_ignore_ascii_case(&s))
                                    })
                                    .and_then(pick)
                                    .or(Some(s))
                            }
                        };
                        body["status"] = json!("RESOLVED");
                        body["winning_outcome_id"] = json!(winner.ok_or("unknown outcome")?);
                    }
                }
                self.helix(Method::Patch, "/predictions", &[], Some(body)).await.map(|_| ())
            }
            "twitch.shoutout" => {
                let bid = self.need_broadcaster()?;
                let to = self.user_id(args).await?;
                let q = [("from_broadcaster_id", bid.clone()), ("to_broadcaster_id", to), ("moderator_id", bid)];
                self.helix(Method::Post, "/chat/shoutouts", &q, None).await.map(|_| ())
            }
            "twitch.raid" => {
                let bid = self.need_broadcaster()?;
                let to = self.user_id(args).await?;
                self.helix(Method::Post, "/raids", &[("from_broadcaster_id", bid), ("to_broadcaster_id", to)], None).await.map(|_| ())
            }
            "twitch.raid.cancel" => {
                let bid = self.need_broadcaster()?;
                self.helix(Method::Delete, "/raids", &[("broadcaster_id", bid)], None).await.map(|_| ())
            }
            "twitch.ad.snooze" => {
                let bid = self.need_broadcaster()?;
                let v = self.helix(Method::Post, "/channels/ads/schedule/snooze", &[("broadcaster_id", bid)], None).await?;
                if let Some(a) = first(&v) {
                    let mut merged = a.clone();
                    merged["duration"] = json!(self.ads.lock().duration);
                    self.apply_ad_schedule(&merged);
                }
                self.poll_ads().await;
                Ok(())
            }
            "twitch.rewards.sync" => {
                self.sync_rewards().await;
                Ok(())
            }
            "twitch.emotes.refresh" => {
                self.refresh_emotes().await;
                Ok(())
            }
            "twitch.users.refresh" => {
                self.sync_roles().await;
                Ok(())
            }
            "twitch.delay.set" => {
                let ms = arg(args, "ms", 0).and_then(|v| v.as_i64()).ok_or("needs `ms` (negative clears the manual value)")?;
                let manual = (ms >= 0).then_some(ms);
                self.delay.lock().manual_ms = manual;
                let _ = match manual {
                    Some(m) => self.ctx.db.kv_set("twitch", "delay.manual_ms", &Value::Int(m)),
                    None => self.ctx.db.kv_del("twitch", "delay.manual_ms"),
                };
                self.update_delay();
                Ok(())
            }
            "mod.ban" | "mod.timeout" => {
                let bid = self.need_broadcaster()?;
                let uid = self.user_id(args).await?;
                let mut data = json!({ "user_id": uid });
                if let Some(r) = arg_str(args, "reason", 1) {
                    data["reason"] = json!(r.chars().take(500).collect::<String>());
                }
                if name == "mod.timeout" {
                    data["duration"] =
                        json!(arg_secs(args, "duration_s", usize::MAX).or_else(|| arg_secs(args, "duration", usize::MAX)).unwrap_or(600).clamp(1, 1_209_600));
                }
                let q = [("broadcaster_id", bid.clone()), ("moderator_id", bid)];
                self.helix(Method::Post, "/moderation/bans", &q, Some(json!({ "data": data }))).await.map(|_| ())
            }
            "mod.unban" => {
                let bid = self.need_broadcaster()?;
                let uid = self.user_id(args).await?;
                self.helix(Method::Delete, "/moderation/bans", &[("broadcaster_id", bid.clone()), ("moderator_id", bid), ("user_id", uid)], None)
                    .await
                    .map(|_| ())
            }
            "mod.delete" => {
                let bid = self.need_broadcaster()?;
                let mid = arg_str(args, "message_id", 0).ok_or("needs `message_id`")?;
                self.helix(Method::Delete, "/moderation/chat", &[("broadcaster_id", bid.clone()), ("moderator_id", bid), ("message_id", mid)], None)
                    .await
                    .map(|_| ())
            }
            "mod.automod.approve" | "mod.automod.deny" => {
                let bid = self.need_broadcaster()?;
                let mid = arg_str(args, "message_id", 0).or_else(|| arg_str(args, "id", usize::MAX)).ok_or("needs `message_id`")?;
                let action = if name.ends_with("approve") { "ALLOW" } else { "DENY" };
                self.helix(Method::Post, "/moderation/automod/message", &[], Some(json!({ "user_id": bid, "msg_id": mid, "action": action }))).await?;
                let mut q = self.automod.lock();
                q.retain(|m| m.get_path("message_id").map(|v| v.to_string()) != Some(mid.clone()));
                self.publish_automod(&q);
                Ok(())
            }
            "mod.blocked_terms.add" => {
                let bid = self.need_broadcaster()?;
                let text = arg_str(args, "text", 0).ok_or("needs `text`")?;
                if !(2..=500).contains(&text.chars().count()) {
                    return Err("blocked terms are 2–500 characters".into());
                }
                self.helix(Method::Post, "/moderation/blocked_terms", &[("broadcaster_id", bid.clone()), ("moderator_id", bid)], Some(json!({ "text": text })))
                    .await
                    .map(|_| ())
            }
            "mod.blocked_terms.remove" => {
                let bid = self.need_broadcaster()?;
                let id = arg_str(args, "id", 0).ok_or("needs `id`")?;
                self.helix(Method::Delete, "/moderation/blocked_terms", &[("broadcaster_id", bid.clone()), ("moderator_id", bid), ("id", id)], None)
                    .await
                    .map(|_| ())
            }
            other => Err(format!("unknown action `{other}`")),
        }
    }

    /// Device code flow for one account (UI: `twitch.auth.*` state).
    async fn device_flow(self: Arc<Self>, a: Account) -> Result<(), String> {
        if a == Account::Bot && !self.cfg().bot {
            return Err("enable the bot account first: [twitch] bot = true".into());
        }
        let p = auth_prefix(a);
        let dc: DeviceCode = self.auth.device_start(a).await.map_err(|e| format!("{e:#}"))?;
        let (cancel_tx, mut cancel) = watch::channel(false);
        if let Some(old) = self.device_cancel.lock()[a as usize].replace(cancel_tx) {
            let _ = old.send(true);
        }
        self.set(&format!("{p}.status"), "pending");
        self.set(&format!("{p}.user_code"), dc.user_code.clone());
        self.set(&format!("{p}.verification_uri"), dc.verification_uri.clone());
        self.set(&format!("{p}.expires_in_s"), dc.expires_in);
        self.set(&format!("{p}.error"), "");
        self.health.lock().auth_pending[a as usize] = true;
        self.update_health();
        self.hub.log("info", OWNER, format!("Twitch {} login: open {} and enter {}", a.as_str(), dc.verification_uri, dc.user_code));
        let me = self.clone();
        tokio::spawn(async move {
            let deadline = Instant::now() + Duration::from_secs(dc.expires_in.max(1) as u64);
            let mut interval = dc.interval.max(1) as u64;
            let outcome = loop {
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
                    _ = cancel.changed() => break Err("cancelled".to_string()),
                }
                if Instant::now() >= deadline {
                    break Err("the device code expired; start again".into());
                }
                me.set(&format!("{p}.expires_in_s"), deadline.saturating_duration_since(Instant::now()).as_secs() as i64);
                match me.auth.device_poll(a, &dc).await {
                    Ok(Poll::Pending) => {}
                    Ok(Poll::SlowDown) => interval += 5,
                    Ok(Poll::Done(_)) => break Ok(()),
                    Err(e) => break Err(format!("{e:#}")),
                }
            };
            me.health.lock().auth_pending[a as usize] = false;
            me.device_cancel.lock()[a as usize] = None;
            match outcome {
                Ok(()) => {
                    me.hub.log("info", OWNER, format!("Twitch {} account authorized", a.as_str()));
                    me.authorized(a).await;
                }
                Err(e) => {
                    me.set(&format!("{p}.status"), if e == "cancelled" { "none" } else { "error" });
                    me.set(&format!("{p}.error"), e.clone());
                    me.set(&format!("{p}.user_code"), "");
                    if e != "cancelled" {
                        me.hub.log("error", OWNER, format!("Twitch {} authorization: {e}", a.as_str()));
                    }
                    me.update_health();
                }
            }
        });
        Ok(())
    }

    /// Rate-limited chat sender (one message at a time; waits for the rolling window).
    async fn chat_sender(self: Arc<Self>) {
        let Some(mut rx) = self.chat_rx.lock().take() else { return };
        while let Some(m) = rx.recv().await {
            loop {
                let wait = self.rate.lock().wait(Instant::now());
                match wait {
                    Some(d) => tokio::time::sleep(d).await,
                    None => break,
                }
            }
            let Some(bid) = self.broadcaster_id() else {
                self.hub.log("warn", OWNER, "chat message dropped: Twitch not connected");
                continue;
            };
            let account = if m.account == Account::Bot && self.bot.read().is_none() { Account::Broadcaster } else { m.account };
            let sender = match account {
                Account::Bot => self.bot.read().as_ref().map(|b| b.id.clone()).unwrap_or_else(|| bid.clone()),
                Account::Broadcaster => bid.clone(),
            };
            let mut body = json!({ "broadcaster_id": bid, "sender_id": sender, "message": cap(&m.text) });
            if let Some(r) = &m.reply_to {
                body["reply_parent_message_id"] = json!(r);
            }
            self.rate.lock().record(Instant::now());
            let sent_at = Instant::now();
            match self.helix.call(account, Method::Post, "/chat/messages", &[], Some(&body)).await {
                Ok(v) => {
                    let d = first(&v).cloned().unwrap_or(J::Null);
                    if d.get("is_sent").and_then(J::as_bool) == Some(false) {
                        let why = d.pointer("/drop_reason/message").and_then(J::as_str).unwrap_or("unknown").to_string();
                        self.hub.log("warn", OWNER, format!("chat message not sent: {why}"));
                    } else if let Some(mid) = d.get("message_id").and_then(J::as_str) {
                        let mut echo = self.echo.lock();
                        if echo.len() > 100 {
                            echo.clear();
                        }
                        echo.insert(mid.to_string(), sent_at);
                    }
                }
                Err(e) => self.hub.log("error", OWNER, format!("chat send failed: {e}")),
            }
        }
    }
}
