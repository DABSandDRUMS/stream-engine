//! Audio control thread: owns the config, builds graphs, keeps the RT graph in sync with the
//! core's resolved state (snapshot → lock-free parameter messages), attaches `hub.audio`
//! slots and taps, routes `audio.*` actions, and publishes meters, perf, and health.

use crate::ana::{self, Setup, Source};
use crate::builder::{self, Built, Conv, EnableRule, ParamBind, PortTable, SLOT_METER_BASE, WasmUse};
use crate::config::{self, AudioConfig};
use crate::dsp::SlotReader;
use crate::graph::*;
use crate::pw::{self, PortSlots, PwHandle, PwPlan};
use crate::sounds::{self, SoundCache};
use crate::taps::{self, TapName, TapProducer};
use crate::wasmfx::DspPatches;
use parking_lot::Mutex;
use se_core::Config;
use se_core::Input;
use se_hub::{EngineCtx, Hub, Snapshot};
use se_proto::{Command, Event, Meta, Op, Origin, Value};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub enum CtlMsg {
    Config(Arc<Config>),
    Action(Box<Command>),
    Shutdown,
}

/// State shared with query handlers (tokio side).
#[derive(Default)]
pub struct Shared {
    /// `audio.mix`: config description + live levels, fx, ducking, perf.
    pub mix: Mutex<Value>,
    pub devices: Mutex<Vec<pw::DeviceInfo>>,
}

/// First tap index for external (`taps::open`) taps; below are analysis taps.
const EXT_TAP_BASE: u16 = 8;
const RT_RING: usize = 16384;

struct ParamRt {
    bind: ParamBind,
    id: Option<usize>,
    sig: Option<usize>,
    last: f32,
}

struct EnableRt {
    rule: EnableRule,
    last: Option<bool>,
}

struct SlotInfo {
    name: String,
    addr: String,
}

struct ExtTap {
    index: u16,
    name: TapName,
}

#[derive(Clone, PartialEq)]
struct AnaKey {
    rate: u32,
    buses: Vec<String>,
    mic: Option<String>,
    beat: String,
    range: (u32, u32),
    talk: (i32, u32),
}

struct Meta2 {
    info: Value,
    pads: Vec<String>,
    sounds: Vec<String>,
    bus_names: Vec<String>,
    bus_meters: Vec<usize>,
    input_names: Vec<String>,
    input_meters: Vec<usize>,
    bus_fx: Vec<String>,
    wasm: Vec<WasmUse>,
    declared_prefixes: Vec<String>,
    latency: usize,
    errors: Vec<String>,
}

pub struct Control {
    ctx: EngineCtx,
    hub: Arc<Hub>,
    cfg: AudioConfig,
    tx: rtrb::Producer<RtMsg>,
    rx: rtrb::Consumer<RtOut>,
    backlog: std::collections::VecDeque<RtMsg>,
    stats: Arc<RtStats>,
    ports: PortTable,
    pw: Option<PwHandle>,
    ana: ana::Handle,
    ana_key: Option<AnaKey>,
    generation: u32,
    params: Vec<ParamRt>,
    slot_params: Vec<ParamRt>,
    enables: Vec<EnableRt>,
    index_ptr: usize,
    sig_ptr: usize,
    duck_last: f32,
    beat_seq: u64,
    slots: Vec<SlotInfo>,
    slot_gen: u64,
    ext_taps: Vec<ExtTap>,
    tap_gen: u64,
    sounds: SoundCache,
    dsp: DspPatches,
    m: Meta2,
    shared: Arc<Shared>,
    published: HashMap<String, Value>,
    patch_errors: HashMap<String, String>,
    last_meter: Instant,
    last_second: Instant,
    last_readback: Instant,
    last_health: Instant,
    last_enable: Instant,
    last_mix: Instant,
    xrun_base: u64,
    xruns_minute: std::collections::VecDeque<(Instant, u64)>,
    config_errors: Vec<String>,
    stem_hint: bool,
    /// Last time the mic input carried sound (preflight "mic signal present", §17.1).
    mic_heard: Option<std::time::Instant>,
    mic_since: std::time::Instant,
}

fn now_ns() -> u64 {
    se_clock::now()
}

fn db(x: f32) -> f64 {
    (20.0 * x.max(1e-5).log10()).max(-100.0) as f64
}

impl Control {
    /// Build the first graph and start PipeWire + analysis. Returns the control object; run
    /// it with [`Control::run`] on its own thread.
    pub fn start(ctx: EngineCtx, shared: Arc<Shared>) -> anyhow::Result<Control> {
        let hub = ctx.hub.clone();
        let (cfg, errs) = match read_config(&ctx) {
            Ok(c) => (c, Vec::new()),
            Err(e) => {
                hub.log("error", "audio", format!("audio config: {e} (using defaults)"));
                (AudioConfig::default(), vec![e])
            }
        };
        taps::set_rate(cfg.rate);
        let (tx, rx_rt) = rtrb::RingBuffer::new(RT_RING);
        let (tx_rt, rx) = rtrb::RingBuffer::new(RT_RING);
        let stats = Arc::new(RtStats::default());
        let engine = Box::new(Engine::new(cfg.rate as f32, rx_rt, tx_rt, stats.clone()));
        let ana = ana::spawn(hub.clone())?;
        let default_budget = (cfg.quantum as f32 / cfg.rate as f32 * 1e6 * 0.25) as u32;
        let mut c = Control {
            hub: hub.clone(),
            cfg,
            tx,
            rx,
            backlog: Default::default(),
            stats: stats.clone(),
            ports: PortTable::default(),
            pw: None,
            ana,
            ana_key: None,
            generation: 0,
            params: Vec::new(),
            slot_params: Vec::new(),
            enables: Vec::new(),
            index_ptr: 0,
            sig_ptr: 0,
            duck_last: f32::NAN,
            beat_seq: 0,
            slots: Vec::new(),
            slot_gen: u64::MAX,
            ext_taps: Vec::new(),
            tap_gen: u64::MAX,
            sounds: SoundCache::default(),
            dsp: DspPatches::new(default_budget),
            m: Meta2 {
                info: Value::Null,
                pads: Vec::new(),
                sounds: Vec::new(),
                bus_names: Vec::new(),
                bus_meters: Vec::new(),
                input_names: Vec::new(),
                input_meters: Vec::new(),
                bus_fx: Vec::new(),
                wasm: Vec::new(),
                declared_prefixes: Vec::new(),
                latency: 0,
                errors: Vec::new(),
            },
            shared,
            published: HashMap::new(),
            patch_errors: HashMap::new(),
            last_meter: Instant::now(),
            last_second: Instant::now(),
            last_readback: Instant::now(),
            last_health: Instant::now() - Duration::from_secs(10),
            last_enable: Instant::now(),
            last_mix: Instant::now(),
            xrun_base: 0,
            xruns_minute: Default::default(),
            config_errors: errs,
            stem_hint: false,
            mic_heard: None,
            mic_since: std::time::Instant::now(),
            ctx,
        };
        c.declare_static();
        c.dsp.scan(&c.ctx.project_root);
        let plan = c.rebuild();
        let ports = Arc::new(PortSlots::default());
        c.pw = Some(pw::spawn(engine, ports, stats, plan)?);
        Ok(c)
    }

