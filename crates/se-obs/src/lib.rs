//! OBS link (PLAN §5, §4.5, §15.8, §17.1, §18, §22): the engine side of `obs.sock`.
//!
//! The stream-engine OBS plugin connects here (`docs/frames-protocol.md`, JSON lines). This
//! crate publishes OBS health as state (`obs.link`, `obs.stream.*`, `obs.record.*`, `obs.fps`,
//! `obs.stale.{wide,tall}`, `obs.output.<id>.*`, `health.obs`), turns plugin events into engine
//! events (`obs.stream_started|stopped`, `obs.record_started|stopped`, `obs.fallback`), keeps
//! the OBS stream/record clock mappings in `hub.clock`, records recording files in the session
//! meta (`recordings`, `clock`), and routes the `obs.*` actions to the plugin.

pub mod config;
pub mod msg;

pub use config::{FallbackMode, ObsConfig};

use msg::{EngineMsg, Hello, OutputStatus, PluginEvent, PluginMsg, RecordEnd, RecordPath, Reply, Status, obs_to_master};
use parking_lot::Mutex;
use se_clock::Mapping;
use se_hub::{Bus, EngineCtx};
use se_proto::{Command, Event, Meta, Op, Origin, Value, ValueType};
use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Notify, mpsc, oneshot, watch};

const TARGET: &str = "obs";
/// Longest accepted line from the plugin.
const MAX_LINE: u64 = 1 << 20;
/// The plugin sends a status every second; silence for this long means it hung.
const IDLE_TIMEOUT: Duration = Duration::from_secs(10);
/// A newer connection replaces the current one only if the current one is this quiet.
const TAKEOVER_QUIET: Duration = Duration::from_secs(5);

/// Where the plugin gets installed (user install, system package, Flatpak OBS).
fn plugin_candidates() -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(home) = std::env::var_os("HOME").map(PathBuf::from) {
        v.push(home.join(".config/obs-studio/plugins/stream-engine/bin/64bit/stream-engine.so"));
        v.push(home.join(".var/app/com.obsproject.Studio/config/obs-studio/plugins/stream-engine/bin/64bit/stream-engine.so"));
    }
    v.push(PathBuf::from("/usr/lib/obs-plugins/stream-engine.so"));
    v.push(PathBuf::from("/usr/lib64/obs-plugins/stream-engine.so"));
    v
}

fn plugin_installed() -> bool {
    plugin_candidates().iter().any(|p| p.exists())
}

/// `obs.output.<id>`: OBS output name, lowercase, non-alphanumerics → `_`.
pub fn output_id(name: &str) -> String {
    let s: String = name.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' }).collect();
    let s = s.trim_matches('_').to_string();
    if s.is_empty() { "output".into() } else { s }
}

struct Conn {
    id: u64,
    tx: mpsc::UnboundedSender<String>,
    kill: Arc<Notify>,
    last_rx: Instant,
    hello: Option<Hello>,
}

#[derive(Clone, Debug, PartialEq)]
struct Recording {
    canvas: String,
    path: String,
    output: String,
    start_ns: u64,
    end_ns: Option<u64>,
    tracks: Value,
}

impl Recording {
    fn value(&self) -> Value {
        let mut v = Value::map()
            .with("canvas", self.canvas.clone())
            .with("path", self.path.clone())
            .with("output", self.output.clone())
            .with("start_ns", self.start_ns as i64)
            .with("tracks", self.tracks.clone());
        if let Some(e) = self.end_ns {
            v = v.with("end_ns", e as i64);
        }
        v
    }
}

type Pending = HashMap<u64, (String, oneshot::Sender<Result<serde_json::Value, String>>)>;

struct Inner {
    cfg: ObsConfig,
    conn: Option<Conn>,
    next_cmd: u64,
    pending: Pending,
    recordings: Vec<Recording>,
    last: HashMap<String, Value>,
    outputs: BTreeSet<String>,
    status: Option<Status>,
    stream_start_obs: u64,
    record_start_obs: u64,
    installed: bool,
    listen_error: Option<String>,
}

/// The running OBS link.
pub struct Obs {
    ctx: EngineCtx,
    inner: Mutex<Inner>,
    next_conn: AtomicU64,
    socket_tx: watch::Sender<PathBuf>,
}

