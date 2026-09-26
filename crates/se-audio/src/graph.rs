//! The real-time audio graph (runs inside the PipeWire filter's process callback).
//!
//! ```text
//! capture ports ─► inputs (gain · A/V delay · fx chain) ─┐
//! hub.audio slots (adaptive reader · gain · delay) ──────┼─► bus sums ─► bus fx chain ─► duck
//! sampler (audio.play, drum layers) · dsp sources ───────┘      │         · gain/mute · limiter
//!                                                               ▼                      │
//!                                         analysis/taps (pre-chain)     program sum ◄──┘ (latency aligned)
//!                                                                         │ fx · gain · limiter
//!                                   bus/program output ports (se-<bus> nodes), monitor, direct
//! ```
//!
//! Everything here is built on the control thread ([`crate::build`]) and moved in through
//! [`RtMsg`]; the callback never allocates, locks, or does I/O. Replaced objects travel back
//! through [`RtOut::Garbage`] to be dropped off the audio thread.

use crate::dsp::{AvDelay, Meter, SlotReader};
use crate::taps::TapProducer;
use se_analysis::{DrumHit, DrumTriggers};
use se_dsp::sampler::{SampleBank, Sampler};
use se_dsp::slot::CompDelay;
use se_dsp::{Ctx, Effect, FxChain, FxSlot, MAX_BLOCK, Smoother, Transport, db_to_gain};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};

/// Maximum `hub.audio` slots, taps, meters, drum pads.
pub const MAX_SLOTS: usize = 64;
pub const MAX_TAPS: usize = 32;
pub const MAX_METERS: usize = 128;
pub const MAX_PADS: usize = 16;
/// Graph-swap crossfade.
pub const SWAP_MS: f32 = 25.0;

/// Port buffers for one callback (PipeWire DSP ports: mono f32).
pub trait PortIo {
    fn input(&self, port: usize) -> Option<&[f32]>;
    fn output(&mut self, port: usize) -> Option<&mut [f32]>;
}

/// Which chain an effect lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChainRef {
    Input(u8),
    Bus(u8),
    Monitor,
    /// `dsp` audio-source patch (single-slot chain).
    Source(u8),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FxWhat {
    Param(u16),
    /// Trigger payload float `k` (`se_core::triggers::TriggerPayload::floats`), dsp patches.
    Payload(u8),
    Wet,
    Dry,
    Enabled,
    Env,
    Active,
}

/// Addressable parameter inside the RT graph.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    BusGain(u8),
    BusMute(u8),
    BusLimiter(u8),
    BusCeiling(u8),
    InputGain(u8),
    InputMute(u8),
    InputDelay(u8),
    Fx {
        chain: ChainRef,
        slot: u8,
        what: FxWhat,
    },
    DuckActive,
    DuckDepth,
    DuckAttack,
    DuckRelease,
    /// Max of the duck signals (e.g. `mic.talking`), 0–1.
    DuckSignal,
    PadThreshold(u8),
    SourceGain(u8),
    MonitorGain,
    /// Persistent slot parameters (not tied to a graph generation).
    SlotGain(u16),
    SlotMute(u16),
    SlotDelay(u16),
}

impl Target {
    fn slot_scoped(&self) -> bool {
        matches!(self, Target::SlotGain(_) | Target::SlotMute(_) | Target::SlotDelay(_))
    }
}

/// A `hub.audio` producer attached to the graph (persistent across graph swaps).
pub struct SlotEntry {
    pub name: String,
    pub reader: SlotReader,
    pub gain: Smoother,
    pub mute: Smoother,
    pub delay: AvDelay,
    pub l: Vec<f32>,
    pub r: Vec<f32>,
    pub meter: usize,
}

impl SlotEntry {
    pub fn new(name: String, reader: SlotReader, sr: f32, max_delay: usize, meter: usize) -> SlotEntry {
        SlotEntry {
            name,
            reader,
            gain: Smoother::with_ms(1.0, 20.0, sr),
            mute: Smoother::with_ms(1.0, 10.0, sr),
            delay: AvDelay::new(max_delay, (0.02 * sr) as u32),
            l: vec![0.0; MAX_BLOCK],
            r: vec![0.0; MAX_BLOCK],
            meter,
        }
    }
}

/// Where a tap reads from (resolved against the current graph).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TapSource {
    /// Raw capture port (mono).
    Port(u16),
    /// Bus input sum before the chain, one channel.
    BusIn(u8, u8),
    /// Bus input sum, stereo interleaved (analysis).
    BusInStereo(u8),
    /// Input after gain/delay, before its chain, stereo interleaved (mic analysis).
    InputStereo(u8),
}

pub struct TapEntry {
    pub producer: TapProducer,
    pub source: Option<TapSource>,
    pub stereo: bool,
}

/// Control → RT.
pub enum RtMsg {
    Swap(Box<Graph>),
    Param {
        generation: u32,
        target: Target,
        value: f32,
    },
    AddSlot {
        index: u16,
        entry: Box<SlotEntry>,
    },
    SlotRoute {
        generation: u32,
        index: u16,
        bus: Option<u8>,
        direct: Option<(u16, f32)>,
    },
    AddTap {
        index: u16,
        entry: Box<TapEntry>,
    },
    TapRoute {
        generation: u32,
        index: u16,
        source: Option<TapSource>,
    },
    Play {
        generation: u32,
        sound: u16,
        velocity: f32,
        gain_db: f32,
        pan: f32,
        pitch: f32,
    },
    StopSounds,
    /// Safe mix: stop performance effects, sounds, ducking; reset chains.
    Panic,
    /// Beat clock anchor: `beat` at master time `ts`, tempo `bpm`.
    Transport {
        bpm: f32,
        beat: f64,
        ts: u64,
    },
    /// Replace one effect instance (wasm hot reload) with a crossfade.
    SwapFx {
        generation: u32,
        chain: ChainRef,
        slot: u8,
        fx: Box<dyn Effect>,
    },
    /// Emit a click on the monitor output and measure its return on `port` (round trip).
    Measure {
        port: u16,
    },
}

pub enum Garbage {
    Graph(Box<Graph>),
    Effect(Box<dyn Effect>),
    Slot(Box<SlotEntry>),
    Tap(Box<TapEntry>),
}

