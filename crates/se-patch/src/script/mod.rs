//! Script patches (`kind = "script"`): one thread and one sandboxed Lua VM per patch.
//!
//! Each worker receives the events its handlers subscribed to (forwarded from the hub bus by
//! the loader), calls `frame(dt, signals)` at the render rate, and publishes the resulting
//! draw list into `hub.draw` slot `patch.<id>`. A runaway script only ever blocks its own
//! thread, and only until the budget hook aborts it.

pub mod convert;
pub mod draw;
pub mod vm;

use crate::manifest::Manifest;
use crate::registry::state;
use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use parking_lot::Mutex;
use se_hub::Hub;
use se_proto::{Event, Value, address};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use vm::{CallError, Inputs, ScriptVm};

/// Consecutive over-budget ticks before a patch is suspended.
pub const OVER_BUDGET_TICKS: u32 = 30;
/// Queued events per patch before new ones are dropped.
pub const EVENT_QUEUE: usize = 1024;

/// Control messages (never dropped).
pub enum Ctrl {
    Load {
        manifest: Arc<Manifest>,
        generation: u64,
    },
    /// The patch folder's newest `patch.toml` failed to parse; the running version stays live.
    ManifestError(String),
    Enable(bool),
    /// Resume a suspended patch with its current VM.
    Resume,
    Stop,
}

/// Live status of a script patch (read by the `patches` query).
#[derive(Clone, Debug, Default)]
pub struct ScriptStatus {
    /// `loaded` | `error` | `suspended` | `disabled` (empty before the first load attempt).
    pub state: &'static str,
    pub error: String,
    /// A VM version is loaded (it keeps running when a reload fails).
    pub live: bool,
    /// Generation of the running version.
    pub live_generation: u64,
    /// CPU per tick, exponential average and recent peak (ms).
    pub cpu_ms_avg: f64,
    pub cpu_ms_peak: f64,
    pub memory_kb: u64,
    pub handlers: usize,
    pub has_frame: bool,
    pub events: u64,
    pub frames: u64,
    pub draw_ops: usize,
}

/// Rate and resolution shared with the loader (updated on config changes).
#[derive(Default)]
pub struct Timing {
    pub fps: AtomicU32,
    /// `width << 32 | height`
    pub resolution: AtomicU64,
}

impl Timing {
    pub fn new(fps: u32, res: [u32; 2]) -> Timing {
        let t = Timing::default();
        t.set(fps, res);
        t
    }
    pub fn set(&self, fps: u32, [w, h]: [u32; 2]) {
        self.fps.store(fps.clamp(1, 240), Ordering::Relaxed);
        self.resolution.store(((w as u64) << 32) | h as u64, Ordering::Relaxed);
    }
    fn get(&self) -> (u32, [u32; 2]) {
        let r = self.resolution.load(Ordering::Relaxed);
        (self.fps.load(Ordering::Relaxed).max(1), [(r >> 32) as u32, r as u32])
    }
}

pub struct Worker {
    ctrl: Sender<Ctrl>,
    events: Sender<Arc<Event>>,
    patterns: Arc<ArcSwap<Vec<String>>>,
    pub status: Arc<Mutex<ScriptStatus>>,
    pub dropped: Arc<AtomicU64>,
    /// Set when the patch is removed: the thread must not publish state any more.
    stopped: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Worker {
    pub fn spawn(id: &str, hub: Arc<Hub>, timing: Arc<Timing>, manifest: Arc<Manifest>, generation: u64, enabled: bool) -> std::io::Result<Worker> {
        let (ctrl, ctrl_rx) = crossbeam_channel::unbounded();
        let (events, events_rx) = crossbeam_channel::bounded(EVENT_QUEUE);
        let patterns = Arc::new(ArcSwap::from_pointee(Vec::new()));
        let status = Arc::new(Mutex::new(ScriptStatus::default()));
        let _ = ctrl.send(Ctrl::Enable(enabled));
        let _ = ctrl.send(Ctrl::Load { manifest, generation });
        let stopped = Arc::new(AtomicBool::new(false));
        let (rid, rpatterns, rstatus, rstopped) = (id.to_string(), patterns.clone(), status.clone(), stopped.clone());
        let thread = std::thread::Builder::new().name(format!("se-lua-{id}")).spawn(move || {
            let run = Run {
                id: rid,
                hub,
                timing,
                patterns: rpatterns,
                status: rstatus,
                stopped: rstopped,
                vm: None,
                pending: None,
                enabled,
                suspended: false,
                published_state: None,
            };
            run.run(ctrl_rx, events_rx)
        })?;
        Ok(Worker { ctrl, events, patterns, status, dropped: Arc::new(AtomicU64::new(0)), stopped, thread: Some(thread) })
    }

    pub fn send(&self, c: Ctrl) {
        let _ = self.ctrl.send(c);
    }

    /// Forward `e` if one of the script's handlers matches it.
    pub fn offer(&self, e: &Arc<Event>) {
        let p = self.patterns.load();
        if p.iter().any(|pat| address::matches(pat, &e.ty)) {
            match self.events.try_send(e.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    self.dropped.fetch_add(1, Ordering::Relaxed);
                }
                Err(TrySendError::Disconnected(_)) => {}
            }
        }
    }