    fn declare_static(&self) {
        let h = &self.hub;
        let ro = |a: &str, m: Meta| h.declare(a, m.readonly().owner("audio"));
        ro("perf.audio.xruns", Meta::int(0, [0.0, 1e12]).describe("audio xruns since start"));
        ro("perf.audio.load", Meta::float(0.0, [0.0, 2.0]).describe("peak DSP time / period (0.5 s)"));
        ro("perf.audio.dsp_ms", Meta::float(0.0, [0.0, 100.0]).unit("ms").describe("average callback time"));
        ro("perf.audio.quantum", Meta::int(0, [0.0, 8192.0]).unit("frames"));
        ro("perf.audio.rate", Meta::int(0, [0.0, 192000.0]).unit("Hz"));
        ro("perf.audio.latency_ms", Meta::float(0.0, [0.0, 1000.0]).unit("ms").describe("engine latency: one quantum + effect lookahead"));
        ro("perf.audio.driver_delay_ms", Meta::float(0.0, [-1000.0, 1000.0]).unit("ms").describe("driver-reported delay to hardware"));
        ro("perf.audio.allocs", Meta::int(0, [0.0, 1e12]).describe("heap allocations inside the audio callback (debug builds)"));
    }

    // ---- config / rebuild -------------------------------------------------------------------

    fn push(&mut self, m: RtMsg) {
        if !self.backlog.is_empty() {
            self.backlog.push_back(m);
            return;
        }
        if let Err(rtrb::PushError::Full(m)) = self.tx.push(m) {
            self.backlog.push_back(m);
        }
    }

    fn flush(&mut self) {
        while let Some(m) = self.backlog.pop_front() {
            if let Err(rtrb::PushError::Full(m)) = self.tx.push(m) {
                self.backlog.push_front(m);
                break;
            }
        }
    }

    /// Build a new graph generation from `self.cfg`; returns the PipeWire plan.
    fn rebuild(&mut self) -> PwPlan {
        let (bank, sound_errs) = sounds::build_bank(&self.cfg, &self.ctx.project_root, &mut self.sounds);
        for e in &sound_errs {
            self.hub.log("error", "audio", e.clone());
        }
        self.generation = self.generation.wrapping_add(1);
        let mut built: Built = builder::build(&self.cfg, self.generation, &mut self.ports, bank, &self.dsp);
        for e in &built.errors {
            self.hub.log("error", "audio", e.clone());
        }
        // declarations (removing effect prefixes that disappeared)
        let mut prefixes: Vec<String> = built.declares.iter().filter_map(|(a, _)| fx_prefix(a)).collect();
        prefixes.sort();
        prefixes.dedup();
        for old in &self.m.declared_prefixes {
            if !prefixes.contains(old) {
                self.hub.submit(Input::Remove { prefix: old.clone() });
            }
        }
        for (a, meta) in &built.declares {
            self.hub.declare(a, meta.clone());
        }
        for (a, spec) in &built.triggers {
            self.hub.submit(Input::DeclareTrigger { address: a.clone(), spec: *spec });
        }
        let graph = built.graph.take().expect("built graph");
        let latency = graph.latency;
        self.push(RtMsg::Swap(graph));
        // params: everything re-sent to the new generation
        self.params = built.params.drain(..).map(|bind| ParamRt { last: f32::NAN, id: None, sig: None, bind }).collect();
        self.enables = built.enables.drain(..).map(|rule| EnableRt { rule, last: None }).collect();
        self.index_ptr = 0;
        self.sig_ptr = 0;
        self.duck_last = f32::NAN;
        // slot routes / taps for the new generation
        let routes: Vec<RtMsg> = self.slots.iter().enumerate().map(|(i, s)| self.slot_route(i as u16, &s.name)).collect();
        for m in routes {
            self.push(m);
        }
        for t in 0..self.ext_taps.len() {
            let src = self.resolve_tap(&self.ext_taps[t].name);
            let index = self.ext_taps[t].index;
            self.push(RtMsg::TapRoute { generation: self.generation, index, source: src });
        }
        self.setup_analysis();
        self.m = Meta2 {
            info: built.info,
            pads: built.pads,
            sounds: built.sounds,
            bus_names: self.cfg.buses.iter().map(|b| b.name.clone()).collect(),
            bus_meters: built.bus_meters,
            input_names: self.cfg.inputs.iter().map(|b| b.name.clone()).collect(),
            input_meters: built.input_meters,
            bus_fx: built.bus_fx,
            wasm: built.wasm,
            declared_prefixes: prefixes,
            latency,
            errors: built.errors.into_iter().chain(sound_errs).collect(),
        };
        PwPlan {
            ins: self.ports.ins.clone(),
            outs: self.ports.outs.clone(),
            links: built.links,
            nodes: built.nodes,
            quantum: self.cfg.quantum,
            rate: self.cfg.rate,
        }
    }

    fn apply_config(&mut self, c: &Config) {
        let section = c.project.extra.get("audio").and_then(|v| v.as_table()).cloned().unwrap_or_default();
        let frags = c.other.get("audio").cloned().unwrap_or_default();
        match config::parse(&section, &frags) {
            Ok(new) => {
                self.config_errors.clear();
                self.dsp.scan(&self.ctx.project_root);
                if new == self.cfg && !self.sounds_changed() {
                    return;
                }
                self.cfg = new;
                let plan = self.rebuild();
                if let Some(pw) = &self.pw {
                    pw.plan(plan);
                }
                self.hub.log("info", "audio", format!("audio graph rebuilt (generation {})", self.generation));
            }
            Err(e) => {
                self.hub.log("error", "audio", format!("audio config: {e} — keeping the last good configuration"));
                self.config_errors = vec![e];
            }
        }
    }