/// RT → control.
pub enum RtOut {
    Garbage(Garbage),
    /// Graph `generation` is now live.
    Live(u32),
    Hit {
        pad: u8,
        velocity: f32,
        ts: u64,
    },
    /// Round-trip measurement result in samples (None = no return detected).
    Measured(Option<u32>),
}

/// Shared RT statistics (atomics; written by the callback, read by the control thread).
pub struct RtStats {
    pub cycles: AtomicU64,
    pub xruns: AtomicU64,
    /// Callback time of the last cycle and the max since the last read (ns).
    pub dsp_ns: AtomicU64,
    pub dsp_ns_max: AtomicU64,
    /// Sum of callback time / period since last read (for averages), and count.
    pub dsp_ns_sum: AtomicU64,
    pub dsp_count: AtomicU64,
    pub quantum: AtomicU32,
    pub rate: AtomicU32,
    /// Driver clock position (frames) and cycle start (ns) of the last cycle.
    pub position: AtomicU64,
    pub nsec: AtomicU64,
    /// Driver-reported delay to hardware (frames, signed).
    pub delay: AtomicI32,
    /// Thread id and scheduling policy of the data thread (-1 until known).
    pub tid: AtomicI32,
    pub policy: AtomicI32,
    pub priority: AtomicI32,
    /// Messages the RT side had to drop (garbage ring full → leaked instead of freed).
    pub leaked: AtomicU64,
    /// Allocation counter hits inside the callback (debug builds with se-alloc installed).
    pub allocs: AtomicU64,
    pub live_gen: AtomicU32,
    pub duck_db: AtomicU32,
    pub latency: AtomicU32,
    pub meters: Vec<Meter>,
    pub fx_active: Vec<AtomicU32>,
}

impl Default for RtStats {
    fn default() -> Self {
        RtStats {
            cycles: AtomicU64::new(0),
            xruns: AtomicU64::new(0),
            dsp_ns: AtomicU64::new(0),
            dsp_ns_max: AtomicU64::new(0),
            dsp_ns_sum: AtomicU64::new(0),
            dsp_count: AtomicU64::new(0),
            quantum: AtomicU32::new(0),
            rate: AtomicU32::new(0),
            position: AtomicU64::new(0),
            nsec: AtomicU64::new(0),
            delay: AtomicI32::new(0),
            tid: AtomicI32::new(-1),
            policy: AtomicI32::new(-1),
            priority: AtomicI32::new(0),
            leaked: AtomicU64::new(0),
            allocs: AtomicU64::new(0),
            live_gen: AtomicU32::new(0),
            duck_db: AtomicU32::new(0f32.to_bits()),
            latency: AtomicU32::new(0),
            meters: (0..MAX_METERS).map(|_| Meter::default()).collect(),
            fx_active: (0..MAX_METERS).map(|_| AtomicU32::new(0)).collect(),
        }
    }
}

pub struct InputStrip {
    pub ports: [Option<u16>; 2],
    pub bus: u8,
    pub gain: Smoother,
    pub mute: Smoother,
    pub delay: AvDelay,
    pub chain: FxChain,
    pub keys: Vec<Option<u8>>,
    /// Aligns this input with the slowest input chain of its bus.
    pub align: CompDelay,
    pub l: Vec<f32>,
    pub r: Vec<f32>,
    /// After gain/delay, before the chain (mic analysis, monitor pre-chain).
    pub pre_l: Vec<f32>,
    pub pre_r: Vec<f32>,
    pub meter: usize,
}

pub struct BusStrip {
    pub sum_l: Vec<f32>,
    pub sum_r: Vec<f32>,
    pub out_l: Vec<f32>,
    pub out_r: Vec<f32>,
    /// Post-chain, pre-fader (monitor feeds).
    pub post_l: Vec<f32>,
    pub post_r: Vec<f32>,
    pub key: Vec<f32>,
    /// Scratch for the program alignment delay.
    pub al_l: Vec<f32>,
    pub al_r: Vec<f32>,
    pub chain: FxChain,
    pub keys: Vec<Option<u8>>,
    pub gain: Smoother,
    pub mute: Smoother,
    pub limiter: Option<FxSlot>,
    pub ceiling_idx: u16,
    pub ducked: bool,
    /// Aligns this bus to the slowest bus before the program sum.
    pub align: CompDelay,
    pub ports: Option<(u16, u16)>,
    pub to_program: bool,
    pub meter: usize,
    pub is_program: bool,
}

pub struct SamplerStrip {
    pub bus: u8,
    pub sampler: Sampler,
}

pub struct SourceStrip {
    pub fx: FxSlot,
    pub bus: u8,
    pub gain: Smoother,
    pub l: Vec<f32>,
    pub r: Vec<f32>,
}

pub struct MonitorStrip {
    pub buses: Vec<u8>,
    pub inputs: Vec<u8>,
    pub chain: FxChain,
    pub gain: Smoother,
    pub ports: [Option<u16>; 2],
    pub l: Vec<f32>,
    pub r: Vec<f32>,
}

/// Post-fader software buses sent to the physical output feeding the external mixer.
/// The captured 16R/24c mix is never part of this strip.
pub struct PlaybackStrip {
    pub buses: Vec<u8>,
    pub ports: [Option<u16>; 2],
    pub l: Vec<f32>,
    pub r: Vec<f32>,
}

pub struct DrumStage {
    pub trig: DrumTriggers,
    /// (input index, channel) per pad.
    pub pads: Vec<(u8, u8)>,
    /// Sampler layer per pad: (sound index, gain dB).
    pub layers: Vec<Option<(u16, f32)>>,
    pub bufs: Vec<Vec<f32>>,
}

pub struct Ducker {
    pub targets: Vec<u8>,
    pub keys: Vec<u8>,
    pub manual: bool,
    pub signal: f32,
    pub depth_db: f32,
    pub attack_ms: f32,
    pub release_ms: f32,
    pub threshold: f32,
    pub hold: u32,
    hold_left: u32,
    key_env: f32,
    cur_db: f32,
    gain: f32,
}

