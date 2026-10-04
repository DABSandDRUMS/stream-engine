//! The song-request actor: owns the queue, policy, player controller and lookups, and talks to
//! the hub (actions `queue.*`, `youtube.*`, `relay.*`; queries `queue`, `queue.*`,
//! `song.player`, `youtube.status`, `relay.status`; state `queue.*`, `song.*` (incl.
//! `song.current.{title,artist,genres,year}` from [`crate::metadata`]); signal
//! `song.position`, `queue.position` (0 … 1 through the current request); events `queue.song_requested|song_started|song_ended|song_error`).

use crate::lookup::{Failed, Found, Lookup};
use crate::metadata::{self, SongInfo};
use crate::player::{Outcome, Player};
use crate::policy::{self, Policy, QueueFacts, Reject, Requester};
use crate::queue::{Entry, Paid, Queue, Status};
use crate::quota::{self, Ledger, Mode};
use crate::relay::Relay;
use crate::secrets::{RELAY_SECRET, SecretStore, YOUTUBE_KEY};
use crate::settings::Settings;
use crate::store::Store;
use crate::text::{self, fmt_duration, now_ms};
use crate::youtube::{self, ApiError, Client};
use anyhow::Result;
use jiff::Timestamp;
use se_hub::{EngineCtx, Hub};
use se_proto::{Command, Event, Meta, Op, Origin, Role, Value, ValueType};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const OWNER: &str = "songs";
/// A video id that exists for key validation (1 unit).
const PROBE_VIDEO: &str = "dQw4w9WgXcQ";
const POSITION_HZ: u64 = 30;

/// Messages into the actor.
pub enum Msg {
    Action(Command),
    Report(Value, Option<oneshot::Sender<Result<Value, String>>>),
    Query(String, Value, oneshot::Sender<Result<Value, String>>),
    /// A request after its (optional) follow-age lookup.
    Requested(Box<Pending>),
    Looked(Box<Pending>, Result<Found, Failed>),
    KeyChecked {
        key: String,
        result: Result<(), ApiError>,
    },
    KeyStored(Option<String>, Result<(), String>),
    SecretStored(Option<String>, Result<(), String>),
    Settings(Box<Settings>),
    SettingsInvalid(String),
    /// A song metadata lookup finished.
    Meta(metadata::Done),
}

/// A request travelling through gate → lookup → queue.
pub struct Pending {
    pub req: Requester,
    pub text: String,
    /// Reply in chat (chat/Twitch-originated) vs. the engine log (operator).
    pub chat: bool,
    pub causal: u64,
}

/// Handle returned by [`spawn`] (tests inject actions/queries through it).
#[derive(Clone)]
pub struct Handle {
    pub tx: mpsc::UnboundedSender<Msg>,
}

impl Handle {
    pub async fn query(&self, name: &str, args: Value) -> Result<Value, String> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(Msg::Query(name.to_string(), args, tx)).map_err(|_| "songs stopped".to_string())?;
        rx.await.map_err(|_| "songs stopped".to_string())?
    }
}

fn declare(hub: &Hub) {
    let ro = |m: Meta, d: &str| m.readonly().owner(OWNER).describe(d);
    hub.declare("queue.open", ro(Meta::boolean(true), "Song requests are open"));
    hub.declare("queue.paused", ro(Meta::boolean(false), "Playback paused by the operator/mods"));
    hub.declare("queue.playing", ro(Meta::boolean(false), "A song is audibly playing (not paused/buffering/ad)"));
    hub.declare("queue.length", ro(Meta::int(0, [0.0, 10_000.0]), "Songs waiting to play (excluding the current one)"));
    hub.declare("queue.pending", ro(Meta::int(0, [0.0, 10_000.0]), "Requests awaiting mod approval"));
    hub.declare("queue.now.title", ro(Meta::string(""), "Now playing: title"));
    hub.declare("queue.now.user", ro(Meta::string(""), "Now playing: requester"));
    hub.declare("queue.now.id", ro(Meta::string(""), "Now playing: YouTube video id"));
    hub.declare("queue.now.channel", ro(Meta::string(""), "Now playing: channel"));
    hub.declare("queue.now.entry", ro(Meta::int(0, [0.0, 9e15]), "Now playing: queue entry id"));
    hub.declare("queue.now.duration", ro(Meta::float(0.0, [0.0, 86_400.0]).unit("s"), "Now playing: duration"));
    hub.declare("queue.next.title", ro(Meta::string(""), "Up next: title"));
    hub.declare("queue.next.user", ro(Meta::string(""), "Up next: requester"));
    hub.declare("queue.url", ro(Meta::string(""), "Public queue page (relay)"));
    hub.declare("queue.theme", ro(Meta::enumeration("win31", &["win31", "modern"]), "Public queue page theme"));
    hub.declare("queue.lookup", ro(Meta::enumeration("off", &["full", "links", "library", "off"]), "What requests can use right now (quota/key)"));
    hub.declare("queue.quota.used", ro(Meta::int(0, [0.0, 1e9]).unit("units"), "YouTube API units used today (Pacific)"));
    hub.declare("queue.quota.remaining", ro(Meta::int(0, [0.0, 1e9]).unit("units"), "YouTube API units left today"));
    hub.declare("song.state", ro(Meta::enumeration("idle", &["idle", "loading", "buffering", "playing", "paused", "ad"]), "Player state"));
    hub.declare("song.duration", ro(Meta::float(0.0, [0.0, 86_400.0]).unit("s"), "Current song duration"));
    hub.declare(
        "song.media",
        ro(
            Meta::string(""),
            "Timeline media key of the song player's current song (`yt:<id>`); only the song queue sets it (local media files report `source.<n>.media`)",
        ),
    );
    hub.declare("song.player", ro(Meta { ty: ValueType::Map, ..Default::default() }, "Desired state of the player page (web/player.html)"));
    hub.declare("song.volume", Meta::float(1.0, [0.0, 1.0]).owner(OWNER).describe("YouTube player volume (the music bus fader comes after it)"));
    hub.declare("song.current.title", ro(Meta::string(""), "Current song: song title (MusicBrainz, else read from the video title); empty when no song"));
    hub.declare("song.current.artist", ro(Meta::string(""), "Current song: artist (MusicBrainz, else read from the video title); empty when unknown"));
    hub.declare(
        "song.current.genres",
        ro(Meta { ty: ValueType::List, default: Value::List(Vec::new()), ..Default::default() }, "Current song: genres, lowercase (MusicBrainz); empty when unknown"),
    );
    hub.declare("song.current.year", ro(Meta { ty: ValueType::Any, default: Value::Null, ..Default::default() }, "Current song: first-release year (int) or null"));
    for h in ["health.youtube", "health.player"] {
        hub.declare(h, ro(Meta { ty: ValueType::Map, ..Default::default() }, "song requests"));
    }
}

