//! DMX output: protocol encoders and the real-time output thread.
//!
//! One thread renders and sends every frame at the rig's rate (≤ 44 Hz) on absolute
//! CLOCK_MONOTONIC deadlines (SCHED_FIFO when permitted), reading the resolved state from the
//! hub snapshot without locks. Buffers are sized when the plan changes; steady-state frames do
//! not allocate. Stats (wake lateness, frame interval jitter, write time) are measured here.

pub mod artnet;
pub mod enttec;
pub mod rdm;
pub mod sacn;

use crate::engine::{Engine, Frame, Plan};
use crate::rig::{OutputKind, Rig};
use arc_swap::ArcSwap;
use parking_lot::Mutex;
use se_hub::Hub;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Frame timing statistics: the last second, plus cumulative percentiles since the output
/// started (10 µs histogram buckets; published once per second by the thread).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Stats {
    pub frames: u64,
    pub fps: f32,
    /// Wake-up lateness versus the deadline (last second).
    pub late_mean_us: f32,
    pub late_max_us: f32,
    /// |frame interval − period| (what fixtures see as timing jitter), last second.
    pub jitter_max_us: f32,
    /// Time spent writing to all outputs (last second).
    pub write_mean_us: f32,
    pub write_max_us: f32,
    /// Frame computation (wake → send) worst case since start.
    pub render_worst_us: f32,
    /// Deadlines missed by more than a period (frames skipped), cumulative.
    pub overruns: u64,
    /// Cumulative since start.
    pub samples: u64,
    pub jitter_p50_us: f32,
    pub jitter_p99_us: f32,
    pub jitter_p999_us: f32,
    pub jitter_worst_us: f32,
    pub late_p99_us: f32,
    pub late_worst_us: f32,
}

/// Latest rendered frame + stats for queries (triple-buffered, lock-free for the thread).
#[derive(Clone, Debug, Default)]
pub struct Monitor {
    pub frame: Frame,
    pub stats: Stats,
    pub beat: f64,
    pub seq: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct OutputStatus {
    pub id: String,
    pub kind: String,
    pub enabled: bool,
    /// ok | fail | off
    pub state: String,
    pub detail: String,
    pub frames: u64,
    pub errors: u64,
}

#[derive(Clone, Debug, Default)]
pub struct RdmReport {
    pub status: String,
    pub firmware: Option<String>,
    pub serial: Option<String>,
    pub devices: Vec<rdm::DeviceInfo>,
    pub at: i64,
    pub running: bool,
}

/// State shared between the control task and the output thread.
pub struct Shared {
    pub plan: ArcSwap<Plan>,
    pub monitor: Mutex<triple_buffer::Output<Monitor>>,
    pub status: Mutex<Vec<OutputStatus>>,
    pub rdm: Mutex<RdmReport>,
    pub rdm_request: AtomicBool,
    pub stop: AtomicBool,
    pub scheduling: Mutex<String>,
    pub frames: AtomicU64,
    /// Cleared when the output thread exits (normally or by panic).
    pub alive: AtomicBool,
    /// sACN component identifier (persistent per installation).
    pub cid: [u8; 16],
    /// Snapshots the output thread is done with, freed by the control task (a snapshot can be
    /// large; its last reference must not be dropped on the real-time thread).
    pub retired: Mutex<rtrb::Consumer<Arc<se_hub::Snapshot>>>,
}

fn mono_ns() -> u64 {
    se_clock::now()
}

fn sleep_until(deadline_ns: u64) {
    let ts = libc::timespec { tv_sec: (deadline_ns / 1_000_000_000) as libc::time_t, tv_nsec: (deadline_ns % 1_000_000_000) as libc::c_long };
    loop {
        // SAFETY: valid timespec; absolute CLOCK_MONOTONIC sleep, null remainder is allowed with TIMER_ABSTIME.
        let r = unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &ts, std::ptr::null_mut()) };
        if r != libc::EINTR {
            break;
        }
    }
}

