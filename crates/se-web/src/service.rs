//! The web-source supervisor: keeps the CEF host running (restart with backoff, liveness
//! pings), keeps one off-screen browser per desired source, and publishes status.

use crate::host::{self, Event, HostArgs, HostPaths, INSTALL_HINT, Session};
use crate::media::Media;
use crate::protocol::{FromHost, PROTOCOL_VERSION, ToHost};
use crate::sources::{self, HostSettings, Owner, Spec};
use parking_lot::Mutex;
use se_api::Auth;
use se_core::Input;
use se_hub::{EngineCtx, Hub};
use se_patch::{PatchSet, Patches};
use se_proto::{Command, Meta, Op, Value};
use std::collections::{BTreeMap, HashMap};
use std::io::Read;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};

const TICK: Duration = Duration::from_millis(500);
/// First start may page in ~1.4 GB of libcef from a cold disk.
const HELLO_TIMEOUT: Duration = Duration::from_secs(30);
const PING_EVERY: Duration = Duration::from_secs(5);
const PONG_TIMEOUT: Duration = Duration::from_secs(20);
const STOP_TIMEOUT: Duration = Duration::from_secs(6);
/// Crash / host-restart messages stay visible this long after the page recovered.
const ERROR_HOLD: Duration = Duration::from_secs(30);
/// A host that ran this long before dying restarts without accumulated backoff.
const STABLE_AFTER: Duration = Duration::from_secs(30);
/// With no web sources left, the host is stopped after this long.
const IDLE_LINGER: Duration = Duration::from_secs(30);
const DEFAULT_LOGIN_URL: &str = "https://accounts.google.com/ServiceLogin?service=youtube&continue=https%3A%2F%2Fwww.youtube.com%2F";

pub const STATUSES: &[&str] = &["starting", "loading", "running", "error", "crashed", "restarting", "paused", "unavailable"];

/// Start the web-source service (returns immediately; never fails the engine).
pub async fn start(ctx: EngineCtx, auth: Arc<Auth>, patches: Patches) -> anyhow::Result<()> {
    start_with(ctx, auth, patches.subscribe()).await
}

/// [`start`] with an explicit patch-set receiver (tests and embedders).
pub async fn start_with(ctx: EngineCtx, auth: Arc<Auth>, patches: watch::Receiver<Arc<PatchSet>>) -> anyhow::Result<()> {
    let table = Arc::new(Mutex::new(Value::map()));
    {
        let table = table.clone();
        ctx.hub.register_query(
            "web",
            Arc::new(move |_, _| {
                let v = table.lock().clone();
                Box::pin(async move { Ok(v) })
            }),
        );
    }
    let actions = ctx.hub.route_actions("web");
    let (events_tx, events_rx) = mpsc::unbounded_channel();
    let sup = Supervisor::new(ctx, auth, table, events_tx);
    tokio::spawn(sup.run(patches, actions, events_rx));
    Ok(())
}

struct HostInfo {
    cef: String,
    chromium: String,
    pid: u32,
    gpu: bool,
    since: Instant,
}

/// What to do once a deliberately stopped host has exited.
enum After {
    Restart,
    Login(String),
    Idle,
}

enum HostState {
    /// Nothing running (not needed, not installed, or waiting to retry).
    Idle,
    Starting(Session, Instant),
    Ready(Session, HostInfo),
    Stopping(Session, Instant, After),
    /// The windowed sign-in browser owns the profile; off-screen sources are paused.
    Login(u32),
}

impl HostState {
    fn session(&self) -> Option<&Session> {
        match self {
            HostState::Starting(s, _) | HostState::Ready(s, _) | HostState::Stopping(s, _, _) => Some(s),
            HostState::Idle | HostState::Login(_) => None,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            HostState::Idle => "idle",
            HostState::Starting(..) => "starting",
            HostState::Ready(..) => "ready",
            HostState::Stopping(..) => "stopping",
            HostState::Login(_) => "login",
        }
    }
}

#[derive(Default, PartialEq)]
struct Published {
    fps: f64,
    crashes: i64,
    status: &'static str,
    error: String,
}

struct Source {
    spec: Spec,
    token: String,
    media: Arc<Mutex<Media>>,
    /// Browser id in the current host run.
    browser: Option<u32>,
    status: &'static str,
    error: String,
    /// Keep `error` visible until then even if the page loads fine again.
    hold_until: Option<Instant>,
    crashes: i64,
    fps: f64,
    frames_seen: u64,
    published: Option<Published>,
}