/// Start the actor. Returns once state is declared and handlers are registered.
pub async fn spawn(ctx: EngineCtx, secrets: Arc<dyn SecretStore>) -> Result<Handle> {
    let hub = ctx.hub.clone();
    let store = Store::open(ctx.db.clone())?;
    let settings = match Settings::parse(ctx.project_section("songs").as_ref(), ctx.project_section("relay").as_ref()) {
        Ok(s) => s,
        Err(e) => {
            hub.log("error", "songs", format!("project.toml: {e}; using defaults"));
            Settings::default()
        }
    };
    let ledger = Ledger::new(ctx.db.clone(), settings.daily_quota, settings.link_reserve)?;
    let yt = Arc::new(Client::new(&settings.api_base)?);
    let (youtube_key, relay_secret) = {
        let s = secrets.clone();
        tokio::task::spawn_blocking(move || {
            let get = |n: &str| match s.get(n) {
                Ok(v) => v.filter(|v| !v.is_empty()),
                Err(e) => {
                    tracing::warn!("keyring: {n}: {e:#}");
                    None
                }
            };
            (get(YOUTUBE_KEY), get(RELAY_SECRET))
        })
        .await?
    };
    declare(&hub);
    let relay = Relay::spawn(hub.clone(), store.clone());
    crate::queue_page::spawn(hub.clone());
    relay.configure(settings.relay_url.clone(), relay_secret.clone());

    let policy: Policy = store.kv_get("policy").and_then(|v| serde_json::from_value(serde_json::Value::from(&v)).ok()).unwrap_or_default();
    let paused = store.kv_get("paused").is_some_and(|v| v.truthy());
    let theme = match store.kv_get("theme").as_ref().and_then(Value::as_str) {
        Some("modern") => "modern",
        _ => "win31",
    };
    let (current, upcoming, pending) = store.load_active()?;
    let mut player = Player::new(settings.crossfade_ms);
    player.set_required_account(&settings.youtube_channel, &settings.youtube_delegate, Instant::now());
    if let Some(c) = &current {
        let at = store
            .kv_get("position")
            .filter(|p| p.get_path("entry").and_then(Value::as_i64) == Some(c.id))
            .and_then(|p| p.get_path("t").and_then(Value::as_f64))
            .unwrap_or(0.0);
        player.play(c.id, &c.video, at, paused);
        tracing::info!("songs: holding restored \"{}\" at {} until the embedded channel is verified", c.title, fmt_duration(at as u32));
    }

    let (tx, rx) = mpsc::unbounded_channel();
    let meta_worker = {
        let tx = tx.clone();
        metadata::Worker::spawn(store.clone(), meta_client(&settings, &hub), move |d| {
            let _ = tx.send(Msg::Meta(d));
        })
    };
    let mut songs = Songs {
        hub: hub.clone(),
        tx: tx.clone(),
        store,
        secrets,
        yt,
        key: youtube_key,
        relay_secret,
        ledger,
        settings,
        policy,
        queue: Queue { current, upcoming, pending },
        player,
        paused,
        theme,
        votes: HashSet::new(),
        inflight: HashMap::new(),
        mode: None,
        last_api_error: None,
        published: BTreeMap::new(),
        player_rev: 0,
        relay,
        last_pos: f32::NAN,
        last_pos_save: Instant::now(),
        anchor: ((0, "idle", 0), 0.0, 0, Instant::now()),
        dirty: true,
        meta: HashMap::new(),
        meta_pending: HashSet::new(),
        meta_worker,
    };
    let restored: Vec<(String, String, String)> =
        songs.queue.current.iter().chain(&songs.queue.upcoming).chain(&songs.queue.pending).map(|e| (e.video.clone(), e.title.clone(), e.channel.clone())).collect();
    for (video, title, channel) in restored {
        songs.ensure_meta(&video, &title, &channel);
    }
    songs.sync_preload();
    songs.check_mode(false);

    // actions
    for prefix in ["queue", "youtube", "relay"] {
        let mut rx = hub.route_actions(prefix);
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(c) = rx.recv().await {
                if tx.send(Msg::Action(c)).is_err() {
                    break;
                }
            }
        });
    }
    // queries
    for prefix in ["queue", "youtube", "relay"] {
        let tx = tx.clone();
        hub.register_query(
            prefix,
            Arc::new(move |name, args| {
                let tx = tx.clone();
                Box::pin(async move {
                    let (otx, orx) = oneshot::channel();
                    tx.send(Msg::Query(name, args, otx)).map_err(|_| "song requests stopped".to_string())?;
                    orx.await.map_err(|_| "song requests stopped".to_string())?
                })
            }),
        );
    }
    {
        let tx = tx.clone();
        hub.register_query(
            "song.player",
            Arc::new(move |_, args| {
                let tx = tx.clone();
                Box::pin(async move {
                    let hello = args.get_path("ev").and_then(Value::as_str) == Some("hello");
                    if hello {
                        let (otx, orx) = oneshot::channel();
                        tx.send(Msg::Report(args, Some(otx))).map_err(|_| "song requests stopped".to_string())?;
                        orx.await.map_err(|_| "song requests stopped".to_string())?
                    } else {
                        tx.send(Msg::Report(args, None)).map_err(|_| "song requests stopped".to_string())?;
                        Ok(Value::Null)
                    }
                })
            }),
        );
    }
    // hot reload of [songs]/[relay]
    {
        let (tx, ctx) = (tx.clone(), ctx.clone());
        let mut cfg = ctx.config.clone();
        tokio::spawn(async move {
            while cfg.changed().await.is_ok() {
                match Settings::parse(ctx.project_section("songs").as_ref(), ctx.project_section("relay").as_ref()) {
                    Ok(s) => {
                        if tx.send(Msg::Settings(Box::new(s))).is_err() {
                            break;
                        }
                    }
                    Err(e) => {
                        ctx.hub.log("error", "songs", format!("project.toml: {e}; playback locked"));
                        if tx.send(Msg::SettingsInvalid(e)).is_err() {
                            break;
                        }
                    }
                }
            }
        });
    }
    tokio::spawn(async move { songs.run(rx).await });
    Ok(Handle { tx })
}

