//! The engine bus. The core runs on its own thread (fixed tick); everything else — the API,
//! adapters, render, audio, lights — talks to it through a [`Hub`]:
//!
//! * inputs (commands, events, signals, declarations) go in through a lock-free channel;
//! * outputs (state changes, events, traces, acks, actions) fan out on a broadcast bus;
//! * subsystem actions (`lights.cue`, `queue.skip`, …) are routed to registered handlers;
//! * real-time threads read the latest [`Snapshot`] without locking (`arc-swap`).

pub mod ctx;
pub mod draw;
pub mod media;

pub use ctx::EngineCtx;

use arc_swap::ArcSwap;
use parking_lot::{Mutex, RwLock};
use se_core::{Core, Input, Output, RuntimeState};
use se_proto::wire::{Provenance, StateEntry, TraceRec};
use se_proto::{Command, Event, Id, Meta, Op, Origin, Ts, Value, address};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{broadcast, mpsc, oneshot};

/// One bus message.
#[derive(Clone, Debug)]
pub enum Bus {
    Changes(Vec<(String, Value)>),
    Event(Event),
    Trace(Vec<TraceRec>),
    Log {
        level: String,
        target: String,
        msg: String,
        ts: Ts,
    },
    Ack {
        id: Id,
        ok: bool,
        error: Option<String>,
    },
    /// An action nobody handled (for UI/debugging).
    Unrouted(Command),
    RuntimeDirty,
}

pub enum CoreQuery {
    Get {
        pattern: String,
        meta: bool,
    },
    Explain(String),
    Trace(Id),
    Named {
        name: String,
        args: Value,
    },
    Runtime,
    Restore(Box<RuntimeState>),
    /// Run arbitrary read-only work against the core (for adapters needing config).
    With(Box<dyn FnOnce(&Core) -> Value + Send>),
}

pub enum CoreReply {
    Entries(Vec<StateEntry>),
    Explain(Option<Provenance>),
    Trace(Vec<TraceRec>),
    Value(Result<Value, String>),
    Runtime(Box<RuntimeState>),
}

pub enum CoreMsg {
    Input(Input),
    Query(CoreQuery, oneshot::Sender<CoreReply>),
    Shutdown,
}

/// Immutable view of all state and signals, published by the core thread.
#[derive(Default)]
pub struct Snapshot {
    pub tick: u64,
    pub now: Ts,
    pub generation: u64,
    pub index: Arc<HashMap<String, usize>>,
    pub values: Vec<Value>,
    pub signal_names: Arc<Vec<String>>,
    pub signal_index: Arc<HashMap<String, usize>>,
    pub signals: Vec<f32>,
}

impl Snapshot {
    pub fn get(&self, a: &str) -> Option<&Value> {
        self.index.get(a).map(|i| &self.values[*i])
    }
    pub fn f32(&self, a: &str) -> Option<f32> {
        self.get(a).and_then(Value::as_f32)
    }
    pub fn f32_or(&self, a: &str, d: f32) -> f32 {
        self.f32(a).unwrap_or(d)
    }
    pub fn str(&self, a: &str) -> Option<&str> {
        self.get(a).and_then(Value::as_str)
    }
    pub fn bool(&self, a: &str) -> bool {
        self.get(a).is_some_and(Value::truthy)
    }
    pub fn signal(&self, n: &str) -> Option<f32> {
        self.signal_index.get(n).map(|i| self.signals[*i])
    }
    /// Id for repeated lookups; valid while `generation` is unchanged.
    pub fn id(&self, a: &str) -> Option<usize> {
        self.index.get(a).copied()
    }
    pub fn value(&self, id: usize) -> &Value {
        &self.values[id]
    }
}

pub type QueryFuture = Pin<Box<dyn Future<Output = Result<Value, String>> + Send>>;
pub type QueryFn = Arc<dyn Fn(String, Value) -> QueryFuture + Send + Sync>;

#[derive(Clone, Debug, Default)]
pub struct HubInfo {
    pub session: String,
    pub version: String,
    pub project: String,
    pub started_wall: i64,
}