    fn sounds_changed(&self) -> bool {
        // sound files edited in place: rebuild when any mtime moved
        self.cfg.sounds.iter().flat_map(|s| s.layers.iter().flat_map(|l| l.files.iter())).any(|f| {
            let p = self.ctx.project_root.join(f);
            std::fs::metadata(&p).and_then(|m| m.modified()).map(|t| t.elapsed().map(|e| e < Duration::from_secs(3)).unwrap_or(false)).unwrap_or(false)
        })
    }

    fn setup_analysis(&mut self) {
        let a = self.cfg.analysis.clone();
        let key = AnaKey {
            rate: self.cfg.rate,
            buses: a.buses.clone(),
            mic: a.mic.clone(),
            beat: a.beat_source.clone(),
            range: (a.min_bpm as u32, a.max_bpm as u32),
            talk: (a.talk_threshold_db as i32, a.talk_hold_ms as u32),
        };
        let mut sources_src: Vec<(String, TapSource, bool)> = Vec::new();
        for b in &a.buses {
            if let Some(i) = self.cfg.bus_index(b) {
                sources_src.push((b.clone(), TapSource::BusInStereo(i as u8), false));
            }
        }
        if let Some(m) = &a.mic
            && let Some(i) = self.cfg.input_index(m)
        {
            sources_src.push(("mic".into(), TapSource::InputStereo(i as u8), true));
        }
        if self.ana_key.as_ref() == Some(&key) {
            for (k, (_, src, _)) in sources_src.iter().enumerate() {
                self.push(RtMsg::TapRoute { generation: self.generation, index: k as u16, source: Some(*src) });
            }
            return;
        }
        let rate = self.cfg.rate;
        let mut sources = Vec::new();
        for (k, (prefix, src, is_mic)) in sources_src.into_iter().enumerate().take(EXT_TAP_BASE as usize) {
            let (sp, sc) = rtrb::RingBuffer::new(rate as usize * 2);
            let (cp, cc) = rtrb::RingBuffer::new(4096);
            let producer = TapProducer { name: format!("analysis.{prefix}"), samples: sp, clock: cp, written: 0, dropped: 0 };
            self.push(RtMsg::AddTap { index: k as u16, entry: Box::new(TapEntry { producer, source: None, stereo: true }) });
            self.push(RtMsg::TapRoute { generation: self.generation, index: k as u16, source: Some(src) });
            sources.push(Source { prefix, samples: sc, clock: cc, is_mic });
        }
        let _ = self.ana.tx.send(ana::Msg::Setup(Box::new(Setup {
            rate,
            sources,
            beat_source: a.beat_source.clone(),
            min_bpm: a.min_bpm,
            max_bpm: a.max_bpm,
            talk_threshold_db: a.talk_threshold_db,
            talk_hold_ms: a.talk_hold_ms,
        })));
        self.ana_key = Some(key);
    }

    // ---- slots and taps ---------------------------------------------------------------------

    fn slot_route(&self, index: u16, name: &str) -> RtMsg {
        let bus = self.cfg.route_slot(name).map(|b| b as u8);
        let direct = self.cfg.direct.iter().find(|d| d.slot == name).and_then(|d| {
            let pname = format!("direct_{}", d.slot.replace('.', "_"));
            self.ports.outs.iter().position(|p| *p == pname).map(|i| (i as u16, d.gain_db))
        });
        RtMsg::SlotRoute { generation: self.generation, index, bus, direct }
    }

    fn poll_slots(&mut self) {
        let g = self.hub.audio.generation.load(Ordering::Acquire);
        if g == self.slot_gen {
            return;
        }
        self.slot_gen = g;
        let sr = self.cfg.rate;
        let max_delay = (self.cfg.max_delay_ms * 0.001 * sr as f32) as usize;
        for (name, stream) in self.hub.audio.take_new() {
            let index = match self.slots.iter().position(|s| s.name == name) {
                Some(i) => i,
                None => {
                    if self.slots.len() >= MAX_SLOTS {
                        self.hub.log("error", "audio", format!("too many audio slots; `{name}` ignored"));
                        continue;
                    }
                    let addr = format!("audio.input.{}", name.replace('.', "_"));
                    self.slots.push(SlotInfo { name: name.clone(), addr: addr.clone() });
                    let i = self.slots.len() - 1;
                    let delay = self.cfg.slot_delay(&name);
                    self.hub.declare(&format!("{addr}.gain"), Meta::float(0.0, [-60.0, 24.0]).unit("dB").describe("source gain").owner("audio"));
                    self.hub.declare(&format!("{addr}.mute"), Meta::boolean(false).owner("audio"));
                    self.hub.declare(
                        &format!("{addr}.delay_ms"),
                        Meta::float(delay as f64, [0.0, self.cfg.max_delay_ms as f64]).unit("ms").describe("A/V delay").owner("audio"),
                    );
                    for (suffix, target, fb) in
                        [("gain", Target::SlotGain(i as u16), 0.0), ("mute", Target::SlotMute(i as u16), 0.0), ("delay_ms", Target::SlotDelay(i as u16), delay)]
                    {
                        let conv = if suffix == "mute" { Conv::Bool } else { Conv::Float };
                        self.slot_params.push(ParamRt {
                            bind: ParamBind { addr: format!("{addr}.{suffix}"), target, conv, fallback: fb },
                            id: None,
                            sig: None,
                            last: f32::NAN,
                        });
                    }
                    i
                }
            };
            let reader = SlotReader::new(stream.consumer, stream.channels, stream.rate, sr, 30.0);
            let entry = SlotEntry::new(name.clone(), reader, sr as f32, max_delay, SLOT_METER_BASE + index);
            self.push(RtMsg::AddSlot { index: index as u16, entry: Box::new(entry) });
            let m = self.slot_route(index as u16, &name);
            self.push(m);
            // params for a replaced slot must be re-sent
            for p in self.slot_params.iter_mut() {
                if matches!(p.bind.target, Target::SlotGain(i) | Target::SlotMute(i) | Target::SlotDelay(i) if i as usize == index) {
                    p.last = f32::NAN;
                }
            }
            match self.cfg.route_slot(&name) {
                Some(b) => self.hub.log("info", "audio", format!("audio source `{name}` → bus {}", self.cfg.buses[b].name)),
                None => self.hub.log("info", "audio", format!("audio source `{name}` attached (direct routes only)")),
            }
        }
        self.index_ptr = 0;
    }