/// Starts the OBS link: declares `obs.*`, listens on obs.sock, routes `obs.*` actions.
pub async fn start(ctx: EngineCtx) -> anyhow::Result<Arc<Obs>> {
    let cfg = match ObsConfig::from_section(ctx.project_section("obs").as_ref()) {
        Ok(c) => c,
        Err(e) => {
            ctx.hub.log("error", TARGET, format!("{e}; using defaults"));
            ObsConfig::default()
        }
    };
    let (socket_tx, socket_rx) = watch::channel(cfg.socket.clone());
    let obs = Arc::new(Obs {
        ctx: ctx.clone(),
        inner: Mutex::new(Inner {
            cfg,
            conn: None,
            next_cmd: 1,
            pending: HashMap::new(),
            recordings: Vec::new(),
            last: HashMap::new(),
            outputs: BTreeSet::new(),
            status: None,
            stream_start_obs: 0,
            record_start_obs: 0,
            installed: plugin_installed(),
            listen_error: None,
        }),
        next_conn: AtomicU64::new(1),
        socket_tx,
    });
    obs.declare();
    obs.reset_link_state();

    tokio::spawn(serve(obs.clone(), socket_rx));
    tokio::spawn(route_actions(obs.clone(), ctx.hub.route_actions("obs")));
    tokio::spawn(watch_config(obs.clone()));
    tokio::spawn(watch_bus(obs.clone()));
    tokio::spawn(watch_install(obs.clone()));
    let q = obs.clone();
    ctx.hub.register_query(
        "obs",
        Arc::new(move |_name, _args| {
            let v = q.describe();
            Box::pin(async move { Ok(v) })
        }),
    );
    Ok(obs)
}

fn ro(meta: Meta, description: &str) -> Meta {
    meta.readonly().owner(TARGET).describe(description)
}

fn string_meta(description: &str) -> Meta {
    ro(Meta::string(""), description)
}

fn map_meta(description: &str) -> Meta {
    ro(Meta { ty: ValueType::Map, ..Default::default() }, description)
}

impl Obs {
    fn declare(&self) {
        let h = &self.ctx.hub;
        let big = [0.0, 1e12];
        let decl: Vec<(&str, Meta)> = vec![
            ("obs.link", ro(Meta::boolean(false), "OBS plugin connected")),
            ("obs.version", string_meta("OBS version")),
            ("obs.plugin.version", string_meta("stream-engine OBS plugin version")),
            ("obs.plugin.installed", ro(Meta::boolean(false), "stream-engine OBS plugin installed on this machine")),
            ("obs.stream.active", ro(Meta::boolean(false), "OBS main stream output active")),
            ("obs.stream.kbps", ro(Meta::float(0.0, [0.0, 1e6]).unit("kbps"), "stream bitrate")),
            ("obs.stream.dropped", ro(Meta::int(0, big), "stream frames dropped (network)")),
            ("obs.stream.total", ro(Meta::int(0, big), "stream frames sent")),
            ("obs.stream.lag_ms", ro(Meta::float(0.0, [0.0, 1e6]).unit("ms"), "video skipped by encoder lag in the last second")),
            ("obs.stream.congestion", ro(Meta::float(0.0, [0.0, 1.0]), "stream output congestion")),
            ("obs.record.active", ro(Meta::boolean(false), "OBS main recording active")),
            ("obs.record.paused", ro(Meta::boolean(false), "OBS main recording paused")),
            ("obs.record.path", string_meta("current or last main recording file")),
            ("obs.record.dir", string_meta("OBS recording directory (profile)")),
            ("obs.record.kbps", ro(Meta::float(0.0, [0.0, 1e7]).unit("kbps"), "recording bitrate")),
            ("obs.fps", ro(Meta::float(0.0, [0.0, 1000.0]).unit("fps"), "OBS render frame rate")),
            ("obs.render.ms", ro(Meta::float(0.0, [0.0, 1000.0]).unit("ms"), "OBS average frame render time")),
            ("obs.render.lagged", ro(Meta::int(0, big), "frames missed due to rendering lag")),
            ("obs.encode.skipped", ro(Meta::int(0, big), "frames skipped due to encoding lag")),
            ("obs.stale.wide", ro(Meta::boolean(true), "wide canvas not arriving in OBS")),
            ("obs.stale.tall", ro(Meta::boolean(true), "tall canvas not arriving in OBS")),
            ("obs.fallback.active", ro(Meta::boolean(false), "an OBS canvas shows the fallback scene")),
            ("obs.scene", string_meta("OBS program scene (main canvas)")),
            ("health.obs", map_meta("OBS plugin connected and receiving both canvases")),
        ];
        for (a, m) in decl {
            h.declare(a, m);
        }
    }

