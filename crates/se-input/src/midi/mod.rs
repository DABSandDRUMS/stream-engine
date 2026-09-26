//! MIDI over the ALSA sequencer: hotplug-aware port discovery, stable device identity
//! (USB port path / serial), per-device control decoding, coalesced signals
//! `midi.<device>.<control>`, events, mappings, MCU/HUI banks, feedback (LED rings, button
//! LEDs, motor faders with touch sensing, scribble strips, meters), learn, and a raw-byte
//! API for other subsystems (Timelines: MTC in/out).

pub mod controls;
pub mod mcu;
pub mod parse;
pub mod seqio;

use crate::Shared;
use crate::config::{Action, BankCfg, Controllers, MapDef, MidiDeviceCfg, Profile, glob, slug};
use crate::exec::{self};
use alsa::seq::{Addr, PortCap, PortSubscribe, PortType, Seq};
use arc_swap::ArcSwap;
use controls::{Decoder, Ev, Kind, Led, Out, RingMode};
use crossbeam_channel::{Receiver, Sender};
use parking_lot::Mutex;
use parse::{Msg, Parser};
use se_proto::{Event, Meta, Op, Origin, Ts, Value};
use serde::Serialize;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::ffi::CString;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------------------------
// Public raw API (process-global; live after `se_input::start`)
// ---------------------------------------------------------------------------------------------

/// A MIDI port another subsystem can listen to or send on.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct PortInfo {
    /// Stable device id: the `controllers/*.toml` id, else a slug of the ALSA client name.
    pub device: String,
    /// ALSA `client:port` name, e.g. `Studio 24c:Studio 24c MIDI 1`.
    pub name: String,
    pub client: i32,
    pub port: i32,
    pub input: bool,
    pub output: bool,
    pub usb_path: Option<String>,
    pub online: bool,
}

/// One complete incoming MIDI message, unfiltered.
#[derive(Clone, Debug)]
pub struct RawMidi {
    pub device: Arc<str>,
    /// Master-clock receive time (`se_clock::now()`).
    pub ts: Ts,
    pub bytes: Vec<u8>,
}

struct Registry {
    ports: ArcSwap<Vec<PortInfo>>,
    subs: Mutex<Vec<(String, Sender<RawMidi>)>>,
    has_subs: AtomicBool,
    out: Mutex<Option<OutSeq>>,
}

struct OutSeq {
    seq: Seq,
    client: i32,
    port: i32,
    /// Destinations our out port is subscribed to.
    connected: HashSet<Addr>,
}

static REG: LazyLock<Registry> = LazyLock::new(|| Registry {
    ports: ArcSwap::from_pointee(Vec::new()),
    subs: Mutex::new(Vec::new()),
    has_subs: AtomicBool::new(false),
    out: Mutex::new(None),
});

fn reg() -> &'static Registry {
    &REG
}

/// All known MIDI ports (online and configured-but-missing).
pub fn ports() -> Vec<PortInfo> {
    reg().ports.load().as_ref().clone()
}

/// Receive every incoming message of matching devices (glob over device id or ALSA name).
pub fn subscribe_raw(pattern: &str) -> Receiver<RawMidi> {
    let (tx, rx) = crossbeam_channel::unbounded();
    let r = reg();
    r.subs.lock().push((pattern.to_string(), tx));
    r.has_subs.store(true, Ordering::Release);
    rx
}

/// A sender for the first online output port matching `pattern`.
#[derive(Clone, Debug)]
pub struct MidiOut {
    pattern: String,
    device: String,
}

pub fn output(pattern: &str) -> Result<MidiOut, String> {
    let p = find_out(pattern)?;
    Ok(MidiOut { pattern: pattern.to_string(), device: p.device })
}

fn port_matches(pattern: &str, p: &PortInfo) -> bool {
    glob(pattern, &p.device) || glob(pattern, &p.name) || glob(pattern, p.name.split(':').next().unwrap_or(""))
}

fn find_out(pattern: &str) -> Result<PortInfo, String> {
    reg().ports.load().iter().find(|p| p.output && p.online && port_matches(pattern, p)).cloned().ok_or_else(|| format!("no MIDI output matches `{pattern}`"))
}

impl MidiOut {
    /// Send raw MIDI immediately (direct, unqueued). Any number of complete messages.
    pub fn send(&self, bytes: &[u8]) -> Result<(), String> {
        let p = find_out(&self.pattern)?;
        send_addr(Addr { client: p.client, port: p.port }, bytes)
    }
    pub fn device(&self) -> &str {
        &self.device
    }
}

