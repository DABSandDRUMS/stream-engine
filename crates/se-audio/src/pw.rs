//! PipeWire host: one `pw_filter` node (`se-engine`) whose real-time process callback runs
//! the [`Engine`], one virtual source node per bus (`se-band`, `se-music`, … — what OBS
//! captures), and the links between them and the hardware (Studio 24c / 16R capture, monitor
//! and direct outputs). Runs its own thread with a PipeWire main loop; reconnects if the
//! PipeWire daemon restarts. Hardware hotplug is handled by re-linking from the registry.

use crate::builder::{BusNode, LinkSpec, LinkTarget};
use crate::graph::{Cycle, Engine, PortIo, RtStats};
use parking_lot::Mutex;
use pipewire as pw;
use pw::spa::sys as spa_sys;
use pw::sys as pw_sys;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{CStr, CString, c_void};
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicPtr, Ordering};
use std::time::{Duration, Instant};

/// Max filter ports per direction.
pub const MAX_PORTS: usize = 128;

/// Port data pointers shared between the main loop (creates ports) and the RT callback.
pub struct PortSlots {
    pub ins: Vec<AtomicPtr<c_void>>,
    pub outs: Vec<AtomicPtr<c_void>>,
}

impl Default for PortSlots {
    fn default() -> Self {
        PortSlots {
            ins: (0..MAX_PORTS).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(),
            outs: (0..MAX_PORTS).map(|_| AtomicPtr::new(std::ptr::null_mut())).collect(),
        }
    }
}

impl PortSlots {
    fn clear(&self) {
        for p in self.ins.iter().chain(&self.outs) {
            p.store(std::ptr::null_mut(), Ordering::Release);
        }
    }
}

/// What the PipeWire side should look like.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PwPlan {
    pub ins: Vec<String>,
    pub outs: Vec<String>,
    pub links: Vec<LinkSpec>,
    pub nodes: Vec<BusNode>,
    pub quantum: u32,
    pub rate: u32,
}

pub enum PwCmd {
    Plan(Box<PwPlan>),
    Quit,
}

/// A PipeWire audio device node (for the UI, health, and target matching).
#[derive(Clone, Debug, PartialEq, serde::Serialize)]
pub struct DeviceInfo {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub class: String,
    pub ports: usize,
}

#[derive(Clone, Debug, Default)]
pub struct PwStatus {
    pub connected: bool,
    pub node_id: Option<u32>,
    pub state: String,
    pub error: Option<String>,
    pub devices: Vec<DeviceInfo>,
    /// Per plan link: resolved and linked.
    pub links: Vec<(LinkSpec, bool)>,
    /// Consumers (links to other nodes) per bus node name, e.g. OBS capturing `se-program`.
    pub consumers: BTreeMap<String, Vec<String>>,
    pub reconnects: u32,
}

pub struct PwHandle {
    tx: pw::channel::Sender<PwCmd>,
    quit: Arc<AtomicBool>,
    pub status: Arc<Mutex<PwStatus>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl PwHandle {
    pub fn plan(&self, p: PwPlan) {
        let _ = self.tx.send(PwCmd::Plan(Box::new(p)));
    }
}

impl Drop for PwHandle {
    fn drop(&mut self) {
        self.quit.store(true, Ordering::Release);
        let _ = self.tx.send(PwCmd::Quit);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Start the PipeWire thread. `engine` runs in the filter's RT callback.
pub fn spawn(engine: Box<Engine>, ports: Arc<PortSlots>, stats: Arc<RtStats>, plan: PwPlan) -> anyhow::Result<PwHandle> {
    let (tx, rx) = pw::channel::channel::<PwCmd>();
    let status = Arc::new(Mutex::new(PwStatus::default()));
    let quit = Arc::new(AtomicBool::new(false));
    let (st, q) = (status.clone(), quit.clone());
    let join = std::thread::Builder::new().name("se-audio-pw".into()).spawn(move || run(engine, ports, stats, rx, st, q, plan))?;
    Ok(PwHandle { tx, quit, status, join: Some(join) })
}

// ---- RT callback ----------------------------------------------------------------------------

struct RtCtx {
    engine: Box<Engine>,
    ports: Arc<PortSlots>,
    stats: Arc<RtStats>,
    ins: Vec<*mut f32>,
    outs: Vec<*mut f32>,
    last_pos: u64,
    last_dur: u64,
    last_xrun: u64,
    last_clock: u32,
    last_cycle: u32,
    warm: u32,
    sched_check: u32,
    tried_fifo: bool,
}

struct PwIo<'a> {
    ins: &'a [*mut f32],
    outs: &'a [*mut f32],
    n: usize,
}

impl PortIo for PwIo<'_> {
    fn input(&self, port: usize) -> Option<&[f32]> {
        let p = *self.ins.get(port)?;
        // SAFETY: PipeWire guarantees `n` valid samples for this cycle in DSP port buffers.
        (!p.is_null()).then(|| unsafe { std::slice::from_raw_parts(p, self.n) })
    }