/// Real-time scheduling for the calling thread: SCHED_FIFO at `prio`, else a negative nice.
fn realtime(prio: i32) -> String {
    // SAFETY: prctl/sched_setscheduler/setpriority on the calling thread with valid arguments.
    unsafe {
        libc::prctl(libc::PR_SET_TIMERSLACK, 1 as libc::c_ulong, 0, 0, 0);
        if prio > 0 {
            let p = libc::sched_param { sched_priority: prio };
            if libc::sched_setscheduler(0, libc::SCHED_FIFO, &p) == 0 {
                return format!("SCHED_FIFO {prio}");
            }
        }
        let err = std::io::Error::last_os_error();
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        if libc::setpriority(libc::PRIO_PROCESS, tid, -10) == 0 {
            return format!("SCHED_OTHER nice -10 (SCHED_FIFO refused: {err})");
        }
        format!("SCHED_OTHER nice 0 (SCHED_FIFO refused: {err})")
    }
}

enum SinkKind {
    Enttec {
        port: String,
        dev: Option<enttec::UsbPro>,
        frame: enttec::DmxFrame,
        universe: usize,
        next_retry: u64,
        rdm_src: Option<rdm::Uid>,
        widget_rate: Option<u8>,
    },
    Sacn {
        socket: Option<UdpSocket>,
        packets: Vec<(usize, sacn::SacnPacket, SocketAddr, u8)>,
    },
    ArtNet {
        socket: Option<UdpSocket>,
        packets: Vec<(usize, artnet::ArtDmx, SocketAddr, u8)>,
    },
}

struct Sink {
    status: usize,
    kind: SinkKind,
}

struct Outputs {
    sinks: Vec<Sink>,
    status: Vec<OutputStatus>,
    dirty: bool,
}

fn bcd(n: u32) -> u32 {
    let mut out = 0u32;
    let mut shift = 0;
    let mut n = n;
    while n > 0 && shift < 32 {
        out |= (n % 10) << shift;
        n /= 10;
        shift += 4;
    }
    out
}