impl Source {
    fn page_url(&self) -> String {
        if self.spec.with_token { sources::with_token(&self.spec.url, &self.token) } else { self.spec.url.clone() }
    }

    fn set_error(&mut self, e: String, hold: bool) {
        self.error = e;
        self.hold_until = hold.then(|| Instant::now() + ERROR_HOLD);
    }

    /// The page loaded fine: clear the error unless a recent crash message must stay visible.
    fn healthy(&mut self) {
        if self.hold_until.is_none_or(|t| Instant::now() >= t) {
            self.error.clear();
            self.hold_until = None;
        }
    }
}

fn random_token() -> std::io::Result<String> {
    let mut b = [0u8; 32];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut b)?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// Restart delay after `failures` consecutive failures: 0.25 s, 0.5 s, 1 s … 30 s.
fn backoff(failures: u32) -> Duration {
    Duration::from_millis((250u64 << failures.saturating_sub(1).min(7)).min(30_000))
}

fn declare(hub: &Hub, slot: &str) {
    let a = |k: &str| format!("web.{slot}.{k}");
    hub.declare(&a("fps"), Meta::float(0.0, [0.0, 60.0]).readonly().unit("fps").owner("web").describe("Measured paint rate of the web source"));
    hub.declare(&a("crashes"), Meta::int(0, [0.0, 1e12]).readonly().owner("web").describe("Renderer crashes since the engine started"));
    hub.declare(&a("status"), Meta::enumeration("starting", STATUSES).readonly().owner("web").describe("Web source state"));
    hub.declare(&a("error"), Meta::string("").readonly().owner("web").describe("Last load or crash problem (empty when healthy)"));
}

struct Supervisor {
    ctx: EngineCtx,
    hub: Arc<Hub>,
    auth: Arc<Auth>,
    table: Arc<Mutex<Value>>,
    events_tx: mpsc::UnboundedSender<Event>,
    base: String,
    sources: BTreeMap<String, Source>,
    /// Sinks per slot, kept for the service lifetime (the hub slot is registered once).
    media: HashMap<String, Arc<Mutex<Media>>>,
    state: HostState,
    settings: HostSettings,
    paths: Option<HostPaths>,
    retry_at: Instant,
    failures: u32,
    restarts: u64,
    last_exit: Option<String>,
    next_id: u32,
    next_session: u64,
    ping_seq: u64,
    last_ping: Instant,
    outstanding_ping: Option<(u64, Instant)>,
    idle_since: Option<Instant>,
    config_errors: Vec<String>,
    health: Value,
    last_tick: Instant,
}

impl Supervisor {
    fn new(ctx: EngineCtx, auth: Arc<Auth>, table: Arc<Mutex<Value>>, events_tx: mpsc::UnboundedSender<Event>) -> Supervisor {
        let now = Instant::now();
        Supervisor {
            base: crate::sources::page_origin(ctx.http),
            hub: ctx.hub.clone(),
            ctx,
            auth,
            table,
            events_tx,
            sources: BTreeMap::new(),
            media: HashMap::new(),
            state: HostState::Idle,
            settings: HostSettings::default(),
            paths: None,
            retry_at: now,
            failures: 0,
            restarts: 0,
            last_exit: None,
            next_id: 1,
            next_session: 0,
            ping_seq: 0,
            last_ping: now,
            outstanding_ping: None,
            idle_since: None,
            config_errors: Vec::new(),
            health: Value::Null,
            last_tick: now,
        }
    }