impl Ducker {
    pub fn new(targets: Vec<u8>, keys: Vec<u8>, depth_db: f32, attack_ms: f32, release_ms: f32, threshold_db: f32, hold: u32) -> Ducker {
        Ducker {
            targets,
            keys,
            manual: false,
            signal: 0.0,
            depth_db,
            attack_ms,
            release_ms,
            threshold: db_to_gain(threshold_db),
            hold,
            hold_left: 0,
            key_env: 0.0,
            cur_db: 0.0,
            gain: 1.0,
        }
    }

    /// Current gain reduction (dB, ≤ 0).
    pub fn db(&self) -> f32 {
        self.cur_db
    }

    /// Advance one block: returns (gain at block start, gain at block end), linear.
    fn step(&mut self, key_ms: f32, n: usize, sr: f32) -> (f32, f32) {
        // key envelope: fast attack, 50 ms release on the key bus mean square
        let k = key_ms.sqrt();
        let rel = (-(n as f32) / (0.05 * sr)).exp();
        self.key_env = if k > self.key_env { k } else { self.key_env * rel + k * (1.0 - rel) };
        let key_on = self.key_env > self.threshold;
        if key_on || self.manual || self.signal > 0.5 {
            self.hold_left = self.hold;
        }
        let on = key_on || self.manual || self.signal > 0.5 || self.hold_left > 0;
        self.hold_left = self.hold_left.saturating_sub(n as u32);
        let target = if on { self.depth_db } else { 0.0 };
        let ms = if target < self.cur_db { self.attack_ms } else { self.release_ms };
        let step = self.depth_db.abs().max(1.0) * n as f32 / (ms.max(1.0) * 0.001 * sr);
        let d = target - self.cur_db;
        self.cur_db += d.clamp(-step, step);
        let start = self.gain;
        self.gain = db_to_gain(self.cur_db);
        (start, self.gain)
    }

    fn reset(&mut self) {
        self.manual = false;
        self.signal = 0.0;
        self.hold_left = 0;
        self.key_env = 0.0;
    }
}

/// Monitor round-trip measurement state.
struct Measure {
    port: u16,
    sent_at: u64,
    phase: u8,
    timeout: u64,
}

pub struct Graph {
    pub generation: u32,
    pub sr: f32,
    pub inputs: Vec<InputStrip>,
    pub buses: Vec<BusStrip>,
    /// Bus index per slot index (set by `SlotRoute`), and direct-output port + gain.
    pub slot_bus: Vec<Option<u8>>,
    pub slot_direct: Vec<Option<(u16, f32)>>,
    pub samplers: Vec<SamplerStrip>,
    pub sound_bus: Vec<u8>,
    pub bank: Box<SampleBank>,
    pub sources: Vec<SourceStrip>,
    pub monitor: Option<MonitorStrip>,
    pub playback: Option<PlaybackStrip>,
    pub drums: Option<DrumStage>,
    pub duck: Ducker,
    /// Index of the program bus.
    pub program: usize,
    /// Output ports written by this graph (zeroed first).
    pub out_ports: Vec<u16>,
    /// Program latency (chain + alignment), samples.
    pub latency: usize,
    tmp_l: Vec<f32>,
    tmp_r: Vec<f32>,
}

