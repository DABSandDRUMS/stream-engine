//! The mixer subsystem: connection lifecycle (discovery, backoff), console state ↔ engine
//! state, meters → signals, `mic.talking`, `mixer.*` actions, queries, and `health.mixer`.

use crate::config::{ChannelRef, MixerConfig};
use crate::map::{Control, Coverage, Kind, Param, fader_to_db};
use crate::snapshots::{self, Step};
use crate::sync::{Remote, SyncConfig, SyncEngine};
use crate::talk::TalkDetector;
use crate::ucnet::client::{self, ClientEvent, ClientOptions, Session};
use crate::ucnet::discovery::{self, Device, Via};
use crate::ucnet::meters::{LevelFrame, group, to_db};
use crate::ucnet::packet::CONTROL_PORT;
use crate::ucnet::tree::{ConsoleInfo, ConsoleState};
use parking_lot::Mutex;
use se_hub::{Bus, EngineCtx, Hub};
use se_proto::{Command, Ease, Event, Meta, Op, Origin, PRIORITY_CHAT, PRIORITY_MANUAL, PRIORITY_PRESET, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc};
use tokio::task::JoinHandle;

/// Override key of snapshot recalls (one layer, a newer recall replaces an older one).
pub const KEY_SNAPSHOT: &str = "mixer:snapshot";
/// Key used to release engine overrides when the console operator takes a control over.
/// Never used for an override, so a manual-priority release with it releases all of them.
pub const KEY_TOUCH: &str = "mixer:touch";
/// Console changes closer together than this belong to one gesture (one release).
const GESTURE: Duration = Duration::from_millis(500);
const EVENT_FLUSH: Duration = Duration::from_millis(100);
const METER_STALE: Duration = Duration::from_secs(5);
const HOUSEKEEPING: Duration = Duration::from_secs(1);
const BACKOFF: [u64; 6] = [1, 2, 4, 8, 16, 30];

/// What queries read.
#[derive(Debug, Clone, Default)]
pub struct Status {
    pub enabled: bool,
    pub connected: bool,
    pub device: Option<Device>,
    pub info: ConsoleInfo,
    pub coverage: Vec<(String, String, bool, &'static str)>,
    pub lines: usize,
    pub auxes: usize,
    pub fxes: usize,
    pub meters_at: Option<Instant>,
    pub last_error: Option<String>,
    pub unconfirmed: u64,
    pub talking: bool,
    pub talk_channel: Option<u16>,
    pub health: (String, String),
    pub prefix: String,
}

impl Status {
    fn to_value(&self) -> Value {
        let mut v = Value::map()
            .with("enabled", self.enabled)
            .with("connected", self.connected)
            .with("prefix", self.prefix.clone())
            .with("model", self.info.model.clone())
            .with("name", self.info.name.clone())
            .with("firmware", self.info.firmware.clone())
            .with("serial", self.info.serial.clone())
            .with("channels", self.lines)
            .with("auxes", self.auxes)
            .with("fxes", self.fxes)
            .with("controls", self.coverage.len())
            .with("meters", self.meters_at.is_some_and(|t| t.elapsed() < METER_STALE))
            .with("unconfirmed", self.unconfirmed)
            .with("talking", self.talking)
            .with("health", Value::map().with("status", self.health.0.clone()).with("detail", self.health.1.clone()));
        if let Some(d) = &self.device {
            v = v.with("host", d.addr.to_string()).with("via", format!("{:?}", d.via).to_lowercase());
        }
        if let Some(e) = &self.last_error {
            v = v.with("last_error", e.clone());
        }
        if let Some(c) = self.talk_channel {
            v = v.with("talk_channel", c as i64);
        }
        v
    }
}

type Shared = Arc<Mutex<Status>>;

/// Start the mixer subsystem (returns immediately; work runs on spawned tasks).
pub async fn start(ctx: EngineCtx) -> anyhow::Result<()> {
    let (cfg, err) = match MixerConfig::parse(ctx.project_section("mixer").as_ref()) {
        Ok(c) => (c, None),
        Err(e) => (MixerConfig::default(), Some(e)),
    };
    let status: Shared = Arc::new(Mutex::new(Status { enabled: cfg.enabled, prefix: cfg.prefix(), ..Default::default() }));
    register_queries(&ctx, status.clone());
    let actions = ctx.hub.route_actions("mixer");
    let bus = ctx.hub.subscribe();
    let svc = Service::new(ctx, cfg, err, status);
    tokio::spawn(svc.run(actions, bus));
    Ok(())
}

fn register_queries(ctx: &EngineCtx, status: Shared) {
    let ctx2 = ctx.clone();
    ctx.hub.register_query(
        "mixer",
        Arc::new(move |name: String, args: Value| {
            let status = status.clone();
            let ctx = ctx2.clone();
            Box::pin(async move {
                match name.as_str() {
                    "mixer" | "mixer.status" => Ok(status.lock().to_value()),
                    "mixer.coverage" => Ok(Value::List(
                        status
                            .lock()
                            .coverage
                            .iter()
                            .map(|(a, p, w, k)| Value::map().with("address", a.clone()).with("path", p.clone()).with("writable", *w).with("kind", *k))
                            .collect(),
                    )),
                    "mixer.snapshots" => {
                        let prefix = status.lock().prefix.clone();
                        let (snaps, errors) = snapshots::load_all(&ctx.kind("mixes"), &prefix);
                        let list: Vec<Value> = snaps
                            .values()
                            .map(|s| {
                                Value::map()
                                    .with("name", s.name.clone())
                                    .with("label", s.label.clone().unwrap_or_else(|| s.name.clone()))
                                    .with("fade_ms", s.fade_ms.map(|f| f as i64).unwrap_or(0))
                                    .with("values", s.values.len())
                            })
                            .collect();
                        Ok(Value::map().with("snapshots", Value::List(list)).with("errors", Value::List(errors.into_iter().map(Value::Str).collect())))
                    }
                    "mixer.discover" => {
                        let secs = args.get_path("seconds").and_then(Value::as_f64).unwrap_or(4.0).clamp(1.0, 15.0);
                        let mut found = discovery::listen(Duration::from_secs_f64(secs)).await.map_err(|e| e.to_string())?;
                        let heard = !found.is_empty();
                        if !heard {
                            found = discovery::probe_lan(CONTROL_PORT).await;
                        }
                        let list = found
                            .iter()
                            .map(|d| {
                                Value::map()
                                    .with("host", d.addr.to_string())
                                    .with("model", d.model.clone())
                                    .with("serial", d.serial.clone())
                                    .with("name", d.name.clone())
                                    .with("via", format!("{:?}", d.via).to_lowercase())
                            })
                            .collect();
                        let mut out = Value::map().with("devices", Value::List(list)).with("broadcast_heard", heard);
                        if !heard {
                            out = out.with("firewall_hint", discovery::FIREWALL_HINT);
                        }
                        Ok(out)
                    }
                    other => Err(format!("unknown query `{other}` (mixer.status, mixer.coverage, mixer.snapshots, mixer.discover)")),
                }
            })
        }),
    );
}

type Attempt = Result<(Session, JoinHandle<String>, mpsc::Receiver<ClientEvent>, Device), String>;

enum Wake {
    Client(Option<ClientEvent>),
    Attempt(Option<Attempt>),
    Closed(String),
    Bus(Result<Arc<Bus>, broadcast::error::RecvError>),
    Action(Option<Command>),
    Config(bool),
    Timer,
}

struct EndSteps {
    at: Instant,
    generation: u64,
    steps: Vec<Step>,
    origin: Origin,
    priority: u16,
    causal: se_proto::Id,
}

struct Service {
    ctx: EngineCtx,
    hub: Arc<Hub>,
    cfg: MixerConfig,
    cfg_error: Option<String>,
    status: Shared,
    sync: SyncEngine,
    /// All declared controls (full address → control).
    controls: BTreeMap<String, Control>,
    coverage: Option<Coverage>,
    session: Option<Session>,
    task: Option<JoinHandle<String>>,
    events: Option<mpsc::Receiver<ClientEvent>>,
    attempt_tx: mpsc::Sender<Attempt>,
    attempt_rx: mpsc::Receiver<Attempt>,
    attempting: bool,
    next_attempt: Instant,
    failures: usize,
    connected_at: Option<Instant>,
    /// Ignore resolved changes while a fresh connection settles.
    settling: bool,
    talk: TalkDetector,
    talk_index: Option<usize>,
    meter_sigs: Vec<(u16, usize, String)>,
    meter_frames: u64,
    changed: BTreeMap<String, Value>,
    changed_flush: Option<Instant>,
    touch: HashMap<String, Instant>,
    recall_generation: u64,
    end_steps: Vec<EndSteps>,
    /// Addresses a snapshot recall has put an override on (panic stops their fades).
    snapshot_addrs: BTreeSet<String>,
    housekeeping: Instant,
    client_id: String,
    health: (String, String),
    warned_talk: bool,
}

async fn recv_opt(rx: &mut Option<mpsc::Receiver<ClientEvent>>) -> Option<ClientEvent> {
    match rx {
        Some(r) => r.recv().await,
        None => std::future::pending().await,
    }
}

async fn join_opt(t: &mut Option<JoinHandle<String>>) -> String {
    match t {
        Some(h) => h.await.unwrap_or_else(|e| format!("session task failed: {e}")),
        None => std::future::pending().await,
    }
}

fn client_id(ctx: &EngineCtx) -> String {
    if let Ok(Some(Value::Str(s))) = ctx.db.kv_get("mixer", "client_id")
        && s.len() == 16
    {
        return s;
    }
    let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0) ^ (std::process::id() as u128) << 64;
    let id = format!("5e{:014x}", (seed ^ (seed >> 57)) as u64 & 0x00ff_ffff_ffff_ffff);
    let _ = ctx.db.kv_set("mixer", "client_id", &Value::Str(id.clone()));
    id
}