    async fn run(
        mut self,
        mut patches: watch::Receiver<Arc<PatchSet>>,
        mut actions: mpsc::UnboundedReceiver<Command>,
        mut events: mpsc::UnboundedReceiver<Event>,
    ) {
        let mut config = self.ctx.config.clone();
        let mut tick = tokio::time::interval(TICK);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let (mut patches_open, mut config_open) = (true, true);
        let mut set = patches.borrow_and_update().clone();
        self.paths = host::find_host(&self.ctx.share_dir);
        self.reconcile(&set);
        loop {
            tokio::select! {
                r = patches.changed(), if patches_open => match r {
                    Ok(()) => {
                        set = patches.borrow_and_update().clone();
                        self.reconcile(&set);
                    }
                    Err(_) => patches_open = false,
                },
                r = config.changed(), if config_open => match r {
                    Ok(()) => {
                        config.borrow_and_update();
                        self.reconcile(&set);
                    }
                    Err(_) => config_open = false,
                },
                Some(ev) = events.recv() => self.on_event(ev),
                Some(cmd) = actions.recv() => self.on_action(cmd),
                _ = tick.tick() => {
                    self.reconcile(&set);
                    self.on_tick();
                }
                _ = tokio::time::sleep_until(self.retry_at.into()), if self.wants_host() => self.spawn_host(),
            }
            self.publish();
        }
    }

    /// Idle with sources to show and the restart delay over (spawn without waiting for a tick).
    fn wants_host(&self) -> bool {
        matches!(self.state, HostState::Idle) && !self.sources.is_empty() && self.installed()
    }

    fn log(&self, level: &str, msg: String) {
        match level {
            "error" => tracing::error!(target: "se_web", "{msg}"),
            "warn" => tracing::warn!(target: "se_web", "{msg}"),
            _ => tracing::info!(target: "se_web", "{msg}"),
        }
        self.hub.log(level, "web", msg);
    }

    fn installed(&self) -> bool {
        self.paths.is_some()
    }

    // ----- desired state ------------------------------------------------------------------

    fn reconcile(&mut self, set: &PatchSet) {
        let config = self.ctx.config.borrow().clone();
        let (desired, settings, errors) = sources::desired(&config, set, &self.base, &self.ctx.share_dir);
        if errors != self.config_errors {
            for e in &errors {
                self.log("error", format!("project.toml {e}"));
            }
            self.config_errors = errors;
        }
        if settings != self.settings {
            self.settings = settings;
            if matches!(self.state, HostState::Starting(..) | HostState::Ready(..)) {
                self.log("info", "[web] settings changed; restarting the CEF host".into());
                self.stop(After::Restart);
            }
        }
        let gone: Vec<String> = self.sources.keys().filter(|k| !desired.contains_key(*k)).cloned().collect();
        for slot in gone {
            self.close(&slot, set);
        }
        for (slot, spec) in desired {
            match self.sources.get(&slot) {
                None => self.add(spec),
                Some(src) if src.spec != spec => self.update(&slot, spec),
                Some(_) => {}
            }
        }
    }

    fn add(&mut self, spec: Spec) {
        let slot = spec.slot.clone();
        let token = match random_token() {
            Ok(t) => t,
            Err(e) => {
                self.log("error", format!("{slot}: no token ({e}); source not opened"));
                return;
            }
        };
        self.auth.add(&token, &format!("web.{}", spec.owner.scope_id()), spec.scope());
        let media = self.media.entry(slot.clone()).or_insert_with(|| Arc::new(Mutex::new(Media::new(&self.hub, &slot)))).clone();
        declare(&self.hub, &slot);
        let mut src = Source {
            spec,
            token,
            media,
            browser: None,
            status: "starting",
            error: String::new(),
            hold_until: None,
            crashes: 0,
            fps: 0.0,
            frames_seen: 0,
            published: None,
        };
        src.frames_seen = src.media.lock().frames;
        if !self.installed() {
            src.status = "unavailable";
            src.error = INSTALL_HINT.into();
        } else if matches!(self.state, HostState::Login(_)) {
            src.status = "paused";
        }
        self.sources.insert(slot.clone(), src);
        self.open(&slot);
    }

    fn update(&mut self, slot: &str, spec: Spec) {
        let Some(src) = self.sources.get_mut(slot) else { return };
        let old = std::mem::replace(&mut src.spec, spec);
        if old.youtube_account != src.spec.youtube_account {
            // Account policy is immutable in the native browser; never navigate an old delegate.
            if let Some(id) = src.browser.take() {
                self.send(&ToHost::Close { id });
            }
            self.open(slot);
            return;
        }
        if old.grants != src.spec.grants {
            // same token, new permissions: the page keeps working without a reload
            self.auth.add(&src.token, &format!("web.{}", src.spec.owner.scope_id()), src.spec.scope());
            let msg = format!("{slot}: page permissions now {:?}", src.spec.grants);
            tracing::info!(target: "se_web", "{msg}");
            self.hub.log("info", "web", msg);
        }
        let Some(id) = src.browser else { return };
        let new = &src.spec;
        let mut msgs = Vec::new();
        if old.url != new.url || old.with_token != new.with_token {
            msgs.push(ToHost::Navigate { id, url: src.page_url() });
        } else if old.generation != new.generation {
            msgs.push(ToHost::Reload { id });
        }
        if old.fps != new.fps {
            msgs.push(ToHost::SetFps { id, fps: new.fps });
        }
        if old.size != new.size {
            msgs.push(ToHost::Resize { id, width: new.size.0, height: new.size.1 });
        }
        for m in msgs {
            self.send(&m);
        }
    }