    fn output(&mut self, port: usize) -> Option<&mut [f32]> {
        let p = *self.outs.get(port)?;
        // SAFETY: as above; each port buffer is distinct.
        (!p.is_null()).then(|| unsafe { std::slice::from_raw_parts_mut(p, self.n) })
    }
}

fn record_sched(stats: &RtStats) {
    // SAFETY: plain syscalls on the calling thread.
    unsafe {
        let tid = libc::gettid();
        let pol = libc::sched_getscheduler(0);
        let mut sp = libc::sched_param { sched_priority: 0 };
        libc::sched_getparam(0, &mut sp);
        stats.tid.store(tid, Ordering::Relaxed);
        stats.policy.store(pol, Ordering::Relaxed);
        stats.priority.store(sp.sched_priority, Ordering::Relaxed);
    }
}

unsafe extern "C" fn on_process(data: *mut c_void, position: *mut spa_sys::spa_io_position) {
    let t0 = Instant::now();
    // SAFETY: `data` is the `RtCtx` we registered; the callback is only called while the
    // filter lives, and only from the data thread.
    let cx = unsafe { &mut *(data as *mut RtCtx) };
    let scope = se_alloc::Scope::begin();
    let (n, nsec, rate, clock_id, cycle, pos, xrun, flags, delay) = if position.is_null() {
        (256usize, se_clock::now(), 48000u32, 0u32, 0u32, 0u64, 0u64, 0u32, 0i64)
    } else {
        // SAFETY: valid for the duration of the callback.
        let c = unsafe { &(*position).clock };
        let rate = if c.rate.denom > 0 { c.rate.denom } else { 48000 };
        (c.duration as usize, c.nsec, rate, c.id, c.cycle, c.position, c.xrun, c.flags, c.delay)
    };
    let n = n.min(8192);
    let st = &cx.stats;
    // xrun detection: driver-reported xrun time, recovery flag, and position discontinuity
    if cx.warm > 0 {
        cx.warm -= 1;
    } else if clock_id == cx.last_clock && cycle == cx.last_cycle {
        let discont = pos != cx.last_pos.wrapping_add(cx.last_dur);
        if xrun > cx.last_xrun || flags & spa_sys::SPA_IO_CLOCK_FLAG_XRUN_RECOVER != 0 || discont {
            st.xruns.fetch_add(1, Ordering::Relaxed);
        }
    } else {
        cx.warm = 8;
    }
    cx.last_clock = clock_id;
    cx.last_cycle = cycle;
    cx.last_pos = pos;
    cx.last_dur = n as u64;
    cx.last_xrun = xrun;
    // gather port buffers
    for (i, slot) in cx.ports.ins.iter().enumerate() {
        let p = slot.load(Ordering::Acquire);
        // SAFETY: port pointers come from pw_filter_add_port on this filter.
        cx.ins[i] = if p.is_null() { std::ptr::null_mut() } else { unsafe { pw_sys::pw_filter_get_dsp_buffer(p, n as u32) as *mut f32 } };
    }
    for (i, slot) in cx.ports.outs.iter().enumerate() {
        let p = slot.load(Ordering::Acquire);
        let b = if p.is_null() { std::ptr::null_mut() } else { unsafe { pw_sys::pw_filter_get_dsp_buffer(p, n as u32) as *mut f32 } };
        if !b.is_null() {
            // SAFETY: n samples valid (see PwIo).
            unsafe { std::ptr::write_bytes(b, 0, n) };
        }
        cx.outs[i] = b;
    }
    let mut io = PwIo { ins: &cx.ins, outs: &cx.outs, n };
    cx.engine.process(&mut io, n, Cycle { nsec, rate });
    // bookkeeping
    if cx.sched_check == 0 {
        if !cx.tried_fifo {
            cx.tried_fifo = true;
            // wasmtime's per-thread signal stack, once (mmap + sigaltstack, no heap)
            se_dsp::wasm::prepare_thread();
            // SAFETY: syscall on this thread; fails with EPERM without rtprio rights.
            unsafe {
                if libc::sched_getscheduler(0) == libc::SCHED_OTHER {
                    let sp = libc::sched_param { sched_priority: 70 };
                    libc::sched_setscheduler(0, libc::SCHED_FIFO | libc::SCHED_RESET_ON_FORK, &sp);
                }
            }
        }
        record_sched(st);
        cx.sched_check = 2048;
    } else {
        cx.sched_check -= 1;
    }
    let allocs = scope.allocs() + scope.frees();
    drop(scope);
    if allocs > 0 {
        st.allocs.fetch_add(allocs, Ordering::Relaxed);
    }
    let dt = t0.elapsed().as_nanos() as u64;
    st.cycles.fetch_add(1, Ordering::Relaxed);
    st.dsp_ns.store(dt, Ordering::Relaxed);
    st.dsp_ns_max.fetch_max(dt, Ordering::Relaxed);
    st.dsp_ns_sum.fetch_add(dt, Ordering::Relaxed);
    st.dsp_count.fetch_add(1, Ordering::Relaxed);
    st.quantum.store(n as u32, Ordering::Relaxed);
    st.rate.store(rate, Ordering::Relaxed);
    st.position.store(pos, Ordering::Relaxed);
    st.nsec.store(nsec, Ordering::Relaxed);
    st.delay.store(delay as i32, Ordering::Relaxed);
}

unsafe extern "C" fn on_state(data: *mut c_void, _old: pw_sys::pw_filter_state, state: pw_sys::pw_filter_state, error: *const std::os::raw::c_char) {
    // SAFETY: `data` is the StateSink registered with the listener.
    let sink = unsafe { &*(data as *const StateSink) };
    // SAFETY: returns a static string.
    let name = unsafe { CStr::from_ptr(pw_sys::pw_filter_state_as_string(state)) }.to_string_lossy().to_string();
    let mut s = sink.status.lock();
    s.state = name;
    if !error.is_null() {
        // SAFETY: NUL-terminated error string from PipeWire.
        s.error = Some(unsafe { CStr::from_ptr(error) }.to_string_lossy().to_string());
    }
}

struct StateSink {
    status: Arc<Mutex<PwStatus>>,
}

// ---- main loop side -------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct NodeG {
    name: String,
    description: String,
    class: String,
}