/// Hard caps from `[safety] caps` (the core enforces them on resolved values).
fn safety_caps(ctx: &EngineCtx) -> Vec<(String, [f64; 2])> {
    let Some(toml::Value::Table(s)) = ctx.project_section("safety") else { return Vec::new() };
    let Some(caps) = s.get("caps").and_then(|c| c.as_table()) else { return Vec::new() };
    caps.iter()
        .filter_map(|(k, v)| {
            let a = v.as_array()?;
            let lo = a.first()?.as_float().or_else(|| a.first()?.as_integer().map(|i| i as f64))?;
            let hi = a.get(1)?.as_float().or_else(|| a.get(1)?.as_integer().map(|i| i as f64))?;
            Some((k.clone(), [lo, hi]))
        })
        .collect()
}

fn text_value(ctl: &Control, st: &ConsoleState) -> Option<Value> {
    let path = ctl.path();
    match ctl.param {
        Param::FxType => {
            let n = st.num(&path)?;
            let label = st.enums.get(&path).and_then(|opts| {
                let i = if opts.len() > 1 { (n * (opts.len() - 1) as f64).round() as usize } else { 0 };
                opts.get(i).cloned()
            });
            Some(Value::Str(label.unwrap_or_else(|| format!("{n}"))))
        }
        _ => st.text(&path).map(|s| Value::Str(s.to_string())),
    }
}

impl Service {
    fn new(ctx: EngineCtx, cfg: MixerConfig, cfg_error: Option<String>, status: Shared) -> Service {
        let (attempt_tx, attempt_rx) = mpsc::channel(2);
        let hub = ctx.hub.clone();
        let client_id = client_id(&ctx);
        let sync = SyncEngine::new(SyncConfig { min_interval: cfg.min_interval(), ..Default::default() });
        let talk = TalkDetector::new(&cfg.talk);
        Service {
            ctx,
            hub,
            cfg,
            cfg_error,
            status,
            sync,
            controls: BTreeMap::new(),
            coverage: None,
            session: None,
            task: None,
            events: None,
            attempt_tx,
            attempt_rx,
            attempting: false,
            next_attempt: Instant::now(),
            failures: 0,
            connected_at: None,
            settling: false,
            talk,
            talk_index: None,
            meter_sigs: Vec::new(),
            meter_frames: 0,
            changed: BTreeMap::new(),
            changed_flush: None,
            touch: HashMap::new(),
            recall_generation: 0,
            end_steps: Vec::new(),
            snapshot_addrs: BTreeSet::new(),
            housekeeping: Instant::now() + HOUSEKEEPING,
            client_id,
            health: (String::new(), String::new()),
            warned_talk: false,
        }
    }

    fn prefix(&self) -> String {
        self.cfg.prefix()
    }

    fn log(&self, level: &str, msg: impl Into<String>) {
        let msg = msg.into();
        match level {
            "error" => tracing::error!(target: "mixer", "{msg}"),
            "warn" => tracing::warn!(target: "mixer", "{msg}"),
            _ => tracing::info!(target: "mixer", "{msg}"),
        }
        self.hub.log(level, "mixer", msg);
    }