pub struct Hub {
    tx: crossbeam_channel::Sender<CoreMsg>,
    bus: broadcast::Sender<Arc<Bus>>,
    pub snapshot: ArcSwap<Snapshot>,
    pending: Mutex<HashMap<Id, oneshot::Sender<(bool, Option<String>)>>>,
    routes: RwLock<Vec<(String, mpsc::UnboundedSender<Command>)>>,
    queries: RwLock<Vec<(String, QueryFn)>>,
    pub info: RwLock<HubInfo>,
    pub clock: Arc<se_clock::Clock>,
    /// Core-thread heartbeat (master clock ns of the last tick), read by the watchdog.
    pub core_heartbeat: AtomicU64,
    /// Render-thread heartbeat (0 until the renderer runs).
    pub render_heartbeat: AtomicU64,
    unrouted_warned: Mutex<std::collections::HashSet<String>>,
    /// CPU video frames from producers (web pages, media) to the renderer.
    pub video: media::VideoSlots,
    /// Audio sample rings from producers (web pages, media, TTS, sfx) to the audio graph.
    pub audio: media::AudioSlots,
    /// 2D draw lists from script patches to the renderer.
    pub draw: draw::DrawSlots,
}

impl Hub {
    pub fn new(clock: Arc<se_clock::Clock>) -> (Arc<Hub>, crossbeam_channel::Receiver<CoreMsg>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (bus, _) = broadcast::channel(8192);
        let hub = Arc::new(Hub {
            tx,
            bus,
            snapshot: ArcSwap::from_pointee(Snapshot::default()),
            pending: Mutex::new(HashMap::new()),
            routes: RwLock::new(Vec::new()),
            queries: RwLock::new(Vec::new()),
            info: RwLock::new(HubInfo::default()),
            clock,
            core_heartbeat: AtomicU64::new(0),
            render_heartbeat: AtomicU64::new(0),
            unrouted_warned: Mutex::new(Default::default()),
            video: Default::default(),
            audio: Default::default(),
            draw: Default::default(),
        });
        (hub, rx)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Bus>> {
        self.bus.subscribe()
    }

    pub fn publish_bus(&self, b: Bus) {
        let _ = self.bus.send(Arc::new(b));
    }

    pub fn log(&self, level: &str, target: &str, msg: impl Into<String>) {
        self.publish_bus(Bus::Log { level: level.into(), target: target.into(), msg: msg.into(), ts: se_clock::now() });
    }

    pub fn submit(&self, i: Input) {
        let _ = self.tx.send(CoreMsg::Input(i));
    }

    pub fn emit(&self, mut e: Event) {
        if e.ts == 0 {
            e.ts = se_clock::now();
        }
        self.submit(Input::Event { event: e });
    }

    pub fn signal(&self, name: &str, value: f32) {
        self.submit(Input::Signal { name: name.into(), value });
    }

    pub fn signals(&self, values: Vec<(String, f32)>) {
        self.submit(Input::Signals { values });
    }

    pub fn declare(&self, address: &str, meta: Meta) {
        self.submit(Input::Declare { address: address.into(), meta });
    }

    pub fn publish(&self, address: &str, value: Value) {
        self.submit(Input::Publish { address: address.into(), value });
    }

    /// Fire-and-forget command.
    pub fn command(&self, mut cmd: Command) -> Id {
        if cmd.ts == 0 {
            cmd.ts = se_clock::now();
        }
        let id = cmd.id;
        self.submit(Input::Command { cmd });
        id
    }

    pub fn op(&self, origin: Origin, op: Op) -> Id {
        self.command(Command::new(origin, op))
    }

    /// Submit and wait for the core's acknowledgement.
    pub async fn exec(&self, cmd: Command) -> Result<(), String> {
        let (tx, rx) = oneshot::channel();
        self.pending.lock().insert(cmd.id, tx);
        self.command(cmd);
        match tokio::time::timeout(std::time::Duration::from_secs(5), rx).await {
            Ok(Ok((true, _))) => Ok(()),
            Ok(Ok((false, e))) => Err(e.unwrap_or_else(|| "failed".into())),
            _ => Err("engine did not respond".into()),
        }
    }

    async fn ask(&self, q: CoreQuery) -> Option<CoreReply> {
        let (tx, rx) = oneshot::channel();
        self.tx.send(CoreMsg::Query(q, tx)).ok()?;
        tokio::time::timeout(std::time::Duration::from_secs(5), rx).await.ok()?.ok()
    }

    pub async fn get(&self, pattern: &str, meta: bool) -> Vec<StateEntry> {
        match self.ask(CoreQuery::Get { pattern: pattern.into(), meta }).await {
            Some(CoreReply::Entries(e)) => e,
            _ => Vec::new(),
        }
    }

    pub async fn explain(&self, a: &str) -> Option<Provenance> {
        match self.ask(CoreQuery::Explain(a.into())).await {
            Some(CoreReply::Explain(p)) => p,
            _ => None,
        }
    }

    pub async fn trace(&self, id: Id) -> Vec<TraceRec> {
        match self.ask(CoreQuery::Trace(id)).await {
            Some(CoreReply::Trace(t)) => t,
            _ => Vec::new(),
        }
    }

    pub async fn runtime(&self) -> Option<RuntimeState> {
        match self.ask(CoreQuery::Runtime).await {
            Some(CoreReply::Runtime(r)) => Some(*r),
            _ => None,
        }
    }

    pub async fn restore(&self, rs: RuntimeState) {
        let _ = self.ask(CoreQuery::Restore(Box::new(rs))).await;
    }

    /// Read-only access to the core on its thread.
    pub async fn with_core(&self, f: impl FnOnce(&Core) -> Value + Send + 'static) -> Value {
        match self.ask(CoreQuery::With(Box::new(f))).await {
            Some(CoreReply::Value(Ok(v))) => v,
            _ => Value::Null,
        }
    }

    /// Named query: registered subsystem handlers first (longest prefix), else the core.
    pub async fn query(&self, name: &str, args: Value) -> Result<Value, String> {
        let handler = {
            let q = self.queries.read();
            q.iter().filter(|(p, _)| name == p || name.starts_with(&format!("{p}."))).max_by_key(|(p, _)| p.len()).map(|(_, f)| f.clone())
        };
        if let Some(h) = handler {
            return h(name.to_string(), args).await;
        }
        match self.ask(CoreQuery::Named { name: name.into(), args }).await {
            Some(CoreReply::Value(v)) => v,
            _ => Err("engine did not respond".into()),
        }
    }

    pub fn register_query(&self, prefix: &str, f: QueryFn) {
        self.queries.write().push((prefix.to_string(), f));
    }

    /// Receive actions whose name equals `prefix` or starts with `prefix.`.
    pub fn route_actions(&self, prefix: &str) -> mpsc::UnboundedReceiver<Command> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.routes.write().push((prefix.to_string(), tx));
        rx
    }