    fn close(&mut self, slot: &str, set: &PatchSet) {
        let Some(src) = self.sources.remove(slot) else { return };
        if let Some(id) = src.browser
            && let Some(s) = self.state.session()
        {
            s.routes.lock().remove(&id);
            self.send(&ToHost::Close { id });
        }
        self.auth.remove(&src.token);
        src.media.lock().clear();
        if let Owner::Patch(id) = &src.spec.owner
            && set.contains_key(id)
        {
            // disabled, not deleted: the loader keeps the address
            self.hub.publish(&format!("patch.{id}.error"), Value::Str(String::new()));
        }
        self.hub.submit(Input::Remove { prefix: format!("web.{slot}") });
    }

    /// Open the source's browser if the host is ready and it has none.
    fn open(&mut self, slot: &str) {
        let HostState::Ready(session, _) = &self.state else { return };
        let Some(src) = self.sources.get_mut(slot) else { return };
        if src.browser.is_some() {
            return;
        }
        let id = self.next_id;
        self.next_id += 1;
        session.routes.lock().insert(id, src.media.clone());
        let msg = ToHost::Open {
            id, url: src.page_url(), width: src.spec.size.0, height: src.spec.size.1, fps: src.spec.fps,
            youtube_account: src.spec.youtube_account.clone(),
        };
        src.browser = Some(id);
        src.status = "loading";
        self.send(&msg);
    }

    /// Send to the current host; a host that can't take a small command is wedged → restart.
    fn send(&mut self, msg: &ToHost) {
        let failed = match self.state.session() {
            Some(s) => s.send(msg).err(),
            None => return,
        };
        if let Some(e) = failed {
            self.log("error", format!("CEF host not accepting commands ({e}); restarting it"));
            if let Some(s) = match &mut self.state {
                HostState::Starting(s, _) | HostState::Ready(s, _) | HostState::Stopping(s, _, _) => Some(s),
                _ => None,
            } {
                s.kill();
            }
        }
    }

    fn slot_of(&self, id: u32) -> Option<String> {
        self.sources.iter().find(|(_, s)| s.browser == Some(id)).map(|(k, _)| k.clone())
    }

    // ----- host lifecycle -----------------------------------------------------------------

    fn spawn_host(&mut self) {
        self.paths = host::find_host(&self.ctx.share_dir);
        let Some(paths) = self.paths.clone() else {
            for s in self.sources.values_mut() {
                s.status = "unavailable";
                s.set_error(INSTALL_HINT.into(), false);
            }
            self.retry_at = Instant::now() + Duration::from_secs(5);
            return;
        };
        let cef_dir = self.ctx.data_dir.join("cef");
        let args = HostArgs {
            profile: cef_dir.join("profile"),
            log_file: cef_dir.join("host.log"),
            gpu: self.settings.gpu,
            devtools_port: self.settings.devtools_port,
        };
        self.next_session += 1;
        match host::spawn(&paths, &args, self.next_session, self.events_tx.clone()) {
            Ok(s) => {
                tracing::info!(target: "se_web", "started {} (pid {})", paths.exe.display(), s.pid);
                self.state = HostState::Starting(s, Instant::now());
                for src in self.sources.values_mut() {
                    if src.status == "unavailable" {
                        src.status = "starting";
                        src.error.clear();
                    }
                }
            }
            Err(e) => {
                self.failures += 1;
                self.last_exit = Some(format!("cannot start {}: {e}", paths.exe.display()));
                self.retry_at = Instant::now() + backoff(self.failures);
                self.log("error", format!("cannot start the CEF host {}: {e}", paths.exe.display()));
            }
        }
    }