    fn set_health(&mut self, status: &str, detail: String) {
        if self.health.0 == status && self.health.1 == detail {
            return;
        }
        self.health = (status.to_string(), detail.clone());
        self.status.lock().health = self.health.clone();
        self.hub.publish("health.mixer", Value::map().with("status", status).with("detail", detail));
    }

    fn declare_base(&self) {
        let p = self.prefix();
        self.hub.declare(&format!("{p}.connected"), Meta::boolean(false).readonly().owner("mixer").describe("Control session with the console is up"));
        self.hub.publish(&format!("{p}.connected"), Value::Bool(self.session.is_some()));
    }

    async fn run(mut self, mut actions: mpsc::UnboundedReceiver<Command>, mut bus: broadcast::Receiver<Arc<Bus>>) {
        let mut config = self.ctx.config.clone();
        self.declare_base();
        if let Some(e) = self.cfg_error.clone() {
            self.log("error", format!("[mixer] config: {e}; fix project.toml (mixer stays offline until then)"));
            self.set_health("fail", format!("config error: {e}"));
        } else if !self.cfg.enabled {
            self.set_health("pass", "disabled in [mixer]".into());
        } else {
            self.set_health("warn", "searching for a StudioLive console…".into());
        }
        loop {
            let wake_at = self.next_wake();
            let w = tokio::select! {
                e = recv_opt(&mut self.events) => Wake::Client(e),
                a = self.attempt_rx.recv() => Wake::Attempt(a),
                r = join_opt(&mut self.task) => Wake::Closed(r),
                m = bus.recv() => Wake::Bus(m),
                c = actions.recv() => Wake::Action(c),
                r = config.changed() => Wake::Config(r.is_ok()),
                _ = tokio::time::sleep_until(tokio::time::Instant::from_std(wake_at)) => Wake::Timer,
            };
            let now = Instant::now();
            match w {
                Wake::Client(Some(ev)) => self.on_client(ev, now),
                Wake::Client(None) => self.events = None,
                Wake::Attempt(Some(Ok((s, task, rx, dev)))) => {
                    self.attempting = false;
                    self.on_connected(s, task, rx, dev).await;
                }
                Wake::Attempt(Some(Err(e))) => {
                    self.attempting = false;
                    self.on_attempt_failed(e);
                }
                Wake::Attempt(None) => {}
                Wake::Closed(reason) => {
                    self.task = None;
                    self.on_disconnected(reason);
                }
                Wake::Bus(Ok(m)) => {
                    if let Bus::Changes(ch) = &*m {
                        self.on_changes(ch, now);
                    }
                }
                Wake::Bus(Err(broadcast::error::RecvError::Lagged(n))) => {
                    tracing::debug!(target: "mixer", "bus lagged by {n}; resyncing from the snapshot");
                    self.resync_from_snapshot(now);
                }
                Wake::Bus(Err(broadcast::error::RecvError::Closed)) => break,
                Wake::Action(Some(cmd)) => self.on_action(cmd).await,
                Wake::Action(None) => break,
                Wake::Config(true) => self.on_config().await,
                Wake::Config(false) => break,
                Wake::Timer => {}
            }
            self.timers(Instant::now()).await;
        }
        self.session = None;
    }

    fn next_wake(&self) -> Instant {
        let mut t = self.housekeeping;
        if let Some(d) = self.sync.next_due() {
            t = t.min(d);
        }
        if let Some(f) = self.changed_flush {
            t = t.min(f);
        }
        if let Some(e) = self.end_steps.iter().map(|e| e.at).min() {
            t = t.min(e);
        }
        if self.wants_attempt() {
            t = t.min(self.next_attempt);
        }
        t
    }

    fn wants_attempt(&self) -> bool {
        self.cfg.enabled && self.cfg_error.is_none() && self.session.is_none() && !self.attempting
    }

    async fn timers(&mut self, now: Instant) {
        if self.wants_attempt() && now >= self.next_attempt {
            self.spawn_attempt();
        }
        self.flush_sends(now);
        if self.changed_flush.is_some_and(|f| now >= f) {
            self.flush_changed();
        }
        if self.end_steps.iter().any(|e| now >= e.at) {
            let (due, rest): (Vec<EndSteps>, Vec<EndSteps>) = std::mem::take(&mut self.end_steps).into_iter().partition(|e| now >= e.at);
            self.end_steps = rest;
            for e in due {
                if e.generation == self.recall_generation {
                    self.submit_steps(&e.steps, e.origin, e.priority, e.causal);
                }
            }
        }
        if now >= self.housekeeping {
            self.housekeeping = now + HOUSEKEEPING;
            let lost = self.sync.expire(now);
            if lost > 0 {
                self.log("warn", format!("{lost} change(s) sent to the console were not confirmed within 2 s"));
            }
            self.status.lock().unconfirmed = self.sync.unconfirmed;
            self.update_health();
        }
    }

    // ---- connection ---------------------------------------------------------------------

    fn spawn_attempt(&mut self) {
        self.attempting = true;
        let cfg = self.cfg.clone();
        let tx = self.attempt_tx.clone();
        let cached: Option<SocketAddr> = match self.ctx.db.kv_get("mixer", "last_host") {
            Ok(Some(Value::Str(s))) => s.parse().ok(),
            _ => None,
        };
        let opts = ClientOptions {
            description: cfg.client_name.clone(),
            identifier: self.client_id.clone(),
            meters: cfg.meters,
            meter_port: cfg.meter_port,
            ..Default::default()
        };
        tokio::spawn(async move {
            let r = attempt(&cfg, cached, &opts).await;
            let _ = tx.send(r).await;
        });
    }

    fn on_attempt_failed(&mut self, e: String) {
        self.failures += 1;
        let wait = BACKOFF[(self.failures - 1).min(BACKOFF.len() - 1)];
        self.next_attempt = Instant::now() + Duration::from_secs(wait);
        if self.failures == 1 || self.failures.is_multiple_of(10) {
            self.log("warn", format!("{e}; retrying (every ≤{}s)", BACKOFF[BACKOFF.len() - 1]));
        }
        self.status.lock().last_error = Some(e.clone());
        let status = if self.cfg.configured { "fail" } else { "warn" };
        self.set_health(status, format!("{e}; retry in {wait}s"));
    }