#[derive(Clone, Debug)]
struct PortG {
    node: u32,
    input: bool,
    port_id: u32,
    name: String,
    monitor: bool,
}

#[derive(Default)]
struct Globals {
    nodes: HashMap<u32, NodeG>,
    ports: HashMap<u32, PortG>,
    /// link id → (output port, input port, output node, input node)
    links: HashMap<u32, (u32, u32, u32, u32)>,
    dirty: bool,
}

fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).expect("no NUL")
}

fn props(kv: &[(&str, String)]) -> *mut pw_sys::pw_properties {
    // SAFETY: pw_properties_new with a terminating NULL key creates an empty set.
    let p = unsafe { pw_sys::pw_properties_new(std::ptr::null()) };
    for (k, v) in kv {
        let (k, v) = (cstr(k), cstr(v));
        // SAFETY: valid NUL-terminated strings; copies are made.
        unsafe { pw_sys::pw_properties_set(p, k.as_ptr(), v.as_ptr()) };
    }
    p
}

struct Filter {
    raw: *mut pw_sys::pw_filter,
    _hook: Box<spa_sys::spa_hook>,
    _events: Box<pw_sys::pw_filter_events>,
    _sink: Box<StateSink>,
    in_ports: Vec<*mut c_void>,
    out_ports: Vec<*mut c_void>,
}