    fn resolve_tap(&self, n: &TapName) -> Option<TapSource> {
        match n {
            TapName::Input { input, ch } => {
                let d = self.cfg.inputs.iter().find(|i| &i.name == input)?;
                (*ch < d.channels.len()).then_some(())?;
                let pname = format!("in_{input}_{}", ch + 1);
                self.ports.ins.iter().position(|p| *p == pname).map(|i| TapSource::Port(i as u16))
            }
            TapName::Bus { bus, ch } => self.cfg.bus_index(bus).map(|b| TapSource::BusIn(b as u8, *ch as u8)),
        }
    }

    fn poll_taps(&mut self) {
        let g = taps::generation();
        if g == self.tap_gen {
            return;
        }
        self.tap_gen = g;
        for p in taps::take_new() {
            let Some(name) = taps::parse_name(&p.name) else {
                self.hub.log("error", "audio", format!("bad tap name `{}` (use input.<input>.<ch> or bus.<bus>.<ch>)", p.name));
                continue;
            };
            let index = EXT_TAP_BASE + self.ext_taps.len() as u16;
            if index as usize >= MAX_TAPS {
                self.hub.log("error", "audio", format!("too many taps; `{}` ignored", p.name));
                continue;
            }
            let src = self.resolve_tap(&name);
            if src.is_none() {
                self.hub.log("warn", "audio", format!("tap `{}` has no source in the current audio config", p.name));
            }
            self.push(RtMsg::AddTap { index, entry: Box::new(TapEntry { producer: p, source: None, stereo: false }) });
            self.push(RtMsg::TapRoute { generation: self.generation, index, source: src });
            self.ext_taps.push(ExtTap { index, name });
        }
    }

    // ---- parameter sync ---------------------------------------------------------------------

    fn sync_params(&mut self) {
        let snap = self.hub.snapshot.load();
        let ip = Arc::as_ptr(&snap.index) as usize;
        let sp = Arc::as_ptr(&snap.signal_index) as usize;
        let resolve = ip != self.index_ptr || sp != self.sig_ptr;
        if resolve {
            self.index_ptr = ip;
            self.sig_ptr = sp;
        }
        let generation = self.generation;
        let mut out: Vec<RtMsg> = Vec::new();
        for p in self.params.iter_mut().chain(self.slot_params.iter_mut()) {
            if resolve {
                p.id = snap.id(&p.bind.addr);
                p.sig = if p.id.is_none() { snap.signal_index.get(&p.bind.addr).copied() } else { None };
            }
            let v = match (p.id, p.sig) {
                (Some(i), _) => p.bind.conv.apply(snap.value(i)).unwrap_or(p.bind.fallback),
                (None, Some(s)) => snap.signals.get(s).copied().unwrap_or(p.bind.fallback),
                _ => p.bind.fallback,
            };
            if !(v == p.last) {
                p.last = v;
                out.push(RtMsg::Param { generation, target: p.bind.target, value: v });
            }
        }
        // ducking signals (e.g. mic.talking): max over the configured names
        let mut duck = 0.0f32;
        for n in &self.cfg.duck.signals {
            let v = snap.signal(n).or_else(|| snap.get(n).map(|v| if v.truthy() { v.as_f32().unwrap_or(1.0) } else { 0.0 })).unwrap_or(0.0);
            duck = duck.max(v);
        }
        if duck != self.duck_last {
            self.duck_last = duck;
            out.push(RtMsg::Param { generation, target: Target::DuckSignal, value: duck });
        }
        if self.last_enable.elapsed() >= Duration::from_millis(20) || resolve {
            self.last_enable = Instant::now();
            for e in &mut self.enables {
                let bypass = snap.get(&e.rule.bypass).is_some_and(Value::truthy);
                let when = e.rule.when.as_ref().is_none_or(|w| w.eval_bool(&SnapScope(&snap)));
                let on = !bypass && when;
                if e.last != Some(on) {
                    e.last = Some(on);
                    out.push(RtMsg::Param { generation, target: e.rule.target, value: if on { 1.0 } else { 0.0 } });
                }
            }
        }
        for m in out {
            self.push(m);
        }
        // beat clock → RT transport
        let (seq, bpm, beat, ts, _conf) = self.ana.beat.load();
        if seq != self.beat_seq && bpm > 0.0 {
            self.beat_seq = seq;
            self.push(RtMsg::Transport { bpm, beat, ts });
        }
    }

    // ---- RT feedback --------------------------------------------------------------------------

    fn drain(&mut self) {
        while let Ok(o) = self.rx.pop() {
            match o {
                RtOut::Garbage(g) => drop(g),
                RtOut::Live(g) => tracing::debug!(target: "audio", "graph generation {g} live"),
                RtOut::Hit { pad, velocity, ts } => {
                    if let Some(name) = self.m.pads.get(pad as usize) {
                        let mut e = Event::new(format!("drums.{name}"), Origin::Audio, Value::map().with("velocity", velocity as f64));
                        e.ts = ts;
                        self.hub.emit(e);
                        self.hub.signal(&format!("drums.{name}"), velocity);
                    }
                }
                RtOut::Measured(r) => {
                    let rate = self.cfg.rate as f64;
                    match r {
                        Some(frames) => {
                            let ms = frames as f64 * 1000.0 / rate;
                            self.hub.publish("audio.monitor.roundtrip_ms", Value::Float(ms));
                            self.hub.log("info", "audio", format!("monitor round trip: {frames} frames = {ms:.2} ms"));
                        }
                        None => self.hub.log("warn", "audio", "monitor round trip: no return detected (connect the monitor output to the measured input)"),
                    }
                }
            }
        }
    }

    // ---- actions ------------------------------------------------------------------------------

    fn core_cmd(&self, from: &Command, op: Op) {
        let mut c = Command::new(from.origin, op);
        c.actor = from.actor.clone();
        c.causal = Some(from.id);
        c.priority = from.priority;
        c.key = from.key.clone();
        self.hub.command(c);
    }