/// Send raw bytes to a sequencer address.
pub(crate) fn send_addr(dest: Addr, bytes: &[u8]) -> Result<(), String> {
    let mut msgs = Vec::new();
    let mut p = Parser::default();
    p.feed(bytes, |m| msgs.push(m));
    let mut g = reg().out.lock();
    let o = g.as_mut().ok_or("MIDI output not running")?;
    // A kernel (rawmidi) port only opens its output while it has a subscriber, so connect our
    // out port to the destination once (a replugged device gets a new address → new connection).
    if !o.connected.contains(&dest) {
        let sub = PortSubscribe::empty().map_err(|e| e.to_string())?;
        sub.set_sender(Addr { client: o.client, port: o.port });
        sub.set_dest(dest);
        match o.seq.subscribe_port(&sub) {
            Ok(()) => {}
            // already connected (e.g. by aconnect)
            Err(e) if e.errno() == libc::EBUSY => {}
            Err(e) => return Err(format!("MIDI connect to {}:{}: {e}", dest.client, dest.port)),
        }
        o.connected.insert(dest);
    }
    for m in &msgs {
        let Some(mut ev) = seqio::msg_event(m) else { continue };
        ev.set_source(o.port);
        ev.set_dest(dest);
        ev.set_direct();
        if let Err(e) = o.seq.event_output_direct(&mut ev) {
            // the device went away (or was replugged at the same address): reconnect next time
            o.connected.remove(&dest);
            return Err(format!("MIDI send to {}:{}: {e}", dest.client, dest.port));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------
// Runtime
// ---------------------------------------------------------------------------------------------

pub enum MidiCmd {
    Config(Arc<Controllers>),
    /// Bank offset: `delta` strips, or absolute `set`.
    Bank {
        device: String,
        delta: Option<i64>,
        set: Option<u32>,
    },
    Send {
        device: String,
        bytes: Vec<u8>,
    },
    Record {
        device: String,
        seconds: f64,
        path: std::path::PathBuf,
    },
    /// Resend all feedback (e.g. after the operator re-powered a surface).
    Refresh,
    Stop,
}

/// Status of one device for queries/UI.
#[derive(Clone, Debug, Serialize, Default)]
pub struct DeviceStatus {
    pub id: String,
    pub configured: bool,
    pub file: String,
    pub online: bool,
    pub client: String,
    pub inputs: Vec<String>,
    pub output: Option<String>,
    pub usb: Option<String>,
    pub serial: Option<String>,
    pub profile: String,
    pub bank: u32,
    pub bank_count: u32,
    pub msgs_in: u64,
    pub msgs_out: u64,
    pub last_activity_ms: Option<u64>,
    pub controls: Vec<ControlStatus>,
    pub maps: Vec<MapStatus>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
pub struct ControlStatus {
    pub name: String,
    pub kind: String,
    pub value: f32,
    pub button: bool,
    pub relative: bool,
    pub feedback: bool,
    pub touched: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct MapStatus {
    pub control: String,
    pub action: String,
    pub target: Option<String>,
    pub feedback: bool,
}

/// One line of the MIDI monitor.
#[derive(Clone, Debug, Serialize)]
pub struct MonitorEntry {
    pub ts_ms: u64,
    pub device: String,
    pub dir: &'static str,
    pub hex: String,
    pub desc: String,
}

#[derive(Default)]
struct BtnState {
    confirm: Option<Instant>,
    cooldown: Option<(Instant, u64)>,
    held: Option<(Instant, bool)>,
    fired: bool,
}

struct Device {
    id: String,
    cfg: Option<Arc<MidiDeviceCfg>>,
    client: Option<i32>,
    client_name: String,
    inputs: Vec<(Addr, String)>,
    output: Option<(Addr, String)>,
    usb: Option<String>,
    serial: Option<String>,
    decoder: Decoder,
    parsers: HashMap<Addr, Parser>,
    pending: BTreeMap<usize, f32>,
    values: HashMap<usize, f32>,
    acc: HashMap<usize, f32>,
    shadow: HashMap<String, (f64, Instant)>,
    touched: HashSet<usize>,
    moved: HashMap<usize, Instant>,
    btn: HashMap<usize, BtnState>,
    fb: HashMap<Out, Vec<u8>>,
    fb_text: HashMap<(u8, u8), String>,
    fb_meter: HashMap<u8, (u8, Instant)>,
    bank: u32,
    msgs_in: u64,
    msgs_out: u64,
    last_activity: Option<Instant>,
    needs_init: bool,
    declared: bool,
    error: Option<String>,
}

impl Device {
    fn new(id: String, cfg: Option<Arc<MidiDeviceCfg>>) -> Device {
        let mut decoder = Decoder::new(cfg.as_ref().map(|c| c.controls.clone()).unwrap_or_default());
        decoder.hui = cfg.as_ref().is_some_and(|c| c.profile == Profile::Hui);
        decoder.auto = cfg.as_ref().is_none_or(|c| c.auto_controls);
        Device {
            id,
            cfg,
            client: None,
            client_name: String::new(),
            inputs: Vec::new(),
            output: None,
            usb: None,
            serial: None,
            decoder,
            parsers: HashMap::new(),
            pending: BTreeMap::new(),
            values: HashMap::new(),
            acc: HashMap::new(),
            shadow: HashMap::new(),
            touched: HashSet::new(),
            moved: HashMap::new(),
            btn: HashMap::new(),
            fb: HashMap::new(),
            fb_text: HashMap::new(),
            fb_meter: HashMap::new(),
            bank: 0,
            msgs_in: 0,
            msgs_out: 0,
            last_activity: None,
            needs_init: true,
            declared: false,
            error: None,
        }
    }

    fn online(&self) -> bool {
        self.client.is_some()
    }

    fn signal(&self, idx: usize) -> String {
        format!("midi.{}.{}", self.id, self.decoder.defs[idx].name)
    }

    fn profile(&self) -> Profile {
        self.cfg.as_ref().map(|c| c.profile).unwrap_or_default()
    }

    fn map_for(&self, control: &str) -> Option<&MapDef> {
        self.cfg.as_ref()?.maps.iter().find(|m| m.control == control)
    }
}

/// Which physical control plays a strip role on a profile (`vpot`, `fader`, `select`, …).
fn strip_control(profile: Profile, role: &str, strip: u32) -> Option<String> {
    let n = strip + 1;
    match profile {
        Profile::XTouchMiniMc => match role {
            "vpot" => Some(format!("enc.{n}")),
            "select" => Some(format!("push.{n}")),
            _ => None,
        },
        Profile::Mcu { .. } | Profile::Hui => match role {
            "vpot" | "fader" | "select" | "mute" | "solo" | "rec" => Some(format!("{role}.{n}")),
            _ => None,
        },
        Profile::Generic => None,
    }
}

/// Inverse of a controller binding's shaping: target value → control position (0–1).
pub fn binding_norm(d: &se_core::config::BindingDef, v: f64) -> f32 {
    let [lo, hi] = d.range.unwrap_or([0.0, 1.0]);
    let shaped = if (hi - lo).abs() < 1e-12 { 0.0 } else { ((v - lo) / (hi - lo)).clamp(0.0, 1.0) };
    // bisection over the monotonic curve
    let (mut a, mut b) = (0.0f64, 1.0f64);
    let f = |u: f64| se_core::bindings::apply_curve(d.curve, u);
    let increasing = f(1.0) >= f(0.0);
    for _ in 0..40 {
        let m = (a + b) / 2.0;
        if (f(m) < shaped) == increasing {
            a = m;
        } else {
            b = m;
        }
    }
    let u = (a + b) / 2.0;
    let g = if d.gain.abs() < 1e-12 { 1.0 } else { d.gain };
    let mut x = (u - d.offset) / g;
    if d.invert {
        x = 1.0 - x;
    }
    x.clamp(0.0, 1.0) as f32
}

pub struct MidiHandle {
    pub tx: Sender<MidiCmd>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl MidiHandle {
    pub fn stop(mut self) {
        let _ = self.tx.send(MidiCmd::Stop);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Open the sequencer and start the MIDI thread.
pub fn start(sh: Arc<Shared>, ctl: Arc<Controllers>) -> Result<MidiHandle, String> {
    let seq = Seq::open(None, None, true).map_err(|e| format!("ALSA sequencer: {e}"))?;
    seq.set_client_name(&CString::new("stream-engine").unwrap_or_default()).map_err(|e| e.to_string())?;
    let in_port = seq
        .create_simple_port(
            &CString::new("controllers in").unwrap_or_default(),
            PortCap::WRITE | PortCap::SUBS_WRITE,
            PortType::MIDI_GENERIC | PortType::APPLICATION,
        )
        .map_err(|e| format!("create input port: {e}"))?;
    let out = Seq::open(None, Some(alsa::Direction::Playback), false).map_err(|e| format!("ALSA sequencer (out): {e}"))?;
    out.set_client_name(&CString::new("stream-engine out").unwrap_or_default()).map_err(|e| e.to_string())?;
    let out_port = out
        .create_simple_port(
            &CString::new("controllers out").unwrap_or_default(),
            PortCap::READ | PortCap::SUBS_READ,
            PortType::MIDI_GENERIC | PortType::APPLICATION,
        )
        .map_err(|e| format!("create output port: {e}"))?;
    let own = vec![seq.client_id().map_err(|e| e.to_string())?, out.client_id().map_err(|e| e.to_string())?];
    *reg().out.lock() = Some(OutSeq { seq: out, client: own[1], port: out_port, connected: HashSet::new() });
    // hotplug announcements
    let me = Addr { client: own[0], port: in_port };
    let sub = PortSubscribe::empty().map_err(|e| e.to_string())?;
    sub.set_sender(Addr::system_announce());
    sub.set_dest(me);
    if let Err(e) = seq.subscribe_port(&sub) {
        sh.hub.log("warn", "midi", format!("no hotplug announcements ({e}); rescanning periodically"));
    }
    let (tx, rx) = crossbeam_channel::unbounded();
    let join = std::thread::Builder::new()
        .name("se-midi".into())
        .spawn(move || {
            let mut rt = Runtime::new(sh, seq, me, own, rx, ctl);
            rt.run();
        })
        .map_err(|e| e.to_string())?;
    Ok(MidiHandle { tx, join: Some(join) })
}

struct Recording {
    device: String,
    until: Instant,
    start: Instant,
    out: std::io::BufWriter<std::fs::File>,
    path: std::path::PathBuf,
}

struct Runtime {
    sh: Arc<Shared>,
    seq: Seq,
    me: Addr,
    own: Vec<i32>,
    rx: Receiver<MidiCmd>,
    ctl: Arc<Controllers>,
    devices: BTreeMap<String, Device>,
    /// client id → device id
    by_client: HashMap<i32, String>,
    subscribed: HashSet<Addr>,
    rescan_at: Option<Instant>,
    last_scan: Instant,
    last_fb: Instant,
    last_status: Instant,
    last_ping: Instant,
    evs: Vec<Ev>,
    recording: Option<Recording>,
}

impl Runtime {
    fn new(sh: Arc<Shared>, seq: Seq, me: Addr, own: Vec<i32>, rx: Receiver<MidiCmd>, ctl: Arc<Controllers>) -> Runtime {
        Runtime {
            sh,
            seq,
            me,
            own,
            rx,
            ctl,
            devices: BTreeMap::new(),
            by_client: HashMap::new(),
            subscribed: HashSet::new(),
            rescan_at: Some(Instant::now()),
            last_scan: Instant::now(),
            last_fb: Instant::now(),
            last_status: Instant::now() - Duration::from_secs(1),
            last_ping: Instant::now(),
            evs: Vec::new(),
            recording: None,
        }
    }

    fn run(&mut self) {
        use alsa::poll::Descriptors;
        loop {
            loop {
                match self.rx.try_recv() {
                    Ok(MidiCmd::Stop) | Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        self.shutdown();
                        return;
                    }
                    Ok(c) => self.command(c),
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                }
            }
            if self.rescan_at.is_some_and(|t| Instant::now() >= t) || self.last_scan.elapsed() > Duration::from_secs(5) {
                self.rescan_at = None;
                self.rescan();
            }
            // wait for input (short timeout keeps flush/feedback/commands responsive)
            let mut fds = match (&self.seq, Some(alsa::Direction::Capture)).get() {
                Ok(f) => f,
                Err(e) => {
                    self.sh.hub.log("error", "midi", format!("sequencer poll: {e}"));
                    std::thread::sleep(Duration::from_millis(100));
                    continue;
                }
            };
            let timeout = if self.devices.values().any(|d| !d.pending.is_empty()) { 2 } else { 10 };
            let _ = alsa::poll::poll(&mut fds, timeout);
            self.read_events();
            self.flush_signals(false);
            self.tick_buttons();
            if self.last_fb.elapsed() >= Duration::from_millis(20) {
                self.last_fb = Instant::now();
                self.feedback();
            }
            if self.last_ping.elapsed() >= Duration::from_millis(900) {
                self.last_ping = Instant::now();
                self.hui_ping();
            }
            if self.last_status.elapsed() >= Duration::from_millis(250) {
                self.last_status = Instant::now();
                self.publish_status();
            }
            if let Some(r) = &self.recording
                && Instant::now() >= r.until
            {
                self.finish_recording();
            }
        }
    }

    fn shutdown(&mut self) {
        // leave LEDs dark
        for d in self.devices.values_mut() {
            let Some((addr, _)) = d.output.clone() else { continue };
            let mut buf = Vec::new();
            for out in d.fb.keys() {
                out.encode(0.0, Led::Off, RingMode::Fill, &mut buf);
            }
            let _ = send_addr(addr, &buf);
        }
        *reg().out.lock() = None;
    }

    fn command(&mut self, c: MidiCmd) {
        match c {
            MidiCmd::Config(ctl) => {
                self.ctl = ctl;
                self.rescan_at = Some(Instant::now());
            }
            MidiCmd::Bank { device, delta, set } => {
                let Some(d) = self.devices.get_mut(&device) else {
                    self.sh.hub.log("warn", "midi", format!("midi.bank: unknown device `{device}`"));
                    return;
                };
                let (size, count) = d.cfg.as_ref().and_then(|c| c.bank.as_ref()).map(|b| (b.size, b.count)).unwrap_or((8, 8));
                let max = count.saturating_sub(size);
                let next = match (set, delta) {
                    (Some(s), _) => s.min(max),
                    (None, Some(dl)) => (d.bank as i64 + dl).clamp(0, max as i64) as u32,
                    _ => d.bank,
                };
                if next != d.bank {
                    d.bank = next;
                    d.fb_text.clear();
                    self.sh.hub.publish(&format!("controllers.midi.{device}.bank"), Value::Int(next as i64));
                }
            }
            MidiCmd::Send { device, bytes } => {
                let Some(d) = self.devices.get_mut(&device) else {
                    self.sh.hub.log("warn", "midi", format!("midi.send: unknown device `{device}`"));
                    return;
                };
                match d.output.clone() {
                    Some((addr, _)) => match send_addr(addr, &bytes) {
                        Ok(()) => {
                            d.msgs_out += 1;
                            monitor(&self.sh, &device, "out", &bytes);
                        }
                        Err(e) => self.sh.hub.log("error", "midi", e),
                    },
                    None => self.sh.hub.log("warn", "midi", format!("midi.send: `{device}` has no output")),
                }
            }
            MidiCmd::Record { device, seconds, path } => {
                if let Some(dir) = path.parent() {
                    let _ = std::fs::create_dir_all(dir);
                }
                match std::fs::File::create(&path) {
                    Ok(f) => {
                        use std::io::Write;
                        let mut out = std::io::BufWriter::new(f);
                        let _ = writeln!(out, "# stream-engine MIDI fixture: device `{device}`, recorded {}", se_clock::wall_now_ns() / 1_000_000_000);
                        let _ = writeln!(out, "# <ms since start> <hex bytes>");
                        self.sh.hub.log("info", "midi", format!("recording `{device}` for {seconds:.0}s → {}", path.display()));
                        self.recording = Some(Recording {
                            device,
                            until: Instant::now() + Duration::from_secs_f64(seconds.clamp(0.5, 600.0)),
                            start: Instant::now(),
                            out,
                            path,
                        });
                    }
                    Err(e) => self.sh.hub.log("error", "midi", format!("record {}: {e}", path.display())),
                }
            }
            MidiCmd::Refresh => {
                for d in self.devices.values_mut() {
                    d.fb.clear();
                    d.fb_text.clear();
                    d.needs_init = true;
                }
            }
            MidiCmd::Stop => {}
        }
    }

    fn finish_recording(&mut self) {
        if let Some(mut r) = self.recording.take() {
            use std::io::Write;
            let _ = r.out.flush();
            self.sh.hub.log("info", "midi", format!("fixture written: {}", r.path.display()));
            self.sh.hub.emit(Event::new("midi.recorded", Origin::Midi, Value::map().with("device", r.device).with("file", r.path.display().to_string())));
        }
    }

    // ---- discovery ----------------------------------------------------------------------

    fn rescan(&mut self) {
        self.last_scan = Instant::now();
        let ports = seqio::list_ports(&self.seq, &self.own);
        let cards = seqio::card_ids();
        // group by client
        let mut clients: BTreeMap<i32, Vec<seqio::SeqPort>> = BTreeMap::new();
        for p in ports {
            clients.entry(p.addr.client).or_default().push(p);
        }
        // assign clients to configured devices (explicit identity first), the rest auto
        let mut assign: BTreeMap<String, (i32, Option<Arc<MidiDeviceCfg>>)> = BTreeMap::new();
        let mut taken: HashSet<i32> = HashSet::new();
        let mut cfgs: Vec<Arc<MidiDeviceCfg>> = self.ctl.midi.iter().cloned().map(Arc::new).collect();
        cfgs.sort_by_key(|c| (c.usb.is_none() && c.serial.is_none(), c.id.clone()));
        for c in &cfgs {
            let mut matching: Vec<i32> = clients
                .iter()
                .filter(|(id, ps)| {
                    let name = &ps[0].client_name;
                    let card = ps[0].card.and_then(|k| cards.get(&k));
                    !taken.contains(id)
                        && (glob(&c.matcher, name) || ps.iter().any(|p| glob(&c.matcher, &format!("{}:{}", p.client_name, p.port_name))))
                        && c.usb.as_ref().is_none_or(|u| card.and_then(|k| k.usb.as_deref()) == Some(u.as_str()))
                        && c.serial.as_ref().is_none_or(|s| card.and_then(|k| k.serial.as_deref()) == Some(s.as_str()))
                })
                .map(|(id, _)| *id)
                .collect();
            matching.sort();
            if let Some(first) = matching.first() {
                if matching.len() > 1 && c.usb.is_none() && c.serial.is_none() {
                    self.sh.hub.log(
                        "warn",
                        "midi",
                        format!(
                            "`{}` matches {} identical devices; add `usb = \"…\"` to {} to pick one (see controllers.midi.ports)",
                            c.id,
                            matching.len(),
                            c.file
                        ),
                    );
                }
                taken.insert(*first);
                assign.insert(c.id.clone(), (*first, Some(c.clone())));
            }
        }
        for (id, ps) in &clients {
            if taken.contains(id) || ps[0].card.is_none() {
                continue; // software clients only when configured
            }
            let base = slug(&ps[0].client_name);
            let mut name = base.clone();
            let mut n = 2;
            while assign.contains_key(&name) || cfgs.iter().any(|c| c.id == name) {
                name = format!("{base}_{n}");
                n += 1;
            }
            assign.insert(name, (*id, None));
        }
        // configured but absent devices stay listed (offline)
        let mut next: BTreeMap<String, Device> = BTreeMap::new();
        for c in &cfgs {
            if !assign.contains_key(&c.id) {
                let mut d = self.devices.remove(&c.id).unwrap_or_else(|| Device::new(c.id.clone(), Some(c.clone())));
                if d.cfg.as_deref() != Some(c) {
                    d = Device::new(c.id.clone(), Some(c.clone()));
                }
                if d.client.is_some() {
                    self.sh.hub.log("warn", "midi", format!("`{}` disconnected", c.id));
                    self.sh.hub.emit(Event::new("midi.disconnected", Origin::Midi, Value::map().with("device", c.id.clone())));
                }
                d.client = None;
                d.inputs.clear();
                d.output = None;
                d.error = Some(format!("no MIDI device matches `{}`", c.matcher));
                next.insert(c.id.clone(), d);
            }
        }
        self.by_client.clear();
        let mut want_subs: HashSet<Addr> = HashSet::new();
        for (id, (client, cfg)) in assign {
            let ps = &clients[&client];
            let port_ok = |p: &seqio::SeqPort| cfg.as_ref().and_then(|c| c.port.as_ref()).is_none_or(|g| glob(g, &p.port_name));
            let inputs: Vec<(Addr, String)> =
                ps.iter().filter(|p| p.readable && port_ok(p)).map(|p| (p.addr, format!("{}:{}", p.client_name, p.port_name))).collect();
            let output = ps.iter().find(|p| p.writable && port_ok(p)).map(|p| (p.addr, format!("{}:{}", p.client_name, p.port_name)));
            let card = ps[0].card.and_then(|k| cards.get(&k)).cloned().unwrap_or_default();
            let mut d = match self.devices.remove(&id) {
                Some(d) if d.cfg == cfg => d,
                _ => Device::new(id.clone(), cfg.clone()),
            };
            let was_online = d.client == Some(client);
            d.client = Some(client);
            d.client_name = ps[0].client_name.clone();
            d.usb = card.usb;
            d.serial = card.serial;
            d.error = None;
            if !was_online {
                d.needs_init = true;
                d.fb.clear();
                d.fb_text.clear();
                d.parsers.clear();
                self.sh.hub.log("info", "midi", format!("`{id}` ← {} ({})", d.client_name, d.usb.as_deref().unwrap_or("no usb path")));
                self.sh.hub.emit(Event::new(
                    "midi.connected",
                    Origin::Midi,
                    Value::map().with("device", id.clone()).with("name", d.client_name.clone()).with("usb", d.usb.clone().map(Value::Str).unwrap_or_default()),
                ));
            }
            d.inputs = inputs;
            d.output = output;
            for (a, _) in &d.inputs {
                want_subs.insert(*a);
            }
            self.by_client.insert(client, id.clone());
            next.insert(id, d);
        }
        // departed auto devices
        for (id, d) in std::mem::take(&mut self.devices) {
            if d.online() && !next.contains_key(&id) {
                self.sh.hub.log("info", "midi", format!("`{id}` removed"));
                self.sh.hub.emit(Event::new("midi.disconnected", Origin::Midi, Value::map().with("device", id)));
            }
        }
        self.devices = next;
        // subscriptions
        for a in want_subs.difference(&self.subscribed.clone()) {
            let Ok(sub) = PortSubscribe::empty() else { continue };
            sub.set_sender(*a);
            sub.set_dest(self.me);
            match self.seq.subscribe_port(&sub) {
                Ok(()) => {
                    self.subscribed.insert(*a);
                }
                Err(e) => self.sh.hub.log("warn", "midi", format!("subscribe {}:{}: {e}", a.client, a.port)),
            }
        }
        let gone: Vec<Addr> = self.subscribed.difference(&want_subs).copied().collect();
        for a in gone {
            let _ = self.seq.unsubscribe_port(a, self.me);
            self.subscribed.remove(&a);
        }
        for d in self.devices.values_mut() {
            if !d.declared {
                d.declared = true;
                let h = &self.sh.hub;
                h.declare(
                    &format!("controllers.midi.{}.connected", d.id),
                    Meta::boolean(false).readonly().owner("controllers").describe("MIDI device connected"),
                );
                h.declare(
                    &format!("controllers.midi.{}.bank", d.id),
                    Meta::int(0, [0.0, 1024.0]).readonly().owner("controllers").describe("MCU/HUI bank offset"),
                );
            }
            self.sh.hub.publish(&format!("controllers.midi.{}.connected", d.id), Value::Bool(d.online()));
        }
        self.publish_ports();
        self.publish_status();
        self.sh.health_dirty();
    }

    fn publish_ports(&self) {
        let mut v = Vec::new();
        for d in self.devices.values() {
            let mut addrs: Vec<(Addr, String, bool, bool)> = d.inputs.iter().map(|(a, n)| (*a, n.clone(), true, false)).collect();
            if let Some((a, n)) = &d.output {
                match addrs.iter_mut().find(|x| x.0 == *a) {
                    Some(x) => x.3 = true,
                    None => addrs.push((*a, n.clone(), false, true)),
                }
            }
            for (a, n, i, o) in addrs {
                v.push(PortInfo {
                    device: d.id.clone(),
                    name: n,
                    client: a.client,
                    port: a.port,
                    input: i,
                    output: o,
                    usb_path: d.usb.clone(),
                    online: d.online(),
                });
            }
            if !d.online() {
                v.push(PortInfo {
                    device: d.id.clone(),
                    name: d.cfg.as_ref().map(|c| c.matcher.clone()).unwrap_or_default(),
                    client: -1,
                    port: -1,
                    input: false,
                    output: false,
                    usb_path: None,
                    online: false,
                });
            }
        }
        reg().ports.store(Arc::new(v));
    }

    fn publish_status(&self) {
        let now = Instant::now();
        let st: Vec<DeviceStatus> = self
            .devices
            .values()
            .map(|d| {
                let fb_ctl: HashSet<&str> =
                    d.cfg.as_ref().map(|c| c.maps.iter().filter(|m| m.feedback).map(|m| m.control.as_str()).collect()).unwrap_or_default();
                DeviceStatus {
                    id: d.id.clone(),
                    configured: d.cfg.is_some(),
                    file: d.cfg.as_ref().map(|c| c.file.clone()).unwrap_or_default(),
                    online: d.online(),
                    client: d.client_name.clone(),
                    inputs: d.inputs.iter().map(|(_, n)| n.clone()).collect(),
                    output: d.output.as_ref().map(|(_, n)| n.clone()),
                    usb: d.usb.clone(),
                    serial: d.serial.clone(),
                    profile: d.profile().name().into(),
                    bank: d.bank,
                    bank_count: d.cfg.as_ref().and_then(|c| c.bank.as_ref()).map(|b| b.count).unwrap_or(0),
                    msgs_in: d.msgs_in,
                    msgs_out: d.msgs_out,
                    last_activity_ms: d.last_activity.map(|t| now.duration_since(t).as_millis() as u64),
                    controls: d
                        .decoder
                        .defs
                        .iter()
                        .enumerate()
                        .map(|(i, c)| ControlStatus {
                            name: c.name.clone(),
                            kind: c.kind.describe(),
                            value: d.values.get(&i).or(d.acc.get(&i)).copied().unwrap_or(0.0),
                            button: c.is_button(),
                            relative: c.kind.is_relative(),
                            feedback: c.out.is_some() && (fb_ctl.contains(c.name.as_str()) || c.motor),
                            touched: d.touched.contains(&i),
                        })
                        .collect(),
                    maps: d
                        .cfg
                        .as_ref()
                        .map(|c| {
                            c.maps
                                .iter()
                                .map(|m| MapStatus {
                                    control: m.control.clone(),
                                    action: if m.target.is_some() {
                                        format!("adjust {}", m.target.as_deref().unwrap_or(""))
                                    } else if !m.inc.is_empty() || !m.dec.is_empty() {
                                        format!("+ {} / − {}", m.inc.join("; "), m.dec.join("; "))
                                    } else {
                                        m.action.describe()
                                    },
                                    target: m.target.clone(),
                                    feedback: m.feedback,
                                })
                                .collect()
                        })
                        .unwrap_or_default(),
                    error: d.error.clone(),
                }
            })
            .collect();
        *self.sh.midi_status.lock() = st;
    }

    // ---- input ------------------------------------------------------------------------

    fn read_events(&mut self) {
        let mut rescan = false;
        let mut incoming: Vec<(Addr, Vec<u8>)> = Vec::new();
        {
            let mut input = self.seq.input();
            loop {
                match input.event_input_pending(true) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let ev = match input.event_input() {
                    Ok(ev) => ev,
                    Err(_) => break,
                };
                use alsa::seq::EventType as T;
                match ev.get_type() {
                    T::ClientStart | T::ClientExit | T::PortStart | T::PortExit | T::PortChange | T::ClientChange => rescan = true,
                    T::PortSubscribed | T::PortUnsubscribed => {}
                    _ => {
                        let mut b = Vec::new();
                        seqio::event_bytes(&ev, &mut b);
                        if !b.is_empty() {
                            incoming.push((ev.get_source(), b));
                        }
                    }
                }
            }
        }
        if rescan {
            // ports appear before their info settles
            self.rescan_at = Some(Instant::now() + Duration::from_millis(250));
        }
        for (src, b) in incoming {
            self.input(src, &b);
        }
    }

    fn input(&mut self, src: Addr, bytes: &[u8]) {
        let Some(id) = self.by_client.get(&src.client).cloned() else { return };
        let ts = se_clock::now();
        let mut msgs = Vec::new();
        {
            let Some(d) = self.devices.get_mut(&id) else { return };
            let p = d.parsers.entry(src).or_default();
            p.feed(bytes, |m| msgs.push(m));
        }
        for m in msgs {
            self.message(&id, m, ts);
        }
    }

    fn message(&mut self, id: &str, m: Msg, ts: Ts) {
        let bytes = m.to_bytes();
        let r = reg();
        if r.has_subs.load(Ordering::Acquire) {
            let dev: Arc<str> = Arc::from(id);
            let mut subs = r.subs.lock();
            let name = self.devices.get(id).map(|d| d.client_name.clone()).unwrap_or_default();
            subs.retain(
                |(pat, tx)| {
                    if glob(pat, id) || glob(pat, &name) { tx.send(RawMidi { device: dev.clone(), ts, bytes: bytes.clone() }).is_ok() } else { true }
                },
            );
        }
        if let Some(rec) = self.recording.as_mut()
            && rec.device == id
        {
            use std::io::Write;
            let _ = writeln!(rec.out, "{} {}", rec.start.elapsed().as_millis(), parse::hex(&bytes));
        }
        if matches!(m, Msg::Realtime(_) | Msg::QuarterFrame(_)) {
            // clock/MTC: raw subscribers only (Timelines)
            if let Some(d) = self.devices.get_mut(id) {
                d.msgs_in += 1;
            }
            return;
        }
        monitor(&self.sh, id, "in", &bytes);
        let mut evs = std::mem::take(&mut self.evs);
        evs.clear();
        {
            let Some(d) = self.devices.get_mut(id) else { return };
            d.msgs_in += 1;
            d.last_activity = Some(Instant::now());
            d.decoder.decode(&m, &mut evs);
        }
        // drums (e-drum notes)
        let pad = match m {
            Msg::NoteOn { ch, note, vel } if vel > 0 => self
                .devices
                .get(id)
                .and_then(|d| d.cfg.as_ref())
                .filter(|cfg| cfg.drum_channel.is_none_or(|c| c == ch))
                .and_then(|cfg| cfg.drums.get(&note).cloned())
                .map(|p| (p, note, vel)),
            _ => None,
        };
        if let Some((pad, note, vel)) = pad {
            self.flush_device(id);
            self.sh.hub.emit(Event::new(
                format!("drums.{pad}"),
                Origin::Midi,
                Value::map().with("velocity", vel as f64 / 127.0).with("note", note as i64).with("device", id.to_string()),
            ));
        }
        for ev in evs.drain(..) {
            self.control_event(id, ev);
        }
        self.evs = evs;
    }

    fn control_event(&mut self, id: &str, ev: Ev) {
        let now = Instant::now();
        if self.sh.learning() {
            self.learn(id, &ev);
        }
        match ev {
            Ev::Value { idx, value, .. } => {
                let Some(d) = self.devices.get_mut(id) else { return };
                d.pending.insert(idx, value);
                d.values.insert(idx, value);
                d.moved.insert(idx, now);
                // MCU/HUI bank fader (motorized: jump semantics)
                let name = d.decoder.defs[idx].name.clone();
                if let Some((addr, lo, hi)) = self.bank_target(id, "fader", &name) {
                    let v = lo + value as f64 * (hi - lo);
                    self.sh.exec.op(Origin::Midi, Op::Set { address: addr, value: Value::Float(v) }, None);
                }
            }
            Ev::Delta { idx, steps } => {
                let (name, acc) = {
                    let Some(d) = self.devices.get_mut(id) else { return };
                    let a = d.acc.entry(idx).or_insert(0.5);
                    *a = (*a + steps as f32 * 0.01).clamp(0.0, 1.0);
                    let a = *a;
                    d.pending.insert(idx, a);
                    d.moved.insert(idx, now);
                    (d.decoder.defs[idx].name.clone(), a)
                };
                let _ = acc;
                let map = self.devices.get(id).and_then(|d| d.map_for(&name).cloned());
                if let Some(m) = map {
                    if let Some(t) = &m.target {
                        let (lo, hi) = self.range_of(t, m.range);
                        self.nudge(id, t, lo, hi, m.step, steps);
                    }
                    let cmds = if steps > 0 { &m.inc } else { &m.dec };
                    if !cmds.is_empty() {
                        for _ in 0..steps.unsigned_abs().min(8) {
                            self.sh.exec.run_list(Origin::Midi, cmds, None);
                        }
                    }
                } else if let Some((addr, lo, hi)) = self.bank_target(id, "vpot", &name) {
                    self.nudge(id, &addr, lo, hi, None, steps);
                }
            }
            Ev::Press { idx, velocity } => {
                self.flush_device(id);
                let (name, sig) = {
                    let Some(d) = self.devices.get_mut(id) else { return };
                    d.values.insert(idx, velocity);
                    d.pending.insert(idx, velocity);
                    (d.decoder.defs[idx].name.clone(), d.signal(idx))
                };
                self.flush_device(id);
                let e = Event::new(sig, Origin::Midi, Value::map().with("velocity", velocity as f64).with("down", true).with("device", id.to_string()));
                let causal = Some(e.id);
                self.sh.hub.emit(e);
                self.button(id, idx, &name, true, causal);
            }
            Ev::Release { idx } => {
                let (name, sig) = {
                    let Some(d) = self.devices.get_mut(id) else { return };
                    d.values.insert(idx, 0.0);
                    d.pending.insert(idx, 0.0);
                    (d.decoder.defs[idx].name.clone(), d.signal(idx))
                };
                self.flush_device(id);
                let e = Event::new(format!("{sig}.up"), Origin::Midi, Value::map().with("down", false).with("device", id.to_string()));
                let causal = Some(e.id);
                self.sh.hub.emit(e);
                self.button(id, idx, &name, false, causal);
            }
            Ev::Touch { idx, on } => {
                let Some(d) = self.devices.get_mut(id) else { return };
                if on {
                    d.touched.insert(idx);
                } else {
                    d.touched.remove(&idx);
                    d.moved.insert(idx, now);
                }
                let sig = d.signal(idx);
                self.sh.hub.signal(&format!("{sig}.touch"), if on { 1.0 } else { 0.0 });
                self.sh.hub.emit(Event::new(format!("{sig}.touch"), Origin::Midi, Value::map().with("on", on).with("device", id.to_string())));
            }
            Ev::Program { ch, program } => {
                self.flush_device(id);
                self.sh.hub.signal(&format!("midi.{id}.program"), program as f32 / 127.0);
                self.sh.hub.emit(Event::new(
                    format!("midi.{id}.program"),
                    Origin::Midi,
                    Value::map().with("program", program as i64).with("channel", ch as i64 + 1).with("device", id.to_string()),
                ));
            }
        }
    }

    /// Relative adjustment of an address (manual priority), tracking a local shadow so fast
    /// turns don't read stale state.
    fn nudge(&mut self, id: &str, target: &str, lo: f64, hi: f64, step: Option<f64>, steps: i32) {
        let snap = self.sh.hub.snapshot.load();
        let Some(d) = self.devices.get_mut(id) else { return };
        let step = step.unwrap_or((hi - lo) / 100.0);
        let cur = match d.shadow.get(target) {
            Some((v, t)) if t.elapsed() < Duration::from_millis(400) => *v,
            _ => snap.get(target).and_then(Value::as_f64).unwrap_or(lo),
        };
        let (a, b) = (lo.min(hi), lo.max(hi));
        let next = (cur + steps as f64 * step).clamp(a, b);
        d.shadow.insert(target.to_string(), (next, Instant::now()));
        let value = match snap.get(target) {
            Some(Value::Int(_)) => Value::Int(next.round() as i64),
            _ => Value::Float(next),
        };
        self.sh.exec.op(Origin::Midi, Op::Set { address: target.to_string(), value }, None);
    }

    fn range_of(&self, addr: &str, over: Option<[f64; 2]>) -> (f64, f64) {
        let r = over.or_else(|| self.sh.meta_range(addr)).unwrap_or([0.0, 1.0]);
        (r[0], r[1])
    }

    /// Bank strip target for a control playing `role` (`fader`, `vpot`, …).
    fn bank_target(&self, id: &str, role: &str, control: &str) -> Option<(String, f64, f64)> {
        let d = self.devices.get(id)?;
        let cfg = d.cfg.as_ref()?;
        let bank = cfg.bank.as_ref()?;
        let tpl = bank_role(bank, role)?;
        let strip = (0..bank.size).find(|s| strip_control(cfg.profile, role, *s).as_deref() == Some(control))?;
        let n = d.bank + strip + 1;
        if n > bank.count {
            return None;
        }
        let addr = tpl.replace("{n}", &n.to_string());
        let (lo, hi) = self.range_of(&addr, None);
        Some((addr, lo, hi))
    }

    fn button(&mut self, id: &str, idx: usize, name: &str, down: bool, causal: Option<se_proto::Id>) {
        let map = self.devices.get(id).and_then(|d| d.map_for(name).cloned());
        let profile = self.devices.get(id).map(|d| d.profile()).unwrap_or_default();
        let Some(m) = map else {
            if !down {
                return;
            }
            // MCU bank navigation and strip switches when unmapped
            let has_bank = self.devices.get(id).and_then(|d| d.cfg.as_ref()).and_then(|c| c.bank.as_ref()).map(|b| b.size);
            if let Some(size) = has_bank
                && profile.is_mcu()
            {
                let delta = match name {
                    "bank.left" => Some(-(size as i64)),
                    "bank.right" => Some(size as i64),
                    "channel.left" => Some(-1),
                    "channel.right" => Some(1),
                    _ => None,
                };
                if let Some(dl) = delta {
                    self.command(MidiCmd::Bank { device: id.to_string(), delta: Some(dl), set: None });
                    return;
                }
            }
            for role in ["select", "mute", "solo", "rec"] {
                if let Some((addr, _, _)) = self.bank_target(id, role, name) {
                    self.sh.exec.press(&Action::Toggle(addr), Origin::Midi, causal);
                    return;
                }
            }
            return;
        };
        let now = Instant::now();
        let cfg = self.sh.core_config();
        let needs_confirm = exec::needs_confirm(&m.action, &m.b, &cfg);
        let Some(d) = self.devices.get_mut(id) else { return };
        let st = d.btn.entry(idx).or_default();
        if down {
            if let Some((t, ms)) = st.cooldown
                && t.elapsed() < Duration::from_millis(ms)
            {
                return;
            }
            if m.b.hold_ms > 0 {
                st.held = Some((now, false));
                return;
            }
            if needs_confirm {
                match st.confirm {
                    Some(t) if t.elapsed() < Duration::from_secs(3) => st.confirm = None,
                    _ => {
                        st.confirm = Some(now);
                        self.sh.hub.log("info", "midi", format!("`{id}.{name}`: press again to confirm {}", m.action.describe()));
                        return;
                    }
                }
            }
            if m.b.cooldown_ms > 0 {
                st.cooldown = Some((now, m.b.cooldown_ms));
            }
            st.fired = true;
            if let Some(page) = self.sh.exec.press(&m.action, Origin::Midi, causal) {
                self.sh.exec.op(Origin::Midi, Op::Action { name: "deck.page".into(), args: Value::map().with("args", vec![Value::Str(page)]) }, causal);
            }
        } else {
            st.held = None;
            if std::mem::take(&mut st.fired) {
                self.sh.exec.release(&m.action, Origin::Midi, causal);
            }
        }
    }

    /// Hold-to-fire MIDI buttons.
    fn tick_buttons(&mut self) {
        let mut fire: Vec<(String, usize)> = Vec::new();
        for (id, d) in self.devices.iter_mut() {
            for (idx, st) in d.btn.iter_mut() {
                if let Some((t, false)) = st.held {
                    let name = &d.decoder.defs[*idx].name;
                    let hold = d.cfg.as_ref().and_then(|c| c.maps.iter().find(|m| &m.control == name)).map(|m| m.b.hold_ms).unwrap_or(0);
                    if hold > 0 && t.elapsed() >= Duration::from_millis(hold) {
                        st.held = Some((t, true));
                        st.fired = true;
                        fire.push((id.clone(), *idx));
                    }
                }
            }
        }
        for (id, idx) in fire {
            let Some(m) = self.devices.get(&id).and_then(|d| d.map_for(&d.decoder.defs[idx].name).cloned()) else { continue };
            self.sh.exec.press(&m.action, Origin::Midi, None);
        }
    }

    fn flush_device(&mut self, id: &str) {
        let Some(d) = self.devices.get_mut(id) else { return };
        if d.pending.is_empty() {
            return;
        }
        let v: Vec<(String, f32)> = std::mem::take(&mut d.pending).into_iter().map(|(i, x)| (d.signal(i), x)).collect();
        self.sh.hub.signals(v);
    }

    /// Coalesced signal flush: at most one batch per device per `coalesce_ms`.
    fn flush_signals(&mut self, force: bool) {
        let mut all: Vec<(String, f32)> = Vec::new();
        for d in self.devices.values_mut() {
            if d.pending.is_empty() {
                continue;
            }
            let every = d.cfg.as_ref().map(|c| c.coalesce_ms).unwrap_or(4);
            let due = d.moved.values().max().is_none_or(|t| t.elapsed() >= Duration::from_millis(every));
            if force || due || d.pending.len() > 32 {
                let p = std::mem::take(&mut d.pending);
                for (i, x) in p {
                    all.push((d.signal(i), x));
                }
            }
        }
        if !all.is_empty() {
            self.sh.hub.signals(all);
        }
    }

    // ---- learn ------------------------------------------------------------------------

    fn learn(&mut self, id: &str, ev: &Ev) {
        let Some(d) = self.devices.get(id) else { return };
        let (idx, kind) = match *ev {
            Ev::Value { idx, value, .. } => {
                // ignore jitter: needs a clear move away from where the control was first seen
                let switch_like = matches!(d.decoder.defs[idx].kind, Kind::Cc { .. }) && d.decoder.defs[idx].auto;
                if !self.sh.learn_moved(id, idx, value, switch_like) {
                    return;
                }
                (idx, crate::learn::Moved::Absolute)
            }
            Ev::Delta { idx, .. } => (idx, crate::learn::Moved::Relative),
            Ev::Press { idx, .. } => (idx, crate::learn::Moved::Button),
            _ => return,
        };
        let def = &d.decoder.defs[idx];
        let same_name = self.devices.values().filter(|x| x.client_name == d.client_name && x.online()).count() > 1;
        let src = crate::learn::Source {
            device: id.to_string(),
            control: def.name.clone(),
            control_def: def.clone(),
            file: d.cfg.as_ref().map(|c| c.file.clone()),
            client_name: d.client_name.clone(),
            usb: if same_name { d.usb.clone() } else { None },
            motor: def.motor,
            moved: kind,
        };
        self.sh.learn_complete(src);
    }

    // ---- feedback -----------------------------------------------------------------------

    fn hui_ping(&mut self) {
        for d in self.devices.values() {
            if d.profile() == Profile::Hui
                && let Some((addr, _)) = d.output
            {
                let _ = send_addr(addr, &mcu::HUI_PING);
            }
        }
    }

    fn feedback(&mut self) {
        let snap = self.sh.hub.snapshot.load();
        let core = self.sh.core_config();
        let page = snap.str("controllers.page").unwrap_or("").to_string();
        let ids: Vec<String> = self.devices.keys().cloned().collect();
        for id in ids {
            let mut want: Vec<(Out, f32, Led, RingMode)> = Vec::new();
            let mut texts: Vec<(u8, u8, String)> = Vec::new();
            let mut meters: Vec<(u8, f32)> = Vec::new();
            let Some(d) = self.devices.get(&id) else { continue };
            let Some((addr, _)) = d.output.clone() else { continue };
            let Some(cfg) = d.cfg.clone() else { continue };
            let now = Instant::now();
            let busy = |idx: usize| d.touched.contains(&idx) || d.moved.get(&idx).is_some_and(|t| now.duration_since(*t) < Duration::from_millis(300));
            // [[map]] feedback
            for m in cfg.maps.iter().filter(|m| m.feedback) {
                let Some(idx) = d.decoder.find(&m.control) else { continue };
                let def = &d.decoder.defs[idx];
                let Some(out) = def.out else { continue };
                if let Some(t) = &m.target {
                    let (lo, hi) = self.range_of(t, m.range);
                    let v = d
                        .shadow
                        .get(t)
                        .filter(|(_, at)| at.elapsed() < Duration::from_millis(400))
                        .map(|(v, _)| *v)
                        .or_else(|| snap.get(t).and_then(Value::as_f64))
                        .unwrap_or(lo);
                    let norm = if (hi - lo).abs() < 1e-12 { 0.0 } else { ((v - lo) / (hi - lo)) as f32 };
                    want.push((out, norm, Led::Off, m.ring));
                } else if m.action != Action::None || m.b.state.is_some() {
                    let st = exec::state(&m.action, &m.b, &snap, &page);
                    let pending_confirm = d.btn.get(&idx).and_then(|b| b.confirm).is_some_and(|t| t.elapsed() < Duration::from_secs(3));
                    let led = if pending_confirm || (st.preview && !st.program) {
                        Led::Blink
                    } else if st.on {
                        Led::On
                    } else {
                        Led::Off
                    };
                    want.push((out, if st.on { 1.0 } else { 0.0 }, led, m.ring));
                }
            }
            // core bindings with feedback = true on this device's signals
            let prefix = format!("midi.{id}.");
            for b in core.bindings.iter().filter(|b| b.feedback && b.enabled && b.signal.starts_with(&prefix)) {
                let control = &b.signal[prefix.len()..];
                let Some(idx) = d.decoder.find(control) else { continue };
                let def = &d.decoder.defs[idx];
                let Some(out) = def.out else { continue };
                if busy(idx) || (def.kind.is_absolute() && !def.motor && !matches!(out, Out::McuRing { .. } | Out::HuiRing { .. } | Out::BehringerRing { .. }))
                {
                    continue;
                }
                if b.target.contains('*') {
                    continue;
                }
                let Some(v) = snap.get(&b.target).and_then(Value::as_f64) else { continue };
                want.push((out, binding_norm(b, v), Led::Off, RingMode::Fill));
            }
            // unmapped relative encoders show their accumulator
            for (idx, def) in d.decoder.defs.iter().enumerate() {
                if let (Kind::Rel { .. }, Some(out)) = (def.kind, def.out)
                    && !cfg.maps.iter().any(|m| m.control == def.name)
                    && self.bank_target(&id, "vpot", &def.name).is_none()
                    && let Some(a) = d.acc.get(&idx)
                {
                    want.push((out, *a, Led::Off, RingMode::Fill));
                }
            }
            // bank strips
            if let Some(bank) = &cfg.bank {
                for s in 0..bank.size {
                    let n = d.bank + s + 1;
                    let in_range = n <= bank.count;
                    for role in ["fader", "vpot", "select", "mute", "solo", "rec"] {
                        let Some(ctl) = strip_control(cfg.profile, role, s) else { continue };
                        let Some(idx) = d.decoder.find(&ctl) else { continue };
                        let def = &d.decoder.defs[idx];
                        let Some(out) = def.out else { continue };
                        if cfg.maps.iter().any(|m| m.control == ctl) {
                            continue;
                        }
                        let Some(tpl) = bank_role(bank, role) else { continue };
                        if !in_range {
                            want.push((out, 0.0, Led::Off, bank.ring));
                            continue;
                        }
                        let a = tpl.replace("{n}", &n.to_string());
                        match role {
                            "fader" => {
                                if def.motor && !busy(idx) {
                                    let (lo, hi) = self.range_of(&a, None);
                                    let v = snap.get(&a).and_then(Value::as_f64).unwrap_or(lo);
                                    want.push((out, ((v - lo) / (hi - lo).max(1e-12)) as f32, Led::Off, bank.ring));
                                }
                            }
                            "vpot" => {
                                let (lo, hi) = self.range_of(&a, None);
                                let v = d
                                    .shadow
                                    .get(&a)
                                    .filter(|(_, at)| at.elapsed() < Duration::from_millis(400))
                                    .map(|(v, _)| *v)
                                    .or_else(|| snap.get(&a).and_then(Value::as_f64))
                                    .unwrap_or(lo);
                                want.push((out, ((v - lo) / (hi - lo).max(1e-12)) as f32, Led::Off, bank.ring));
                            }
                            _ => {
                                let on = snap.bool(&a);
                                want.push((out, on as u8 as f32, if on { Led::On } else { Led::Off }, bank.ring));
                            }
                        }
                    }
                    if cfg.profile.is_mcu() || cfg.profile == Profile::Hui {
                        let text = if !in_range {
                            String::new()
                        } else {
                            bank.name
                                .as_ref()
                                .map(|t| t.replace("{n}", &n.to_string()))
                                .and_then(|a| snap.str(&a).map(String::from))
                                .unwrap_or_else(|| format!("CH {n}"))
                        };
                        texts.push((s as u8, 0, text));
                        if let Some(mt) = &bank.meter
                            && in_range
                        {
                            let sig = mt.replace("{n}", &n.to_string());
                            meters.push((s as u8, snap.signal(&sig).unwrap_or(0.0)));
                        }
                    }
                }
            }
            // send changes
            let profile = cfg.profile;
            let Some(d) = self.devices.get_mut(&id) else { continue };
            let mut buf: Vec<u8> = Vec::new();
            if d.needs_init {
                d.needs_init = false;
                for i in &cfg.init {
                    buf.extend_from_slice(i);
                }
            }
            for (out, norm, led, ring) in want {
                let mut b = Vec::with_capacity(6);
                out.encode(norm, led, ring, &mut b);
                if d.fb.get(&out) != Some(&b) {
                    buf.extend_from_slice(&b);
                    d.fb.insert(out, b);
                }
            }
            for (strip, row, text) in texts {
                if d.fb_text.get(&(strip, row)) != Some(&text) {
                    match profile {
                        Profile::Hui => mcu::hui_scribble(strip, &text, &mut buf),
                        Profile::Mcu { .. } => mcu::mcu_scribble(mcu::MCU_MAIN, strip, row, &text, &mut buf),
                        _ => {}
                    }
                    d.fb_text.insert((strip, row), text);
                }
            }
            for (strip, level) in meters {
                let l = (level.clamp(0.0, 1.0) * 12.0).round() as u8;
                let stale = d.fb_meter.get(&strip).is_none_or(|(prev, t)| *prev != l || t.elapsed() > Duration::from_millis(250));
                if stale {
                    match profile {
                        Profile::Hui => {
                            mcu::hui_meter(strip, 0, level, &mut buf);
                            mcu::hui_meter(strip, 1, level, &mut buf);
                        }
                        Profile::Mcu { .. } => mcu::mcu_meter(strip, level, &mut buf),
                        _ => {}
                    }
                    d.fb_meter.insert(strip, (l, Instant::now()));
                }
            }
            if !buf.is_empty() {
                match send_addr(addr, &buf) {
                    Ok(()) => {
                        d.msgs_out += 1;
                        monitor(&self.sh, &id, "out", &buf);
                    }
                    Err(e) => {
                        d.error = Some(e.clone());
                        d.fb.clear();
                        self.rescan_at = Some(Instant::now() + Duration::from_millis(200));
                    }
                }
            }
        }
    }
}

fn bank_role<'a>(b: &'a BankCfg, role: &str) -> Option<&'a str> {
    match role {
        "fader" => b.fader.as_deref(),
        "vpot" => b.vpot.as_deref(),
        "select" => b.select.as_deref(),
        "mute" => b.mute.as_deref(),
        "solo" => b.solo.as_deref(),
        "rec" => b.rec.as_deref(),
        _ => None,
    }
}

/// Append to the MIDI monitor ring (for `controllers.midi.monitor`).
fn monitor(sh: &Shared, device: &str, dir: &'static str, bytes: &[u8]) {
    if !sh.monitor_on() {
        return;
    }
    let mut desc = String::new();
    let mut p = Parser::default();
    p.feed(bytes, |m| {
        if !desc.is_empty() {
            desc.push_str("; ");
        }
        desc.push_str(&describe(&m));
    });
    let mut g = sh.monitor.lock();
    if g.len() >= 500 {
        g.pop_front();
    }
    g.push_back(MonitorEntry { ts_ms: se_clock::now() / 1_000_000, device: device.to_string(), dir, hex: parse::hex(bytes), desc });
}

pub fn describe(m: &Msg) -> String {
    match m {
        Msg::NoteOn { ch, note, vel } => format!("note on ch{} {note} vel {vel}", ch + 1),
        Msg::NoteOff { ch, note, .. } => format!("note off ch{} {note}", ch + 1),
        Msg::PolyPressure { ch, note, value } => format!("poly pressure ch{} {note} {value}", ch + 1),
        Msg::Cc { ch, cc, value } => format!("cc ch{} {cc} = {value}", ch + 1),
        Msg::Program { ch, program } => format!("program ch{} {program}", ch + 1),
        Msg::ChannelPressure { ch, value } => format!("pressure ch{} {value}", ch + 1),
        Msg::PitchBend { ch, value } => format!("pitch-bend ch{} {value}", ch + 1),
        Msg::SysEx(b) => format!("sysex {} bytes", b.len()),
        Msg::QuarterFrame(v) => format!("mtc qf {:x}:{:x}", v >> 4, v & 15),
        Msg::SongPosition(p) => format!("song position {p}"),
        Msg::SongSelect(s) => format!("song select {s}"),
        Msg::TuneRequest => "tune request".into(),
        Msg::Realtime(b) => format!("realtime {b:02X}"),
    }
}

/// Parse a fixture file (`<ms> <hex>` lines, `#` comments).
pub fn read_fixture(text: &str) -> Vec<(u64, Vec<u8>)> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'))
        .filter_map(|l| {
            let (t, rest) = l.split_once(char::is_whitespace)?;
            Some((t.parse().ok()?, parse::parse_hex(rest)?))
        })
        .collect()
}

/// Build a decoder for a configured device (tests and fixture replay).
pub fn decoder_for(cfg: &MidiDeviceCfg) -> Decoder {
    let mut d = Decoder::new(cfg.controls.clone());
    d.hui = cfg.profile == Profile::Hui;
    d.auto = cfg.auto_controls;
    d
}