    async fn on_connected(&mut self, s: Session, task: JoinHandle<String>, rx: mpsc::Receiver<ClientEvent>, dev: Device) {
        let st = s.initial.clone();
        let info = st.info();
        if let Some(want) = &self.cfg.serial
            && !info.serial.is_empty()
            && &info.serial != want
        {
            self.log("warn", format!("{} at {} is serial {}, not the configured {want}; skipping", info.model, dev.addr, info.serial));
            drop(s);
            self.on_attempt_failed(format!("configured console {want} not found"));
            return;
        }
        self.failures = 0;
        self.connected_at = Some(Instant::now());
        self.session = Some(s);
        self.task = Some(task);
        self.events = Some(rx);
        let _ = self.ctx.db.kv_set("mixer", "last_host", &Value::Str(dev.addr.to_string()));
        let cov = Coverage::measure(&st);
        self.log(
            "info",
            format!(
                "connected to {} \"{}\" fw {} serial {} at {} via {:?}: {} ch, {} aux, {} fx, {} controls",
                info.model,
                info.name,
                info.firmware,
                info.serial,
                dev.addr,
                dev.via,
                cov.lines.len(),
                cov.auxes.len(),
                cov.fxbuses.len(),
                cov.controls.len()
            ),
        );
        self.settling = true;
        self.install(&st, cov, &info, &dev);
        // barrier: once the core has processed our declarations and publishes, compare
        let payload = Value::map()
            .with("model", info.model.clone())
            .with("host", dev.addr.to_string())
            .with("serial", info.serial.clone())
            .with("firmware", info.firmware.clone());
        let _ = self.hub.exec(Command::new(Origin::Mixer, Op::Emit { ty: "mixer.connected".into(), payload })).await;
        self.reconcile().await;
        self.settling = false;
        // only now: commands issued before this point were reconciled away (console wins)
        self.hub.publish(&format!("{}.connected", self.prefix()), Value::Bool(true));
        self.enforce_caps(Instant::now());
        self.update_health();
    }

    /// Declare the measured address set and publish the console's values.
    fn install(&mut self, st: &ConsoleState, cov: Coverage, info: &ConsoleInfo, dev: &Device) {
        let p = self.prefix();
        let old: BTreeSet<String> = self.controls.keys().cloned().collect();
        self.controls.clear();
        self.sync.clear();
        let now = Instant::now();
        let mut coverage_rows = Vec::new();
        for ctl in &cov.controls {
            let addr = format!("{p}.{}", ctl.rel_addr());
            let writable = self.cfg.is_writable(&addr);
            self.hub.declare(&addr, ctl.meta(writable));
            match ctl.kind() {
                Kind::Text => {
                    if let Some(v) = text_value(ctl, st) {
                        self.hub.publish(&addr, v);
                    }
                }
                _ => {
                    let n = st.num(&ctl.path());
                    let id = self.sync.track(ctl.clone(), addr.clone(), writable, n);
                    if let Some(n) = n {
                        self.hub.publish(&addr, ctl.value_from(n));
                        // a readback like any other: its resolved change is never sent back
                        self.sync.note_readback(id, n, now);
                    }
                    if ctl.param == Param::Fader {
                        let db = format!("{p}.{}.db", ctl.strip.addr());
                        self.hub
                            .declare(&db, Meta::float(-120.0, [-120.0, 10.0]).unit("dB").readonly().owner("mixer").describe("Fader level in dB (−120 = −∞)"));
                        self.hub.publish(&db, Value::Float(fader_to_db(n.unwrap_or(0.0))));
                    }
                }
            }
            let kind = match ctl.kind() {
                Kind::Level => "level",
                Kind::Bool => "bool",
                Kind::Text => "text",
            };
            coverage_rows.push((addr.clone(), ctl.path(), writable && ctl.settable(), kind));
            self.controls.insert(addr, ctl.clone());
        }
        for gone in old.difference(&self.controls.keys().cloned().collect()) {
            self.hub.submit(se_core::Input::Remove { prefix: gone.clone() });
        }
        let info_meta = |d: &str| Meta::string("").readonly().owner("mixer").describe(d);
        for (k, v, d) in [
            ("model", info.model.clone(), "Console model"),
            ("console_name", info.name.clone(), "Console name"),
            ("firmware", info.firmware.clone(), "Console firmware (pin it; updates can change UCNET)"),
            ("serial", info.serial.clone(), "Console serial number"),
            ("host", dev.addr.to_string(), "Console address"),
        ] {
            let a = format!("{p}.{k}");
            self.hub.declare(&a, info_meta(d));
            self.hub.publish(&a, Value::Str(v));
        }
        for (k, n) in [("channels", cov.lines.len()), ("auxes", cov.auxes.len()), ("fxes", cov.fxbuses.len())] {
            let a = format!("{p}.{k}");
            self.hub.declare(&a, Meta::int(0, [0.0, 256.0]).readonly().owner("mixer"));
            self.hub.publish(&a, Value::Int(n as i64));
        }

        // meters → signal names (precomputed; frames only fill values)
        self.meter_sigs.clear();
        for (i, n) in cov.lines.iter().enumerate() {
            self.meter_sigs.push((group::INPUT, i, format!("{p}.meter.ch.{n}")));
        }
        for (i, n) in cov.auxes.iter().enumerate() {
            self.meter_sigs.push((group::AUX_LIMITER_OUT, i, format!("{p}.meter.aux.{n}")));
        }
        for (i, n) in cov.fxbuses.iter().enumerate() {
            self.meter_sigs.push((group::FX_SENDS, i, format!("{p}.meter.fx.{n}")));
        }
        if cov.main {
            self.meter_sigs.push((group::MAIN_LIMITER_OUT, 0, format!("{p}.meter.main.l")));
            self.meter_sigs.push((group::MAIN_LIMITER_OUT, 1, format!("{p}.meter.main.r")));
        }
        self.resolve_talk_channel(st, &cov);

        {
            let mut s = self.status.lock();
            s.connected = true;
            s.device = Some(Device { model: info.model.clone(), serial: info.serial.clone(), name: info.name.clone(), ..dev.clone() });
            s.info = info.clone();
            s.coverage = coverage_rows;
            s.lines = cov.lines.len();
            s.auxes = cov.auxes.len();
            s.fxes = cov.fxbuses.len();
            s.last_error = None;
            s.meters_at = None;
            s.prefix = p.clone();
        }
        self.coverage = Some(cov);
    }

    fn resolve_talk_channel(&mut self, st: &ConsoleState, cov: &Coverage) {
        self.talk = TalkDetector::new(&self.cfg.talk);
        self.talk_index = match &self.cfg.talk.channel {
            None => None,
            Some(ChannelRef::Number(n)) => cov.lines.iter().position(|c| c == n),
            Some(ChannelRef::Label(l)) => cov.lines.iter().position(|c| st.text(&format!("line/ch{c}/username")).is_some_and(|u| u.eq_ignore_ascii_case(l))),
        };
        let ch = self.talk_index.map(|i| cov.lines[i]);
        self.status.lock().talk_channel = ch;
        if self.cfg.talk.channel.is_some() && ch.is_none() && !self.warned_talk {
            self.warned_talk = true;
            self.log("warn", format!("[mixer.talk] channel {:?} is not on this console; mic.talking stays 0", self.cfg.talk.channel));
        }
    }