    fn action(&mut self, cmd: Command) {
        let Op::Action { name, args } = &cmd.op else { return };
        let arg = |k: &str| args.get_path(k).cloned().or_else(|| if k == "0" { args.as_list().and_then(|l| l.first().cloned()) } else { None });
        let pos = |i: usize| args.get_path("args").and_then(Value::as_list).and_then(|l| l.get(i)).cloned();
        let num = |k: &str, d: f32| arg(k).and_then(|v| v.as_f32()).unwrap_or(d);
        match name.as_str() {
            "audio.play" => {
                let sound = arg("sound").or_else(|| pos(0)).and_then(|v| v.as_str().map(str::to_string)).or_else(|| args.as_str().map(str::to_string));
                let Some(sound) = sound else {
                    self.hub.log("warn", "audio", "audio.play needs {sound}");
                    return;
                };
                match self.m.sounds.iter().position(|s| *s == sound) {
                    Some(i) => {
                        let m = RtMsg::Play {
                            generation: self.generation,
                            sound: i as u16,
                            velocity: num("velocity", 1.0).clamp(0.0, 1.0),
                            gain_db: num("gain", 0.0),
                            pan: num("pan", 0.0).clamp(-1.0, 1.0),
                            pitch: num("pitch", 0.0),
                        };
                        self.push(m);
                    }
                    None => self.hub.log("warn", "audio", format!("audio.play: unknown sound `{sound}` (have: {})", self.m.sounds.join(", "))),
                }
            }
            "audio.stop" => self.push(RtMsg::StopSounds),
            "audio.duck" => {
                let on = match arg("on").or_else(|| pos(0)) {
                    Some(Value::Bool(b)) => Some(b),
                    Some(Value::Str(s)) if s == "on" || s == "true" => Some(true),
                    Some(Value::Str(s)) if s == "off" || s == "false" => Some(false),
                    Some(Value::Str(s)) if s == "toggle" => None,
                    Some(v) => Some(v.truthy()),
                    None => Some(true),
                };
                let on = on.unwrap_or_else(|| !self.hub.snapshot.load().bool("audio.duck.active"));
                if let Some(d) = arg("depth").and_then(|v| v.as_f64()) {
                    self.core_cmd(&cmd, Op::Set { address: "audio.duck.depth".into(), value: Value::Float(d) });
                }
                let mut key_cmd = cmd.clone();
                key_cmd.key = Some(cmd.key.clone().unwrap_or_else(|| "audio.duck".into()));
                if on {
                    self.core_cmd(&key_cmd, Op::Set { address: "audio.duck.active".into(), value: Value::Bool(true) });
                    let hold = arg("hold").and_then(|v| match v {
                        Value::Str(s) => se_proto::parse_duration_ms(&s),
                        v => v.as_f64().map(|x| x as u64),
                    });
                    if let Some(ms) = hold {
                        let hub = self.hub.clone();
                        let mut rel = Command::new(key_cmd.origin, Op::Release { address: "audio.duck.active".into() });
                        rel.key = key_cmd.key.clone();
                        rel.priority = key_cmd.priority;
                        rel.actor = key_cmd.actor.clone();
                        rel.causal = Some(cmd.id);
                        std::thread::spawn(move || {
                            std::thread::sleep(Duration::from_millis(ms));
                            hub.command(rel);
                        });
                    }
                } else {
                    self.core_cmd(&key_cmd, Op::Release { address: "audio.duck.active".into() });
                }
            }
            "audio.panic" => {
                self.push(RtMsg::Panic);
                // safe mix: back to the project's base gains/mutes, no ducking
                let mut addrs: Vec<String> = Vec::new();
                for b in &self.cfg.buses {
                    addrs.push(format!("audio.bus.{}.gain", b.name));
                    addrs.push(format!("audio.bus.{}.mute", b.name));
                }
                addrs.push("audio.duck.active".into());
                for a in addrs {
                    let mut c = Command::new(Origin::System, Op::Release { address: a });
                    c.priority = Some(se_proto::PRIORITY_MANUAL);
                    c.causal = Some(cmd.id);
                    self.hub.command(c);
                }
                for p in self.params.iter_mut() {
                    p.last = f32::NAN;
                }
                self.hub.log("warn", "audio", "audio panic: sounds stopped, effects released, safe mix");
            }
            "audio.tap" => {
                let _ = self.ana.tx.send(ana::Msg::Tap(if cmd.ts > 0 { cmd.ts } else { now_ns() }));
            }
            "audio.tap.clear" => {
                let _ = self.ana.tx.send(ana::Msg::ClearTap);
            }
            "audio.reload" => {
                let c = self.ctx.config.borrow().clone();
                self.cfg = AudioConfig { quantum: 0, ..self.cfg.clone() };
                self.apply_config(&c);
            }
            "audio.monitor.measure" => {
                let input = arg("input").and_then(|v| v.as_str().map(str::to_string)).or_else(|| self.cfg.inputs.first().map(|i| i.name.clone()));
                let port = input.and_then(|i| self.ports.ins.iter().position(|p| *p == format!("in_{i}_1")));
                match (port, &self.cfg.monitor) {
                    (Some(p), Some(_)) => self.push(RtMsg::Measure { port: p as u16 }),
                    (_, None) => self.hub.log("warn", "audio", "audio.monitor.measure: no [audio.monitor] configured"),
                    (None, _) => self.hub.log("warn", "audio", "audio.monitor.measure: unknown input"),
                }
            }
            n if n.starts_with("audio.fx.") => self.fx_action(&cmd, &n["audio.fx.".len()..], args),
            other => self.hub.log("warn", "audio", format!("unknown audio action `{other}`")),
        }
    }