    fn dispatch_action(&self, c: Command) {
        let Op::Action { name, .. } = &c.op else { return };
        let target = {
            let r = self.routes.read();
            r.iter()
                .filter(|(p, tx)| (name == p || name.starts_with(&format!("{p}."))) && !tx.is_closed())
                .max_by_key(|(p, _)| p.len())
                .map(|(_, tx)| tx.clone())
        };
        match target {
            Some(tx) => {
                let _ = tx.send(c);
            }
            None => {
                if self.unrouted_warned.lock().insert(name.clone()) {
                    tracing::warn!("no handler for action `{name}`");
                }
                self.publish_bus(Bus::Unrouted(c));
            }
        }
    }

    fn resolve_ack(&self, id: Id, ok: bool, error: Option<String>) {
        if let Some(tx) = self.pending.lock().remove(&id) {
            let _ = tx.send((ok, error.clone()));
        }
        self.publish_bus(Bus::Ack { id, ok, error });
    }

    pub fn shutdown(&self) {
        let _ = self.tx.send(CoreMsg::Shutdown);
    }
}

/// Hooks called on the core thread.
pub struct RunnerHooks {
    /// Replayable inputs applied during the last batch of ticks (session log).
    pub on_applied: Box<dyn FnMut(Vec<(u64, Input)>) + Send>,
    /// Snapshot publish interval in ticks.
    pub snapshot_every: u64,
    /// Called on the core thread whenever persistent runtime state changed (crash recovery
    /// must not lag behind acknowledged commands). Must not block.
    pub on_runtime: Option<Box<dyn FnMut(RuntimeState) + Send>>,
}