    /// Console-authoritative at connect: engine overrides that disagree with the console
    /// (restored after a restart, set while offline) are released instead of slamming faders.
    async fn reconcile(&mut self) {
        let entries = self.hub.get(&format!("{}.**", self.prefix()), false).await;
        let resolved: HashMap<String, Value> = entries.into_iter().map(|e| (e.address, e.value)).collect();
        let mut released = 0;
        let ids: Vec<usize> = (0..self.sync.len()).collect();
        for id in ids {
            let t = self.sync.get(id);
            let (addr, ctl) = (t.addr.clone(), t.control.clone());
            let (Some(m), Some(r)) = (self.sync.mirror(id), resolved.get(&addr).and_then(|v| ctl.number_of(v))) else { continue };
            if ctl.same(m, r) {
                continue;
            }
            if let Some(p) = self.hub.explain(&addr).await
                && p.layers.iter().any(|l| l.kind == "override" || l.kind == "animation")
            {
                self.hub.command(Command::new(Origin::Mixer, Op::Release { address: addr }).with_key(KEY_TOUCH));
                released += 1;
            }
        }
        if released > 0 {
            self.log("info", format!("released {released} engine override(s) that disagreed with the console at connect"));
        }
    }

    /// Output levels above their `[safety] caps` are pulled down to the cap.
    fn enforce_caps(&mut self, now: Instant) {
        let caps = safety_caps(&self.ctx);
        if caps.is_empty() {
            return;
        }
        let prefix = self.prefix();
        let ids: Vec<usize> = (0..self.sync.len()).collect();
        for id in ids {
            let t = self.sync.get(id);
            if t.control.kind() != Kind::Level || !t.writable || !t.control.settable() {
                continue;
            }
            let Some(m) = self.sync.mirror(id) else { continue };
            let Some((_, [_, hi])) = caps
                .iter()
                .find(|(pat, _)| se_proto::address::matches(pat, &t.addr) || se_proto::address::matches(pat, t.addr.trim_start_matches(&format!("{prefix}."))))
            else {
                continue;
            };
            if m > hi + 2.0 / 65535.0 {
                let addr = t.addr.clone();
                self.log("warn", format!("{addr} is at {m:.3}, above its safety cap {hi:.3}: lowering it"));
                self.sync.resolved(&addr, *hi, now);
            }
        }
    }

    fn on_disconnected(&mut self, reason: String) {
        let was = self.session.take().is_some();
        self.events = None;
        let p = self.prefix();
        self.hub.publish(&format!("{p}.connected"), Value::Bool(false));
        // meters and talking must not stick at their last value
        let mut sigs: Vec<(String, f32)> = self.meter_sigs.iter().map(|(_, _, n)| (n.clone(), 0.0)).collect();
        sigs.push((format!("{p}.meter.main"), 0.0));
        if self.talk.reset().is_some() || self.status.lock().talking {
            sigs.push(("mic.talking".into(), 0.0));
        }
        self.hub.signals(sigs);
        {
            let mut s = self.status.lock();
            s.connected = false;
            s.talking = false;
            s.last_error = Some(reason.clone());
        }
        let long = self.connected_at.take().is_some_and(|t| t.elapsed() > Duration::from_secs(30));
        if long {
            self.failures = 0;
        }
        self.failures += 1;
        let wait = BACKOFF[(self.failures - 1).min(BACKOFF.len() - 1)];
        self.next_attempt = Instant::now() + Duration::from_secs(wait);
        if was {
            self.log("warn", format!("console disconnected: {reason}; reconnecting in {wait}s"));
            self.hub.emit(Event::new("mixer.disconnected", Origin::Mixer, Value::map().with("reason", reason.clone())));
        }
        let status = if self.cfg.configured { "fail" } else { "warn" };
        self.set_health(status, format!("disconnected: {reason}; retry in {wait}s"));
    }

    fn update_health(&mut self) {
        if self.session.is_none() || self.cfg_error.is_some() {
            return;
        }
        let s = self.status.lock().clone();
        let d = s.device.as_ref().map(|d| d.addr.ip().to_string()).unwrap_or_default();
        let mut detail = format!("{} \"{}\" fw {} at {d} ({} ch, {} aux, {} fx)", s.info.model, s.info.name, s.info.firmware, s.lines, s.auxes, s.fxes);
        let mut status = "pass";
        let meters_ok = !self.cfg.meters || s.meters_at.is_some_and(|t| t.elapsed() < METER_STALE);
        let settled = self.connected_at.is_some_and(|t| t.elapsed() > METER_STALE);
        if !meters_ok && settled {
            status = "warn";
            detail.push_str(&format!("; no meter data — allow it: sudo ufw allow in proto udp from {d} port 53000"));
        }
        let (snaps, _) = snapshots::load_all(&self.ctx.kind("mixes"), &self.prefix());
        if !snaps.contains_key(&self.cfg.panic_snapshot) {
            status = "warn";
            detail.push_str(&format!("; no safe mix for panic (store one: `stream mixer.snapshot.store snapshot={}`)", self.cfg.panic_snapshot));
        }
        if s.unconfirmed > 0 {
            detail.push_str(&format!("; {} unconfirmed change(s)", s.unconfirmed));
        }
        self.set_health(status, detail);
    }

    // ---- console → engine ----------------------------------------------------------------