pub struct Songs {
    hub: Arc<Hub>,
    tx: mpsc::UnboundedSender<Msg>,
    store: Store,
    secrets: Arc<dyn SecretStore>,
    yt: Arc<Client>,
    key: Option<String>,
    relay_secret: Option<String>,
    ledger: Ledger,
    settings: Settings,
    policy: Policy,
    queue: Queue,
    player: Player,
    paused: bool,
    theme: &'static str,
    votes: HashSet<String>,
    /// Lookups in flight per login (count toward per-user limits).
    inflight: HashMap<String, usize>,
    /// Last announced lookup mode.
    mode: Option<&'static str>,
    last_api_error: Option<ApiError>,
    published: BTreeMap<&'static str, Value>,
    player_rev: u64,
    relay: Relay,
    last_pos: f32,
    last_pos_save: Instant,
    /// `((entry, state, player rev), position, unix ms, taken)`.
    anchor: ((i64, &'static str, u64), f64, i64, Instant),
    dirty: bool,
    /// Metadata of queued/current songs by video id (MusicBrainz match, else parsed title).
    meta: HashMap<String, SongInfo>,
    /// Videos with a lookup queued in the worker.
    meta_pending: HashSet<String>,
    meta_worker: metadata::Worker,
}

/// The MusicBrainz client for these settings (`None` when `[songs] metadata = false`).
fn meta_client(s: &Settings, hub: &Hub) -> Option<Arc<metadata::Client>> {
    if !s.metadata {
        return None;
    }
    match metadata::Client::new(&s.metadata_base) {
        Ok(c) => Some(Arc::new(c)),
        Err(e) => {
            hub.log("error", "songs", format!("MusicBrainz client: {e:#}"));
            None
        }
    }
}

fn arg_str(args: &Value, k: &str) -> Option<String> {
    args.get_path(k).map(|v| v.to_string()).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// First positional argument (`queue.reject 12` → `args.args[0]`).
fn positional(args: &Value, i: usize) -> Option<&Value> {
    args.get_path("args").and_then(Value::as_list).and_then(|l| l.get(i))
}

/// The request text: `text`/`query`/`input`/`url`, followed by any positional words (the
/// one-line syntax splits `text=bohemian rhapsody` into `text=bohemian` + `rhapsody`).
fn request_text(args: &Value) -> String {
    let keyed = ["text", "query", "input", "url"].iter().find_map(|k| arg_str(args, k));
    let rest = match args.get_path("args") {
        Some(Value::List(l)) => l.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(" "),
        Some(Value::Str(s)) => s.clone(),
        _ => String::new(),
    };
    match keyed {
        Some(k) if rest.trim().is_empty() => k,
        Some(k) => format!("{k} {}", rest.trim()),
        None => rest.trim().to_string(),
    }
}

fn chat_origin(c: &Command) -> bool {
    matches!(c.origin, Origin::Chat | Origin::Twitch | Origin::Relay) || c.actor.as_ref().is_some_and(|a| a.platform == "twitch")
}

impl Songs {
    async fn run(mut self, mut rx: mpsc::UnboundedReceiver<Msg>) {
        let mut pos_tick = tokio::time::interval(Duration::from_millis(1000 / POSITION_HZ));
        pos_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut slow = tokio::time::interval(Duration::from_secs(1));
        slow.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        self.flush();
        loop {
            tokio::select! {
                m = rx.recv() => {
                    let Some(m) = m else { break };
                    self.handle(m);
                    while let Ok(m) = rx.try_recv() {
                        self.handle(m);
                    }
                    self.flush();
                }
                _ = pos_tick.tick() => self.tick_position(),
                _ = slow.tick() => {
                    self.tick_slow();
                    self.flush();
                }
            }
        }
    }

    fn handle(&mut self, m: Msg) {
        let rev = self.player.rev();
        self.player.expire_account(Instant::now());
        self.dirty |= self.player.rev() != rev;
        match m {
            Msg::Action(c) => self.action(c),
            Msg::Report(r, reply) => {
                let now = Instant::now();
                if r.get_path("ev").and_then(Value::as_str) == Some("hello") {
                    let page = r.get_path("page").and_then(Value::as_str).unwrap_or("");
                    let d = self.player.hello(page, now);
                    self.player_rev = 0; // force republish
                    self.dirty = true;
                    if let Some(reply) = reply {
                        let _ = reply.send(Ok(d.with("volume", self.volume())));
                    }
                    return;
                }
                for o in self.player.report(&r, now) {
                    self.player_outcome(o);
                }
                // Unverified identity already stops both player slots and reports
                // `queue.paused`; never persist a pause for it. A page reconnect or
                // restart must resume on its own once the approved channel verifies.
                if r.get_path("ev").and_then(Value::as_str) == Some("account")
                    && self.player.account_verified(now)
                    && !self.paused
                    && self.queue.current.is_none()
                {
                    self.advance(None);
                }
                if let Some(entry) = self.player.take_started() {
                    self.started(entry);
                }
                self.dirty = true;
            }
            Msg::Query(name, args, reply) => {
                let _ = reply.send(self.query(&name, &args));
            }
            Msg::Requested(p) => self.gate_and_lookup(*p),
            Msg::Looked(p, r) => self.looked(*p, r),
            Msg::KeyChecked { key, result } => self.key_checked(key, result),
            Msg::KeyStored(key, r) => match r {
                Ok(()) => {
                    let set = key.is_some();
                    self.key = key;
                    self.last_api_error = None;
                    self.hub.log("info", "songs", if set { "YouTube API key saved to the keyring" } else { "YouTube API key removed" });
                    self.check_mode(false);
                }
                Err(e) => self.hub.log("error", "songs", format!("could not store the YouTube API key: {e}")),
            },
            Msg::SecretStored(secret, r) => match r {
                Ok(()) => {
                    self.relay_secret = secret;
                    self.relay.configure(self.settings.relay_url.clone(), self.relay_secret.clone());
                    self.hub.log("info", "relay", "relay secret saved to the keyring");
                }
                Err(e) => self.hub.log("error", "relay", format!("could not store the relay secret: {e}")),
            },
            Msg::Settings(s) => self.apply_settings(*s),
            Msg::SettingsInvalid(error) => {
                self.player.set_required_account("", "", Instant::now());
                self.set_paused(true);
                self.hub.log("warn", "songs", format!("YouTube account lock: {error}; fix [songs] youtube_channel and verify again"));
                self.dirty = true;
            }
            Msg::Meta(d) => {
                self.meta_pending.remove(&d.video);
                if let Some(info) = d.info {
                    self.meta.insert(d.video, info);
                    self.dirty = true;
                }
            }
        }
    }

    fn apply_settings(&mut self, s: Settings) {
        if s.api_base != self.settings.api_base {
            match Client::new(&s.api_base) {
                Ok(c) => self.yt = Arc::new(c),
                Err(e) => self.hub.log("error", "songs", format!("YouTube client: {e:#}")),
            }
        }
        self.ledger.limit = s.daily_quota;
        self.ledger.link_reserve = s.link_reserve;
        self.player.set_crossfade(s.crossfade_ms);
        self.player.set_required_account(&s.youtube_channel, &s.youtube_delegate, Instant::now());
        self.relay.configure(s.relay_url.clone(), self.relay_secret.clone());
        if s.metadata != self.settings.metadata || s.metadata_base != self.settings.metadata_base {
            self.meta_worker.set_client(meta_client(&s, &self.hub));
        }
        self.settings = s;
        self.check_mode(false);
        self.dirty = true;
    }

    fn volume(&self) -> f64 {
        self.hub.snapshot.load().get("song.volume").and_then(Value::as_f64).unwrap_or(1.0)
    }

    // ---- output ---------------------------------------------------------------------------

    fn say(&self, text: String, causal: Option<u64>) {
        let args = Value::map().with("args", Value::List(vec![Value::Str(text)]));
        self.hub.command(Command::new(Origin::System, Op::Action { name: "bot.say".into(), args }).caused_by(causal));
    }

    /// Chat reply (or engine log for operator requests).
    fn reply(&self, chat: bool, key: &str, vars: &[(&str, &str)], causal: Option<u64>) {
        if let Some(msg) = self.settings.messages.render(key, vars) {
            if chat {
                self.say(msg, causal);
            } else {
                self.hub.log("info", "songs", msg);
            }
        }
    }

    fn announce(&self, key: &str, vars: &[(&str, &str)]) {
        if let Some(msg) = self.settings.messages.render(key, vars) {
            self.say(msg, None);
        }
    }

    fn emit(&self, ty: &str, payload: Value, causal: Option<u64>) {
        self.hub.emit(Event::new(ty, Origin::System, payload).with_causal(causal));
    }

    fn twitch_redemption(&self, e: &Entry, fulfill: bool) {
        let Some(p) = &e.paid else { return };
        let Some(rid) = &p.redemption_id else { return };
        if p.fulfilled {
            return;
        }
        let mut args = Value::map().with("redemption_id", rid.clone());
        if let Some(r) = &p.reward_id {
            args = args.with("reward_id", r.clone());
        }
        let name = if fulfill { "twitch.fulfill" } else { "twitch.refund" };
        self.hub.command(Command::new(Origin::System, Op::Action { name: name.into(), args }));
    }

    fn refund_request(&self, r: &Requester) {
        if let Some(rid) = &r.redemption_id {
            let mut args = Value::map().with("redemption_id", rid.clone());
            if let Some(rw) = &r.reward_id {
                args = args.with("reward_id", rw.clone());
            }
            self.hub.command(Command::new(Origin::System, Op::Action { name: "twitch.refund".into(), args }));
        }
    }

    fn set(&mut self, addr: &'static str, v: Value) {
        if self.published.get(addr) != Some(&v) {
            self.hub.publish(addr, v.clone());
            self.published.insert(addr, v);
        }
    }

    fn health(&mut self, addr: &'static str, status: &str, detail: String) {
        self.set(addr, Value::map().with("status", status).with("detail", detail));
    }

    /// Publish state, player desired state and the relay snapshot after changes.
    fn flush(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;
        let now = Timestamp::now();
        let cur = self.queue.current.clone();
        let next = self.queue.upcoming.first().cloned();
        let st = self.player.state();
        let verified = self.player.account_verified(Instant::now());
        self.set("queue.open", Value::Bool(self.policy.open && verified));
        self.set("queue.paused", Value::Bool(self.paused || !verified));
        self.set("song.account", self.player.account(Instant::now()));
        self.player_health();
        self.set("queue.playing", Value::Bool(st == "playing"));
        self.set("queue.length", Value::from(self.queue.upcoming.len()));
        self.set("queue.pending", Value::from(self.queue.pending.len()));
        self.set("queue.now.title", Value::from(cur.as_ref().map(|c| c.title.clone()).unwrap_or_default()));
        self.set("queue.now.user", Value::from(cur.as_ref().map(|c| c.user.clone()).unwrap_or_default()));
        self.set("queue.now.id", Value::from(cur.as_ref().map(|c| c.video.clone()).unwrap_or_default()));
        self.set("queue.now.channel", Value::from(cur.as_ref().map(|c| c.channel.clone()).unwrap_or_default()));
        self.set("queue.now.entry", Value::from(cur.as_ref().map(|c| c.id).unwrap_or(0)));
        self.set("queue.now.duration", Value::Float(cur.as_ref().map(|c| c.duration_s as f64).unwrap_or(0.0)));
        self.set("queue.next.title", Value::from(next.as_ref().map(|c| c.title.clone()).unwrap_or_default()));
        self.set("queue.next.user", Value::from(next.as_ref().map(|c| c.user.clone()).unwrap_or_default()));
        self.set("queue.url", Value::from(self.settings.queue_url.clone().unwrap_or_default()));
        self.set("queue.theme", Value::from(self.theme));
        self.set("queue.lookup", Value::from(self.lookup_mode(now)));
        let day = self.ledger.day(now);
        self.set("queue.quota.used", Value::from(day.used));
        self.set("queue.quota.remaining", Value::from(self.ledger.remaining(now)));
        self.set("song.state", Value::from(st));
        let dur = match self.player.duration() {
            d if d > 0.0 => d,
            _ => cur.as_ref().map(|c| c.duration_s as f64).unwrap_or(0.0),
        };
        self.set("song.duration", Value::Float((dur * 1000.0).round() / 1000.0));
        self.set("song.media", Value::from(cur.as_ref().map(|c| format!("yt:{}", c.video)).unwrap_or_default()));
        let info = cur.as_ref().map(|c| self.meta.get(&c.video).cloned().unwrap_or_else(|| SongInfo::parsed(&c.title, &c.channel)));
        self.set("song.current.title", Value::from(info.as_ref().map(|i| i.title.clone()).unwrap_or_default()));
        self.set("song.current.artist", Value::from(info.as_ref().and_then(|i| i.artist.clone()).unwrap_or_default()));
        self.set("song.current.genres", Value::from(info.as_ref().map(|i| i.genres.clone()).unwrap_or_default()));
        self.set("song.current.year", info.as_ref().and_then(|i| i.year).map(Value::from).unwrap_or(Value::Null));
        if self.player.rev() != self.player_rev {
            self.player_rev = self.player.rev();
            let d = self.player.desired();
            self.hub.publish("song.player", d.clone());
            self.published.insert("song.player", d);
        }
        self.youtube_health(now);
        self.reanchor();
        self.relay.snapshot(self.public_snapshot());
    }

    fn lookup_mode(&self, now: Timestamp) -> &'static str {
        if self.policy.library_only {
            "library"
        } else if self.key.is_none() || matches!(self.last_api_error, Some(ApiError::KeyInvalid(_) | ApiError::NotEnabled(_))) {
            "off"
        } else {
            self.ledger.mode(now).as_str()
        }
    }

    fn youtube_health(&mut self, now: Timestamp) {
        let day = self.ledger.day(now);
        let reset = quota::next_reset(now).to_zoned(jiff::tz::TimeZone::system()).strftime("%H:%M").to_string();
        let (st, detail) = match (&self.key, &self.last_api_error) {
            (None, _) => ("warn", "no YouTube API key — `streamctl do youtube.key.set <key>`; requests use the library only".to_string()),
            (_, Some(e @ (ApiError::KeyInvalid(_) | ApiError::NotEnabled(_)))) => ("fail", e.to_string()),
            _ => match self.ledger.mode(now) {
                Mode::Full => ("pass", format!("API key set; {}/{} units used today", day.used, self.ledger.limit)),
                Mode::Links => ("warn", format!("search quota used up ({}/{}) — links and library only until {reset}", day.used, self.ledger.limit)),
                Mode::Library => ("warn", format!("YouTube quota used up — library only until {reset}")),
            },
        };
        self.health("health.youtube", st, detail);
    }

    /// Announce lookup-mode changes caused by quota (not by policy or a missing key).
    fn check_mode(&mut self, announce: bool) {
        let now = Timestamp::now();
        let m = self.lookup_mode(now);
        if self.mode != Some(m) {
            let prev = self.mode.replace(m);
            self.dirty = true;
            let quota_caused = self.key.is_some() && !self.policy.library_only;
            if announce && quota_caused && prev.is_some() {
                match m {
                    "links" => self.announce("quota_links", &[]),
                    "library" => self.announce("quota_library", &[]),
                    "full" if matches!(prev, Some("links" | "library")) => self.announce("quota_back", &[]),
                    _ => {}
                }
            }
        }
    }

    /// Progress anchor for the public page: re-taken only when the song, player state or
    /// desired state changes (or every 30 s), so the snapshot doesn't change every tick.
    fn reanchor(&mut self) {
        let now = Instant::now();
        let key = (self.queue.current.as_ref().map(|c| c.id).unwrap_or(0), self.player.state(), self.player.rev());
        if key != self.anchor.0 || now.duration_since(self.anchor.3) >= Duration::from_secs(30) {
            self.anchor = (key, (self.player.position(now) * 10.0).round() / 10.0, now_ms(), now);
        }
    }

    fn public_snapshot(&self) -> Value {
        let ((_, st, _), position, at, _) = self.anchor;
        let now_v = self.queue.current.as_ref().map(|c| {
            Value::map()
                .with("title", c.title.clone())
                .with("channel", c.channel.clone())
                .with("user", c.user.clone())
                .with("video", c.video.clone())
                .with("duration", c.duration_s as f64)
                .with("position", position)
                .with("playing", st == "playing")
                .with("at", at)
        });
        let upcoming: Vec<Value> = self
            .queue
            .upcoming
            .iter()
            .take(50)
            .enumerate()
            .map(|(i, e)| {
                Value::map()
                    .with("pos", (i + 1) as i64)
                    .with("title", e.title.clone())
                    .with("channel", e.channel.clone())
                    .with("user", e.user.clone())
                    .with("video", e.video.clone())
                    .with("duration", e.duration_s as f64)
            })
            .collect();
        Value::map()
            .with("v", 1)
            .with("theme", self.theme)
            .with("open", self.policy.open)
            .with("paused", self.paused)
            .with("now", now_v.unwrap_or(Value::Null))
            .with("upcoming", upcoming)
            .with("length", self.queue.upcoming.len())
    }

    fn tick_position(&mut self) {
        let pos = self.player.position(Instant::now()) as f32;
        if pos != self.last_pos {
            self.last_pos = pos;
            self.hub.signal("song.position", pos);
            let d = self.player.duration();
            let dur = if d > 0.0 { d as f32 } else { self.queue.current.as_ref().map(|c| c.duration_s as f32).unwrap_or(0.0) };
            self.hub.signal("queue.position", queue_progress(pos, dur));
        }
    }

    fn tick_slow(&mut self) {
        let now = Instant::now();
        self.check_mode(true);
        self.player.expire_account(now);
        if let Some(c) = &self.queue.current
            && now.duration_since(self.last_pos_save) >= Duration::from_secs(5)
        {
            self.last_pos_save = now;
            let _ = self.store.kv_set("position", &Value::map().with("entry", c.id).with("t", self.player.position(now)));
        }
        self.dirty = true;
    }

    fn player_health(&mut self) {
        let now = Instant::now();
        let (status, detail) = if self.player.account_verified(now) {
            ("pass", format!("embedded YouTube channel {} verified ({})", self.settings.youtube_channel, self.player.state()))
        } else {
            ("warn", self.player.account_error().to_string())
        };
        self.health("health.player", status, detail);
    }

    // ---- actions --------------------------------------------------------------------------

    /// Mod-only actions: chat/relay commands need a mod actor; operator surfaces are trusted.
    fn is_mod(c: &Command) -> bool {
        match &c.actor {
            Some(a) => a.top_role() >= Role::Mod,
            None => !chat_origin(c),
        }
    }

    fn action(&mut self, c: Command) {
        let Op::Action { name, args } = &c.op else { return };
        let (name, args) = (name.clone(), args.clone());
        let causal = Some(c.id);
        let modonly = matches!(
            name.as_str(),
            "queue.skip"
                | "queue.approve"
                | "queue.reject"
                | "queue.reorder"
                | "queue.pause"
                | "queue.resume"
                | "queue.open"
                | "queue.close"
                | "queue.clear"
                | "queue.ban_user"
                | "queue.unban_user"
                | "queue.ban_song"
                | "queue.unban_song"
                | "queue.library.reset"
                | "queue.seek"
                | "queue.policy.set"
                | "queue.policy.reset"
                | "queue.theme.set"
        ) || name.starts_with("youtube.")
            || name.starts_with("relay.");
        if modonly && !Self::is_mod(&c) {
            self.hub.log("warn", "songs", format!("{name}: not permitted for {}", c.actor.as_ref().map(|a| a.name.as_str()).unwrap_or("?")));
            return;
        }
        if matches!(name.as_str(), "queue.open" | "queue.resume") && !self.player.account_verified(Instant::now()) {
            self.hub.log("warn", "songs", format!("{name} blocked: {}", self.player.account_error()));
            return;
        }
        match name.as_str() {
            "queue.request" => self.request(&c, &args),
            "queue.theme.set" => {
                let theme = match args.get_path("theme").or_else(|| positional(&args, 0)).and_then(Value::as_str) {
                    Some("win31") => "win31",
                    Some("modern") => "modern",
                    _ => {
                        self.hub.log("error", "songs", "queue.theme.set: theme must be win31 or modern");
                        return;
                    }
                };
                if theme != self.theme {
                    match self.store.kv_set("theme", &Value::from(theme)) {
                        Ok(()) => {
                            self.theme = theme;
                            self.dirty = true;
                        }
                        Err(e) => self.hub.log("error", "songs", format!("queue.theme.set: could not save theme: {e:#}")),
                    }
                }
            }
            "queue.skip" => self.skip(Status::Skipped, "skipped", causal),
            "queue.voteskip" => self.voteskip(&c, &args),
            "queue.remove" => self.remove(&c, &args),
            "queue.approve" => self.approve(&args, causal),
            "queue.reject" => self.reject_pending(&args, causal),
            "queue.reorder" => {
                let id = args.get_path("id").or_else(|| positional(&args, 0)).and_then(text::parse_id);
                let to = args.get_path("to").or_else(|| positional(&args, 1)).and_then(Value::as_i64);
                match (id, to) {
                    (Some(id), Some(to)) if self.queue.reorder(id, to.max(1) as usize) => {
                        self.save_order();
                        self.sync_preload();
                    }
                    _ => self.hub.log("warn", "songs", "queue.reorder {id, to}: unknown entry"),
                }
            }
            "queue.pause" => self.set_paused(true),
            "queue.resume" => self.set_paused(false),
            "queue.open" | "queue.close" => {
                let open = name == "queue.open";
                if self.policy.open != open {
                    self.policy.open = open;
                    self.save_policy();
                    self.announce(if open { "opened" } else { "closed" }, &[]);
                }
            }
            "queue.clear" => {
                let all: Vec<Entry> = self.queue.upcoming.drain(..).chain(self.queue.pending.drain(..)).collect();
                for mut e in all {
                    self.twitch_redemption(&e, false);
                    e.status = Status::Removed;
                    e.ended_at = Some(now_ms());
                    let _ = self.store.update_entry(&e);
                }
                self.sync_preload();
            }
            "queue.ban_user" | "queue.unban_user" => {
                let Some(user) = arg_str(&args, "user").or_else(|| positional(&args, 0).map(|v| v.to_string())).map(|u| text::login(&u)) else { return };
                if name == "queue.ban_user" {
                    if !self.policy.banned_users.contains(&user) {
                        self.policy.banned_users.push(user.clone());
                        self.save_policy();
                    }
                    // their waiting requests go too
                    for id in self.queue.ids_of(&user) {
                        if let Some(mut e) = self.queue.take(id) {
                            self.twitch_redemption(&e, false);
                            e.status = Status::Removed;
                            e.note = Some("user banned".into());
                            let _ = self.store.update_entry(&e);
                        }
                    }
                    self.save_order();
                    self.sync_preload();
                    self.announce("banned_user", &[("user", &user)]);
                } else if self.policy.banned_users.contains(&user) {
                    self.policy.banned_users.retain(|u| *u != user);
                    self.save_policy();
                    self.announce("unbanned_user", &[("user", &user)]);
                }
            }
            "queue.ban_song" => self.ban_song(&args),
            "queue.unban_song" => {
                if let Some(v) = arg_str(&args, "video").or_else(|| arg_str(&args, "id")).or_else(|| positional(&args, 0).map(|v| v.to_string())) {
                    let v = youtube::extract_video_id(&v).unwrap_or(v);
                    self.policy.blocked_videos.retain(|b| *b != v);
                    self.save_policy();
                }
            }
            // forget "can't be played here" marks (one video, or all with no argument)
            "queue.library.reset" => {
                let target =
                    arg_str(&args, "video").or_else(|| positional(&args, 0).map(|v| v.to_string())).map(|v| youtube::extract_video_id(&v).unwrap_or(v));
                match self.store.clear_unplayable(target.as_deref()) {
                    Ok(n) => self.hub.log("info", "songs", format!("cleared the unplayable mark on {n} librar{}", if n == 1 { "y song" } else { "y songs" })),
                    Err(e) => self.hub.log("error", "songs", format!("queue.library.reset: {e:#}")),
                }
            }
            "queue.seek" => {
                if let Some(t) = args.get_path("t").or_else(|| positional(&args, 0)).and_then(|v| match v {
                    Value::Str(s) => se_proto::parse_duration_ms(s).map(|ms| ms as f64 / 1000.0).or_else(|| s.parse().ok()),
                    other => other.as_f64(),
                }) {
                    self.player.seek(t, Instant::now());
                }
            }
            "queue.policy.set" => {
                let changes = match &args {
                    Value::Map(m) => Value::Map(m.iter().filter(|(k, _)| k.as_str() != "args").map(|(k, v)| (k.clone(), v.clone())).collect()),
                    _ => Value::Null,
                };
                match self.policy.patch(&changes) {
                    Ok((p, _)) if p.open && !self.policy.open && !self.player.account_verified(Instant::now()) => {
                        self.hub.log("warn", "songs", format!("queue.policy.set open blocked: {}", self.player.account_error()));
                    }
                    Ok((p, changed)) => {
                        if !changed.is_empty() {
                            self.policy = p;
                            self.save_policy();
                            self.hub.log("info", "songs", format!("queue policy: {} changed", changed.join(", ")));
                            self.check_mode(false);
                        }
                    }
                    Err(e) => self.hub.log("error", "songs", format!("queue.policy.set: {e}")),
                }
            }
            "queue.policy.reset" => {
                self.policy = Policy {
                    banned_users: std::mem::take(&mut self.policy.banned_users),
                    blocked_videos: std::mem::take(&mut self.policy.blocked_videos),
                    ..Default::default()
                };
                self.save_policy();
            }
            "youtube.key.set" => {
                let Some(key) = arg_str(&args, "key").or_else(|| positional(&args, 0).map(|v| v.to_string())).map(|k| k.trim().to_string()) else {
                    self.hub.log("error", "songs", "youtube.key.set needs {key}");
                    return;
                };
                if key.len() < 20 || key.len() > 200 || !key.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
                    self.hub.log("error", "songs", "that doesn't look like a Google API key");
                    return;
                }
                self.hub.log("info", "songs", "checking the YouTube API key…");
                let (yt, tx, ledger) = (self.yt.clone(), self.tx.clone(), self.ledger.clone());
                tokio::spawn(async move {
                    let _ = ledger.charge(Timestamp::now(), quota::Kind::List);
                    let result = yt.videos(&key, &[PROBE_VIDEO.to_string()]).await.map(|_| ());
                    let _ = tx.send(Msg::KeyChecked { key, result });
                });
            }
            "youtube.key.clear" => self.store_key(None),
            "relay.secret.set" => {
                let Some(s) = arg_str(&args, "secret").or_else(|| positional(&args, 0).map(|v| v.to_string())) else {
                    self.hub.log("error", "relay", "relay.secret.set needs {secret}");
                    return;
                };
                if s.len() < 32 || s.bytes().any(|b| !b.is_ascii_graphic()) {
                    self.hub.log("error", "relay", "the relay secret must be at least 32 printable characters (e.g. `openssl rand -hex 32`)");
                    return;
                }
                self.store_secret(Some(s));
            }
            "relay.secret.clear" => self.store_secret(None),
            "relay.reconnect" => self.relay.reconnect(),
            other => self.hub.log("warn", "songs", format!("unknown action {other}")),
        }
        self.dirty = true;
    }

    fn key_checked(&mut self, key: String, result: Result<(), ApiError>) {
        match result {
            Ok(()) | Err(ApiError::QuotaExceeded) => self.store_key(Some(key)),
            Err(ApiError::Network(e)) => {
                self.hub.log("warn", "songs", format!("couldn't verify the key ({e}); saving it anyway"));
                self.store_key(Some(key));
            }
            Err(e) => self.hub.log("error", "songs", format!("key not saved: {e}")),
        }
    }

    fn store_key(&self, key: Option<String>) {
        let (s, tx) = (self.secrets.clone(), self.tx.clone());
        tokio::task::spawn_blocking(move || {
            let r = match &key {
                Some(k) => s.set(YOUTUBE_KEY, k),
                None => s.delete(YOUTUBE_KEY),
            };
            let _ = tx.send(Msg::KeyStored(key, r.map_err(|e| format!("{e:#}"))));
        });
    }

    fn store_secret(&self, secret: Option<String>) {
        let (s, tx) = (self.secrets.clone(), self.tx.clone());
        tokio::task::spawn_blocking(move || {
            let r = match &secret {
                Some(k) => s.set(RELAY_SECRET, k),
                None => s.delete(RELAY_SECRET),
            };
            let _ = tx.send(Msg::SecretStored(secret, r.map_err(|e| format!("{e:#}"))));
        });
    }

    fn save_policy(&mut self) {
        if let Ok(js) = serde_json::to_value(&self.policy) {
            let _ = self.store.kv_set("policy", &Value::from(js));
        }
        self.dirty = true;
    }

    fn save_order(&self) {
        let ids: Vec<i64> = self.queue.upcoming.iter().map(|e| e.id).collect();
        if let Err(e) = self.store.save_order(&ids) {
            self.hub.log("error", "songs", format!("save queue order: {e:#}"));
        }
    }

    fn set_paused(&mut self, paused: bool) {
        if !paused && !self.player.account_verified(Instant::now()) {
            return;
        }
        if self.paused == paused {
            return;
        }
        self.paused = paused;
        let _ = self.store.kv_set("paused", &Value::Bool(paused));
        self.player.set_paused(paused);
        if !paused && self.queue.current.is_none() {
            self.advance(None);
        }
        self.dirty = true;
    }

    // ---- requests -------------------------------------------------------------------------

    fn requester(&self, c: &Command, args: &Value) -> Requester {
        let num = |k: &str| args.get_path(k).and_then(Value::as_i64).unwrap_or(0).clamp(0, u32::MAX as i64) as u32;
        let (login, display, user_id, role) = match &c.actor {
            Some(a) => (text::login(&a.name), text::display(&a.name, 40), Some(a.id.clone()).filter(|s| !s.is_empty()), a.top_role()),
            None => {
                let u = arg_str(args, "user").unwrap_or_else(|| "streamer".into());
                let role = if chat_origin(c) { Role::Everyone } else { Role::Owner };
                (text::login(&u), text::display(&u, 40), None, role)
            }
        };
        Requester {
            login,
            display,
            user_id,
            role,
            follow_age_s: args.get_path("follow_age_s").and_then(Value::as_i64).filter(|a| *a >= 0),
            bits: num("bits"),
            points: num("points"),
            redemption_id: arg_str(args, "redemption_id"),
            reward_id: arg_str(args, "reward_id"),
            operator: c.actor.is_none() && !chat_origin(c),
        }
    }

    fn request(&mut self, c: &Command, args: &Value) {
        let req = self.requester(c, args);
        let text = request_text(args);
        let p = Pending { req, text, chat: chat_origin(c) || c.actor.is_some(), causal: c.id };
        if !self.player.account_verified(Instant::now()) {
            return self.rejected(&p, Reject::Lookup(self.player.account_error().to_string()));
        }
        if p.text.trim().is_empty() {
            return self.rejected(&p, Reject::Empty);
        }
        let needs_follow = self.policy.min_follow_days > 0 && p.req.role < Role::Sub && !p.req.operator && p.req.follow_age_s.is_none();
        match (&p.req.user_id, needs_follow) {
            (Some(uid), true) => {
                let (hub, tx, uid) = (self.hub.clone(), self.tx.clone(), uid.clone());
                let mut p = p;
                tokio::spawn(async move {
                    if let Ok(v) = hub.query("twitch.follow_age", Value::map().with("user_id", uid)).await
                        && v.get_path("following").is_some_and(Value::truthy)
                    {
                        p.req.follow_age_s = v.get_path("age_s").and_then(Value::as_i64).filter(|a| *a >= 0);
                    }
                    let _ = tx.send(Msg::Requested(Box::new(p)));
                });
            }
            _ => self.gate_and_lookup(p),
        }
    }

    fn facts(&self, login: &str) -> QueueFacts {
        let inflight = self.inflight.get(login).copied().unwrap_or(0);
        QueueFacts {
            length: self.queue.len() + self.inflight.values().sum::<usize>(),
            user_open: self.queue.open_requests(login) + inflight,
            last_request_ms: self.store.last_request(login),
        }
    }

    fn gate_and_lookup(&mut self, p: Pending) {
        if !self.player.account_verified(Instant::now()) {
            return self.rejected(&p, Reject::Lookup(self.player.account_error().to_string()));
        }
        if let Err(r) = self.policy.gate_requester(&p.req, &self.facts(&p.req.login), now_ms()) {
            return self.rejected(&p, r);
        }
        *self.inflight.entry(p.req.login.clone()).or_default() += 1;
        let lk = Lookup {
            yt: self.yt.clone(),
            store: self.store.clone(),
            ledger: self.ledger.clone(),
            key: self.key.clone(),
            region: self.settings.region.clone(),
            cache_days: self.settings.cache_days,
            search_results: self.settings.search_results,
            library_only: self.policy.library_only,
        };
        let tx = self.tx.clone();
        tokio::spawn(async move {
            let r = lk.resolve(&p.text).await;
            let _ = tx.send(Msg::Looked(Box::new(p), r));
        });
    }

    fn rejected(&mut self, p: &Pending, r: Reject) {
        tracing::info!("songs: request from {} rejected: {} ({})", p.req.login, r.code(), p.text);
        self.refund_request(&p.req);
        self.reply(p.chat, "rejected", &[("user", &p.req.display), ("reason", &r.reason())], Some(p.causal));
    }

    /// A lookup finished: pick, gate and queue; quota-mode announcements go after the reply.
    fn looked(&mut self, p: Pending, r: Result<Found, Failed>) {
        self.looked_inner(p, r);
        self.check_mode(true);
    }

    fn looked_inner(&mut self, p: Pending, r: Result<Found, Failed>) {
        if let Some(n) = self.inflight.get_mut(&p.req.login) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.inflight.remove(&p.req.login);
            }
        }
        if !self.player.account_verified(Instant::now()) {
            return self.rejected(&p, Reject::Lookup(self.player.account_error().to_string()));
        }
        let found = match r {
            Ok(f) => {
                if !matches!(self.last_api_error, Some(ApiError::QuotaExceeded)) && f.source == crate::lookup::Source::Api {
                    self.last_api_error = None;
                }
                f
            }
            Err(f) => {
                if let Some(e) = &f.api {
                    tracing::warn!("songs: lookup failed: {e}");
                    self.last_api_error = Some(e.clone());
                }
                return self.rejected(&p, f.reject);
            }
        };
        // the queue may have changed while the lookup ran
        if let Err(r) = self.policy.gate_requester(&p.req, &self.facts(&p.req.login), now_ms()) {
            return self.rejected(&p, r);
        }
        let now = now_ms();
        let mut first_reject = None;
        let mut chosen = None;
        for c in &found.candidates {
            let verdict = if let Some(why) = &c.unplayable {
                Err(Reject::Unplayable(why.clone()))
            } else {
                policy::platform_check(&c.video, &self.settings.region)
                    .and_then(|_| self.policy.gate_video(&c.video, &p.req, self.queue.find_video(&c.video.id), self.store.last_played(&c.video.id), now))
            };
            match verdict {
                Ok(()) => {
                    chosen = Some(c.video.clone());
                    break;
                }
                // the requested song itself is queued/recent: don't substitute another result
                Err(r @ (Reject::Duplicate(_) | Reject::Repeat(_))) if first_reject.is_none() => {
                    first_reject = Some(r);
                    break;
                }
                // unplayable/blocked: a later result (another upload) may be fine
                Err(r) => {
                    first_reject.get_or_insert(r);
                }
            }
        }
        let Some(v) = chosen else {
            return self.rejected(&p, first_reject.unwrap_or(Reject::NoResults));
        };
        let approval = self.policy.needs_approval(&p.req);
        let paid = p.req.paid().then(|| Paid {
            bits: p.req.bits,
            points: p.req.points,
            redemption_id: p.req.redemption_id.clone(),
            reward_id: p.req.reward_id.clone(),
            skip_line: self.policy.skips_line(&p.req),
            fulfilled: false,
        });
        let mut e = Entry {
            id: 0,
            video: v.id.clone(),
            title: text::display(&v.title, 100),
            channel: text::display(&v.channel, 60),
            duration_s: v.duration_s,
            user: p.req.display.clone(),
            user_id: p.req.user_id.clone(),
            role: p.req.role,
            status: if approval { Status::Pending } else { Status::Queued },
            paid,
            requested_at: now,
            started_at: None,
            ended_at: None,
            note: None,
        };
        match self.store.insert_entry(&e) {
            Ok(id) => e.id = id,
            Err(err) => {
                self.hub.log("error", "songs", format!("queue write failed: {err:#}"));
                return self.rejected(&p, Reject::Lookup("queue storage failed".into()));
            }
        }
        self.ensure_meta(&v.id, &v.title, &v.channel);
        let dur = fmt_duration(e.duration_s);
        let id_s = e.id.to_string();
        self.emit(
            "queue.song_requested",
            self.entry_value(&e, None)
                .with("pending", approval)
                .with("source", found.source.as_str())
                .with("user_id", e.user_id.clone().map(Value::from).unwrap_or(Value::Null)),
            Some(p.causal),
        );
        if approval {
            let (title, user) = (e.title.clone(), e.user.clone());
            self.queue.pending.push(e);
            self.reply(p.chat, "pending", &[("user", &user), ("title", &title), ("id", &id_s), ("duration", &dur)], Some(p.causal));
        } else {
            let (title, user) = (e.title.clone(), e.user.clone());
            let pos = self.queue.enqueue(e);
            self.save_order();
            let pos_s = pos.to_string();
            self.reply(p.chat, "added", &[("user", &user), ("title", &title), ("duration", &dur), ("pos", &pos_s), ("id", &id_s)], Some(p.causal));
            if self.queue.current.is_none() && !self.paused {
                self.advance(Some(p.causal));
            } else {
                self.sync_preload();
            }
        }
        self.dirty = true;
    }

    // ---- queue flow -----------------------------------------------------------------------

    /// Make the next upcoming entry current (or go idle).
    fn advance(&mut self, causal: Option<u64>) {
        if !self.player.account_verified(Instant::now()) {
            self.player.stop();
            self.sync_preload();
            return;
        }
        self.votes.clear();
        match self.queue.pop_next() {
            Some(mut e) => {
                e.status = Status::Playing;
                e.started_at = Some(now_ms());
                let _ = self.store.update_entry(&e);
                let _ = self.store.record_play(&e.video);
                self.player.play(e.id, &e.video, 0.0, self.paused);
                let (title, user) = (e.title.clone(), e.user.clone());
                self.ensure_meta(&e.video, &e.title, &e.channel);
                self.queue.current = Some(e);
                self.save_order();
                self.reply(true, "now_playing", &[("title", &title), ("user", &user)], causal);
                if let Some(entry) = self.player.take_started() {
                    self.started(entry);
                }
            }
            None => {
                self.queue.current = None;
                self.player.stop();
            }
        }
        self.sync_preload();
        self.dirty = true;
    }

    fn sync_preload(&mut self) {
        let next = if self.queue.current.is_some() { self.queue.upcoming.first().map(|e| (e.id, e.video.clone())) } else { None };
        self.player.preload(next.as_ref().map(|(id, v)| (*id, v.as_str())), !self.paused);
        self.dirty = true;
    }

    /// Playback actually began (first non-stalled `playing` report).
    fn started(&mut self, entry: i64) {
        let Some(snapshot) = self.queue.current.clone().filter(|c| c.id == entry) else { return };
        if snapshot.paid.as_ref().is_some_and(|p| p.redemption_id.is_some() && !p.fulfilled) {
            self.twitch_redemption(&snapshot, true);
            if let Some(cur) = self.queue.current.as_mut() {
                if let Some(p) = cur.paid.as_mut() {
                    p.fulfilled = true;
                }
                let _ = self.store.update_entry(cur);
            }
        }
        self.emit("queue.song_started", self.entry_value(&snapshot, None), None);
    }

    /// End the current entry with `status` and move on.
    fn finish_current(&mut self, status: Status, reason: &str, causal: Option<u64>) -> Option<Entry> {
        let mut e = self.queue.current.take()?;
        let played = e.paid.as_ref().is_some_and(|p| p.fulfilled);
        if !played && status != Status::Played {
            // never started (error before playback, or skipped during load): refund
            self.twitch_redemption(&e, false);
        }
        e.status = status;
        e.ended_at = Some(now_ms());
        if status == Status::Error {
            e.note = Some(reason.to_string());
        }
        let _ = self.store.update_entry(&e);
        let pos = self.player.position(Instant::now());
        self.emit("queue.song_ended", self.entry_value(&e, None).with("reason", reason).with("position", (pos * 10.0).round() / 10.0), causal);
        self.advance(causal);
        Some(e)
    }

    fn skip(&mut self, status: Status, reason: &str, causal: Option<u64>) {
        if let Some(e) = self.finish_current(status, reason, causal) {
            self.announce("skipped", &[("title", &e.title), ("user", &e.user)]);
        }
    }

    fn voteskip(&mut self, c: &Command, args: &Value) {
        let Some(cur) = self.queue.current.clone() else { return };
        let needed = self.policy.voteskip_votes;
        if needed == 0 {
            self.announce("voteskip_off", &[]);
            return;
        }
        let voter = c.actor.as_ref().map(|a| text::login(&a.name)).or_else(|| arg_str(args, "user").map(|u| text::login(&u))).unwrap_or_default();
        if voter.is_empty() || !self.votes.insert(voter) {
            return;
        }
        let votes = self.votes.len() as u32;
        if votes >= needed {
            self.announce("voteskip_passed", &[("title", &cur.title)]);
            self.finish_current(Status::Skipped, "voteskip", Some(c.id));
        } else {
            self.announce("voteskip", &[("votes", &votes.to_string()), ("needed", &needed.to_string()), ("title", &cur.title)]);
        }
    }

    fn player_outcome(&mut self, o: Outcome) {
        match o {
            Outcome::Ended(entry) => {
                if self.queue.current.as_ref().is_some_and(|c| c.id == entry) {
                    self.finish_current(Status::Played, "ended", None);
                }
            }
            Outcome::Error(entry, code) => {
                let Some(cur) = self.queue.current.clone().filter(|c| c.id == entry) else { return };
                let why = player_error(code);
                self.unplayable(&cur, code, why);
                self.emit("queue.song_error", self.entry_value(&cur, None).with("code", code).with("reason", why), None);
                self.announce("error_skip", &[("title", &cur.title), ("reason", why), ("user", &cur.user)]);
                self.finish_current(Status::Error, why, None);
            }
            Outcome::PreloadError(entry, code) => {
                let why = player_error(code);
                if let Some(mut e) = self.queue.take(entry) {
                    self.unplayable(&e, code, why);
                    self.twitch_redemption(&e, false);
                    self.emit("queue.song_error", self.entry_value(&e, None).with("code", code).with("reason", why), None);
                    self.announce("error_skip", &[("title", &e.title), ("reason", why), ("user", &e.user)]);
                    e.status = Status::Error;
                    e.note = Some(why.to_string());
                    e.ended_at = Some(now_ms());
                    let _ = self.store.update_entry(&e);
                    self.save_order();
                    self.sync_preload();
                }
            }
        }
    }

    fn unplayable(&self, e: &Entry, code: i64, why: &str) {
        tracing::warn!("songs: player error {code} for {} ({}): {why}", e.video, e.title);
        self.hub.log("warn", "songs", format!("\"{}\" can't be played ({why}, error {code}) — skipped", e.title));
        if matches!(code, 100 | 101 | 150) {
            let _ = self.store.mark_unplayable(&e.video, why);
        }
    }

    // ---- song metadata --------------------------------------------------------------------

    /// Make sure a queued/current song has metadata: the cached match (or the title read
    /// from the video), plus a background MusicBrainz lookup when the cache has nothing
    /// current. Never touches the player or the queue.
    fn ensure_meta(&mut self, video: &str, title: &str, channel: &str) {
        if self.meta_pending.contains(video) {
            return;
        }
        // the library has the untruncated title
        let (title, channel) = self.store.video(video).map(|c| (c.video.title, c.video.channel)).unwrap_or_else(|| (title.to_string(), channel.to_string()));
        let row = self.store.song_meta(video);
        let info = row.as_ref().and_then(|r| r.info.clone()).unwrap_or_else(|| SongInfo::parsed(&title, &channel));
        self.prune_meta();
        self.meta.insert(video.to_string(), info);
        if self.settings.metadata && row.is_none_or(|r| r.due(now_ms() / 1000)) {
            self.meta_pending.insert(video.to_string());
            self.meta_worker.submit(metadata::Job { video: video.to_string(), title, channel });
        }
        self.dirty = true;
    }

    /// Keep the metadata map to songs still in the queue once it grows.
    fn prune_meta(&mut self) {
        if self.meta.len() < 256 {
            return;
        }
        let live: HashSet<&str> = self.queue.current.iter().chain(&self.queue.upcoming).chain(&self.queue.pending).map(|e| e.video.as_str()).collect();
        self.meta.retain(|v, _| live.contains(v.as_str()));
    }

    /// An entry as published in events/queries, with `artist`, `genres` and `year`.
    fn entry_value(&self, e: &Entry, pos: Option<usize>) -> Value {
        match self.meta.get(&e.video) {
            Some(info) => info.with_fields(e.to_value(pos)),
            None => self
                .store
                .song_meta(&e.video)
                .and_then(|r| r.info)
                .unwrap_or_else(|| SongInfo::parsed(&e.title, &e.channel))
                .with_fields(e.to_value(pos)),
        }
    }

    fn find_arg_entry(&self, args: &Value) -> Option<i64> {
        args.get_path("id").or_else(|| positional(args, 0)).and_then(text::parse_id)
    }

    fn remove(&mut self, c: &Command, args: &Value) {
        let actor_login = c.actor.as_ref().map(|a| text::login(&a.name));
        let is_mod = Self::is_mod(c);
        let user = arg_str(args, "user").map(|u| text::login(&u));
        let last = args.get_path("last").is_some_and(Value::truthy);
        let mut targets: Vec<i64> = Vec::new();
        if let Some(i) = args.get_path("index").and_then(Value::as_i64) {
            if let Some(e) = self.queue.upcoming.get((i.max(1) - 1) as usize) {
                targets.push(e.id);
            }
        } else if let Some(id) = self.find_arg_entry(args).filter(|_| user.is_none()) {
            targets.push(id);
        } else if let Some(u) = user.or_else(|| actor_login.clone().filter(|_| last)) {
            if last {
                targets.extend(self.queue.last_of(&u));
            } else {
                targets.extend(self.queue.ids_of(&u));
            }
        }
        for id in targets {
            let own = self
                .queue
                .upcoming
                .iter()
                .chain(self.queue.pending.iter())
                .find(|e| e.id == id)
                .is_some_and(|e| actor_login.as_deref().is_some_and(|l| e.user.eq_ignore_ascii_case(l)));
            if !(is_mod || own) {
                continue;
            }
            if let Some(mut e) = self.queue.take(id) {
                self.twitch_redemption(&e, false);
                e.status = Status::Removed;
                e.ended_at = Some(now_ms());
                let _ = self.store.update_entry(&e);
                self.reply(chat_origin(c), "removed", &[("title", &e.title), ("user", &e.user)], Some(c.id));
            }
        }
        self.save_order();
        self.sync_preload();
    }

    fn approve(&mut self, args: &Value, causal: Option<u64>) {
        let Some(id) = self.find_arg_entry(args) else { return };
        let Some(mut e) = self.queue.take_pending(id) else {
            self.hub.log("warn", "songs", format!("queue.approve: #{id} is not pending"));
            return;
        };
        e.status = Status::Queued;
        let _ = self.store.update_entry(&e);
        let (title, user) = (e.title.clone(), e.user.clone());
        let pos = self.queue.enqueue(e);
        self.save_order();
        self.announce("approved", &[("user", &user), ("title", &title), ("pos", &pos.to_string()), ("id", &id.to_string())]);
        if self.queue.current.is_none() && !self.paused {
            self.advance(causal);
        } else {
            self.sync_preload();
        }
    }

    fn reject_pending(&mut self, args: &Value, _causal: Option<u64>) {
        let Some(id) = self.find_arg_entry(args) else { return };
        let Some(mut e) = self.queue.take(id) else {
            self.hub.log("warn", "songs", format!("queue.reject: #{id} is not waiting"));
            return;
        };
        let reason = arg_str(args, "reason").map(|r| text::display(&r, 80));
        self.twitch_redemption(&e, false);
        e.status = Status::Rejected;
        e.note = reason.clone();
        e.ended_at = Some(now_ms());
        let _ = self.store.update_entry(&e);
        let why = Reject::ModRejected(reason).reason();
        self.announce("rejected", &[("user", &e.user), ("reason", &why), ("title", &e.title)]);
        self.save_order();
        self.sync_preload();
    }

    fn ban_song(&mut self, args: &Value) {
        let target = arg_str(args, "id").or_else(|| arg_str(args, "video")).or_else(|| positional(args, 0).map(|v| v.to_string()));
        let (video, title) = match target {
            None => match &self.queue.current {
                Some(c) => (c.video.clone(), c.title.clone()),
                None => return,
            },
            Some(t) => {
                let entry_id = text::parse_id(&Value::Str(t.clone()));
                let from_entry = entry_id.and_then(|id| {
                    self.queue
                        .current
                        .iter()
                        .chain(self.queue.upcoming.iter())
                        .chain(self.queue.pending.iter())
                        .find(|e| e.id == id)
                        .map(|e| (e.video.clone(), e.title.clone()))
                });
                match from_entry {
                    Some(x) => x,
                    None => {
                        let v = youtube::extract_video_id(&t).unwrap_or(t);
                        let title = self.store.video(&v).map(|c| c.video.title).unwrap_or_else(|| v.clone());
                        (v, title)
                    }
                }
            }
        };
        if !self.policy.blocked_videos.contains(&video) {
            self.policy.blocked_videos.push(video.clone());
            self.save_policy();
        }
        // drop queued copies; skip it if it's playing
        let queued: Vec<i64> = self.queue.upcoming.iter().chain(self.queue.pending.iter()).filter(|e| e.video == video).map(|e| e.id).collect();
        for id in queued {
            if let Some(mut e) = self.queue.take(id) {
                self.twitch_redemption(&e, false);
                e.status = Status::Removed;
                e.note = Some("song banned".into());
                let _ = self.store.update_entry(&e);
            }
        }
        self.save_order();
        self.announce("banned_song", &[("title", &title)]);
        if self.queue.current.as_ref().is_some_and(|c| c.video == video) {
            self.finish_current(Status::Skipped, "banned", None);
        } else {
            self.sync_preload();
        }
    }

    // ---- queries --------------------------------------------------------------------------

    fn query(&mut self, name: &str, args: &Value) -> Result<Value, String> {
        let now = Timestamp::now();
        match name {
            "queue" => Ok(self.ui_snapshot(now)),
            "queue.policy" => {
                Ok(Value::map().with("policy", self.policy.to_value()).with("fields", policy::FIELDS.iter().map(|f| f.to_value()).collect::<Vec<_>>()))
            }
            "queue.library" => {
                let limit = args.get_path("limit").and_then(Value::as_i64).unwrap_or(200).clamp(1, 5000) as usize;
                Ok(Value::List(self.store.library(args.get_path("q").and_then(Value::as_str), limit)))
            }
            "queue.history" => {
                let n = args.get_path("n").and_then(Value::as_i64).unwrap_or(50).clamp(1, 1000) as usize;
                Ok(Value::List(self.store.history(n).iter().map(|e| self.entry_value(e, None)).collect()))
            }
            "youtube.status" => Ok(Value::map()
                .with("key_set", self.key.is_some())
                .with("lookup", self.lookup_mode(now))
                .with("quota", self.quota_value(now))
                .with("account", self.player.account(Instant::now()))
                .with("last_error", self.last_api_error.as_ref().map(|e| Value::from(e.to_string())).unwrap_or(Value::Null))
                .with(
                    "days",
                    self.ledger
                        .history(14)
                        .into_iter()
                        .map(|(d, q)| {
                            Value::map().with("day", d).with("used", q.used).with("searches", q.searches).with("lists", q.lists).with("exhausted", q.exhausted)
                        })
                        .collect::<Vec<_>>(),
                )),
            "relay.status" => Ok(self.relay.status().to_value().with("queue_url", self.settings.queue_url.clone().unwrap_or_default())),
            other => Err(format!("unknown query `{other}`")),
        }
    }

    fn quota_value(&self, now: Timestamp) -> Value {
        let d = self.ledger.day(now);
        Value::map()
            .with("used", d.used)
            .with("limit", self.ledger.limit)
            .with("remaining", self.ledger.remaining(now))
            .with("searches", d.searches)
            .with("lists", d.lists)
            .with("resets_at", quota::next_reset(now).as_second())
    }

    fn ui_snapshot(&self, now: Timestamp) -> Value {
        let inow = Instant::now();
        let current = self.queue.current.as_ref().map(|c| {
            self.entry_value(c, None)
                .with("position", (self.player.position(inow) * 10.0).round() / 10.0)
                .with("state", self.player.state())
                .with("votes", self.votes.len())
                .with("votes_needed", self.policy.voteskip_votes)
        });
        Value::map()
            .with("theme", self.theme)
            .with("open", self.policy.open && self.player.account_verified(inow))
            .with("paused", self.paused || !self.player.account_verified(inow))
            .with("account", self.player.account(inow))
            .with("lookup", self.lookup_mode(now))
            .with("url", self.settings.queue_url.clone().unwrap_or_default())
            .with("quota", self.quota_value(now))
            .with("now", current.unwrap_or(Value::Null))
            .with("upcoming", self.queue.upcoming.iter().enumerate().map(|(i, e)| self.entry_value(e, Some(i + 1))).collect::<Vec<_>>())
            .with("pending", self.queue.pending.iter().map(|e| self.entry_value(e, None)).collect::<Vec<_>>())
            .with("history", self.store.history(20).iter().map(|e| self.entry_value(e, None)).collect::<Vec<_>>())
    }
}

/// YouTube IFrame API error codes → short reason.
pub fn player_error(code: i64) -> &'static str {
    match code {
        2 => "invalid video id",
        5 => "HTML5 player error",
        100 => "video removed or private",
        101 | 150 => "embedding disabled by the owner",
        _ => "player error",
    }
}

/// `queue.position`: how far the current request has played, 0 … 1 (0 without a known length).
fn queue_progress(pos_s: f32, dur_s: f32) -> f32 {
    if dur_s > 0.0 && pos_s.is_finite() { (pos_s / dur_s).clamp(0.0, 1.0) } else { 0.0 }
}