impl Graph {
    /// Assemble a graph; buffers are allocated here (control thread).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        generation: u32,
        sr: f32,
        inputs: Vec<InputStrip>,
        buses: Vec<BusStrip>,
        samplers: Vec<SamplerStrip>,
        sound_bus: Vec<u8>,
        bank: Box<SampleBank>,
        sources: Vec<SourceStrip>,
        monitor: Option<MonitorStrip>,
        playback: Option<PlaybackStrip>,
        drums: Option<DrumStage>,
        duck: Ducker,
    ) -> Graph {
        let program = buses.iter().position(|b| b.is_program).expect("program bus");
        let mut out_ports = Vec::new();
        for b in &buses {
            if let Some((l, r)) = b.ports {
                out_ports.push(l);
                out_ports.push(r);
            }
        }
        if let Some(m) = &monitor {
            out_ports.extend(m.ports.iter().flatten());
        }
        if let Some(p) = &playback {
            out_ports.extend(p.ports.iter().flatten());
        }
        let mut g = Graph {
            generation,
            sr,
            inputs,
            buses,
            slot_bus: vec![None; MAX_SLOTS],
            slot_direct: vec![None; MAX_SLOTS],
            samplers,
            sound_bus,
            bank,
            sources,
            monitor,
            playback,
            drums,
            duck,
            program,
            out_ports,
            latency: 0,
            tmp_l: vec![0.0; MAX_BLOCK],
            tmp_r: vec![0.0; MAX_BLOCK],
        };
        g.align();
        g
    }

    /// Delay faster inputs (per bus) and faster buses so everything reaches the program sum
    /// time-aligned (latency compensation, §8.5).
    fn align(&mut self) {
        let mut in_lat = vec![0usize; self.buses.len()];
        for i in &self.inputs {
            let b = i.bus as usize;
            in_lat[b] = in_lat[b].max(i.chain.latency());
        }
        for i in &mut self.inputs {
            i.align.set_delay(in_lat[i.bus as usize] - i.chain.latency());
        }
        let lat: Vec<usize> =
            self.buses.iter().enumerate().map(|(k, b)| in_lat[k] + b.chain.latency() + b.limiter.as_ref().map(FxSlot::latency).unwrap_or(0)).collect();
        let max = self.buses.iter().zip(&lat).filter(|(b, _)| b.to_program).map(|(_, l)| *l).max().unwrap_or(0);
        for (b, l) in self.buses.iter_mut().zip(&lat) {
            b.align.set_delay(if b.to_program { max - l } else { 0 });
        }
        self.latency = max + lat[self.program];
    }

    pub fn declare_ports(&self, out: &mut Vec<u16>) {
        out.extend_from_slice(&self.out_ports);
    }

    fn fx_slot(&mut self, chain: ChainRef, slot: u8) -> Option<&mut FxSlot> {
        let s = slot as usize;
        match chain {
            ChainRef::Input(i) => self.inputs.get_mut(i as usize)?.chain.slots.get_mut(s),
            ChainRef::Bus(b) => self.buses.get_mut(b as usize)?.chain.slots.get_mut(s),
            ChainRef::Monitor => self.monitor.as_mut()?.chain.slots.get_mut(s),
            ChainRef::Source(i) => (s == 0).then(|| self.sources.get_mut(i as usize).map(|x| &mut x.fx)).flatten(),
        }
    }

    fn set(&mut self, t: Target, v: f32) {
        match t {
            Target::BusGain(b) => {
                if let Some(x) = self.buses.get_mut(b as usize) {
                    x.gain.set(db_to_gain(v.clamp(-100.0, 24.0)));
                }
            }
            Target::BusMute(b) => {
                if let Some(x) = self.buses.get_mut(b as usize) {
                    x.mute.set(if v > 0.5 { 0.0 } else { 1.0 });
                }
            }
            Target::BusLimiter(b) => {
                if let Some(l) = self.buses.get_mut(b as usize).and_then(|x| x.limiter.as_mut()) {
                    l.set_enabled(v > 0.5);
                }
            }
            Target::BusCeiling(b) => {
                if let Some(x) = self.buses.get_mut(b as usize) {
                    let idx = x.ceiling_idx as usize;
                    if let Some(l) = x.limiter.as_mut() {
                        l.set_param(idx, v);
                    }
                }
            }
            Target::InputGain(i) => {
                if let Some(x) = self.inputs.get_mut(i as usize) {
                    x.gain.set(db_to_gain(v.clamp(-100.0, 24.0)));
                }
            }
            Target::InputMute(i) => {
                if let Some(x) = self.inputs.get_mut(i as usize) {
                    x.mute.set(if v > 0.5 { 0.0 } else { 1.0 });
                }
            }
            Target::InputDelay(i) => {
                let sr = self.sr;
                if let Some(x) = self.inputs.get_mut(i as usize) {
                    x.delay.set((v.max(0.0) * 0.001 * sr) as usize);
                }
            }
            Target::Fx { chain, slot, what } => {
                if let Some(s) = self.fx_slot(chain, slot) {
                    match what {
                        FxWhat::Param(p) => s.set_param(p as usize, v),
                        FxWhat::Payload(k) => s.set_payload(k as usize, v),
                        FxWhat::Wet => s.set_wet(v),
                        FxWhat::Dry => s.set_dry(v),
                        FxWhat::Enabled => s.set_enabled(v > 0.5),
                        FxWhat::Env => s.set_env(v),
                        FxWhat::Active => s.set_trigger(v > 0.5),
                    }
                }
            }
            Target::DuckActive => self.duck.manual = v > 0.5,
            Target::DuckDepth => self.duck.depth_db = v.clamp(-60.0, 0.0),
            Target::DuckAttack => self.duck.attack_ms = v.max(1.0),
            Target::DuckRelease => self.duck.release_ms = v.max(1.0),
            Target::DuckSignal => self.duck.signal = v,
            Target::PadThreshold(p) => {
                if let Some(d) = self.drums.as_mut()
                    && (p as usize) < d.pads.len()
                {
                    d.trig.set_threshold_db(p as usize, v);
                }
            }
            Target::SourceGain(i) => {
                if let Some(s) = self.sources.get_mut(i as usize) {
                    s.gain.set(db_to_gain(v.clamp(-100.0, 24.0)));
                }
            }
            Target::MonitorGain => {
                if let Some(m) = self.monitor.as_mut() {
                    m.gain.set(db_to_gain(v.clamp(-100.0, 24.0)));
                }
            }
            Target::SlotGain(_) | Target::SlotMute(_) | Target::SlotDelay(_) => {}
        }
    }

    fn panic(&mut self) {
        let sr = self.sr;
        for s in &mut self.samplers {
            s.sampler.stop_all((0.01 * sr) as u32);
        }
        for i in &mut self.inputs {
            for s in &mut i.chain.slots {
                s.set_trigger(false);
                s.set_env(0.0);
            }
        }
        for b in &mut self.buses {
            for s in &mut b.chain.slots {
                s.set_trigger(false);
                s.set_env(0.0);
            }
        }
        self.duck.reset();
    }

    /// Render one chunk (≤ MAX_BLOCK). Writes bus, monitor, and playback outputs into internal buffers.
    #[allow(clippy::too_many_arguments)]
    fn render(
        &mut self,
        io: &dyn PortIo,
        off: usize,
        n: usize,
        slots: &mut [Option<Box<SlotEntry>>],
        transport: Transport,
        frame: u64,
        ts: u64,
        out: &mut rtrb::Producer<RtOut>,
    ) {
        let sr = self.sr;
        for b in &mut self.buses {
            b.sum_l[..n].fill(0.0);
            b.sum_r[..n].fill(0.0);
        }
        // drum triggers on raw capture (before gain/fx), fire sampler layers immediately
        if let Some(d) = self.drums.as_mut() {
            let mut refs: [&[f32]; MAX_PADS] = [&[]; MAX_PADS];
            let np = d.pads.len().min(MAX_PADS);
            for (k, (inp, ch)) in d.pads.iter().enumerate().take(np) {
                let port = self.inputs.get(*inp as usize).and_then(|i| i.ports[*ch as usize]);
                let buf = &mut d.bufs[k][..n];
                match port.and_then(|p| io.input(p as usize)) {
                    Some(src) if src.len() >= off + n => buf.copy_from_slice(&src[off..off + n]),
                    _ => buf.fill(0.0),
                }
            }
            for (r, b) in refs.iter_mut().zip(&d.bufs).take(np) {
                *r = &b[..n];
            }
            let layers = &d.layers;
            let samplers = &mut self.samplers;
            let bank = &mut self.bank;
            let sound_bus = &self.sound_bus;
            let mut on_hit = |h: DrumHit| {
                let ts_hit = ts + ((h.frame.saturating_sub(frame)) as f64 * 1e9 / sr as f64) as u64;
                let _ = out.push(RtOut::Hit { pad: h.pad as u8, velocity: h.velocity, ts: ts_hit });
                if let Some(Some((sound, gain_db))) = layers.get(h.pad) {
                    let bus = sound_bus.get(*sound as usize).copied();
                    if let Some(s) = samplers.iter_mut().find(|s| Some(s.bus) == bus) {
                        s.sampler.play(bank, *sound as usize, h.velocity, db_to_gain(*gain_db), 0.0, 0.0);
                    }
                }
            };
            d.trig.process(&refs[..np], frame, &mut on_hit);
        }
        // inputs
        for inp in &mut self.inputs {
            let (l, r) = (&mut inp.l[..n], &mut inp.r[..n]);
            let src_l = inp.ports[0].and_then(|p| io.input(p as usize)).filter(|s| s.len() >= off + n);
            let src_r = inp.ports[1].and_then(|p| io.input(p as usize)).filter(|s| s.len() >= off + n);
            match src_l {
                Some(s) => l.copy_from_slice(&s[off..off + n]),
                None => l.fill(0.0),
            }
            match (src_r, inp.ports[1]) {
                (Some(s), _) => r.copy_from_slice(&s[off..off + n]),
                (None, None) => r.copy_from_slice(l),
                (None, Some(_)) => r.fill(0.0),
            }
            for i in 0..n {
                let g = inp.gain.next() * inp.mute.next();
                l[i] *= g;
                r[i] *= g;
            }
            inp.delay.process(l, r);
            inp.pre_l[..n].copy_from_slice(l);
            inp.pre_r[..n].copy_from_slice(r);
        }
        for k in 0..self.inputs.len() {
            let (bus, has_fx) = (self.inputs[k].bus as usize, !self.inputs[k].chain.slots.is_empty());
            if has_fx {
                let inp = &mut self.inputs[k];
                process_chain(&mut inp.chain, &inp.keys, &self.buses, sr, transport, &mut inp.l[..n], &mut inp.r[..n]);
            }
            {
                let inp = &mut self.inputs[k];
                inp.align.process(&mut inp.l[..n], &mut inp.r[..n]);
            }
            let inp = &self.inputs[k];
            let b = &mut self.buses[bus];
            for i in 0..n {
                b.sum_l[i] += inp.l[i];
                b.sum_r[i] += inp.r[i];
            }
        }
        // slots
        for (idx, s) in slots.iter_mut().enumerate() {
            let Some(s) = s.as_mut() else { continue };
            if let Some(bus) = self.slot_bus.get(idx).copied().flatten()
                && let Some(b) = self.buses.get_mut(bus as usize)
            {
                for i in 0..n {
                    b.sum_l[i] += s.l[i];
                    b.sum_r[i] += s.r[i];
                }
            }
        }
        // sampler voices and dsp sources
        for s in &mut self.samplers {
            let b = &mut self.buses[s.bus as usize];
            s.sampler.process(&self.bank, &mut b.sum_l[..n], &mut b.sum_r[..n]);
        }
        for src in &mut self.sources {
            let (l, r) = (&mut src.l[..n], &mut src.r[..n]);
            l.fill(0.0);
            r.fill(0.0);
            let ctx = Ctx { sr, transport, key: &[] };
            src.fx.process(&ctx, l, r);
            let b = &mut self.buses[src.bus as usize];
            for i in 0..n {
                let g = src.gain.next();
                b.sum_l[i] += l[i] * g;
                b.sum_r[i] += r[i] * g;
            }
        }
        // sidechain keys (mono bus input sums)
        for b in &mut self.buses {
            for i in 0..n {
                b.key[i] = 0.5 * (b.sum_l[i] + b.sum_r[i]);
            }
        }
        // ducking
        let mut key_ms = 0.0f32;
        for k in &self.duck.keys {
            let b = &self.buses[*k as usize];
            let ms = b.key[..n].iter().map(|x| x * x).sum::<f32>() / n.max(1) as f32;
            key_ms = key_ms.max(ms);
        }
        let (g0, g1) = self.duck.step(key_ms, n, sr);
        // bus chains → duck → fader → limiter
        let prog = self.program;
        for k in 0..self.buses.len() {
            if k == prog {
                continue;
            }
            {
                let bus = &mut self.buses[k];
                bus.out_l[..n].copy_from_slice(&bus.sum_l[..n]);
                bus.out_r[..n].copy_from_slice(&bus.sum_r[..n]);
            }
            if !self.buses[k].chain.slots.is_empty() {
                let (before, rest) = self.buses.split_at_mut(k);
                let (bus, after) = rest.split_first_mut().expect("bus");
                let keys = KeySrc { before, after, own: k };
                process_chain_split(&mut bus.chain, &bus.keys, &keys, sr, transport, &mut bus.out_l[..n], &mut bus.out_r[..n]);
            }
            let bus = &mut self.buses[k];
            bus.post_l[..n].copy_from_slice(&bus.out_l[..n]);
            bus.post_r[..n].copy_from_slice(&bus.out_r[..n]);
            let duck = if bus.ducked { Some((g0, g1)) } else { None };
            finish_bus(bus, n, sr, transport, duck);
        }
        // program sum (aligned)
        {
            let (tl, tr) = (&mut self.tmp_l[..n], &mut self.tmp_r[..n]);
            tl.fill(0.0);
            tr.fill(0.0);
            for (k, b) in self.buses.iter_mut().enumerate() {
                if k == prog || !b.to_program {
                    continue;
                }
                // alignment delay runs in place on a copy of the output
                let (al, ar) = (&mut b.al_l[..n], &mut b.al_r[..n]);
                al.copy_from_slice(&b.out_l[..n]);
                ar.copy_from_slice(&b.out_r[..n]);
                b.align.process(al, ar);
                for i in 0..n {
                    tl[i] += al[i];
                    tr[i] += ar[i];
                }
            }
            let p = &mut self.buses[prog];
            p.sum_l[..n].copy_from_slice(tl);
            p.sum_r[..n].copy_from_slice(tr);
            p.out_l[..n].copy_from_slice(tl);
            p.out_r[..n].copy_from_slice(tr);
        }
        if !self.buses[prog].chain.slots.is_empty() {
            let (before, rest) = self.buses.split_at_mut(prog);
            let (bus, after) = rest.split_first_mut().expect("program");
            let keys = KeySrc { before, after, own: prog };
            process_chain_split(&mut bus.chain, &bus.keys, &keys, sr, transport, &mut bus.out_l[..n], &mut bus.out_r[..n]);
        }
        {
            let p = &mut self.buses[prog];
            p.post_l[..n].copy_from_slice(&p.out_l[..n]);
            p.post_r[..n].copy_from_slice(&p.out_r[..n]);
            finish_bus(p, n, sr, transport, None);
        }
        // monitor
        if let Some(m) = self.monitor.as_mut() {
            let (l, r) = (&mut m.l[..n], &mut m.r[..n]);
            l.fill(0.0);
            r.fill(0.0);
            for b in &m.buses {
                let b = &self.buses[*b as usize];
                for i in 0..n {
                    l[i] += b.post_l[i];
                    r[i] += b.post_r[i];
                }
            }
            for k in &m.inputs {
                let x = &self.inputs[*k as usize];
                for i in 0..n {
                    l[i] += x.l[i];
                    r[i] += x.r[i];
                }
            }
            let ctx = Ctx { sr, transport, key: &[] };
            m.chain.process(&ctx, l, r);
            for i in 0..n {
                let g = m.gain.next();
                l[i] *= g;
                r[i] *= g;
            }
        }
        if let Some(p) = self.playback.as_mut() {
            let (l, r) = (&mut p.l[..n], &mut p.r[..n]);
            l.fill(0.0);
            r.fill(0.0);
            for b in &p.buses {
                let bus = &self.buses[*b as usize];
                for i in 0..n {
                    l[i] += bus.out_l[i];
                    r[i] += bus.out_r[i];
                }
            }
        }
    }

    /// Write this graph's outputs for the chunk. `ramp` = (from, to) gain across the chunk
    /// (graph swap crossfade); `add` mixes into what's already in the port buffer.
    fn write_ports(&self, io: &mut dyn PortIo, off: usize, n: usize, ramp: (f32, f32), add: bool) {
        let step = (ramp.1 - ramp.0) / n.max(1) as f32;
        let mut put = |port: u16, src: &[f32]| {
            if let Some(dst) = io.output(port as usize)
                && dst.len() >= off + n
            {
                let dst = &mut dst[off..off + n];
                if ramp == (1.0, 1.0) && !add {
                    dst.copy_from_slice(&src[..n]);
                } else {
                    for i in 0..n {
                        let g = ramp.0 + step * i as f32;
                        let v = src[i] * g;
                        dst[i] = if add { dst[i] + v } else { v };
                    }
                }
            }
        };
        for b in &self.buses {
            if let Some((pl, pr)) = b.ports {
                put(pl, &b.out_l);
                put(pr, &b.out_r);
            }
        }
        if let Some(m) = &self.monitor {
            if let Some(p) = m.ports[0] {
                put(p, &m.l);
            }
            if let Some(p) = m.ports[1] {
                put(p, &m.r);
            }
        }
        if let Some(p) = &self.playback {
            if let Some(port) = p.ports[0] {
                put(port, &p.l);
            }
            if let Some(port) = p.ports[1] {
                put(port, &p.r);
            }
        }
    }

    fn tap_block<'a>(&'a self, src: TapSource, io: &'a dyn PortIo, off: usize, n: usize) -> Option<(&'a [f32], Option<&'a [f32]>)> {
        match src {
            TapSource::Port(p) => io.input(p as usize).filter(|s| s.len() >= off + n).map(|s| (&s[off..off + n], None)),
            TapSource::BusIn(b, ch) => {
                let b = self.buses.get(b as usize)?;
                Some((if ch == 0 { &b.sum_l[..n] } else { &b.sum_r[..n] }, None))
            }
            TapSource::BusInStereo(b) => {
                let b = self.buses.get(b as usize)?;
                Some((&b.sum_l[..n], Some(&b.sum_r[..n])))
            }
            TapSource::InputStereo(i) => {
                let x = self.inputs.get(i as usize)?;
                Some((&x.pre_l[..n], Some(&x.pre_r[..n])))
            }
        }
    }
}