    /// `audio.fx.<verb> bus=<b>|input=<i> fx=<e> …` → core commands on the effect address.
    fn fx_action(&self, cmd: &Command, verb: &str, args: &Value) {
        let s = |k: &str| args.get_path(k).and_then(Value::as_str).map(str::to_string);
        let base = match (s("bus"), s("input")) {
            (Some(b), _) => format!("audio.bus.{b}"),
            (None, Some(i)) => format!("audio.input.{i}"),
            _ => {
                self.hub.log("warn", "audio", format!("audio.fx.{verb} needs bus=<bus> or input=<input>"));
                return;
            }
        };
        let Some(fx) = s("fx") else {
            self.hub.log("warn", "audio", format!("audio.fx.{verb} needs fx=<effect>"));
            return;
        };
        let addr = format!("{base}.fx.{fx}");
        let op = match verb {
            "trigger" => {
                let mut payload = args.clone();
                if let Value::Map(m) = &mut payload {
                    m.remove("bus");
                    m.remove("input");
                    m.remove("fx");
                }
                Op::Trigger { address: addr, payload }
            }
            "release" => Op::Release { address: addr },
            "bypass" => Op::Set { address: format!("{addr}.bypass"), value: Value::Bool(args.get_path("on").map(Value::truthy).unwrap_or(true)) },
            "enable" => Op::Set { address: format!("{addr}.bypass"), value: Value::Bool(false) },
            "set" => {
                let (Some(p), Some(v)) = (s("param"), args.get_path("value").cloned()) else {
                    self.hub.log("warn", "audio", "audio.fx.set needs param=<name> value=<v>");
                    return;
                };
                Op::Set { address: format!("{addr}.{p}"), value: v }
            }
            other => {
                self.hub.log("warn", "audio", format!("unknown audio.fx action `{other}` (trigger, release, bypass, enable, set)"));
                return;
            }
        };
        self.core_cmd(cmd, op);
    }

    // ---- publishing ---------------------------------------------------------------------------

    fn publish_if(&mut self, addr: &str, v: Value, min_delta: f64) {
        let changed = match (self.published.get(addr), &v) {
            (Some(Value::Float(a)), Value::Float(b)) => (a - b).abs() >= min_delta,
            (Some(a), b) => a != b,
            (None, _) => true,
        };
        if changed {
            self.published.insert(addr.to_string(), v.clone());
            self.hub.publish(addr, v);
        }
    }

    fn meters(&mut self) {
        let mut sig = Vec::with_capacity(32);
        let mut levels = Vec::new();
        for (k, name) in self.m.bus_names.iter().enumerate() {
            let (rms, pk) = self.stats.meters[self.m.bus_meters[k]].take();
            sig.push((format!("audio.{name}.level"), rms));
            sig.push((format!("audio.{name}.peak"), pk));
            levels.push((name.clone(), rms, pk));
        }
        for (k, name) in self.m.input_names.iter().enumerate() {
            let (rms, pk) = self.stats.meters[self.m.input_meters[k]].take();
            sig.push((format!("audio.input.{name}.level"), rms));
            sig.push((format!("audio.input.{name}.peak"), pk));
        }
        for (i, s) in self.slots.iter().enumerate() {
            let (rms, pk) = self.stats.meters[SLOT_METER_BASE + i].take();
            sig.push((format!("{}.level", s.addr), rms));
            sig.push((format!("{}.peak", s.addr), pk));
        }
        let duck = f32::from_bits(self.stats.duck_db.load(Ordering::Relaxed));
        sig.push(("audio.duck.amount".into(), (-duck / self.cfg.duck.depth_db.abs().max(1.0)).clamp(0.0, 1.0)));
        self.hub.signals(sig);
        // slow readbacks for the state tree (API/CLI)
        if self.last_readback.elapsed() >= Duration::from_secs(1) {
            self.last_readback = Instant::now();
            for (name, rms, pk) in levels {
                self.publish_if(&format!("audio.bus.{name}.level"), Value::Float(db(rms).round()), 1.0);
                self.publish_if(&format!("audio.bus.{name}.peak"), Value::Float(db(pk).round()), 1.0);
            }
            for t in self.cfg.duck.targets.clone() {
                self.publish_if(&format!("audio.bus.{t}.ducked"), Value::Float((duck as f64 * 2.0).round() / 2.0), 0.5);
            }
        }
    }

    fn perf(&mut self) {
        let st = self.stats.clone();
        let xr = st.xruns.load(Ordering::Relaxed);
        let q = st.quantum.load(Ordering::Relaxed);
        let rate = st.rate.load(Ordering::Relaxed).max(1);
        let max_ns = st.dsp_ns_max.swap(0, Ordering::Relaxed);
        let sum = st.dsp_ns_sum.swap(0, Ordering::Relaxed);
        let cnt = st.dsp_count.swap(0, Ordering::Relaxed).max(1);
        let period_ns = q as f64 * 1e9 / rate as f64;
        let load = if period_ns > 0.0 { max_ns as f64 / period_ns } else { 0.0 };
        self.publish_if("perf.audio.xruns", Value::Int(xr as i64), 0.0);
        self.publish_if("perf.audio.load", Value::Float((load * 100.0).round() / 100.0), 0.01);
        self.publish_if("perf.audio.dsp_ms", Value::Float(((sum as f64 / cnt as f64) / 1e6 * 1000.0).round() / 1000.0), 0.005);
        self.publish_if("perf.audio.quantum", Value::Int(q as i64), 0.0);
        self.publish_if("perf.audio.rate", Value::Int(rate as i64), 0.0);
        let lat = (q as f64 + self.stats.latency.load(Ordering::Relaxed) as f64) * 1000.0 / rate as f64;
        self.publish_if("perf.audio.latency_ms", Value::Float((lat * 100.0).round() / 100.0), 0.01);
        let dd = st.delay.load(Ordering::Relaxed) as f64 * 1000.0 / rate as f64;
        self.publish_if("perf.audio.driver_delay_ms", Value::Float((dd * 100.0).round() / 100.0), 0.05);
        self.publish_if("perf.audio.allocs", Value::Int(st.allocs.load(Ordering::Relaxed) as i64), 0.0);
        self.xruns_minute.push_back((Instant::now(), xr));
        while self.xruns_minute.front().is_some_and(|(t, _)| t.elapsed() > Duration::from_secs(60)) {
            self.xruns_minute.pop_front();
        }
        // wasm dsp patches: status → patch.<id>.error
        let mut errs: Vec<(String, String)> = Vec::new();
        for w in &self.m.wasm {
            errs.push((w.patch.clone(), w.status.error_text().unwrap_or_default()));
        }
        for (id, e) in &self.dsp.errors {
            errs.push((id.clone(), e.clone()));
        }
        for (id, e) in errs {
            if self.patch_errors.get(&id) != Some(&e) {
                self.patch_errors.insert(id.clone(), e.clone());
                self.hub.publish(&format!("patch.{id}.error"), Value::Str(e.clone()));
                if !e.is_empty() {
                    self.hub.log("error", "audio", format!("dsp patch `{id}`: {e}"));
                }
            }
        }
        // hot reload of changed dsp modules (block-boundary crossfade)
        let changed = self.dsp.scan(&self.ctx.project_root);
        for id in changed {
            let uses: Vec<(ChainRef, u8, bool)> =
                self.m.wasm.iter().filter(|w| w.patch == id).map(|w| (w.chain, w.slot, matches!(w.chain, ChainRef::Source(_)))).collect();
            for (chain, slot, source) in uses {
                match self.dsp.instantiate(&id, self.cfg.rate as f32, source) {
                    Ok((fx, status, _)) => {
                        if let Some(w) = self.m.wasm.iter_mut().find(|w| w.patch == id && w.chain == chain && w.slot == slot) {
                            w.status = status;
                        }
                        self.push(RtMsg::SwapFx { generation: self.generation, chain, slot, fx: Box::new(fx) });
                        self.hub.log("info", "audio", format!("dsp patch `{id}` reloaded"));
                    }
                    Err(e) => self.hub.log("error", "audio", format!("dsp patch `{id}` reload failed: {e} (old version stays live)")),
                }
            }
            // params may map differently after a manifest change
            for p in self.params.iter_mut() {
                p.last = f32::NAN;
            }
        }
    }