impl Filter {
    fn add_port(&mut self, name: &str, input: bool) -> *mut c_void {
        let p = props(&[("format.dsp", "32 bit float mono audio".into()), ("port.name", name.into())]);
        let dir = if input { spa_sys::SPA_DIRECTION_INPUT } else { spa_sys::SPA_DIRECTION_OUTPUT };
        // SAFETY: filter is alive; props ownership passes to PipeWire.
        let data = unsafe {
            pw_sys::pw_filter_add_port(
                self.raw,
                dir,
                pw_sys::pw_filter_port_flags_PW_FILTER_PORT_FLAG_MAP_BUFFERS,
                std::mem::size_of::<u64>(),
                p,
                std::ptr::null_mut(),
                0,
            )
        };
        if input {
            self.in_ports.push(data);
        } else {
            self.out_ports.push(data);
        }
        data
    }

    fn set_latency(&self, quantum: u32, rate: u32) {
        let k = cstr("node.latency");
        let v = cstr(&format!("{quantum}/{rate}"));
        let item = spa_sys::spa_dict_item { key: k.as_ptr(), value: v.as_ptr() };
        let dict = spa_sys::spa_dict { flags: 0, n_items: 1, items: &item };
        // SAFETY: dict is valid for the call; PipeWire copies it.
        unsafe { pw_sys::pw_filter_update_properties(self.raw, std::ptr::null_mut(), &dict) };
    }

    fn node_id(&self) -> Option<u32> {
        // SAFETY: filter alive.
        let id = unsafe { pw_sys::pw_filter_get_node_id(self.raw) };
        (id != pw_sys::PW_ID_ANY).then_some(id)
    }
}

impl Drop for Filter {
    fn drop(&mut self) {
        // SAFETY: disconnect stops the RT callback before destroy frees the filter.
        unsafe {
            pw_sys::pw_filter_disconnect(self.raw);
            pw_sys::pw_filter_destroy(self.raw);
        }
    }
}

fn norm(s: &str) -> String {
    s.to_lowercase()
}

/// Resolve a target string against nodes of `class` (exact node.name wins, then
/// case-insensitive substring of name or description; lowest id for stability).
fn find_node(g: &Globals, target: &str, class: &str) -> Option<u32> {
    let mut exact = None;
    let mut sub: Vec<u32> = Vec::new();
    let t = norm(target);
    for (id, n) in &g.nodes {
        // `Audio/Source` also matches virtual sources (loopbacks, test signal nodes); our own
        // bus nodes are never inputs or outputs (feedback)
        if !n.class.starts_with(class) || n.name.starts_with("se-") {
            continue;
        }
        if n.name == target {
            exact = Some(*id);
        } else if norm(&n.name).contains(&t) || norm(&n.description).contains(&t) {
            sub.push(*id);
        }
    }
    exact.or_else(|| sub.into_iter().min())
}

/// 1-based channel → port global of `node` in `input` direction (by port.id; monitor ports
/// only for sources without real outputs).
fn node_port(g: &Globals, node: u32, input: bool, channel: u32) -> Option<u32> {
    // real ports first; virtual sources (null-sink based) only have monitor outputs
    let pick = |monitor: bool| -> Vec<(u32, u32)> {
        g.ports.iter().filter(|(_, p)| p.node == node && p.input == input && p.monitor == monitor).map(|(id, p)| (p.port_id, *id)).collect()
    };
    let mut ports = pick(false);
    if ports.is_empty() && !input {
        ports = pick(true);
    }
    ports.sort();
    ports.get(channel.checked_sub(1)? as usize).map(|(_, id)| *id)
}

fn own_port(g: &Globals, node: u32, name: &str, input: bool) -> Option<u32> {
    g.ports.iter().find(|(_, p)| p.node == node && p.name == name && p.input == input).map(|(id, _)| *id)
}

struct Session {
    core: pw::core::CoreRc,
    globals: Rc<RefCell<Globals>>,
    links: HashMap<(u32, u32), pw::link::Link>,
    nodes: HashMap<String, pw::node::Node>,
    status: Arc<Mutex<PwStatus>>,
}

impl Session {
    fn drop_link(&self, l: pw::link::Link) {
        let _ = self.core.destroy_object(l);
    }

    fn clear(&mut self) {
        for (_, l) in std::mem::take(&mut self.links) {
            self.drop_link(l);
        }
        for (_, n) in std::mem::take(&mut self.nodes) {
            let _ = self.core.destroy_object(n);
        }
    }