/// Sidechain key lookup across a split bus slice.
struct KeySrc<'a> {
    before: &'a [BusStrip],
    after: &'a [BusStrip],
    own: usize,
}

impl KeySrc<'_> {
    fn key(&self, bus: usize) -> Option<&[f32]> {
        if bus < self.own {
            Some(&self.before[bus].key)
        } else if bus > self.own {
            Some(&self.after[bus - self.own - 1].key)
        } else {
            None
        }
    }
}

fn process_chain(chain: &mut FxChain, keys: &[Option<u8>], buses: &[BusStrip], sr: f32, transport: Transport, l: &mut [f32], r: &mut [f32]) {
    let n = l.len();
    for (k, s) in chain.slots.iter_mut().enumerate() {
        let key: &[f32] = match keys.get(k).copied().flatten() {
            Some(b) => buses.get(b as usize).map(|x| &x.key[..n]).unwrap_or(&[]),
            None => &[],
        };
        let ctx = Ctx { sr, transport, key };
        s.process(&ctx, l, r);
    }
}

fn process_chain_split(chain: &mut FxChain, keys: &[Option<u8>], src: &KeySrc, sr: f32, transport: Transport, l: &mut [f32], r: &mut [f32]) {
    let n = l.len();
    for (k, s) in chain.slots.iter_mut().enumerate() {
        let key: &[f32] = match keys.get(k).copied().flatten() {
            Some(b) => src.key(b as usize).map(|x| &x[..n]).unwrap_or(&[]),
            None => &[],
        };
        let ctx = Ctx { sr, transport, key };
        s.process(&ctx, l, r);
    }
}