    fn health(&mut self) {
        let pw = self.pw.as_ref().map(|p| p.status.lock().clone()).unwrap_or_default();
        *self.shared.devices.lock() = pw.devices.clone();
        let h = |s: &str, d: String| Value::map().with("status", s).with("detail", d);
        let conn = if pw.connected && pw.state == "streaming" {
            h("pass", format!("se-engine node {} streaming", pw.node_id.unwrap_or(0)))
        } else if pw.connected {
            h("warn", format!("PipeWire connected, filter {}", if pw.state.is_empty() { "starting" } else { &pw.state }))
        } else {
            h("fail", format!("PipeWire not connected: {}", pw.error.clone().unwrap_or_default()))
        };
        self.publish_if("health.audio.pipewire", conn, 0.0);
        let (st, detail) = crate::rt::rt_health(&self.stats);
        self.publish_if("health.audio.rt", h(st, detail), 0.0);
        for inp in self.cfg.inputs.clone() {
            let prefix = format!("in_{}_", inp.name);
            let links: Vec<bool> = pw.links.iter().filter(|(l, _)| l.port.starts_with(&prefix)).map(|(_, ok)| *ok).collect();
            let v = if !links.is_empty() && links.iter().all(|x| *x) {
                h("pass", format!("`{}` captured from {} ch {:?}", inp.name, inp.target, inp.channels))
            } else {
                h("fail", format!("`{}`: no PipeWire source matching `{}` with channels {:?}", inp.name, inp.target, inp.channels))
            };
            self.publish_if(&format!("health.audio.input.{}", inp.name), v, 0.0);
        }
        let prog = pw.consumers.get("se-program").cloned().unwrap_or_default();
        let obs = if prog.is_empty() {
            h("warn", "nothing is capturing se-program (run `streamctl do obs.setup`, or add it as an audio source in OBS)".into())
        } else {
            h("pass", format!("se-program → {}", prog.join(", ")))
        };
        self.publish_if("health.audio.obs", obs, 0.0);
        if let Some(mic) = self.cfg.analysis.mic.clone() {
            let level = self.ana.latest.lock().get("mic").and_then(|v| v.get_path("peak")).and_then(Value::as_f64).unwrap_or(0.0);
            let now = std::time::Instant::now();
            if level > MIC_PRESENT {
                self.mic_heard = Some(now);
            }
            let v = mic_health(&mic, self.mic_heard.map(|t| now.duration_since(t)), now.duration_since(self.mic_since));
            self.publish_if("health.audio.mic", v, 0.0);
        }
        let recent = match (self.xruns_minute.front(), self.xruns_minute.back()) {
            (Some((_, a)), Some((_, b))) => b - a,
            _ => 0,
        };
        let x = if recent == 0 { h("pass", "no xruns in the last minute".into()) } else { h("warn", format!("{recent} xrun(s) in the last minute")) };
        self.publish_if("health.audio.xruns", x, 0.0);
        let errs: Vec<String> = self.config_errors.iter().chain(&self.m.errors).cloned().collect();
        let c = if errs.is_empty() { h("pass", "audio config loaded".into()) } else { h("warn", errs.join("; ")) };
        self.publish_if("health.audio.config", c, 0.0);
        // 16R USB multichannel (M12): point at the stem config once it shows up
        if !self.stem_hint {
            let targets: Vec<String> = self.cfg.inputs.iter().map(|i| i.target.to_lowercase()).collect();
            if let Some(d) = pw
                .devices
                .iter()
                .find(|d| d.class == "Audio/Source" && (d.name.to_lowercase().contains("studiolive") || d.description.to_lowercase().contains("16r")))
            {
                let used = targets.iter().any(|t| d.name.to_lowercase().contains(t.as_str()) || d.description.to_lowercase().contains(t.as_str()));
                if !used {
                    self.hub.log("info", "audio", format!("{} is present with {} capture channels — add [audio.inputs.<name>] entries (target = \"{}\", channels = [n]) for drum/vocal stems (docs/audio.md)", d.description, d.ports, d.name));
                }
                self.stem_hint = true;
            }
        }
        let leaked = self.stats.leaked.load(Ordering::Relaxed);
        if leaked > 0 {
            self.hub.log("warn", "audio", format!("{leaked} RT objects leaked (garbage ring full)"));
        }
        let _ = &self.xrun_base;
    }