impl Outputs {
    fn build(rig: &Rig, cid: [u8; 16]) -> Outputs {
        let mut sinks = Vec::new();
        let mut status = Vec::new();
        for o in &rig.outputs {
            let si = status.len();
            status.push(OutputStatus {
                id: o.id.clone(),
                kind: o.kind_name().into(),
                enabled: o.enabled,
                state: if o.enabled { "fail".into() } else { "off".into() },
                detail: if o.enabled { "not opened yet".into() } else { "disabled".into() },
                frames: 0,
                errors: 0,
            });
            if !o.enabled {
                continue;
            }
            let uidx = |u: u16| rig.universe_index(u).unwrap_or(0);
            let kind = match &o.kind {
                OutputKind::EnttecPro { port, universe, channels, widget_rate } => SinkKind::Enttec {
                    port: port.clone(),
                    dev: None,
                    frame: enttec::DmxFrame::new(*channels),
                    universe: uidx(*universe),
                    next_retry: 0,
                    rdm_src: None,
                    widget_rate: *widget_rate,
                },
                OutputKind::Sacn { universes, priority, destination, source_name, port } => {
                    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).and_then(|s| {
                        s.set_multicast_ttl_v4(8)?;
                        s.set_nonblocking(true)?;
                        Ok(s)
                    });
                    let packets = universes
                        .iter()
                        .map(|u| {
                            let ip = destination.unwrap_or(IpAddr::V4(sacn::multicast_addr(*u)));
                            (uidx(*u), sacn::SacnPacket::new(cid, source_name, *u, *priority), SocketAddr::new(ip, *port), 0u8)
                        })
                        .collect();
                    match socket {
                        Ok(s) => {
                            status[si].state = "ok".into();
                            status[si].detail =
                                format!("sACN universes {universes:?} → {}", destination.map(|d| d.to_string()).unwrap_or_else(|| "multicast".into()));
                            SinkKind::Sacn { socket: Some(s), packets }
                        }
                        Err(e) => {
                            status[si].detail = format!("socket: {e}");
                            SinkKind::Sacn { socket: None, packets }
                        }
                    }
                }
                OutputKind::ArtNet { destination, universes, net, subnet, port } => {
                    let socket = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)).and_then(|s| {
                        s.set_broadcast(true)?;
                        s.set_nonblocking(true)?;
                        Ok(s)
                    });
                    let packets = universes
                        .iter()
                        .map(|u| {
                            let pa = ((*net as u16) << 8) | ((*subnet as u16) << 4) | ((u - 1) & 0xF);
                            (uidx(*u), artnet::ArtDmx::new(pa), SocketAddr::new(*destination, *port), 1u8)
                        })
                        .collect();
                    match socket {
                        Ok(s) => {
                            status[si].state = "ok".into();
                            status[si].detail = format!("Art-Net universes {universes:?} → {destination}");
                            SinkKind::ArtNet { socket: Some(s), packets }
                        }
                        Err(e) => {
                            status[si].detail = format!("socket: {e}");
                            SinkKind::ArtNet { socket: None, packets }
                        }
                    }
                }
            };
            sinks.push(Sink { status: si, kind });
        }
        Outputs { sinks, status, dirty: true }
    }

    /// (Re)open USB devices whose retry time has come. Blocking (≤ ~1 s); returns true when
    /// it did I/O so the caller can exclude this frame from jitter stats.
    fn connect(&mut self, now: u64) -> bool {
        let mut did = false;
        for s in self.sinks.iter_mut() {
            let SinkKind::Enttec { port, dev, next_retry, rdm_src, widget_rate, .. } = &mut s.kind else { continue };
            if dev.is_some() || now < *next_retry {
                continue;
            }
            did = true;
            *next_retry = now + 2_000_000_000;
            let st = &mut self.status[s.status];
            let path = if port == "auto" { enttec::UsbPro::find_port() } else { Some(port.clone()) };
            let Some(path) = path else {
                st.state = "fail".into();
                st.detail = "no ENTTEC DMX USB PRO found in /dev/serial/by-id".into();
                self.dirty = true;
                continue;
            };
            match enttec::UsbPro::open(&path) {
                Ok(mut d) => {
                    let mut params = d.get_params();
                    if let (Some(want), Ok(p)) = (*widget_rate, &params)
                        && p.rate != want
                    {
                        params = d.set_rate(want);
                    }
                    let serial = d.get_serial().ok().flatten();
                    *rdm_src = serial.map(|n| rdm::Uid((0x454E_u64 << 32) | bcd(n) as u64));
                    st.state = "ok".into();
                    st.detail = match params {
                        Ok(p) => format!(
                            "{path} · firmware {} ({}) · break {:.0} µs · MAB {:.0} µs · widget rate {}{}",
                            p.version(),
                            p.kind(),
                            p.break_us,
                            p.mab_us,
                            if p.rate == 0 { "max".to_string() } else { format!("{}/s", p.rate) },
                            serial.map(|n| format!(" · serial {n}")).unwrap_or_default()
                        ),
                        Err(e) => format!("{path} · widget parameters unavailable: {e}"),
                    };
                    *dev = Some(d);
                }
                Err(e) => {
                    st.state = "fail".into();
                    st.detail = format!("{path}: {e}");
                }
            }
            self.dirty = true;
        }
        did
    }

    /// Send one frame everywhere. No allocation.
    fn send(&mut self, universes: &[[u8; 512]], now: u64) {
        for s in self.sinks.iter_mut() {
            let st = &mut self.status[s.status];
            match &mut s.kind {
                SinkKind::Enttec { dev, frame, universe, next_retry, .. } => {
                    let Some(d) = dev else { continue };
                    frame.set(&universes[*universe]);
                    match d.send_dmx(frame) {
                        Ok(()) => st.frames += 1,
                        Err(e) => {
                            st.errors += 1;
                            st.state = "fail".into();
                            st.detail = format!("write failed ({e}); reconnecting");
                            *dev = None;
                            *next_retry = now + 2_000_000_000;
                            self.dirty = true;
                        }
                    }
                }
                SinkKind::Sacn { socket: Some(sock), packets } => {
                    let mut ok = true;
                    for (u, p, dest, seq) in packets.iter_mut() {
                        *seq = seq.wrapping_add(1);
                        p.set(*seq, &universes[*u]);
                        if sock.send_to(p.bytes(), *dest).is_err() {
                            ok = false;
                        }
                    }
                    if ok {
                        st.frames += 1;
                    } else {
                        st.errors += 1;
                    }
                }
                SinkKind::ArtNet { socket: Some(sock), packets } => {
                    let mut ok = true;
                    for (u, p, dest, seq) in packets.iter_mut() {
                        *seq = if *seq == 255 { 1 } else { *seq + 1 };
                        p.set(*seq, 0, &universes[*u]);
                        if sock.send_to(p.bytes(), *dest).is_err() {
                            ok = false;
                        }
                    }
                    if ok {
                        st.frames += 1;
                    } else {
                        st.errors += 1;
                    }
                }
                _ => {}
            }
        }
    }

    /// sACN: announce stream termination (E1.31 §6.2.6: three packets).
    fn terminate(&mut self, universes: &[[u8; 512]]) {
        for s in self.sinks.iter_mut() {
            if let SinkKind::Sacn { socket: Some(sock), packets } = &mut s.kind {
                for _ in 0..3 {
                    for (u, p, dest, seq) in packets.iter_mut() {
                        *seq = seq.wrapping_add(1);
                        p.set_terminated(true);
                        p.set(*seq, &universes[*u]);
                        let _ = sock.send_to(p.bytes(), *dest);
                    }
                }
            }
        }
    }

    fn rdm(&mut self) -> RdmReport {
        let mut rep = RdmReport { at: (se_clock::wall_now_ns() / 1_000_000) as i64, ..Default::default() };
        let Some(s) = self.sinks.iter_mut().find(|s| matches!(s.kind, SinkKind::Enttec { .. })) else {
            rep.status = "no ENTTEC DMX USB PRO output configured".into();
            return rep;
        };
        let SinkKind::Enttec { dev, rdm_src, .. } = &mut s.kind else { unreachable!() };
        let Some(d) = dev else {
            rep.status = "ENTTEC DMX USB PRO not connected".into();
            return rep;
        };
        if let Some(p) = d.params() {
            rep.firmware = Some(format!("{} ({})", p.version(), p.kind()));
        }
        // the widget UID is ENTTEC's ESTA id + the BCD serial, so its hex digits are the serial
        rep.serial = rdm_src.map(|u| format!("{:08X} (RDM UID {u})", u.device()));
        let src = rdm_src.unwrap_or(rdm::Uid((0x454E_u64 << 32) | 1));
        match rdm::discover(d, src) {
            Ok(uids) => {
                for u in &uids {
                    match rdm::device_info(d, src, *u) {
                        Ok(Some(info)) => rep.devices.push(info),
                        Ok(None) | Err(_) => rep.devices.push(unknown_device(*u)),
                    }
                }
                rep.status = if uids.is_empty() {
                    "discovery finished: no RDM responders answered".into()
                } else {
                    format!("discovery finished: {} device(s)", uids.len())
                };
            }
            Err(e) => rep.status = format!("RDM unavailable: {e}"),
        }
        rep
    }
}