    fn stop(&mut self, after: After) {
        match std::mem::replace(&mut self.state, HostState::Idle) {
            HostState::Starting(mut s, _) | HostState::Ready(mut s, _) => {
                if s.send(&ToHost::Shutdown).is_err() {
                    s.kill();
                }
                self.state = HostState::Stopping(s, Instant::now(), after);
            }
            HostState::Stopping(s, t, _) => self.state = HostState::Stopping(s, t, after),
            other => self.state = other,
        }
        for src in self.sources.values_mut() {
            src.browser = None;
        }
    }

    fn start_login(&mut self, url: String) {
        let Some(paths) = host::find_host(&self.ctx.share_dir) else {
            self.log("error", format!("web.login: {INSTALL_HINT}"));
            return;
        };
        let profile = self.ctx.data_dir.join("cef/profile");
        match host::spawn_login(&paths, &profile, &url, self.events_tx.clone()) {
            Ok(pid) => {
                self.state = HostState::Login(pid);
                for src in self.sources.values_mut() {
                    src.browser = None;
                    src.status = "paused";
                }
                self.log("info", format!("sign-in window opened ({}); web sources resume when it is closed", sources::redact(&url)));
            }
            Err(e) => {
                self.log("error", format!("web.login: cannot start {}: {e}", paths.exe.display()));
                self.retry_at = Instant::now();
            }
        }
    }

    fn on_exit(&mut self, status: String) {
        let state = std::mem::replace(&mut self.state, HostState::Idle);
        for src in self.sources.values_mut() {
            src.browser = None;
            src.media.lock().detach();
        }
        let (was_ready, uptime) = match &state {
            HostState::Ready(_, info) => (true, info.since.elapsed()),
            _ => (false, Duration::ZERO),
        };
        match state {
            HostState::Stopping(_, _, After::Restart) | HostState::Stopping(_, _, After::Idle) => {
                self.retry_at = Instant::now();
                tracing::info!(target: "se_web", "CEF host stopped ({status})");
            }
            HostState::Stopping(_, _, After::Login(url)) => self.start_login(url),
            _ => {
                self.restarts += 1;
                self.failures = if was_ready && uptime > STABLE_AFTER { 1 } else { self.failures + 1 };
                let delay = backoff(self.failures);
                self.retry_at = Instant::now() + delay;
                self.log("error", format!("CEF host exited ({status}); restarting in {:.1} s", delay.as_secs_f32()));
                for src in self.sources.values_mut() {
                    src.status = "restarting";
                    src.set_error(format!("CEF host exited ({status}) — restarting"), true);
                }
                self.last_exit = Some(status);
            }
        }
        self.outstanding_ping = None;
    }

    // ----- events -------------------------------------------------------------------------

    fn on_event(&mut self, ev: Event) {
        let current = self.state.session().map(|s| s.id);
        match ev {
            Event::Msg { session, msg } if Some(session) == current => self.on_msg(msg),
            Event::Exited { session, status } if Some(session) == current => self.on_exit(status),
            Event::LoginExited { status } => {
                if matches!(self.state, HostState::Login(_)) {
                    self.state = HostState::Idle;
                    self.retry_at = Instant::now();
                    self.log("info", format!("sign-in window closed ({status}); restarting web sources"));
                }
            }
            Event::Msg { .. } | Event::Exited { .. } => {}
        }
    }