    fn reconcile(&mut self, plan: &PwPlan, filter: &Filter) {
        let own = filter.node_id();
        // bus nodes
        for n in &plan.nodes {
            if self.nodes.contains_key(&n.node) {
                continue;
            }
            let p = pw::properties::properties! {
                "factory.name" => "support.null-audio-sink",
                "node.name" => n.node.as_str(),
                "node.description" => n.description.as_str(),
                "media.class" => "Audio/Source/Virtual",
                "audio.position" => "FL,FR",
                "audio.channels" => "2",
                "node.virtual" => "true",
                "priority.session" => "0",
                "priority.driver" => "1",
                "object.linger" => "false",
                "session.suspend-timeout-seconds" => "0"
            };
            match self.core.create_object::<pw::node::Node>("adapter", &p) {
                Ok(node) => {
                    self.nodes.insert(n.node.clone(), node);
                }
                Err(e) => self.status.lock().error = Some(format!("create {}: {e}", n.node)),
            }
        }
        let keep: Vec<String> = plan.nodes.iter().map(|n| n.node.clone()).collect();
        let gone: Vec<String> = self.nodes.keys().filter(|k| !keep.contains(k)).cloned().collect();
        for k in gone {
            if let Some(n) = self.nodes.remove(&k) {
                let _ = self.core.destroy_object(n);
            }
        }

        // links
        let g = self.globals.borrow();
        let mut want: Vec<((u32, u32), (u32, u32))> = Vec::new();
        let mut states = Vec::new();
        for spec in &plan.links {
            let resolved = own.and_then(|own| {
                let mine = own_port(&g, own, &spec.port, spec.input)?;
                let (node, port) = match &spec.to {
                    LinkTarget::Source { target, channel } => {
                        let n = find_node(&g, target, "Audio/Source")?;
                        (n, node_port(&g, n, false, *channel)?)
                    }
                    LinkTarget::Sink { target, channel } => {
                        let n = find_node(&g, target, "Audio/Sink")?;
                        (n, node_port(&g, n, true, *channel)?)
                    }
                    LinkTarget::Bus { node, channel } => {
                        let n = g.nodes.iter().find(|(_, x)| &x.name == node).map(|(id, _)| *id)?;
                        (n, node_port(&g, n, true, *channel)?)
                    }
                };
                Some(if spec.input { ((port, mine), (node, own)) } else { ((mine, port), (own, node)) })
            });
            states.push((spec.clone(), resolved.is_some()));
            if let Some(w) = resolved {
                want.push(w);
            }
        }
        let existing: Vec<(u32, u32)> = g.links.values().map(|(o, i, _, _)| (*o, *i)).collect();
        // consumers of our bus nodes (e.g. OBS)
        let mut consumers: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for n in &plan.nodes {
            if let Some((bid, _)) = g.nodes.iter().find(|(_, x)| x.name == n.node) {
                let mut c: Vec<String> = g
                    .links
                    .values()
                    .filter(|(_, _, on, inn)| on == bid && Some(*inn) != own)
                    .filter_map(|(_, _, _, inn)| g.nodes.get(inn).map(|x| x.name.clone()))
                    .collect();
                c.sort();
                c.dedup();
                consumers.insert(n.node.clone(), c);
            }
        }
        drop(g);
        let want_keys: Vec<(u32, u32)> = want.iter().map(|(k, _)| *k).collect();
        let stale: Vec<(u32, u32)> = self.links.keys().filter(|k| !want_keys.contains(k)).copied().collect();
        for k in stale {
            if let Some(l) = self.links.remove(&k) {
                self.drop_link(l);
            }
        }
        for ((op, ip), (on, inn)) in want {
            if self.links.contains_key(&(op, ip)) || existing.contains(&(op, ip)) {
                continue;
            }
            let p = pw::properties::properties! {
                "link.output.port" => op.to_string(),
                "link.input.port" => ip.to_string(),
                "link.output.node" => on.to_string(),
                "link.input.node" => inn.to_string(),
                "object.linger" => "false"
            };
            match self.core.create_object::<pw::link::Link>("link-factory", &p) {
                Ok(l) => {
                    self.links.insert((op, ip), l);
                }
                Err(e) => self.status.lock().error = Some(format!("link {op}->{ip}: {e}")),
            }
        }
        let mut s = self.status.lock();
        s.links = states;
        s.consumers = consumers;
        s.node_id = own;
        let g = self.globals.borrow();
        let mut devs: Vec<DeviceInfo> = g
            .nodes
            .iter()
            .filter(|(_, n)| n.class == "Audio/Source" || n.class == "Audio/Sink")
            .map(|(id, n)| DeviceInfo {
                id: *id,
                name: n.name.clone(),
                description: n.description.clone(),
                class: n.class.clone(),
                ports: g.ports.values().filter(|p| p.node == *id && !p.monitor && p.input == (n.class == "Audio/Sink")).count(),
            })
            .collect();
        devs.sort_by(|a, b| a.name.cmp(&b.name));
        s.devices = devs;
    }
}