    /// Publishes only values that changed. Call with the lock held.
    fn set(&self, inner: &mut Inner, addr: &str, v: impl Into<Value>) {
        let v = v.into();
        if inner.last.get(addr) == Some(&v) {
            return;
        }
        inner.last.insert(addr.to_string(), v.clone());
        self.ctx.hub.publish(addr, v);
    }

    fn reset_link_state(&self) {
        let mut g = self.inner.lock();
        self.clear_link(&mut g);
    }

    /// State while no plugin is connected: nothing is confirmed live.
    fn clear_link(&self, g: &mut Inner) {
        self.set(g, "obs.link", false);
        let installed = g.installed;
        self.set(g, "obs.plugin.installed", installed);
        for a in ["obs.stream.active", "obs.record.active", "obs.record.paused", "obs.fallback.active"] {
            self.set(g, a, false);
        }
        for a in ["obs.stream.kbps", "obs.record.kbps", "obs.fps", "obs.stream.lag_ms", "obs.stream.congestion"] {
            self.set(g, a, 0.0);
        }
        self.set(g, "obs.stale.wide", true);
        self.set(g, "obs.stale.tall", true);
        let outputs: Vec<String> = g.outputs.iter().cloned().collect();
        for id in outputs {
            self.set(g, &format!("obs.output.{id}.active"), false);
            self.set(g, &format!("obs.output.{id}.kbps"), 0.0);
        }
        g.status = None;
        self.update_health(g);
    }

    fn update_health(&self, g: &mut Inner) {
        let (status, detail) = health(g);
        self.set(g, "health.obs", Value::map().with("status", status).with("detail", detail));
    }

    fn meta(&self, key: &str, value: Value) {
        let args = Value::map().with("key", key).with("value", value);
        self.ctx.hub.command(Command::new(Origin::Obs, Op::Action { name: "session.meta".into(), args }));
    }

    fn write_recordings(&self, g: &Inner) {
        self.meta("recordings", Value::List(g.recordings.iter().map(Recording::value).collect()));
    }

    fn write_clock(&self) {
        let maps = self.ctx.hub.clock.mappings();
        match serde_json::to_value(&maps) {
            Ok(v) => self.meta("clock", Value::from(v)),
            Err(e) => tracing::warn!(target: "obs", "clock mappings not serializable: {e}"),
        }
    }

    fn send(&self, g: &Inner, m: &EngineMsg) -> bool {
        g.conn.as_ref().is_some_and(|c| c.tx.send(m.line()).is_ok())
    }

    fn config_msg(cfg: &ObsConfig) -> EngineMsg {
        EngineMsg::Config {
            stale_ms: cfg.stale_ms,
            fallback_mode: cfg.fallback_mode.as_str().into(),
            fallback_scene: cfg.fallback_scene.clone(),
            fallback_text: cfg.fallback_text.clone(),
        }
    }