    pub fn is_finished(&self) -> bool {
        self.thread.as_ref().is_none_or(|t| t.is_finished())
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let _ = self.ctrl.send(Ctrl::Stop);
        // the thread exits on its own (a running callback is bounded by the budget hook)
        self.thread.take();
    }
}

struct Run {
    id: String,
    hub: Arc<Hub>,
    timing: Arc<Timing>,
    patterns: Arc<ArcSwap<Vec<String>>>,
    status: Arc<Mutex<ScriptStatus>>,
    stopped: Arc<AtomicBool>,
    vm: Option<ScriptVm>,
    pending: Option<(Arc<Manifest>, u64)>,
    enabled: bool,
    suspended: bool,
    published_state: Option<(&'static str, String)>,
}

impl Run {
    fn running(&self) -> bool {
        self.enabled && !self.suspended && self.vm.is_some()
    }

    fn set_status(&mut self, st: &'static str, error: String) {
        {
            let mut s = self.status.lock();
            s.state = st;
            s.error = error.clone();
            s.live = self.vm.is_some();
        }
        let next = (st, error);
        if self.published_state.as_ref() != Some(&next) && !self.stopped.load(Ordering::Acquire) {
            let a = format!("patch.{}", self.id);
            self.hub.publish(&format!("{a}.state"), Value::Str(next.0.into()));
            self.hub.publish(&format!("{a}.error"), Value::Str(next.1.clone()));
            self.published_state = Some(next);
        }
    }

    fn sync_patterns(&self) {
        let p = match (&self.vm, self.running()) {
            (Some(vm), true) => vm.patterns(),
            _ => Vec::new(),
        };
        if **self.patterns.load() != p {
            self.patterns.store(Arc::new(p));
        }
    }

    fn inputs_res(&self) -> [u32; 2] {
        self.timing.get().1
    }

    fn load(&mut self, writer: &mut se_hub::draw::DrawWriter) {
        let Some((m, generation)) = self.pending.take() else { return };
        let snap = self.hub.snapshot.load();
        let inputs = Inputs { snap: &snap, resolution: self.inputs_res() };
        let target = format!("patch.{}", self.id);
        match ScriptVm::load(&m, self.hub.clone(), &inputs) {
            Ok(vm) => {
                let replaced = self.vm.is_some();
                {
                    let mut s = self.status.lock();
                    s.live_generation = generation;
                    s.handlers = vm.patterns().len();
                    s.has_frame = vm.has_frame();
                    s.memory_kb = (vm.used_memory() / 1024) as u64;
                    s.cpu_ms_avg = 0.0;
                    s.cpu_ms_peak = 0.0;
                }
                if !vm.has_frame() {
                    writer.publish_with(|_| {});
                }
                self.vm = Some(vm);
                self.suspended = false;
                self.hub.log("info", &target, if replaced { "reloaded" } else { "loaded" });
                self.set_status(if self.enabled { state::LOADED } else { state::DISABLED }, String::new());
            }
            Err(e) => {
                let msg = match &e {
                    CallError::Budget(why) => format!("{}: top-level code over budget: {why}", m.entry),
                    CallError::Script(s) => s.clone(),
                };
                let live = if self.vm.is_some() { " (previous version still live)" } else { "" };
                self.hub.log("error", &target, format!("load failed{live}: {msg}"));
                self.set_status(state::ERROR, msg);
            }
        }
        self.sync_patterns();
    }

    fn suspend(&mut self, why: String, writer: &mut se_hub::draw::DrawWriter) {
        self.suspended = true;
        writer.publish_with(|_| {});
        self.status.lock().draw_ops = 0;
        let msg = format!("suspended: {why}");
        self.hub.log("error", &format!("patch.{}", self.id), msg.clone());
        self.set_status(state::SUSPENDED, msg);
        self.sync_patterns();
    }

    fn call_failed(&mut self, e: CallError, writer: &mut se_hub::draw::DrawWriter) {
        match e {
            CallError::Budget(why) => self.suspend(why, writer),
            CallError::Script(msg) => {
                if self.status.lock().error != msg {
                    self.hub.log("error", &format!("patch.{}", self.id), msg.clone());
                }
                self.set_status(state::ERROR, msg);
            }
        }
    }