fn unknown_device(uid: rdm::Uid) -> rdm::DeviceInfo {
    rdm::DeviceInfo {
        uid,
        protocol: 0,
        model_id: 0,
        category: 0,
        software_version: 0,
        footprint: 0,
        personality: 0,
        personalities: 0,
        start_address: 0,
        sub_devices: 0,
        sensors: 0,
        manufacturer: None,
        model: None,
        label: Some("no DEVICE_INFO reply".into()),
    }
}

const BUCKETS: usize = 1024;
const BUCKET_US: u32 = 10;

/// Histogram with 10 µs buckets (last bucket = overflow); percentiles without allocation.
struct Hist {
    b: [u32; BUCKETS],
    n: u64,
    max: u32,
}

impl Hist {
    fn new() -> Hist {
        Hist { b: [0; BUCKETS], n: 0, max: 0 }
    }
    fn push(&mut self, us: u32) {
        self.b[((us / BUCKET_US) as usize).min(BUCKETS - 1)] += 1;
        self.n += 1;
        self.max = self.max.max(us);
    }
    /// Upper bound of the bucket holding quantile `q`.
    fn quantile(&self, q: f64) -> f32 {
        if self.n == 0 {
            return 0.0;
        }
        let target = ((self.n as f64) * q).ceil().max(1.0) as u64;
        let mut acc = 0u64;
        for (i, c) in self.b.iter().enumerate() {
            acc += *c as u64;
            if acc >= target {
                return (((i as u32 + 1) * BUCKET_US).min(self.max.max(BUCKET_US))) as f32;
            }
        }
        self.max as f32
    }
}