    fn on_msg(&mut self, msg: FromHost) {
        match msg {
            FromHost::Hello { protocol, cef, chromium, pid, gpu } => {
                if protocol != PROTOCOL_VERSION {
                    self.log("error", format!("CEF host speaks protocol {protocol}, engine {PROTOCOL_VERSION}: reinstall it (scripts/install-cef.sh)"));
                    self.stop(After::Idle);
                    self.retry_at = Instant::now() + Duration::from_secs(3600);
                    return;
                }
                if !matches!(self.state, HostState::Starting(..)) {
                    return;
                }
                let HostState::Starting(s, _) = std::mem::replace(&mut self.state, HostState::Idle) else { return };
                self.log(
                    "info",
                    format!("CEF host ready: CEF {cef}, Chromium {chromium}, pid {pid}, {}", if gpu { "GPU (ANGLE/Vulkan)" } else { "software rendering" }),
                );
                self.state = HostState::Ready(s, HostInfo { cef, chromium, pid, gpu, since: Instant::now() });
                self.last_ping = Instant::now();
                let slots: Vec<String> = self.sources.keys().cloned().collect();
                for slot in slots {
                    self.open(&slot);
                }
            }
            FromHost::Loading { id } => {
                if let Some(src) = self.slot_of(id).and_then(|k| self.sources.get_mut(&k))
                    && src.status != "crashed"
                {
                    src.status = "loading";
                }
            }
            FromHost::Loaded { id, status } => {
                let Some(slot) = self.slot_of(id) else { return };
                let Some(src) = self.sources.get_mut(&slot) else { return };
                if status >= 400 {
                    src.status = "error";
                    let e = format!("HTTP {status} loading {}", sources::redact(&src.page_url()));
                    src.set_error(e.clone(), false);
                    self.log("warn", format!("{slot}: {e}"));
                } else {
                    src.status = "running";
                    src.healthy();
                }
            }
            FromHost::LoadFailed { id, url, code, text } => {
                let Some(slot) = self.slot_of(id) else { return };
                let Some(src) = self.sources.get_mut(&slot) else { return };
                src.status = "error";
                let e = format!("load failed: {} ({text}, {code})", sources::redact(&url));
                src.set_error(e.clone(), false);
                self.log("warn", format!("{slot}: {e}"));
            }
            FromHost::RendererGone { id, reason, retry_ms } => {
                let Some(slot) = self.slot_of(id) else { return };
                let Some(src) = self.sources.get_mut(&slot) else { return };
                src.crashes += 1;
                src.status = "crashed";
                src.set_error(format!("renderer crashed ({reason}) — restarted"), true);
                self.log("warn", format!("{slot}: renderer crashed ({reason}); reloading in {retry_ms} ms"));
            }
            FromHost::Closed { id } => {
                if let Some(s) = self.state.session() {
                    s.routes.lock().remove(&id);
                }
                // A page closed itself (window.close()): reopen it on the next tick.
                if let Some(src) = self.slot_of(id).and_then(|k| self.sources.get_mut(&k)) {
                    src.browser = None;
                    src.status = "starting";
                }
            }
            FromHost::Pong { seq } => {
                if self.outstanding_ping.is_some_and(|(s, _)| s == seq) {
                    self.outstanding_ping = None;
                }
            }
            FromHost::Log { level, msg } => {
                let level = match level.as_str() {
                    "error" | "warn" => level,
                    _ => "info".to_string(),
                };
                self.log(&level, sources::redact(&msg));
            }
            FromHost::Console { id, msg } => {
                let slot = self.slot_of(id).unwrap_or_else(|| format!("browser {id}"));
                self.hub.log("warn", "web", format!("{slot}: console: {}", sources::redact(&msg)));
            }
            FromHost::Surface { .. } | FromHost::Frame { .. } | FromHost::AudioRing { .. } | FromHost::Audio { .. } => {}
        }
    }