    fn run(mut self, ctrl: Receiver<Ctrl>, events: Receiver<Arc<Event>>) {
        let mut writer = self.hub.draw.register(&format!("patch.{}", self.id));
        let mut next = Instant::now();
        let mut last_frame = Instant::now();
        let mut tick_used = Duration::ZERO;
        let mut over = 0u32;
        let mut peak_window = 0u32;
        loop {
            if self.enabled && self.pending.is_some() {
                self.load(&mut writer);
                last_frame = Instant::now();
                next = last_frame;
            }
            let running = self.running();
            let wait = if running { next.saturating_duration_since(Instant::now()) } else { Duration::from_secs(3600) };
            crossbeam_channel::select! {
                recv(ctrl) -> m => match m {
                    Ok(Ctrl::Load { manifest, generation }) => {
                        self.pending = Some((manifest, generation));
                        if !self.enabled {
                            self.set_status(state::DISABLED, String::new());
                        }
                    }
                    Ok(Ctrl::ManifestError(msg)) => {
                        let live = if self.vm.is_some() { " (previous version still live)" } else { "" };
                        self.hub.log("error", &format!("patch.{}", self.id), format!("manifest error{live}: {msg}"));
                        self.set_status(state::ERROR, msg);
                    }
                    Ok(Ctrl::Enable(on)) => {
                        self.enabled = on;
                        if on {
                            self.suspended = false;
                            if self.vm.is_some() && self.pending.is_none() {
                                self.set_status(state::LOADED, String::new());
                            }
                            last_frame = Instant::now();
                            next = last_frame;
                            over = 0;
                        } else {
                            writer.publish_with(|_| {});
                            self.status.lock().draw_ops = 0;
                            self.set_status(state::DISABLED, String::new());
                        }
                        self.sync_patterns();
                    }
                    Ok(Ctrl::Resume) => {
                        if self.suspended {
                            self.suspended = false;
                            over = 0;
                            last_frame = Instant::now();
                            next = last_frame;
                            self.set_status(state::LOADED, String::new());
                            self.hub.log("info", &format!("patch.{}", self.id), "resumed");
                            self.sync_patterns();
                        }
                    }
                    Ok(Ctrl::Stop) | Err(_) => break,
                },
                recv(events) -> e => {
                    let Ok(e) = e else { break };
                    if !self.running() {
                        continue;
                    }
                    let snap = self.hub.snapshot.load();
                    let inputs = Inputs { snap: &snap, resolution: self.inputs_res() };
                    let vm = self.vm.as_mut().expect("running implies a VM");
                    let r = vm.refresh(&inputs).map_err(|err| CallError::Script(err.to_string())).and_then(|_| vm.dispatch(&e));
                    self.status.lock().events += 1;
                    match r {
                        Ok(d) => tick_used += d,
                        Err(err) => self.call_failed(err, &mut writer),
                    }
                    let n = self.vm.as_ref().map(|v| v.patterns().len()).unwrap_or(0);
                    if n != self.status.lock().handlers {
                        self.status.lock().handlers = n;
                        self.sync_patterns();
                    }
                },
                default(wait) => {
                    if !self.running() {
                        continue;
                    }
                    let (fps, res) = self.timing.get();
                    let period = Duration::from_secs_f64(1.0 / fps as f64);
                    let now = Instant::now();
                    let dt = (now - last_frame).as_secs_f64().min(0.25);
                    last_frame = now;
                    let snap = self.hub.snapshot.load();
                    let inputs = Inputs { snap: &snap, resolution: res };
                    let vm = self.vm.as_mut().expect("running implies a VM");
                    let cpu_ms = vm.limits().cpu_ms;
                    let mut ops_len = 0;
                    let r = vm.refresh(&inputs).map_err(|err| CallError::Script(err.to_string())).and_then(|_| {
                        vm.frame(dt, |ops| {
                            ops_len = ops.len();
                            writer.publish_with(|dst| dst.append(ops));
                        })
                    });
                    let mem = vm.used_memory();
                    vm.begin_tick();
                    match r {
                        Ok(d) => tick_used += d,
                        Err(err) => self.call_failed(err, &mut writer),
                    }
                    let used = tick_used.as_secs_f64() * 1000.0;
                    tick_used = Duration::ZERO;
                    {
                        let mut s = self.status.lock();
                        s.cpu_ms_avg = s.cpu_ms_avg * 0.95 + used * 0.05;
                        peak_window += 1;
                        if peak_window >= fps * 2 {
                            peak_window = 0;
                            s.cpu_ms_peak = used;
                        } else {
                            s.cpu_ms_peak = s.cpu_ms_peak.max(used);
                        }
                        s.memory_kb = (mem / 1024) as u64;
                        s.frames += 1;
                        s.draw_ops = ops_len;
                    }
                    if !self.suspended {
                        if used > cpu_ms {
                            over += 1;
                            if over >= OVER_BUDGET_TICKS {
                                over = 0;
                                let avg = self.status.lock().cpu_ms_avg;
                                self.suspend(format!("over CPU budget for {OVER_BUDGET_TICKS} ticks in a row ({used:.2} ms/tick, average {avg:.2} ms, budget {cpu_ms} ms)"), &mut writer);
                            }
                        } else {
                            over = 0;
                        }
                    }
                    next += period;
                    let now = Instant::now();
                    if next < now {
                        next = now + period;
                    }
                },
            }
        }
        writer.publish_with(|_| {});
    }
}