    /// Sends a command to the plugin and waits for its reply.
    pub async fn command(&self, op: &str) -> Result<serde_json::Value, String> {
        let (rx, timeout) = {
            let mut g = self.inner.lock();
            if g.conn.as_ref().and_then(|c| c.hello.as_ref()).is_none() {
                return Err("OBS plugin not connected".into());
            }
            let id = g.next_cmd;
            g.next_cmd += 1;
            let (tx, rx) = oneshot::channel();
            g.pending.insert(id, (op.to_string(), tx));
            if !self.send(&g, &EngineMsg::Cmd { id, op: op.into() }) {
                g.pending.remove(&id);
                return Err("OBS plugin connection closed".into());
            }
            (rx, Duration::from_millis(g.cfg.command_timeout_ms))
        };
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(r)) => r,
            Ok(Err(_)) => Err("OBS plugin disconnected before answering".into()),
            Err(_) => {
                let mut g = self.inner.lock();
                g.pending.retain(|_, (o, tx)| !(o == op && tx.is_closed()));
                Err(format!("OBS did not answer `{op}` within {} ms", timeout.as_millis()))
            }
        }
    }

    // ---- connection lifecycle ------------------------------------------------------------

    /// Registers a new connection. Returns `None` if another live plugin is connected.
    fn attach(&self, tx: mpsc::UnboundedSender<String>) -> Option<(u64, Arc<Notify>)> {
        let mut g = self.inner.lock();
        if let Some(c) = &g.conn {
            if c.last_rx.elapsed() < TAKEOVER_QUIET {
                return None;
            }
            self.ctx.hub.log("warn", TARGET, "previous OBS plugin connection went quiet; replacing it");
            c.kill.notify_one();
        }
        let id = self.next_conn.fetch_add(1, Ordering::Relaxed);
        let kill = Arc::new(Notify::new());
        g.conn = Some(Conn { id, tx, kill: kill.clone(), last_rx: Instant::now(), hello: None });
        Some((id, kill))
    }

    fn detach(&self, id: u64, why: &str) {
        let mut g = self.inner.lock();
        if g.conn.as_ref().map(|c| c.id) != Some(id) {
            return;
        }
        let had_hello = g.conn.as_ref().is_some_and(|c| c.hello.is_some());
        g.conn = None;
        for (_, (op, tx)) in g.pending.drain() {
            let _ = tx.send(Err(format!("OBS plugin disconnected while running `{op}`")));
        }
        self.clear_link(&mut g);
        drop(g);
        if had_hello {
            self.ctx.hub.log("warn", TARGET, format!("OBS plugin disconnected ({why})"));
        }
    }

    fn on_line(&self, id: u64, line: &[u8]) {
        let msg = match msg::parse(line) {
            Ok(Some(m)) => m,
            Ok(None) => return,
            Err(e) => {
                self.ctx.hub.log("warn", TARGET, format!("bad message from OBS plugin: {e}"));
                return;
            }
        };
        let mut g = self.inner.lock();
        match g.conn.as_mut() {
            Some(c) if c.id == id => c.last_rx = Instant::now(),
            _ => return,
        }
        match msg {
            PluginMsg::Hello(h) => self.on_hello(&mut g, h),
            PluginMsg::Status(s) => self.on_status(&mut g, *s),
            PluginMsg::Event(e) => self.on_event(&mut g, e),
            PluginMsg::RecordPath(r) => self.on_record_path(&mut g, r),
            PluginMsg::RecordEnd(r) => self.on_record_end(&mut g, r),
            PluginMsg::Reply(r) => on_reply(&mut g, r),
        }
    }

    fn on_hello(&self, g: &mut Inner, h: Hello) {
        let msg = Self::config_msg(&g.cfg);
        self.ctx.hub.log("info", TARGET, format!("OBS {} connected (plugin {}, sources: {})", h.obs, h.plugin, h.canvases.join(", ")));
        self.set(g, "obs.link", true);
        self.set(g, "obs.version", h.obs.clone());
        self.set(g, "obs.plugin.version", h.plugin.clone());
        g.installed = true;
        self.set(g, "obs.plugin.installed", true);
        if let Some(c) = g.conn.as_mut() {
            c.hello = Some(h);
        }
        self.send(g, &msg);
        self.update_health(g);
    }

    fn on_status(&self, g: &mut Inner, s: Status) {
        self.set(g, "obs.stream.active", s.streaming);
        self.set(g, "obs.stream.kbps", round1(s.kbps));
        self.set(g, "obs.stream.dropped", s.dropped);
        self.set(g, "obs.stream.total", s.total);
        self.set(g, "obs.stream.lag_ms", round1(s.lag_ms));
        self.set(g, "obs.stream.congestion", (s.congestion * 1000.0).round() / 1000.0);
        self.set(g, "obs.record.active", s.recording);
        self.set(g, "obs.record.paused", s.rec_paused);
        self.set(g, "obs.record.kbps", round1(s.rec_kbps));
        self.set(g, "obs.record.dir", s.record_dir.clone());
        if !s.record_path.is_empty() {
            self.set(g, "obs.record.path", s.record_path.clone());
        }
        self.set(g, "obs.fps", round1(s.fps));
        self.set(g, "obs.render.ms", (s.render_ms * 100.0).round() / 100.0);
        self.set(g, "obs.render.lagged", s.lagged);
        self.set(g, "obs.encode.skipped", s.skipped);
        self.set(g, "obs.scene", s.scene.clone());
        self.set(g, "obs.stale.wide", s.stale.get("wide").copied().unwrap_or(true));
        self.set(g, "obs.stale.tall", s.stale.get("tall").copied().unwrap_or(true));
        self.set(g, "obs.fallback.active", s.fallback);
        for o in &s.outputs {
            self.publish_output(g, o);
        }
        self.update_clock(g, &s);
        g.status = Some(s);
        self.update_health(g);
    }

    fn publish_output(&self, g: &mut Inner, o: &OutputStatus) {
        let id = output_id(&o.name);
        let p = format!("obs.output.{id}");
        if g.outputs.insert(id.clone()) {
            let h = &self.ctx.hub;
            h.declare(&format!("{p}.active"), ro(Meta::boolean(false), "OBS output active"));
            h.declare(&format!("{p}.kbps"), ro(Meta::float(0.0, [0.0, 1e7]).unit("kbps"), "output bitrate"));
            h.declare(&format!("{p}.dropped"), ro(Meta::int(0, [0.0, 1e12]), "frames dropped"));
            h.declare(&format!("{p}.total"), ro(Meta::int(0, [0.0, 1e12]), "frames sent"));
            h.declare(&format!("{p}.label"), string_meta("OBS output name"));
            h.declare(&format!("{p}.canvas"), string_meta("canvas the output encodes"));
            h.declare(&format!("{p}.kind"), ro(Meta::enumeration("stream", &["stream", "record"]), "output kind"));
        }
        self.set(g, &format!("{p}.active"), o.active);
        self.set(g, &format!("{p}.kbps"), round1(o.kbps));
        self.set(g, &format!("{p}.dropped"), o.dropped);
        self.set(g, &format!("{p}.total"), o.total);
        self.set(g, &format!("{p}.label"), o.name.clone());
        self.set(g, &format!("{p}.canvas"), o.canvas.clone());
        self.set(g, &format!("{p}.kind"), o.kind.clone());
    }

    /// `obs_stream` / `obs_record` mappings: other clock = ns since the first frame of the
    /// stream / current main recording file.
    fn update_clock(&self, g: &mut Inner, s: &Status) {
        if s.obs_ns == 0 || s.mono_ns == 0 {
            return;
        }
        let mut changed = false;
        let (stream_start, record_start) = (s.stream_start_ns, s.record_start_ns);
        let (prev_stream, prev_record) = (g.stream_start_obs, g.record_start_obs);
        self.ctx.hub.clock.update(|m| {
            if stream_start != 0 {
                if stream_start != prev_stream {
                    m.obs_stream = Mapping::at(obs_to_master(stream_start, s.obs_ns, s.mono_ns), 0);
                    changed = true;
                }
                m.obs_stream.observe(s.mono_ns, s.obs_ns as i128 - stream_start as i128);
            }
            if record_start != 0 {
                if record_start != prev_record {
                    m.obs_record = Mapping::at(obs_to_master(record_start, s.obs_ns, s.mono_ns), 0);
                    changed = true;
                }
                m.obs_record.observe(s.mono_ns, s.obs_ns as i128 - record_start as i128);
            }
        });
        if stream_start != 0 {
            g.stream_start_obs = stream_start;
        }
        if record_start != 0 {
            g.record_start_obs = record_start;
        }
        if changed {
            self.write_clock();
        }
    }

    fn emit(&self, ty: &str, ts: u64, payload: Value) {
        let mut e = Event::new(ty, Origin::Obs, payload);
        e.ts = ts;
        self.ctx.hub.emit(e);
    }

    fn on_event(&self, g: &mut Inner, e: PluginEvent) {
        let ts = if e.mono_ns != 0 { obs_to_master(e.obs_ns, e.obs_ns, e.mono_ns) } else { 0 };
        let path = e.path.clone().unwrap_or_default();
        match e.name.as_str() {
            "stream_started" => {
                self.set(g, "obs.stream.active", true);
                self.emit("obs.stream_started", ts, Value::map());
            }
            "stream_stopped" => {
                self.set(g, "obs.stream.active", false);
                self.emit("obs.stream_stopped", ts, Value::map());
            }
            "record_started" => {
                self.set(g, "obs.record.active", true);
                if !path.is_empty() {
                    self.set(g, "obs.record.path", path.clone());
                }
                self.emit("obs.record_started", ts, Value::map().with("path", path).with("canvas", "wide"));
            }
            "record_stopped" => {
                self.set(g, "obs.record.active", false);
                self.set(g, "obs.record.paused", false);
                if !path.is_empty() {
                    self.set(g, "obs.record.path", path.clone());
                }
                self.emit("obs.record_stopped", ts, Value::map().with("path", path).with("canvas", "wide"));
            }
            "record_paused" => self.set(g, "obs.record.paused", true),
            "record_unpaused" => self.set(g, "obs.record.paused", false),
            "scene_fallback" | "scene_restored" => {
                let active = e.name == "scene_fallback";
                if active {
                    self.set(g, "obs.fallback.active", true);
                }
                let mut p = Value::map()
                    .with("active", active)
                    .with("canvas", e.canvas.clone().unwrap_or_default())
                    .with("scene", e.scene.clone().unwrap_or_default())
                    .with("canvases", Value::List(e.canvases.iter().map(|c| Value::from(c.as_str())).collect()))
                    .with("reason", e.reason.clone().unwrap_or_default());
                if let Some(f) = &e.from {
                    p = p.with("from", f.clone());
                }
                if let Some(t) = &e.to {
                    p = p.with("to", t.clone());
                }
                let level = if active { "warn" } else { "info" };
                let msg = if active {
                    format!(
                        "OBS canvas '{}' switched to '{}' ({})",
                        e.canvas.as_deref().unwrap_or("?"),
                        e.scene.as_deref().unwrap_or("?"),
                        e.reason.as_deref().unwrap_or("?")
                    )
                } else {
                    format!(
                        "OBS canvas '{}' back on '{}' ({})",
                        e.canvas.as_deref().unwrap_or("?"),
                        e.to.as_deref().unwrap_or("?"),
                        e.reason.as_deref().unwrap_or("?")
                    )
                };
                self.ctx.hub.log(level, TARGET, msg);
                self.emit("obs.fallback", ts, p);
            }
            other => tracing::debug!(target: "obs", "plugin event `{other}`"),
        }
    }

    fn on_record_path(&self, g: &mut Inner, r: RecordPath) {
        let start = obs_to_master(r.start_obs_ns, r.obs_ns, r.mono_ns);
        let tracks = Value::from(r.tracks.clone());
        match g.recordings.iter_mut().find(|x| x.path == r.path) {
            Some(x) => {
                x.canvas = r.canvas.clone();
                x.output = r.output.clone();
                x.tracks = tracks;
                x.end_ns = None;
            }
            None => {
                g.recordings.push(Recording { canvas: r.canvas.clone(), path: r.path.clone(), output: r.output.clone(), start_ns: start, end_ns: None, tracks })
            }
        }
        if r.canvas == "wide" {
            self.set(g, "obs.record.path", r.path.clone());
        }
        self.ctx.hub.log("info", TARGET, format!("recording {} ({}): {}", r.canvas, r.output, r.path));
        self.write_recordings(g);
    }

    fn on_record_end(&self, g: &mut Inner, r: RecordEnd) {
        let end = obs_to_master(r.end_obs_ns, r.obs_ns, r.mono_ns);
        if let Some(x) = g.recordings.iter_mut().find(|x| x.path == r.path) {
            x.end_ns = Some(end);
            self.write_recordings(g);
        }
    }

    // ---- config / session -----------------------------------------------------------------

    fn apply_config(&self, cfg: ObsConfig) {
        let mut g = self.inner.lock();
        if g.cfg == cfg {
            return;
        }
        let socket_changed = g.cfg.socket != cfg.socket;
        let msg = Self::config_msg(&cfg);
        g.cfg = cfg.clone();
        if g.conn.as_ref().is_some_and(|c| c.hello.is_some()) {
            self.send(&g, &msg);
        }
        drop(g);
        if socket_changed {
            let _ = self.socket_tx.send(cfg.socket);
        }
    }

    /// A new session starts: carry over recordings still being written.
    fn on_session_rotated(&self) {
        let mut g = self.inner.lock();
        g.recordings.retain(|r| r.end_ns.is_none());
        self.write_recordings(&g);
        drop(g);
        self.write_clock();
    }

    /// Snapshot for the UI / CLI (`obs` query).
    pub fn describe(&self) -> Value {
        let g = self.inner.lock();
        let hello = g.conn.as_ref().and_then(|c| c.hello.as_ref());
        let mut v = Value::map()
            .with("connected", hello.is_some())
            .with("socket", g.cfg.socket.to_string_lossy().to_string())
            .with("stale_ms", g.cfg.stale_ms as i64)
            .with("fallback_mode", g.cfg.fallback_mode.as_str())
            .with("fallback_scene", g.cfg.fallback_scene.clone())
            .with("installed", g.installed)
            .with("recordings", Value::List(g.recordings.iter().map(Recording::value).collect()));
        if let Some(h) = hello {
            v = v
                .with("obs", h.obs.clone())
                .with("plugin", h.plugin.clone())
                .with("sources", Value::List(h.canvases.iter().map(|c| Value::from(c.as_str())).collect()));
        }
        if let Some(s) = &g.status {
            v = v
                .with("feeds", Value::from(serde_json::Value::Object(s.feeds.clone().into_iter().collect())))
                .with("sources_per_canvas", Value::Map(s.sources.iter().map(|(k, n)| (k.clone(), Value::Int(*n as i64))).collect()));
        }
        if let Some(e) = &g.listen_error {
            v = v.with("error", e.clone());
        }
        v
    }
}