enum End {
    Quit,
    Lost(String),
}

fn run(
    engine: Box<Engine>,
    ports: Arc<PortSlots>,
    stats: Arc<RtStats>,
    rx: pw::channel::Receiver<PwCmd>,
    status: Arc<Mutex<PwStatus>>,
    quit: Arc<AtomicBool>,
    plan: PwPlan,
) {
    pw::init();
    let mut rt = Box::new(RtCtx {
        engine,
        ports,
        stats,
        ins: vec![std::ptr::null_mut(); MAX_PORTS],
        outs: vec![std::ptr::null_mut(); MAX_PORTS],
        last_pos: 0,
        last_dur: 0,
        last_xrun: 0,
        last_clock: 0,
        last_cycle: 0,
        warm: 16,
        sched_check: 0,
        tried_fifo: false,
    });
    let plan = Rc::new(RefCell::new(plan));
    let mut rx = Some(rx);
    loop {
        let (end, r) = session(&mut rt, rx.take().expect("receiver"), &status, &quit, &plan);
        rx = Some(r);
        rt.ports.clear();
        match end {
            End::Quit => break,
            End::Lost(e) => {
                {
                    let mut s = status.lock();
                    s.connected = false;
                    s.error = Some(e.clone());
                    s.reconnects += 1;
                }
                tracing::warn!(target: "audio", "PipeWire connection lost ({e}); reconnecting");
                for _ in 0..10 {
                    if quit.load(Ordering::Acquire) {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(100));
                }
            }
        }
        if quit.load(Ordering::Acquire) {
            break;
        }
    }
}

fn connect(context: &pw::context::ContextRc) -> Result<pw::core::CoreRc, pw::Error> {
    match context.connect_rc(None) {
        Ok(c) => Ok(c),
        Err(e) => {
            // Without XDG_RUNTIME_DIR (e.g. started from a bare shell) try the default
            // per-user socket path.
            if std::env::var_os("XDG_RUNTIME_DIR").is_none() && std::env::var_os("PIPEWIRE_RUNTIME_DIR").is_none() {
                // SAFETY: getuid has no failure modes.
                let path = format!("/run/user/{}/pipewire-0", unsafe { libc::getuid() });
                context.connect_rc(Some(pw::properties::properties! { "remote.name" => path.as_str() }))
            } else {
                Err(e)
            }
        }
    }
}