    fn on_action(&mut self, cmd: Command) {
        let Op::Action { name, args } = &cmd.op else { return };
        let arg = |key: &str| args.get_path(key).or_else(|| args.get_path("args.0")).and_then(Value::as_str).map(str::to_string);
        match name.as_str() {
            "web.reload" => {
                let targets: Vec<(String, Option<u32>)> = match arg("slot") {
                    Some(slot) => match self.sources.get(&slot) {
                        Some(s) => vec![(slot, s.browser)],
                        None => {
                            let known: Vec<&String> = self.sources.keys().collect();
                            self.log("warn", format!("web.reload: no web source `{slot}` (sources: {known:?})"));
                            return;
                        }
                    },
                    None => self.sources.iter().map(|(k, s)| (k.clone(), s.browser)).collect(),
                };
                for (slot, browser) in targets {
                    if let Some(id) = browser {
                        self.send(&ToHost::Reload { id });
                        tracing::info!(target: "se_web", "{slot}: reload");
                    }
                }
            }
            "web.login" => {
                let url = arg("url").unwrap_or_else(|| DEFAULT_LOGIN_URL.to_string());
                if !(url.starts_with("https://") || url.starts_with("http://")) {
                    self.log("error", format!("web.login: `{url}` is not an http(s) URL"));
                    return;
                }
                if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
                    self.log(
                        "error",
                        "web.login needs a graphical session: DISPLAY or WAYLAND_DISPLAY must be set for the engine (systemctl --user import-environment DISPLAY WAYLAND_DISPLAY)".into(),
                    );
                    return;
                }
                if !self.installed() && host::find_host(&self.ctx.share_dir).is_none() {
                    self.log("error", format!("web.login: {INSTALL_HINT}"));
                    return;
                }
                match &mut self.state {
                    HostState::Login(_) => self.log("info", "the sign-in window is already open".into()),
                    HostState::Stopping(_, _, after) => *after = After::Login(url),
                    HostState::Idle => self.start_login(url),
                    HostState::Starting(..) | HostState::Ready(..) => self.stop(After::Login(url)),
                }
            }
            other => self.log("warn", format!("unknown action {other} (web.reload [slot], web.login [url])")),
        }
    }

    // ----- periodic work ------------------------------------------------------------------

    fn on_tick(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.last_tick).as_secs_f64().max(1e-3);
        self.last_tick = now;
        let mut kill = None;
        match &mut self.state {
            HostState::Idle => {
                self.paths = host::find_host(&self.ctx.share_dir);
                if !self.sources.is_empty() && now >= self.retry_at {
                    self.spawn_host();
                }
            }
            HostState::Starting(_, since) => {
                if since.elapsed() > HELLO_TIMEOUT {
                    kill = Some(format!("CEF host did not start within {} s", HELLO_TIMEOUT.as_secs()));
                }
            }
            HostState::Ready(s, info) => {
                if let Some((_, sent)) = self.outstanding_ping {
                    if sent.elapsed() > PONG_TIMEOUT {
                        kill = Some(format!("CEF host unresponsive for {} s", PONG_TIMEOUT.as_secs()));
                    }
                } else if self.last_ping.elapsed() > PING_EVERY {
                    self.ping_seq += 1;
                    self.last_ping = now;
                    self.outstanding_ping = Some((self.ping_seq, now));
                    if s.send(&ToHost::Ping { seq: self.ping_seq }).is_err() {
                        kill = Some("CEF host not accepting commands".into());
                    }
                }
                if info.since.elapsed() > STABLE_AFTER {
                    self.failures = 0;
                }
            }
            HostState::Stopping(_, since, _) => {
                if since.elapsed() > STOP_TIMEOUT {
                    kill = Some("CEF host did not stop in time".into());
                }
            }
            HostState::Login(_) => {}
        }
        if let Some(reason) = kill {
            self.log("error", format!("{reason}; killing it"));
            if let HostState::Starting(s, _) | HostState::Ready(s, _) | HostState::Stopping(s, _, _) = &mut self.state {
                s.kill();
            }
        }
        // Stop an idle host after a while (no web sources left).
        if self.sources.is_empty() && matches!(self.state, HostState::Ready(..)) {
            let since = *self.idle_since.get_or_insert(now);
            if now.duration_since(since) > IDLE_LINGER {
                self.stop(After::Idle);
                self.idle_since = None;
            }
        } else {
            self.idle_since = None;
        }
        let ready = matches!(self.state, HostState::Ready(..));
        let mut reopen = Vec::new();
        for (slot, src) in self.sources.iter_mut() {
            let frames = src.media.lock().frames;
            let rate = (frames.saturating_sub(src.frames_seen)) as f64 / dt;
            src.frames_seen = frames;
            src.fps = if src.fps == 0.0 { rate } else { src.fps * 0.5 + rate * 0.5 };
            if src.fps < 0.05 {
                src.fps = 0.0;
            }
            if src.hold_until.is_some_and(|t| now >= t) {
                src.hold_until = None;
                if src.status == "running" {
                    src.error.clear();
                }
            }
            if ready && src.browser.is_none() {
                reopen.push(slot.clone());
            }
        }
        for slot in reopen {
            self.open(&slot);
        }
    }

    // ----- publication --------------------------------------------------------------------

    fn health(&self) -> Value {
        let (status, detail) = match &self.state {
            HostState::Ready(_, i) => (
                "pass",
                format!(
                    "CEF {} (Chromium {}), pid {}, {} source(s), {}",
                    i.cef,
                    i.chromium,
                    i.pid,
                    self.sources.len(),
                    if i.gpu { "GPU" } else { "software rendering" }
                ),
            ),
            HostState::Login(_) => ("warn", "sign-in window open; web sources paused until it is closed".into()),
            HostState::Stopping(..) => ("warn", "restarting the CEF host".into()),
            HostState::Starting(..) | HostState::Idle if !self.installed() => ("warn", INSTALL_HINT.into()),
            HostState::Starting(..) if self.failures < 3 => ("warn", "starting the CEF host".into()),
            HostState::Idle if self.sources.is_empty() => ("pass", "CEF installed; idle (no web sources)".into()),
            HostState::Idle | HostState::Starting(..) => match &self.last_exit {
                None => ("warn", "starting the CEF host".into()),
                Some(why) => {
                    let st = if self.failures >= 5 { "fail" } else { "warn" };
                    (st, format!("CEF host {why}; restarting (see {})", self.ctx.data_dir.join("cef/host.log").display()))
                }
            },
        };
        Value::map().with("status", status).with("detail", detail)
    }

    fn publish(&mut self) {
        let h = self.health();
        if h != self.health {
            self.hub.publish("health.cef", h.clone());
            self.health = h;
        }
        for (slot, src) in self.sources.iter_mut() {
            let now = Published { fps: (src.fps * 10.0).round() / 10.0, crashes: src.crashes, status: src.status, error: src.error.clone() };
            let old = src.published.take().unwrap_or(Published { fps: -1.0, crashes: -1, status: "", error: "\u{0}".into() });
            let a = |k: &str| format!("web.{slot}.{k}");
            if now.fps != old.fps {
                self.hub.publish(&a("fps"), Value::Float(now.fps));
            }
            if now.crashes != old.crashes {
                self.hub.publish(&a("crashes"), Value::Int(now.crashes));
            }
            if now.status != old.status {
                self.hub.publish(&a("status"), Value::Str(now.status.into()));
            }
            if now.error != old.error {
                self.hub.publish(&a("error"), Value::Str(now.error.clone()));
                if let Owner::Patch(id) = &src.spec.owner {
                    self.hub.publish(&format!("patch.{id}.error"), Value::Str(now.error.clone()));
                }
            }
            src.published = Some(now);
        }
        self.update_table();
    }

    fn update_table(&self) {
        let (pid, cef, chromium, gpu) = match &self.state {
            HostState::Ready(_, i) => (Value::Int(i.pid as i64), Value::Str(i.cef.clone()), Value::Str(i.chromium.clone()), Value::Bool(i.gpu)),
            HostState::Starting(s, _) | HostState::Stopping(s, _, _) => (Value::Int(s.pid as i64), Value::Null, Value::Null, Value::Null),
            HostState::Login(pid) => (Value::Int(*pid as i64), Value::Null, Value::Null, Value::Null),
            HostState::Idle => (Value::Null, Value::Null, Value::Null, Value::Null),
        };
        let host = Value::map()
            .with("state", self.state.name())
            .with("pid", pid)
            .with("cef", cef)
            .with("chromium", chromium)
            .with("gpu", gpu)
            .with("installed", self.installed())
            .with("exe", self.paths.as_ref().map(|p| Value::Str(p.exe.display().to_string())).unwrap_or(Value::Null))
            .with("restarts", Value::Int(self.restarts as i64))
            .with("health", self.health.clone());
        let list: Vec<Value> = self
            .sources
            .iter()
            .map(|(slot, s)| {
                let m = s.media.lock();
                Value::map()
                    .with("slot", slot.as_str())
                    .with("url", sources::redact(&s.page_url()))
                    .with("size", Value::List(vec![Value::Int(s.spec.size.0 as i64), Value::Int(s.spec.size.1 as i64)]))
                    .with("frame_size", Value::List(vec![Value::Int(m.size.0 as i64), Value::Int(m.size.1 as i64)]))
                    .with("fps", Value::Float((s.fps * 10.0).round() / 10.0))
                    .with("target_fps", Value::Int(s.spec.fps as i64))
                    .with("status", s.status)
                    .with("error", s.error.as_str())
                    .with("crashes", Value::Int(s.crashes))
                    .with("frames", Value::Int(m.frames as i64))
                    .with("latency_ms", Value::Float((m.latency_ms * 100.0).round() / 100.0))
                    .with("audio_samples", Value::Int(m.audio_samples as i64))
            })
            .collect();
        *self.table.lock() = Value::map().with("host", host).with("sources", Value::List(list));
    }
}