fn on_reply(g: &mut Inner, r: Reply) {
    if let Some((_, tx)) = g.pending.remove(&r.id) {
        let _ = tx.send(if r.ok { Ok(r.result) } else { Err(r.error.unwrap_or_else(|| "failed".into())) });
    }
}

fn round1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// `health.obs` (§17.1): plugin connected and receiving frames on both canvases.
fn health(g: &Inner) -> (&'static str, String) {
    if let Some(e) = &g.listen_error {
        return ("fail", e.clone());
    }
    let Some(hello) = g.conn.as_ref().and_then(|c| c.hello.as_ref()) else {
        let detail = if g.installed {
            format!("OBS is not running or the stream-engine plugin is not connected ({})", g.cfg.socket.display())
        } else {
            "stream-engine OBS plugin not installed (run obs-plugin/install.sh, restart OBS)".to_string()
        };
        return ("fail", detail);
    };
    let Some(s) = &g.status else {
        return ("warn", format!("OBS {} connected; waiting for its first status", hello.obs));
    };
    let mut status = "pass";
    let mut problems = Vec::new();
    for canvas in ["wide", "tall"] {
        if s.sources.get(canvas).copied().unwrap_or(0) == 0 {
            problems.push(format!("no `stream-engine: {canvas}` source in OBS"));
            if status == "pass" {
                status = "warn";
            }
        } else if s.stale.get(canvas).copied().unwrap_or(true) {
            problems.push(format!("{canvas} canvas: no frames reaching OBS"));
            status = "fail";
        }
    }
    if problems.is_empty() { (status, format!("OBS {}, plugin {}: receiving wide + tall", hello.obs, hello.plugin)) } else { (status, problems.join("; ")) }
}