    fn on_client(&mut self, ev: ClientEvent, now: Instant) {
        match ev {
            ClientEvent::Param { path, value } => self.on_remote(&path, value as f64, now),
            ClientEvent::Faders(groups) => {
                for g in &groups {
                    let Some(name) = g.group_name() else { continue };
                    for (i, v) in g.values.iter().enumerate() {
                        self.on_remote(&format!("{name}/ch{}/volume", i + 1), *v, now);
                    }
                }
            }
            ClientEvent::Text { path, value } => {
                if let Some(ctl) = Control::from_path(&path)
                    && ctl.kind() == Kind::Text
                {
                    let addr = format!("{}.{}", self.prefix(), ctl.rel_addr());
                    if self.controls.contains_key(&addr) {
                        self.hub.publish(&addr, Value::Str(value));
                    }
                }
            }
            ClientEvent::State(st) => {
                // console scene/project recall: everything may have moved
                self.log("info", "console resent its full state (scene or project recall)");
                let ids: Vec<(String, f64)> = self.sync.iter().filter_map(|(_, t)| st.num(&t.path).map(|n| (t.path.clone(), n))).collect();
                for (path, n) in ids {
                    self.on_remote(&path, n, now);
                }
                for (addr, ctl) in self.controls.clone() {
                    if ctl.kind() == Kind::Text
                        && let Some(v) = text_value(&ctl, &st)
                    {
                        self.hub.publish(&addr, v);
                    }
                }
            }
            ClientEvent::Meters(frame) => self.on_meters(&frame, now),
            ClientEvent::Json(j) => tracing::debug!(target: "mixer", "console message: {j}"),
            ClientEvent::Error(e) => tracing::warn!(target: "mixer", "{e}"),
        }
    }

    fn on_remote(&mut self, path: &str, v: f64, now: Instant) {
        let Some((id, r)) = self.sync.remote(path, v, now) else { return };
        if r == Remote::Unchanged {
            return;
        }
        let t = self.sync.get(id);
        let (addr, ctl, writable) = (t.addr.clone(), t.control.clone(), t.writable);
        if self.sync.needs_publish(id, v) {
            self.hub.publish(&addr, ctl.value_from(v));
            if ctl.param == Param::Fader {
                self.hub.publish(&format!("{}.{}.db", self.prefix(), ctl.strip.addr()), Value::Float(fader_to_db(v)));
            }
        }
        self.sync.note_readback(id, v, now);
        if r != Remote::External {
            return;
        }
        self.changed.insert(addr.clone(), ctl.value_from(v));
        self.changed_flush.get_or_insert(now + EVENT_FLUSH);
        if self.cfg.touch_release && writable && ctl.settable() {
            let continuing = self.touch.get(&addr).is_some_and(|t| now.duration_since(*t) < GESTURE);
            self.touch.insert(addr.clone(), now);
            if !continuing {
                let hub = self.hub.clone();
                tokio::spawn(async move {
                    if let Some(p) = hub.explain(&addr).await
                        && p.layers.iter().any(|l| l.kind == "override" || l.kind == "animation")
                    {
                        hub.command(Command::new(Origin::Mixer, Op::Release { address: addr }).with_key(KEY_TOUCH));
                    }
                });
            }
        }
    }

    fn flush_changed(&mut self) {
        self.changed_flush = None;
        for (addr, v) in std::mem::take(&mut self.changed) {
            self.hub.emit(Event::new("mixer.changed", Origin::Mixer, Value::map().with("address", addr).with("value", v).with("source", "console")));
        }
    }

    fn on_meters(&mut self, f: &LevelFrame, now: Instant) {
        self.meter_frames += 1;
        let first = {
            let mut s = self.status.lock();
            let first = s.meters_at.is_none();
            s.meters_at = Some(now);
            first
        };
        if first {
            self.update_health();
        }
        let p = self.prefix();
        let mut sigs: Vec<(String, f32)> = Vec::with_capacity(self.meter_sigs.len() + 2);
        let mut main = 0f32;
        for (g, i, name) in &self.meter_sigs {
            let l = f.level(*g, *i);
            if *g == group::MAIN_LIMITER_OUT {
                main = main.max(l);
            }
            sigs.push((name.clone(), l));
        }
        sigs.push((format!("{p}.meter.main"), main));
        if let Some(i) = self.talk_index {
            let level = f.level(group::INPUT, i);
            if let Some(on) = self.talk.update(level, now) {
                self.status.lock().talking = on;
                tracing::debug!(target: "mixer", "mic.talking = {on} ({:.1} dBFS)", to_db(level));
            }
            sigs.push(("mic.talking".into(), if self.talk.talking() { 1.0 } else { 0.0 }));
        }
        self.hub.signals(sigs);
    }

    // ---- engine → console ----------------------------------------------------------------

    fn on_changes(&mut self, changes: &[(String, Value)], now: Instant) {
        if self.settling || self.session.is_none() {
            return;
        }
        let pre = format!("{}.", self.prefix());
        for (addr, v) in changes {
            if !addr.starts_with(&pre) {
                continue;
            }
            if let Some(id) = self.sync.by_addr(addr)
                && let Some(n) = self.sync.get(id).control.number_of(v)
            {
                self.sync.resolved(addr, n, now);
            }
        }
    }

    fn resync_from_snapshot(&mut self, now: Instant) {
        if self.settling || self.session.is_none() {
            return;
        }
        let snap = self.hub.snapshot.load();
        let items: Vec<(String, f64)> =
            self.sync.iter().filter_map(|(_, t)| snap.get(&t.addr).and_then(|v| t.control.number_of(v)).map(|n| (t.addr.clone(), n))).collect();
        for (a, n) in items {
            self.sync.resolved(&a, n, now);
        }
    }

    fn flush_sends(&mut self, now: Instant) {
        if self.session.is_none() {
            return;
        }
        let out = self.sync.due(now);
        let p = self.prefix();
        for o in out {
            let ctl = self.sync.get(o.id).control.clone();
            // defence in depth: the sync engine only queues writable controls
            if !ctl.settable() || !self.cfg.is_writable(&o.addr) {
                continue;
            }
            let Some(s) = &self.session else { return };
            if s.set_param(&o.path, o.value as f32).is_err() {
                return;
            }
            self.hub.publish(&o.addr, ctl.value_from(o.value));
            if ctl.param == Param::Fader {
                self.hub.publish(&format!("{p}.{}.db", ctl.strip.addr()), Value::Float(fader_to_db(o.value)));
            }
        }
    }

    // ---- actions --------------------------------------------------------------------------

    async fn on_action(&mut self, cmd: Command) {
        let Op::Action { name, args } = &cmd.op else { return };
        if chat_originated(&cmd) {
            self.log("warn", format!("refused `{name}` from {} (chat can never touch the mixer)", cmd.origin.as_str()));
            return;
        }
        let r = match name.as_str() {
            "mixer.snapshot.recall" => self.recall(&cmd, args),
            "mixer.snapshot.store" => self.store(args).await,
            "mixer.panic" => self.panic(&cmd),
            "mixer.reconnect" => {
                self.session = None;
                if let Some(t) = self.task.take() {
                    t.abort();
                }
                self.on_disconnected("reconnect requested".into());
                self.failures = 0;
                self.next_attempt = Instant::now();
                Ok(())
            }
            other => Err(format!("unknown mixer action `{other}` (mixer.snapshot.recall|store, mixer.panic, mixer.reconnect)")),
        };
        if let Err(e) = r {
            self.log("error", format!("{name}: {e}"));
        }
    }