fn answer(core: &mut Core, q: CoreQuery) -> CoreReply {
    match q {
        CoreQuery::Get { pattern, meta } => {
            let st = core.state();
            let ids: Vec<usize> = st.matching(&pattern).collect();
            CoreReply::Entries(
                ids.into_iter()
                    .map(|i| {
                        let p = st.param(i);
                        StateEntry { address: p.addr.clone(), value: p.resolved.clone(), meta: meta.then(|| p.meta.clone()) }
                    })
                    .collect(),
            )
        }
        CoreQuery::Explain(a) => CoreReply::Explain(core.explain(&a)),
        CoreQuery::Trace(id) => CoreReply::Trace(core.trace_chain(id)),
        CoreQuery::Named { name, args } => CoreReply::Value(core.query(&name, &args)),
        CoreQuery::Runtime => CoreReply::Runtime(Box::new(core.runtime_state())),
        CoreQuery::Restore(rs) => {
            core.restore(&rs);
            CoreReply::Value(Ok(Value::Null))
        }
        CoreQuery::With(f) => CoreReply::Value(Ok(f(core))),
    }
}

fn publish_snapshot(hub: &Hub, core: &Core, cache: &mut (u64, Arc<HashMap<String, usize>>, u64, Arc<Vec<String>>, Arc<HashMap<String, usize>>)) {
    let st = core.state();
    if cache.0 != st.generation || cache.1.is_empty() && !st.is_empty() {
        cache.0 = st.generation;
        cache.1 = Arc::new(st.params().iter().enumerate().map(|(i, p)| (p.addr.clone(), i)).collect());
    }
    let sg = core.signals();
    if cache.2 != sg.generation || cache.3.len() != sg.names().len() {
        cache.2 = sg.generation;
        cache.3 = Arc::new(sg.names().to_vec());
        cache.4 = Arc::new(sg.names().iter().enumerate().map(|(i, n)| (n.clone(), i)).collect());
    }
    hub.snapshot.store(Arc::new(Snapshot {
        tick: core.tick_index(),
        now: core.now(),
        generation: st.generation,
        index: cache.1.clone(),
        values: st.params().iter().map(|p| p.resolved.clone()).collect(),
        signal_names: cache.3.clone(),
        signal_index: cache.4.clone(),
        signals: sg.values().to_vec(),
    }));
}

fn route_outputs(hub: &Hub, outs: Vec<Output>) {
    let mut changes: Vec<(String, Value)> = Vec::new();
    for o in outs {
        match o {
            Output::Changes(c) => changes.extend(c),
            Output::Event(e) => hub.publish_bus(Bus::Event(e)),
            Output::Action(c) => hub.dispatch_action(c),
            Output::Ack { id, ok, error } => hub.resolve_ack(id, ok, error),
            Output::PersistBase { address, value } => {
                let c = Command::new(
                    Origin::System,
                    Op::Action { name: "project.persist_base".into(), args: Value::map().with("address", address).with("value", value) },
                );
                hub.dispatch_action(c);
            }
            Output::Trace(t) => hub.publish_bus(Bus::Trace(t)),
            Output::RuntimeDirty => hub.publish_bus(Bus::RuntimeDirty),
            Output::Log { level, msg } => hub.log(level, "core", msg),
        }
    }
    if !changes.is_empty() {
        // collapse to the latest value per address
        let mut seen = std::collections::HashSet::new();
        let mut dedup: Vec<(String, Value)> = Vec::with_capacity(changes.len());
        for (a, v) in changes.into_iter().rev() {
            if seen.insert(a.clone()) {
                dedup.push((a, v));
            }
        }
        dedup.reverse();
        hub.publish_bus(Bus::Changes(dedup));
    }
}