fn session(
    rt: &mut Box<RtCtx>,
    rx: pw::channel::Receiver<PwCmd>,
    status: &Arc<Mutex<PwStatus>>,
    quit: &Arc<AtomicBool>,
    plan: &Rc<RefCell<PwPlan>>,
) -> (End, pw::channel::Receiver<PwCmd>) {
    let mainloop = match pw::main_loop::MainLoopRc::new(None) {
        Ok(m) => m,
        Err(e) => return (End::Lost(format!("main loop: {e}")), rx),
    };
    let context = match pw::context::ContextRc::new(&mainloop, None) {
        Ok(c) => c,
        Err(e) => return (End::Lost(format!("context: {e}")), rx),
    };
    let core = match connect(&context) {
        Ok(c) => c,
        Err(e) => return (End::Lost(format!("connect: {e}")), rx),
    };
    let lost: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let _core_listener = {
        let (lost, ml) = (lost.clone(), mainloop.clone());
        core.add_listener_local()
            .error(move |id, _seq, res, msg| {
                if id == pw::core::PW_ID_CORE {
                    *lost.borrow_mut() = Some(format!("{msg} ({res})"));
                    ml.quit();
                }
            })
            .register()
    };
    let registry = match core.get_registry_rc() {
        Ok(r) => r,
        Err(e) => return (End::Lost(format!("registry: {e}")), rx),
    };
    let globals = Rc::new(RefCell::new(Globals::default()));
    let _reg_listener = {
        let (g1, g2) = (globals.clone(), globals.clone());
        registry
            .add_listener_local()
            .global(move |obj| {
                let Some(p) = obj.props else { return };
                let mut g = g1.borrow_mut();
                match obj.type_ {
                    pw::types::ObjectType::Node => {
                        g.nodes.insert(
                            obj.id,
                            NodeG {
                                name: p.get("node.name").unwrap_or("").to_string(),
                                description: p.get("node.description").or(p.get("node.nick")).unwrap_or("").to_string(),
                                class: p.get("media.class").unwrap_or("").to_string(),
                            },
                        );
                    }
                    pw::types::ObjectType::Port => {
                        let node = p.get("node.id").and_then(|s| s.parse().ok()).unwrap_or(u32::MAX);
                        g.ports.insert(
                            obj.id,
                            PortG {
                                node,
                                input: p.get("port.direction") == Some("in"),
                                port_id: p.get("port.id").and_then(|s| s.parse().ok()).unwrap_or(0),
                                name: p.get("port.name").unwrap_or("").to_string(),
                                monitor: p.get("port.monitor") == Some("true"),
                            },
                        );
                    }
                    pw::types::ObjectType::Link => {
                        let num = |k: &str| p.get(k).and_then(|s| s.parse().ok()).unwrap_or(u32::MAX);
                        g.links.insert(obj.id, (num("link.output.port"), num("link.input.port"), num("link.output.node"), num("link.input.node")));
                    }
                    _ => return,
                }
                g.dirty = true;
            })
            .global_remove(move |id| {
                let mut g = g2.borrow_mut();
                g.nodes.remove(&id);
                g.ports.remove(&id);
                g.links.remove(&id);
                g.dirty = true;
            })
            .register()
    };

    // the engine filter node
    let cur = plan.borrow().clone();
    let fprops = props(&[
        ("media.type", "Audio".into()),
        ("media.category", "Filter".into()),
        ("media.role", "DSP".into()),
        ("node.name", "se-engine".into()),
        ("node.description", "stream-engine audio graph".into()),
        ("node.latency", format!("{}/{}", cur.quantum, cur.rate)),
        ("node.always-process", "true".into()),
        ("node.want-driver", "true".into()),
        ("node.pause-on-idle", "false".into()),
        // we make every link ourselves; the session manager must not auto-connect the filter
        ("node.autoconnect", "false".into()),
        ("node.dont-reconnect", "true".into()),
    ]);
    let events = Box::new(pw_sys::pw_filter_events {
        version: pw_sys::PW_VERSION_FILTER_EVENTS,
        destroy: None,
        state_changed: None,
        io_changed: None,
        param_changed: None,
        add_buffer: None,
        remove_buffer: None,
        process: Some(on_process),
        drained: None,
        command: None,
    });
    let sink = Box::new(StateSink { status: status.clone() });
    let name = cstr("se-engine");
    // SAFETY: loop, props, and events are valid; `rt` outlives the filter (dropped first).
    let raw = unsafe {
        pw_sys::pw_filter_new_simple(mainloop.loop_().as_raw_ptr(), name.as_ptr(), fprops, events.as_ref(), rt.as_mut() as *mut RtCtx as *mut c_void)
    };
    if raw.is_null() {
        return (End::Lost("pw_filter_new_simple failed".into()), rx);
    }
    // a second listener just for state (the simple constructor's data is the RT context)
    let mut hook: Box<spa_sys::spa_hook> = Box::new(unsafe { std::mem::zeroed() });
    let state_events = Box::new(pw_sys::pw_filter_events {
        version: pw_sys::PW_VERSION_FILTER_EVENTS,
        destroy: None,
        state_changed: Some(on_state),
        io_changed: None,
        param_changed: None,
        add_buffer: None,
        remove_buffer: None,
        process: None,
        drained: None,
        command: None,
    });
    // SAFETY: hook/events/sink are boxed and outlive the filter (kept in `Filter`).
    unsafe { pw_sys::pw_filter_add_listener(raw, hook.as_mut(), state_events.as_ref(), sink.as_ref() as *const StateSink as *mut c_void) };
    // the simple constructor's events struct must also outlive the filter
    let _events_keep = events;
    let mut filter = Filter { raw, _hook: hook, _events: state_events, _sink: sink, in_ports: Vec::new(), out_ports: Vec::new() };
    let ensure_ports = |filter: &mut Filter, plan: &PwPlan, slots: &PortSlots| {
        for (i, n) in plan.ins.iter().enumerate().skip(filter.in_ports.len()).take(MAX_PORTS.saturating_sub(filter.in_ports.len())) {
            let p = filter.add_port(n, true);
            slots.ins[i].store(p, Ordering::Release);
        }
        for (i, n) in plan.outs.iter().enumerate().skip(filter.out_ports.len()).take(MAX_PORTS.saturating_sub(filter.out_ports.len())) {
            let p = filter.add_port(n, false);
            slots.outs[i].store(p, Ordering::Release);
        }
    };
    ensure_ports(&mut filter, &cur, &rt.ports);
    // SAFETY: filter valid.
    let rc = unsafe { pw_sys::pw_filter_connect(filter.raw, pw_sys::pw_filter_flags_PW_FILTER_FLAG_RT_PROCESS, std::ptr::null_mut(), 0) };
    if rc < 0 {
        return (End::Lost(format!("pw_filter_connect: {rc}")), rx);
    }
    {
        let mut s = status.lock();
        s.connected = true;
        s.error = None;
    }

    let filter = Rc::new(RefCell::new(filter));
    let sess =
        Rc::new(RefCell::new(Session { core: core.clone(), globals: globals.clone(), links: HashMap::new(), nodes: HashMap::new(), status: status.clone() }));
    let quit_flag = Rc::new(RefCell::new(false));
    let slots = rt.ports.clone();
    let attached = {
        let (plan, filter, sess, ml, qf, g, slots) =
            (plan.clone(), filter.clone(), sess.clone(), mainloop.clone(), quit_flag.clone(), globals.clone(), slots.clone());
        rx.attach(mainloop.loop_(), move |cmd| match cmd {
            PwCmd::Plan(p) => {
                let old_q = (plan.borrow().quantum, plan.borrow().rate);
                *plan.borrow_mut() = *p;
                let pl = plan.borrow();
                ensure_ports(&mut filter.borrow_mut(), &pl, &slots);
                if (pl.quantum, pl.rate) != old_q {
                    filter.borrow().set_latency(pl.quantum, pl.rate);
                }
                g.borrow_mut().dirty = true;
                sess.borrow_mut().reconcile(&pl, &filter.borrow());
            }
            PwCmd::Quit => {
                *qf.borrow_mut() = true;
                ml.quit();
            }
        })
    };
    let timer = {
        let (plan, filter, sess, g, ml, q) = (plan.clone(), filter.clone(), sess.clone(), globals.clone(), mainloop.clone(), quit.clone());
        let last = RefCell::new(Instant::now());
        mainloop.loop_().add_timer(move |_| {
            if q.load(Ordering::Acquire) {
                ml.quit();
                return;
            }
            let dirty = std::mem::take(&mut g.borrow_mut().dirty);
            if dirty || last.borrow().elapsed() > Duration::from_secs(2) {
                *last.borrow_mut() = Instant::now();
                sess.borrow_mut().reconcile(&plan.borrow(), &filter.borrow());
            }
        })
    };
    timer.update_timer(Some(Duration::from_millis(50)), Some(Duration::from_millis(100)));
    mainloop.run();

    let end = if *quit_flag.borrow() || quit.load(Ordering::Acquire) {
        End::Quit
    } else {
        End::Lost(lost.borrow().clone().unwrap_or_else(|| "main loop stopped".into()))
    };
    drop(timer);
    let rx = attached.deattach();
    // tear down in order: links/nodes (proxies), then the filter (stops RT callbacks)
    sess.borrow_mut().clear();
    drop(sess);
    drop(filter);
    drop(_events_keep);
    status.lock().connected = false;
    (end, rx)
}