    fn control_for(&self, addr: &str) -> Option<Control> {
        self.controls.get(addr).cloned()
    }

    fn submit_steps(&mut self, steps: &[Step], origin: Origin, priority: u16, causal: se_proto::Id) {
        for s in steps {
            let (addr, op) = match s {
                Step::Animate { addr, to, ms } => (addr, Op::Animate { address: addr.clone(), to: Value::Float(*to), ms: *ms, ease: Ease::Smoothstep }),
                Step::Set { addr, value } => (addr, Op::Set { address: addr.clone(), value: value.clone() }),
            };
            self.snapshot_addrs.insert(addr.clone());
            self.hub.command(Command::new(origin, op).with_priority(Some(priority)).with_key(KEY_SNAPSHOT).caused_by(Some(causal)));
        }
    }

    fn recall(&mut self, cmd: &Command, args: &Value) -> Result<(), String> {
        let name = args.get_path("snapshot").or_else(|| args.get_path("args.0")).and_then(Value::as_str).ok_or("needs `snapshot`")?.to_string();
        if self.session.is_none() {
            return Err(format!("mixer not connected; snapshot `{name}` not recalled"));
        }
        let (snaps, errors) = snapshots::load_all(&self.ctx.kind("mixes"), &self.prefix());
        let Some(snap) = snaps.get(&name) else {
            let why = errors.iter().find(|e| e.starts_with(&format!("mixes/{name}.toml"))).cloned().unwrap_or_else(|| format!("no mixes/{name}.toml"));
            return Err(format!("unknown snapshot `{name}`: {why}"));
        };
        let fade = match args.get_path("fade").or_else(|| args.get_path("args.1")) {
            None | Some(Value::Null) => snap.fade_ms.unwrap_or(0),
            Some(Value::Str(s)) => se_proto::parse_ms(s).ok_or_else(|| format!("bad fade `{s}`"))?,
            Some(v) => v.as_f64().filter(|f| *f >= 0.0).map(|f| f as u32).ok_or_else(|| format!("bad fade `{v}`"))?,
        };
        let plan = snapshots::plan_recall(snap, fade, |a| self.control_for(a), |a| self.cfg.is_writable(a));
        // a new recall supersedes pending end-of-fade steps of an older one
        self.recall_generation += 1;
        self.end_steps.clear();
        let prio = cmd.priority().min(PRIORITY_MANUAL);
        self.submit_steps(&plan.now, cmd.origin, prio, cmd.id);
        if !plan.at_end.is_empty() {
            self.end_steps.push(EndSteps {
                at: Instant::now() + Duration::from_millis(fade as u64),
                generation: self.recall_generation,
                steps: plan.at_end.clone(),
                origin: cmd.origin,
                priority: prio,
                causal: cmd.id,
            });
        }
        if !plan.skipped.is_empty() {
            self.log(
                "warn",
                format!("snapshot `{name}`: skipped {} address(es) not on this console or not writable: {}", plan.skipped.len(), plan.skipped.join(", ")),
            );
        }
        let applied = plan.now.len() + plan.at_end.len();
        self.log("info", format!("recalling snapshot `{name}` ({applied} values, fade {fade} ms, priority {prio})"));
        self.hub.emit(
            Event::new(
                "mixer.snapshot.recalled",
                Origin::Mixer,
                Value::map().with("snapshot", name).with("fade", fade as i64).with("applied", applied).with("skipped", plan.skipped.len()),
            )
            .with_causal(Some(cmd.id)),
        );
        Ok(())
    }

    async fn store(&mut self, args: &Value) -> Result<(), String> {
        let name = args.get_path("snapshot").or_else(|| args.get_path("args.0")).and_then(Value::as_str).ok_or("needs `snapshot`")?.to_string();
        if !snapshots::valid_name(&name) {
            return Err(format!("snapshot name `{name}` must be letters, digits, `_` or `-`"));
        }
        if self.coverage.is_none() {
            return Err("the console has not been connected yet; nothing to store".into());
        }
        let patterns: Vec<String> = match args.get_path("include") {
            Some(Value::List(l)) => l.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            Some(Value::Str(s)) => s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect(),
            _ => self.cfg.store.clone(),
        };
        let entries = self.hub.get(&format!("{}.**", self.prefix()), false).await;
        let values = snapshots::collect(
            entries.iter().filter(|e| self.controls.contains_key(&e.address)).map(|e| (e.address.as_str(), &e.value)),
            &self.prefix(),
            &patterns,
        );
        if values.is_empty() {
            return Err(format!("no mixer values match {patterns:?}"));
        }
        let label = args.get_path("label").and_then(Value::as_str);
        let rel = snapshots::write(&self.ctx.project_root, &name, label, &values).map_err(|e| e.to_string())?;
        self.log("info", format!("stored snapshot `{name}` ({} values) in {rel}", values.len()));
        self.hub.emit(Event::new("mixer.snapshot.stored", Origin::Mixer, Value::map().with("snapshot", name).with("values", values.len()).with("file", rel)));
        Ok(())
    }

    fn panic(&mut self, cmd: &Command) -> Result<(), String> {
        // stop running crossfades where they are
        self.recall_generation += 1;
        self.end_steps.clear();
        for a in std::mem::take(&mut self.snapshot_addrs) {
            self.hub.command(
                Command::new(Origin::Mixer, Op::Release { address: a }).with_priority(Some(PRIORITY_PRESET)).with_key(KEY_SNAPSHOT).caused_by(Some(cmd.id)),
            );
        }
        let name = self.cfg.panic_snapshot.clone();
        let (snaps, _) = snapshots::load_all(&self.ctx.kind("mixes"), &self.prefix());
        let Some(snap) = snaps.get(&name) else {
            self.enforce_caps(Instant::now());
            return Err(format!("panic: no safe mix `mixes/{name}.toml`; crossfades stopped and caps enforced only"));
        };
        if self.session.is_none() {
            return Err("panic: mixer not connected".into());
        }
        let plan = snapshots::plan_recall(snap, 0, |a| self.control_for(a), |a| self.cfg.is_writable(a));
        // the safe mix wins over every engine layer on its addresses
        for s in &plan.now {
            let a = match s {
                Step::Animate { addr, .. } | Step::Set { addr, .. } => addr.clone(),
            };
            self.hub.command(Command::new(Origin::Mixer, Op::Release { address: a }).with_key(KEY_TOUCH).caused_by(Some(cmd.id)));
        }
        self.submit_steps(&plan.now, Origin::Mixer, PRIORITY_PRESET, cmd.id);
        self.log("warn", format!("panic: safe mix `{name}` recalled ({} values)", plan.now.len()));
        self.enforce_caps(Instant::now());
        Ok(())
    }