/// Run the core on the current thread until shutdown.
pub fn run_core(mut core: Core, hub: Arc<Hub>, rx: crossbeam_channel::Receiver<CoreMsg>, mut hooks: RunnerHooks) {
    let period = core.period();
    let mut cache = (u64::MAX, Arc::new(HashMap::new()), u64::MAX, Arc::new(Vec::new()), Arc::new(HashMap::new()));
    let mut last_snap_tick = 0u64;
    publish_snapshot(&hub, &core, &mut cache);
    route_outputs(&hub, core.drain_outputs());
    loop {
        let now = se_clock::now();
        let next = core.now() + period;
        let wait = std::time::Duration::from_nanos(next.saturating_sub(now));
        let first = rx.recv_timeout(wait);
        let mut msgs = Vec::new();
        match first {
            Ok(m) => msgs.push(m),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
        while let Ok(m) = rx.try_recv() {
            msgs.push(m);
            if msgs.len() > 10_000 {
                break;
            }
        }
        let mut stop = false;
        for m in msgs {
            match m {
                CoreMsg::Input(i) => core.submit(i),
                CoreMsg::Query(q, tx) => {
                    let r = answer(&mut core, q);
                    let _ = tx.send(r);
                }
                CoreMsg::Shutdown => stop = true,
            }
        }
        let ran = core.advance_to(se_clock::now());
        if ran > 0 {
            hub.core_heartbeat.store(se_clock::now(), Ordering::Relaxed);
            let applied = core.drain_applied();
            if !applied.is_empty() {
                (hooks.on_applied)(applied);
            }
            let outs = core.drain_outputs();
            if let Some(f) = hooks.on_runtime.as_mut()
                && outs.iter().any(|o| matches!(o, Output::RuntimeDirty))
            {
                f(core.runtime_state());
            }
            route_outputs(&hub, outs);
            if core.tick_index() >= last_snap_tick + hooks.snapshot_every {
                last_snap_tick = core.tick_index();
                publish_snapshot(&hub, &core, &mut cache);
            }
        }
        if stop {
            break;
        }
    }
    tracing::info!("core thread stopped");
}

/// Convenience: glob match for subscription filters.
pub fn any_match(patterns: &[String], s: &str) -> bool {
    patterns.iter().any(|p| address::matches(p, s))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> se_core::Config {
        let files = vec![
            se_core::SourceFile { kind: "project".into(), name: "project".into(), path: "project.toml".into(), table: toml_table("schema = 1") },
            se_core::SourceFile {
                kind: "presets".into(),
                name: "hype".into(),
                path: "presets/hype.toml".into(),
                table: toml_table("hold = \"1s\"\nset = { \"fx.x\" = 1.0 }\nlights = { cue = \"flash\" }"),
            },
        ];
        se_core::Config::build(&files)
    }

    fn toml_table(s: &str) -> toml::Table {
        s.parse().unwrap()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_ack_actions_snapshot() {
        let clock = Arc::new(se_clock::Clock::new());
        let (hub, rx) = Hub::new(clock);
        let core = Core::new(cfg(), se_clock::now());
        let h2 = hub.clone();
        let t = std::thread::spawn(move || run_core(core, h2, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 2, on_runtime: None }));
        let mut lights = hub.route_actions("lights");
        hub.exec(Command::new(Origin::Cli, Op::PresetFire { name: "hype".into(), payload: Value::Null })).await.unwrap();
        let err = hub.exec(Command::new(Origin::Cli, Op::PresetFire { name: "nope".into(), payload: Value::Null })).await.unwrap_err();
        assert!(err.contains("unknown preset"));
        let a = tokio::time::timeout(std::time::Duration::from_secs(2), lights.recv()).await.unwrap().unwrap();
        assert_eq!(a.op.describe().split_whitespace().next(), Some("lights.cue"));
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert_eq!(hub.snapshot.load().f32("fx.x"), Some(1.0));
        let e = hub.get("fx.*", true).await;
        assert_eq!(e.len(), 1);
        assert!(hub.query("presets", Value::Null).await.unwrap().as_list().unwrap().len() == 1);
        hub.shutdown();
        t.join().unwrap();
    }
}