// ---- socket ------------------------------------------------------------------------------

fn bind(path: &Path) -> Result<UnixListener, String> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if let Some(dir) = path.parent() {
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    }
    if path.exists() {
        if std::os::unix::net::UnixStream::connect(path).is_ok() {
            return Err(format!("another engine already serves {}", path.display()));
        }
        std::fs::remove_file(path).map_err(|e| format!("cannot remove stale {}: {e}", path.display()))?;
    }
    let l = UnixListener::bind(path).map_err(|e| format!("cannot listen on {}: {e}", path.display()))?;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    Ok(l)
}

async fn serve(obs: Arc<Obs>, mut socket_rx: watch::Receiver<PathBuf>) {
    loop {
        let path = socket_rx.borrow_and_update().clone();
        match bind(&path) {
            Ok(listener) => {
                {
                    let mut g = obs.inner.lock();
                    g.listen_error = None;
                    obs.update_health(&mut g);
                }
                tracing::info!(target: "obs", "listening on {}", path.display());
                loop {
                    tokio::select! {
                        r = listener.accept() => match r {
                            Ok((stream, _)) => { tokio::spawn(connection(obs.clone(), stream)); }
                            Err(e) => {
                                obs.ctx.hub.log("error", TARGET, format!("accept on {}: {e}", path.display()));
                                tokio::time::sleep(Duration::from_millis(200)).await;
                            }
                        },
                        r = socket_rx.changed() => {
                            if r.is_err() { return; }
                            break;
                        }
                    }
                }
                drop(listener);
                let _ = std::fs::remove_file(&path);
            }
            Err(e) => {
                obs.ctx.hub.log("error", TARGET, e.clone());
                {
                    let mut g = obs.inner.lock();
                    g.listen_error = Some(e);
                    obs.update_health(&mut g);
                }
                tokio::select! {
                    _ = tokio::time::sleep(Duration::from_secs(5)) => {}
                    r = socket_rx.changed() => if r.is_err() { return; },
                }
            }
        }
    }
}