fn finish_bus(bus: &mut BusStrip, n: usize, sr: f32, transport: Transport, duck: Option<(f32, f32)>) {
    let (l, r) = (&mut bus.out_l[..n], &mut bus.out_r[..n]);
    let dstep = duck.map(|(a, b)| (a, (b - a) / n.max(1) as f32));
    for i in 0..n {
        let mut g = bus.gain.next() * bus.mute.next();
        if let Some((a, s)) = dstep {
            g *= a + s * i as f32;
        }
        l[i] *= g;
        r[i] *= g;
    }
    if let Some(lim) = bus.limiter.as_mut() {
        let ctx = Ctx { sr, transport, key: &[] };
        lim.process(&ctx, l, r);
    }
}

/// Clock info for one callback.
#[derive(Clone, Copy, Debug, Default)]
pub struct Cycle {
    /// Cycle start, master clock ns.
    pub nsec: u64,
    pub rate: u32,
}

/// RT-side state owned by the process callback.
pub struct Engine {
    graph: Option<Box<Graph>>,
    fading: Option<Box<Graph>>,
    fade: Smoother,
    /// Samples the new graph stays muted after a swap (its delay lines fill with real audio
    /// before the crossfade starts).
    hold: u32,
    slots: Vec<Option<Box<SlotEntry>>>,
    taps: Vec<Option<Box<TapEntry>>>,
    rx: rtrb::Consumer<RtMsg>,
    tx: rtrb::Producer<RtOut>,
    pub stats: Arc<RtStats>,
    sr: f32,
    frame: u64,
    bpm: f32,
    beat: f64,
    anchor: Option<(f64, u64, f32)>,
    measure: Option<Measure>,
    interleave: Vec<f32>,
}