/// Per-second window + cumulative histograms.
struct Ring {
    n: u32,
    late_sum: u64,
    late_max: u32,
    jitter_max: u32,
    write_sum: u64,
    write_max: u32,
    render_worst: u32,
    overruns: u64,
    late: Hist,
    jitter: Hist,
}

impl Ring {
    fn new() -> Ring {
        Ring { n: 0, late_sum: 0, late_max: 0, jitter_max: 0, write_sum: 0, write_max: 0, render_worst: 0, overruns: 0, late: Hist::new(), jitter: Hist::new() }
    }
    fn push(&mut self, late: u32, jitter: u32, write: u32, render: u32) {
        self.render_worst = self.render_worst.max(render);
        self.n += 1;
        self.late_sum += late as u64;
        self.late_max = self.late_max.max(late);
        self.jitter_max = self.jitter_max.max(jitter);
        self.write_sum += write as u64;
        self.write_max = self.write_max.max(write);
        self.late.push(late);
        self.jitter.push(jitter);
    }
    /// Close the one-second window.
    fn stats(&mut self, frames: u64, secs: f64) -> Stats {
        let n = self.n.max(1) as f32;
        let s = Stats {
            frames,
            fps: (self.n as f64 / secs) as f32,
            late_mean_us: self.late_sum as f32 / n,
            late_max_us: self.late_max as f32,
            jitter_max_us: self.jitter_max as f32,
            write_mean_us: self.write_sum as f32 / n,
            write_max_us: self.write_max as f32,
            render_worst_us: self.render_worst as f32,
            overruns: self.overruns,
            samples: self.jitter.n,
            jitter_p50_us: self.jitter.quantile(0.5),
            jitter_p99_us: self.jitter.quantile(0.99),
            jitter_p999_us: self.jitter.quantile(0.999),
            jitter_worst_us: self.jitter.max as f32,
            late_p99_us: self.late.quantile(0.99),
            late_worst_us: self.late.max as f32,
        };
        self.n = 0;
        self.late_sum = 0;
        self.late_max = 0;
        self.jitter_max = 0;
        self.write_sum = 0;
        self.write_max = 0;
        s
    }
}