    // ---- config ---------------------------------------------------------------------------

    async fn on_config(&mut self) {
        let parsed = MixerConfig::parse(self.ctx.project_section("mixer").as_ref());
        let new = match parsed {
            Ok(c) => c,
            Err(e) => {
                self.log("error", format!("[mixer] config: {e}; keeping the last good settings"));
                return;
            }
        };
        let had_error = self.cfg_error.take().is_some();
        if new == self.cfg && !had_error {
            return;
        }
        let reconnect = new.enabled != self.cfg.enabled
            || new.name != self.cfg.name
            || new.host != self.cfg.host
            || new.serial != self.cfg.serial
            || new.meters != self.cfg.meters
            || new.meter_port != self.cfg.meter_port
            || new.client_name != self.cfg.client_name;
        let rename = new.name != self.cfg.name;
        let rewrite = new.writable != self.cfg.writable;
        let old_prefix = self.prefix();
        self.cfg = new;
        self.sync.set_config(SyncConfig { min_interval: self.cfg.min_interval(), ..Default::default() });
        self.status.lock().enabled = self.cfg.enabled;
        if rename {
            self.hub.submit(se_core::Input::Remove { prefix: old_prefix });
            self.controls.clear();
            self.status.lock().prefix = self.prefix();
            self.declare_base();
        }
        if reconnect || had_error {
            self.session = None;
            if let Some(t) = self.task.take() {
                t.abort();
            }
            if self.connected_at.is_some() {
                self.on_disconnected("configuration changed".into());
            }
            self.failures = 0;
            self.next_attempt = Instant::now();
            if !self.cfg.enabled {
                self.set_health("pass", "disabled in [mixer]".into());
            }
            return;
        }
        if rewrite && self.session.is_some() {
            // re-declare with the new readonly flags; the console state is unchanged
            if let Some(s) = &self.session {
                let st = s.initial.clone();
                if let Some(cov) = self.coverage.clone() {
                    let dev = self.status.lock().device.clone();
                    if let Some(dev) = dev {
                        let info = st.info();
                        let mirrors: Vec<(String, f64)> = self.sync.iter().filter_map(|(id, t)| self.sync.mirror(id).map(|m| (t.path.clone(), m))).collect();
                        let mut live = (*st).clone();
                        for (p, m) in mirrors {
                            live.values.insert(p, crate::ucnet::tree::Leaf::Num(m));
                        }
                        self.install(&live, cov, &info, &dev);
                    }
                }
            }
        }
        if let (Some(s), Some(cov)) = (&self.session, self.coverage.clone()) {
            let st = s.initial.clone();
            self.resolve_talk_channel(&st, &cov);
        }
        self.update_health();
    }
}

/// Chat can never touch the mixer. The core already refuses chat-priority mixer commands;
/// the adapter checks again so no single gate is trusted.
pub fn chat_originated(cmd: &Command) -> bool {
    cmd.priority() <= PRIORITY_CHAT || matches!(cmd.origin, Origin::Chat | Origin::Twitch | Origin::Relay)
}

/// Find and connect to a console: configured host, else the last known host, broadcast
/// discovery, and finally the LAN probe.
async fn attempt(cfg: &MixerConfig, cached: Option<SocketAddr>, opts: &ClientOptions) -> Attempt {
    let mut tried: Vec<String> = Vec::new();
    let try_one = |d: Device| async move {
        let (tx, rx) = mpsc::channel(1024);
        client::connect(d.addr, opts, tx).await.map(|(s, t)| (s, t, rx, d.clone())).map_err(|e| format!("{e:#}"))
    };
    if let Some(addr) = cfg.host_addr()? {
        return try_one(Device { model: String::new(), serial: String::new(), name: String::new(), addr, via: Via::Configured }).await;
    }
    if let Some(addr) = cached {
        match try_one(Device { model: String::new(), serial: String::new(), name: String::new(), addr, via: Via::Cached }).await {
            Ok(r) => return Ok(r),
            Err(e) => tried.push(format!("last known {addr}: {e}")),
        }
    }
    let heard = match discovery::listen(cfg.discovery).await {
        Ok(d) => d,
        Err(e) => {
            tried.push(format!("broadcast listen: {e}"));
            Vec::new()
        }
    };
    let mut candidates: Vec<Device> = heard.into_iter().filter(|d| cfg.serial.as_ref().is_none_or(|s| &d.serial == s)).collect();
    let broadcast_found = !candidates.is_empty();
    if candidates.is_empty() && cfg.probe {
        candidates = discovery::probe_lan(CONTROL_PORT).await.into_iter().filter(|d| Some(d.addr) != cached).collect();
    }
    for d in candidates {
        let addr = d.addr;
        match try_one(d).await {
            Ok(r) => return Ok(r),
            Err(e) => tried.push(format!("{addr}: {e}")),
        }
    }
    let mut msg = String::from("no StudioLive console found");
    if !broadcast_found {
        msg.push_str(&format!(" (no discovery broadcast heard in {:?}; if a host firewall is on: {})", cfg.discovery, discovery::FIREWALL_HINT));
    }
    if !tried.is_empty() {
        msg.push_str(&format!("; tried {}", tried.join("; ")));
    }
    if cfg.probe {
        msg.push_str("; LAN probe of TCP 53000 found nothing — set [mixer] host if the console is on another subnet");
    }
    Err(msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_refuses_chat_originated_actions() {
        let recall = || Op::Action { name: "mixer.snapshot.recall".into(), args: Value::map().with("snapshot", "x") };
        assert!(chat_originated(&Command::new(Origin::Chat, recall())));
        assert!(chat_originated(&Command::new(Origin::Twitch, recall())));
        // a chat-priority command from any origin (e.g. a chat-fired preset's action)
        assert!(chat_originated(&Command::new(Origin::Rule, recall()).with_priority(Some(PRIORITY_CHAT))));
        // chat origin even if someone raised the priority
        assert!(chat_originated(&Command::new(Origin::Relay, recall()).with_priority(Some(PRIORITY_MANUAL))));
        assert!(!chat_originated(&Command::new(Origin::Deck, recall())));
        assert!(!chat_originated(&Command::new(Origin::System, Op::Action { name: "mixer.panic".into(), args: Value::Null })));
    }
}