async fn connection(obs: Arc<Obs>, stream: UnixStream) {
    let (rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let Some((id, kill)) = obs.attach(tx) else {
        let line = EngineMsg::Error { error: "another OBS instance is already connected to this engine".into() }.line();
        let _ = wr.write_all(line.as_bytes()).await;
        obs.ctx.hub.log("warn", TARGET, "rejected a second OBS plugin connection");
        return;
    };
    let writer = tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if wr.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
    });
    let mut reader = BufReader::new(rd);
    let mut buf = Vec::with_capacity(4096);
    let why = loop {
        buf.clear();
        let mut limited = (&mut reader).take(MAX_LINE + 1);
        let read = tokio::select! {
            _ = kill.notified() => break "replaced by a new connection".to_string(),
            r = tokio::time::timeout(IDLE_TIMEOUT, limited.read_until(b'\n', &mut buf)) => r,
        };
        match read {
            Err(_) => break format!("no message for {} s", IDLE_TIMEOUT.as_secs()),
            Ok(Err(e)) => break format!("read error: {e}"),
            Ok(Ok(0)) => break "closed by OBS".to_string(),
            Ok(Ok(_)) if buf.last() != Some(&b'\n') => {
                break if buf.len() as u64 > MAX_LINE { "line too long".to_string() } else { "closed by OBS".to_string() };
            }
            Ok(Ok(_)) => {
                buf.pop();
                if !buf.is_empty() {
                    obs.on_line(id, &buf);
                }
            }
        }
    };
    obs.detach(id, &why);
    writer.abort();
}