    fn mix_value(&self) -> Value {
        let snap = self.hub.snapshot.load();
        let latest = self.ana.latest.lock().clone();
        let st = &self.stats;
        let pw = self.pw.as_ref().map(|p| p.status.lock().clone()).unwrap_or_default();
        let active: Vec<Value> = self
            .m
            .bus_fx
            .iter()
            .enumerate()
            .map(|(k, a)| Value::map().with("address", a.clone()).with("active", st.fx_active.get(k).map(|x| x.load(Ordering::Relaxed) != 0).unwrap_or(false)))
            .collect();
        let slots: Vec<Value> = self
            .slots
            .iter()
            .map(|s| {
                Value::map()
                    .with("name", s.name.clone())
                    .with("address", s.addr.clone())
                    .with("bus", self.cfg.route_slot(&s.name).map(|b| Value::Str(self.cfg.buses[b].name.clone())).unwrap_or_default())
            })
            .collect();
        let links: Vec<Value> =
            pw.links.iter().map(|(l, ok)| Value::map().with("port", l.port.clone()).with("target", format!("{:?}", l.to)).with("ok", *ok)).collect();
        let consumers: BTreeMap<String, Value> =
            pw.consumers.iter().map(|(k, v)| (k.clone(), Value::List(v.iter().cloned().map(Value::Str).collect()))).collect();
        let rate = st.rate.load(Ordering::Relaxed).max(1) as f64;
        let _ = &snap;
        self.m
            .info
            .clone()
            .with("generation", self.generation as i64)
            .with("fx_active", Value::List(active))
            .with("slots", Value::List(slots))
            .with("analysis", Value::Map(latest))
            .with("links", Value::List(links))
            .with("consumers", Value::Map(consumers))
            .with(
                "perf",
                Value::map()
                    .with("xruns", st.xruns.load(Ordering::Relaxed) as i64)
                    .with("quantum", st.quantum.load(Ordering::Relaxed) as i64)
                    .with("rate", rate as i64)
                    .with("dsp_ms", st.dsp_ns.load(Ordering::Relaxed) as f64 / 1e6)
                    .with("latency_ms", (st.quantum.load(Ordering::Relaxed) as f64 + self.m.latency as f64) * 1000.0 / rate)
                    .with("allocs", st.allocs.load(Ordering::Relaxed) as i64)
                    .with("rt_policy", crate::rt::policy_name(st.policy.load(Ordering::Relaxed)))
                    .with("rt_priority", st.priority.load(Ordering::Relaxed) as i64),
            )
            .with(
                "pipewire",
                Value::map()
                    .with("connected", pw.connected)
                    .with("state", pw.state.clone())
                    .with("error", pw.error.clone().unwrap_or_default())
                    .with("reconnects", pw.reconnects as i64),
            )
            .with("errors", Value::List(self.config_errors.iter().chain(&self.m.errors).cloned().map(Value::Str).collect()))
            .with("duck_db", f32::from_bits(st.duck_db.load(Ordering::Relaxed)) as f64)
    }

    /// Main loop (blocking; run on a dedicated thread).
    pub fn run(mut self, rx: std::sync::mpsc::Receiver<CtlMsg>) {
        loop {
            match rx.recv_timeout(Duration::from_millis(2)) {
                Ok(CtlMsg::Config(c)) => self.apply_config(&c),
                Ok(CtlMsg::Action(cmd)) => self.action(*cmd),
                Ok(CtlMsg::Shutdown) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            }
            self.flush();
            self.poll_slots();
            self.poll_taps();
            self.sync_params();
            self.drain();
            if self.last_meter.elapsed() >= Duration::from_millis(20) {
                self.last_meter = Instant::now();
                self.meters();
            }
            if self.last_second.elapsed() >= Duration::from_secs(1) {
                self.last_second = Instant::now();
                self.perf();
            }
            if self.last_mix.elapsed() >= Duration::from_millis(100) {
                self.last_mix = Instant::now();
                *self.shared.mix.lock() = self.mix_value();
            }
            if self.last_health.elapsed() >= Duration::from_secs(5) {
                self.last_health = Instant::now();
                self.health();
            }
        }
        // stop PipeWire first (RT callbacks end), then drop what came back
        self.pw = None;
        self.drain();
    }
}

/// Expression scope over the snapshot (state, signals, `mode`, `scene`).
struct SnapScope<'a>(&'a Snapshot);

impl se_expr::Scope for SnapScope<'_> {
    fn lookup(&self, path: &str) -> Value {
        match path {
            "mode" => return self.0.get("show.mode").cloned().unwrap_or_default(),
            "scene" => return self.0.get("show.scene.program").cloned().unwrap_or_default(),
            _ => {}
        }
        if let Some(v) = self.0.get(path) {
            return v.clone();
        }
        self.0.signal(path).map(|v| Value::Float(v as f64)).unwrap_or_default()
    }
}

/// `audio.bus.music.fx.stutter.division` → `audio.bus.music.fx.stutter`.
fn fx_prefix(a: &str) -> Option<String> {
    let i = a.find(".fx.")?;
    let name = a[i + 4..].split('.').next()?;
    Some(format!("{}.fx.{name}", &a[..i]))
}

pub fn read_config(ctx: &EngineCtx) -> Result<AudioConfig, String> {
    let section = ctx.project_section("audio").and_then(|v| v.as_table().cloned()).unwrap_or_default();
    config::parse(&section, &ctx.kind("audio"))
}

/// Peak above which the mic counts as picking something up (≈ −60 dBFS: room tone, not silence).
const MIC_PRESENT: f64 = 0.001;

/// Mic preflight: sound within the last minute passes; a silent mic warns (muted, unplugged, or
/// the wrong input) once it's been watched for 10 s.
fn mic_health(input: &str, since_heard: Option<std::time::Duration>, watched: std::time::Duration) -> Value {
    let h = |s: &str, d: String| Value::map().with("status", s).with("detail", d);
    match since_heard {
        Some(d) if d.as_secs() < 60 => h("pass", format!("mic `{input}` is picking up sound")),
        _ if watched.as_secs() < 10 => h("pass", format!("listening to mic `{input}`…")),
        Some(_) => h("warn", format!("no sound from mic `{input}` in the last minute: is it muted or unplugged?")),
        None => h("warn", format!("no sound from mic `{input}` yet: is it muted or unplugged?")),
    }
}

#[cfg(test)]
mod mic_tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn silent_mic_warns_after_a_grace_period_and_recent_sound_passes() {
        let st = |heard: Option<u64>, watched: u64| {
            mic_health("vox", heard.map(Duration::from_secs), Duration::from_secs(watched)).get_path("status").and_then(Value::as_str).unwrap().to_string()
        };
        assert_eq!(st(None, 3), "pass", "just started listening");
        assert_eq!(st(None, 30), "warn", "never heard");
        assert_eq!(st(Some(5), 300), "pass");
        assert_eq!(st(Some(59), 300), "pass");
        assert_eq!(st(Some(61), 300), "warn", "went quiet");
    }
}