/// Start the output thread.
pub fn spawn(
    hub: Arc<Hub>,
    shared: Arc<Shared>,
    mut monitor_in: triple_buffer::Input<Monitor>,
    mut retire: rtrb::Producer<Arc<se_hub::Snapshot>>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name("se-dmx".into()).spawn(move || {
        struct Alive(Arc<Shared>);
        impl Drop for Alive {
            fn drop(&mut self) {
                self.0.alive.store(false, Ordering::Release);
            }
        }
        shared.alive.store(true, Ordering::Release);
        let _alive = Alive(shared.clone());
        let plan = shared.plan.load_full();
        *shared.scheduling.lock() = realtime(plan.rig.rt_priority);
        let mut engine = Engine::new(plan.clone());
        let mut outs = Outputs::build(&plan.rig, shared.cid);
        let mut period = (1e9 / plan.rig.rate_hz as f64) as u64;
        let mut ring = Ring::new();
        let mut deadline = mono_ns() + period;
        let mut last_send = 0u64;
        let mut window_start = mono_ns();
        let mut frames = 0u64;
        let mut seq = 0u64;
        let mut stats = Stats::default();
        let mut held: Option<Arc<se_hub::Snapshot>> = None;
        loop {
            sleep_until(deadline);
            let woke = mono_ns();
            if shared.stop.load(Ordering::Acquire) {
                outs.terminate(&engine.frame.universes);
                break;
            }
            let mut exclude = false;
            // config reload: new plan → new engine buffers and outputs (allocates, rare)
            let cur = shared.plan.load();
            if !Arc::ptr_eq(&cur, engine.plan()) {
                let p = cur.clone();
                drop(cur);
                let outputs_changed = p.rig.outputs != engine.plan().rig.outputs || p.rig.universes != engine.plan().rig.universes;
                engine.set_plan(p.clone());
                if outputs_changed {
                    outs.terminate(&engine.frame.universes);
                    outs = Outputs::build(&p.rig, shared.cid);
                }
                period = (1e9 / p.rig.rate_hz as f64) as u64;
                exclude = true;
            } else {
                drop(cur);
            }
            exclude |= outs.connect(woke);
            let snap = hub.snapshot.load_full();
            engine.render(&snap, woke);
            // hand the previous snapshot to the control task instead of possibly freeing it here
            if !held.as_ref().is_some_and(|h| Arc::ptr_eq(h, &snap)) {
                if let Some(old) = held.replace(snap)
                    && let Err(rtrb::PushError::Full(old)) = retire.push(old)
                {
                    drop(old);
                }
            } else {
                drop(snap);
            }
            let t0 = mono_ns();
            outs.send(&engine.frame.universes, t0);
            let t1 = mono_ns();
            frames += 1;
            shared.frames.store(frames, Ordering::Relaxed);
            if !exclude && last_send != 0 {
                let late = woke.saturating_sub(deadline) / 1000;
                let interval = t0.saturating_sub(last_send) as i64;
                let jitter = (interval - period as i64).unsigned_abs() / 1000;
                ring.push(late as u32, jitter as u32, ((t1 - t0) / 1000) as u32, (t0.saturating_sub(woke) / 1000) as u32);
            }
            last_send = if exclude { 0 } else { t0 };
            if woke.saturating_sub(window_start) >= 1_000_000_000 {
                stats = ring.stats(frames, woke.saturating_sub(window_start) as f64 / 1e9);
                window_start = woke;
            }
            if outs.dirty
                && let Some(mut st) = shared.status.try_lock()
            {
                st.clone_from(&outs.status);
                outs.dirty = false;
            } else if frames.is_multiple_of(44)
                && let Some(mut st) = shared.status.try_lock()
            {
                // frame/error counters (no allocation: same-length clone_from of plain data)
                for (d, s) in st.iter_mut().zip(&outs.status) {
                    d.frames = s.frames;
                    d.errors = s.errors;
                }
            }
            seq += 1;
            {
                let m = monitor_in.input_buffer_mut();
                m.frame.universes.clone_from(&engine.frame.universes);
                m.frame.heads.clone_from(&engine.frame.heads);
                m.frame.limited = engine.frame.limited;
                m.frame.suppressed = engine.frame.suppressed;
                m.frame.strobe_capped = engine.frame.strobe_capped;
                m.stats = stats;
                m.beat = engine.beat_position();
                m.seq = seq;
            }
            monitor_in.publish();
            if shared.rdm_request.swap(false, Ordering::AcqRel) {
                shared.rdm.lock().running = true;
                let rep = outs.rdm();
                *shared.rdm.lock() = rep;
                outs.dirty = true;
                exclude = true;
            }
            let now = mono_ns();
            if exclude {
                // after our own blocking work (device open, RDM, reload) restart the cadence from
                // now instead of catching up with back-to-back frames
                deadline = now + period;
                last_send = 0;
            } else {
                deadline += period;
                if now > deadline + period {
                    // missed a whole frame: skip it rather than bunching frames
                    ring.overruns += 1;
                    deadline = now + period;
                    last_send = 0;
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bcd_encoding_for_rdm_uid() {
        assert_eq!(bcd(405589), 0x0040_5589);
        assert_eq!(bcd(0), 0);
        assert_eq!(bcd(12345678), 0x1234_5678);
    }

    #[test]
    fn percentiles_window_and_cumulative() {
        let mut r = Ring::new();
        for i in 0..1000u32 {
            r.push(i, i, 1, 1);
        }
        let s = r.stats(1000, 1.0);
        assert_eq!(s.jitter_worst_us, 999.0);
        assert!((s.jitter_p50_us - 500.0).abs() <= 10.0, "{}", s.jitter_p50_us);
        assert!((s.jitter_p99_us - 990.0).abs() <= 10.0, "{}", s.jitter_p99_us);
        assert!((s.fps - 1000.0).abs() < 1e-3);
        r.push(5, 5, 1, 1);
        let s2 = r.stats(1001, 1.0);
        assert_eq!(s2.late_max_us, 5.0, "window resets");
        assert_eq!(s2.samples, 1001, "cumulative keeps counting");
        assert_eq!(s2.jitter_worst_us, 999.0);
    }
}