// ---- tasks --------------------------------------------------------------------------------

/// `obs.<action>` → plugin op.
pub fn action_op(name: &str) -> Option<&'static str> {
    Some(match name {
        "obs.stream.start" => "stream.start",
        "obs.stream.stop" => "stream.stop",
        "obs.record.start" => "record.start",
        "obs.record.stop" => "record.stop",
        "obs.fallback.on" => "fallback.on",
        "obs.fallback.off" => "fallback.off",
        "obs.fallback.setup" => "fallback.setup",
        "obs.setup" => "setup",
        _ => return None,
    })
}

async fn route_actions(obs: Arc<Obs>, mut rx: mpsc::UnboundedReceiver<Command>) {
    while let Some(c) = rx.recv().await {
        let Op::Action { name, .. } = &c.op else { continue };
        let Some(op) = action_op(name) else {
            obs.ctx.hub.log("warn", TARGET, format!("unknown action `{name}`"));
            continue;
        };
        let (obs, name) = (obs.clone(), name.clone());
        tokio::spawn(async move {
            // Rehearsal never goes on air (§17.2). Ask the core rather than the snapshot: a
            // "mode, then stream.start" pair from one client is then judged after the mode change.
            if op == "stream.start" && obs.ctx.hub.with_core(|core| Value::Bool(core.mode_str() == "rehearsal")).await.truthy() {
                obs.ctx.hub.log("info", TARGET, "rehearsal: not going on air (obs.stream.start skipped; recording still works)");
                obs.ctx.hub.emit(Event::new("obs.dry_run", Origin::Obs, Value::map().with("action", name.as_str())));
                return;
            }
            match obs.command(op).await {
                Ok(v) => obs.ctx.hub.log("info", TARGET, format!("{name}: {}", if v.is_null() { "ok".to_string() } else { v.to_string() })),
                Err(e) => obs.ctx.hub.log("error", TARGET, format!("{name} failed: {e}")),
            }
        });
    }
}

async fn watch_config(obs: Arc<Obs>) {
    let mut rx = obs.ctx.config.clone();
    while rx.changed().await.is_ok() {
        let section = rx.borrow_and_update().project.extra.get("obs").cloned();
        match ObsConfig::from_section(section.as_ref()) {
            Ok(cfg) => obs.apply_config(cfg),
            Err(e) => obs.ctx.hub.log("error", TARGET, format!("{e}; keeping the previous [obs] settings")),
        }
    }
}

async fn watch_bus(obs: Arc<Obs>) {
    let mut rx = obs.ctx.hub.subscribe();
    loop {
        match rx.recv().await {
            Ok(b) => {
                if let Bus::Event(e) = &*b
                    && e.ty == "session.closed"
                {
                    obs.on_session_rotated();
                }
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => return,
        }
    }
}

async fn watch_install(obs: Arc<Obs>) {
    let mut t = tokio::time::interval(Duration::from_secs(30));
    loop {
        t.tick().await;
        let installed = plugin_installed();
        let mut g = obs.inner.lock();
        if g.conn.as_ref().and_then(|c| c.hello.as_ref()).is_some() || g.installed == installed {
            continue;
        }
        g.installed = installed;
        obs.set(&mut g, "obs.plugin.installed", installed);
        obs.update_health(&mut g);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_ids() {
        assert_eq!(output_id("simple_stream"), "simple_stream");
        assert_eq!(output_id("Aitum Vertical Stream #2"), "aitum_vertical_stream__2");
        assert_eq!(output_id("--"), "output");
    }

    #[test]
    fn actions_map_to_plugin_ops() {
        assert_eq!(action_op("obs.record.start"), Some("record.start"));
        assert_eq!(action_op("obs.fallback.off"), Some("fallback.off"));
        assert_eq!(action_op("obs.stream"), None);
    }
}