impl Engine {
    pub fn new(sr: f32, rx: rtrb::Consumer<RtMsg>, tx: rtrb::Producer<RtOut>, stats: Arc<RtStats>) -> Engine {
        Engine {
            graph: None,
            fading: None,
            fade: Smoother::with_ms(1.0, SWAP_MS, sr),
            hold: 0,
            slots: (0..MAX_SLOTS).map(|_| None).collect(),
            taps: (0..MAX_TAPS).map(|_| None).collect(),
            rx,
            tx,
            stats,
            sr,
            frame: 0,
            bpm: 120.0,
            beat: 0.0,
            anchor: None,
            measure: None,
            interleave: vec![0.0; MAX_BLOCK * 2],
        }
    }

    fn trash(&mut self, g: Garbage) {
        if let Err(rtrb::PushError::Full(RtOut::Garbage(g))) = self.tx.push(RtOut::Garbage(g)) {
            // never free on the audio thread: leak instead (counted; the ring is sized so
            // this does not happen in practice)
            std::mem::forget(g);
            self.stats.leaked.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn apply(&mut self, m: RtMsg) {
        let generation = self.graph.as_ref().map(|g| g.generation);
        match m {
            RtMsg::Swap(g) => {
                let new_gen = g.generation;
                self.stats.latency.store(g.latency as u32, Ordering::Relaxed);
                if let Some(old) = self.graph.replace(g) {
                    if let Some(f) = self.fading.replace(old) {
                        self.trash(Garbage::Graph(f));
                    }
                    self.fade.reset(0.0);
                    self.fade.set(1.0);
                    self.hold = self.stats.latency.load(Ordering::Relaxed);
                } else {
                    self.fade.reset(1.0);
                }
                self.stats.live_gen.store(new_gen, Ordering::Relaxed);
                let _ = self.tx.push(RtOut::Live(new_gen));
            }
            RtMsg::Param { generation: g, target, value } => {
                if target.slot_scoped() {
                    let sr = self.sr;
                    let idx = match target {
                        Target::SlotGain(i) | Target::SlotMute(i) | Target::SlotDelay(i) => i as usize,
                        _ => unreachable!(),
                    };
                    if let Some(Some(s)) = self.slots.get_mut(idx) {
                        match target {
                            Target::SlotGain(_) => s.gain.set(db_to_gain(value.clamp(-100.0, 24.0))),
                            Target::SlotMute(_) => s.mute.set(if value > 0.5 { 0.0 } else { 1.0 }),
                            _ => s.delay.set((value.max(0.0) * 0.001 * sr) as usize),
                        }
                    }
                } else if Some(g) == generation
                    && let Some(graph) = self.graph.as_mut()
                {
                    graph.set(target, value);
                }
            }
            RtMsg::AddSlot { index, entry } => {
                if let Some(cell) = self.slots.get_mut(index as usize) {
                    if let Some(old) = cell.replace(entry) {
                        self.trash(Garbage::Slot(old));
                    }
                } else {
                    self.trash(Garbage::Slot(entry));
                }
            }
            RtMsg::SlotRoute { generation: g, index, bus, direct } => {
                if Some(g) == generation
                    && let Some(graph) = self.graph.as_mut()
                    && (index as usize) < MAX_SLOTS
                {
                    graph.slot_bus[index as usize] = bus;
                    graph.slot_direct[index as usize] = direct;
                }
            }
            RtMsg::AddTap { index, entry } => {
                if let Some(cell) = self.taps.get_mut(index as usize) {
                    if let Some(old) = cell.replace(entry) {
                        self.trash(Garbage::Tap(old));
                    }
                } else {
                    self.trash(Garbage::Tap(entry));
                }
            }
            RtMsg::TapRoute { generation: g, index, source } => {
                if Some(g) == generation
                    && let Some(Some(t)) = self.taps.get_mut(index as usize)
                {
                    t.source = source;
                }
            }
            RtMsg::Play { generation: g, sound, velocity, gain_db, pan, pitch } => {
                if Some(g) == generation
                    && let Some(graph) = self.graph.as_mut()
                    && let Some(bus) = graph.sound_bus.get(sound as usize).copied()
                    && let Some(s) = graph.samplers.iter_mut().find(|s| s.bus == bus)
                {
                    s.sampler.play(&mut graph.bank, sound as usize, velocity, db_to_gain(gain_db), pan, pitch);
                }
            }
            RtMsg::StopSounds => {
                let sr = self.sr;
                if let Some(graph) = self.graph.as_mut() {
                    for s in &mut graph.samplers {
                        s.sampler.stop_all((0.01 * sr) as u32);
                    }
                }
            }
            RtMsg::Panic => {
                if let Some(graph) = self.graph.as_mut() {
                    graph.panic();
                }
            }
            RtMsg::Transport { bpm, beat, ts } => {
                self.anchor = Some((beat, ts, bpm.clamp(20.0, 400.0)));
            }
            RtMsg::SwapFx { generation: g, chain, slot, fx } => {
                let retired =
                    if Some(g) == generation { self.graph.as_mut().and_then(|gr| gr.fx_slot(chain, slot)).map(|s| s.swap_effect(fx)) } else { Some(Some(fx)) };
                if let Some(Some(old)) = retired {
                    self.trash(Garbage::Effect(old));
                }
            }
            RtMsg::Measure { port } => {
                self.measure = Some(Measure { port, sent_at: 0, phase: 0, timeout: 0 });
            }
        }
    }

    /// Advance the beat clock for a block starting at `ts` (master ns) of `n` frames.
    fn transport(&mut self, ts: u64, n: usize) -> Transport {
        if let Some((beat, at, bpm)) = self.anchor {
            let est = beat + (ts as f64 - at as f64) * bpm as f64 / 60e9;
            let err = est - self.beat;
            if err.abs() > 0.5 || (bpm - self.bpm).abs() > 8.0 {
                self.beat = est;
            } else {
                // slew towards the analysis clock (no audible jumps in synced effects)
                self.beat += err * 0.05;
            }
            self.bpm = bpm;
        }
        let t = Transport { bpm: self.bpm, beat: self.beat, beats_per_bar: 4 };
        self.beat += n as f64 * self.bpm as f64 / (60.0 * self.sr as f64);
        t
    }

    /// Process one PipeWire cycle of `frames` frames.
    pub fn process(&mut self, io: &mut dyn PortIo, frames: usize, cycle: Cycle) {
        while let Ok(m) = self.rx.pop() {
            self.apply(m);
        }
        let mut off = 0;
        while off < frames {
            let n = (frames - off).min(MAX_BLOCK);
            let ts = cycle.nsec + (off as f64 * 1e9 / self.sr as f64) as u64;
            self.chunk(io, off, n, ts);
            off += n;
        }
        if self.fading.is_some() && self.fade.settled() {
            let f = self.fading.take().expect("fading");
            self.trash(Garbage::Graph(f));
        }
        // hand retired hot-swapped effects back for dropping
        if let Some(g) = self.graph.as_mut() {
            let mut retired: [Option<Box<dyn Effect>>; 4] = [None, None, None, None];
            let mut k = 0;
            for s in g.inputs.iter_mut().flat_map(|i| i.chain.slots.iter_mut()).chain(g.buses.iter_mut().flat_map(|b| b.chain.slots.iter_mut())) {
                if k < retired.len()
                    && let Some(fx) = s.take_retired()
                {
                    retired[k] = Some(fx);
                    k += 1;
                }
            }
            for fx in retired.into_iter().flatten() {
                self.trash(Garbage::Effect(fx));
            }
        }
    }

    fn chunk(&mut self, io: &mut dyn PortIo, off: usize, n: usize, ts: u64) {
        let transport = self.transport(ts, n);
        // read slots once per chunk (shared by the live and the fading graph)
        for s in self.slots.iter_mut().flatten() {
            let (l, r) = (&mut s.l[..n], &mut s.r[..n]);
            s.reader.read(l, r);
            for i in 0..n {
                let g = s.gain.next() * s.mute.next();
                l[i] *= g;
                r[i] *= g;
            }
            s.delay.process(l, r);
            self.stats.meters[s.meter].record(l, r);
        }
        let Some(mut g) = self.graph.take() else {
            return;
        };
        g.render(io, off, n, &mut self.slots, transport, self.frame, ts, &mut self.tx);
        // zero the ports this graph owns, then write (with the swap crossfade if any)
        let (f0, f1) = if self.hold > 0 {
            self.hold = self.hold.saturating_sub(n as u32);
            (0.0, 0.0)
        } else {
            (self.fade.value(), self.fade.skip(n as u32))
        };
        g.write_ports(io, off, n, (f0, f1), false);
        if let Some(mut old) = self.fading.take() {
            old.render(io, off, n, &mut self.slots, transport, self.frame, ts, &mut self.tx);
            for p in &old.out_ports {
                if !g.out_ports.contains(p)
                    && let Some(d) = io.output(*p as usize)
                    && d.len() >= off + n
                {
                    d[off..off + n].fill(0.0);
                }
            }
            old.write_ports(io, off, n, (1.0 - f0, 1.0 - f1), true);
            self.fading = Some(old);
        }
        // direct routes (slot → port)
        for (idx, s) in self.slots.iter().enumerate() {
            let (Some(s), Some((port, gain))) = (s, g.slot_direct.get(idx).copied().flatten()) else { continue };
            if let Some(d) = io.output(port as usize)
                && d.len() >= off + n
            {
                let gl = db_to_gain(gain);
                for i in 0..n {
                    d[off + i] = s.l[i] * gl;
                }
            }
        }
        // meters: buses then inputs
        for b in &g.buses {
            self.stats.meters[b.meter].record(&b.out_l[..n], &b.out_r[..n]);
        }
        for i in &g.inputs {
            self.stats.meters[i.meter].record(&i.pre_l[..n], &i.pre_r[..n]);
        }
        for (k, fx) in g.buses.iter().flat_map(|b| b.chain.slots.iter()).enumerate().take(MAX_METERS) {
            self.stats.fx_active[k].store(fx.active() as u32, Ordering::Relaxed);
        }
        self.stats.duck_db.store(g.duck.db().to_bits(), Ordering::Relaxed);
        // taps
        for t in self.taps.iter_mut().flatten() {
            let Some(src) = t.source else { continue };
            let Some((a, b)) = g.tap_block(src, io, off, n) else { continue };
            if t.stereo {
                let il = &mut self.interleave[..2 * n];
                let b = b.unwrap_or(a);
                for i in 0..n {
                    il[2 * i] = a[i];
                    il[2 * i + 1] = b[i];
                }
                t.producer.push(il, ts);
            } else {
                t.producer.push(a, ts);
            }
        }
        // round-trip measurement: click on the monitor output, detect on the capture port
        if let Some(m) = self.measure.as_mut() {
            let sr = self.sr as u64;
            match m.phase {
                0 => {
                    if let Some(p) = g.monitor.as_ref().and_then(|x| x.ports[0])
                        && let Some(d) = io.output(p as usize)
                        && d.len() > off
                    {
                        d[off] = 0.9;
                        m.sent_at = self.frame;
                        m.timeout = self.frame + sr / 2;
                        m.phase = 1;
                    } else {
                        let _ = self.tx.push(RtOut::Measured(None));
                        self.measure = None;
                    }
                }
                _ => {
                    let hit = io.input(m.port as usize).filter(|s| s.len() >= off + n).and_then(|s| s[off..off + n].iter().position(|x| x.abs() > 0.3));
                    if let Some(i) = hit {
                        let rt = (self.frame + i as u64 - m.sent_at) as u32;
                        let _ = self.tx.push(RtOut::Measured(Some(rt)));
                        self.measure = None;
                    } else if self.frame > m.timeout {
                        let _ = self.tx.push(RtOut::Measured(None));
                        self.measure = None;
                    }
                }
            }
        }
        self.graph = Some(g);
        self.frame += n as u64;
    }
}
