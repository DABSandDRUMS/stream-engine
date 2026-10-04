//! Builds a real-time [`Graph`] from the audio config (control thread; allocates), together
//! with everything the control side needs to drive it: the address → target parameter map,
//! declarations for the state tree, trigger specs, enable rules (`bypass` + `when`), the
//! PipeWire port/link plan, and the mix description for the UI.

use crate::config::{AudioConfig, FxDef, FxKind};
use crate::dsp::AvDelay;
use crate::graph::*;
use crate::wasmfx::DspPatches;
use se_analysis::{DrumPadConfig, DrumTriggers};
use se_core::triggers::TriggerSpec;
use se_dsp::sampler::{SampleBank, Sampler};
use se_dsp::slot::{CompDelay, MAX_COMP};
use se_dsp::wasm::WasmStatus;
use se_dsp::{FxChain, FxSlot, MAX_BLOCK, ParamKind, ParamSpec, Smoother, db_to_gain};
use se_proto::{Meta, Value};
use std::sync::Arc;

/// Append-only port name table (index = RT port index; ports are never removed while the
/// engine runs, so indices stay valid for every graph generation).
#[derive(Default, Clone, Debug)]
pub struct PortTable {
    pub ins: Vec<String>,
    pub outs: Vec<String>,
}

impl PortTable {
    pub fn input(&mut self, name: &str) -> u16 {
        match self.ins.iter().position(|n| n == name) {
            Some(i) => i as u16,
            None => {
                self.ins.push(name.to_string());
                (self.ins.len() - 1) as u16
            }
        }
    }

    pub fn output(&mut self, name: &str) -> u16 {
        match self.outs.iter().position(|n| n == name) {
            Some(i) => i as u16,
            None => {
                self.outs.push(name.to_string());
                (self.outs.len() - 1) as u16
            }
        }
    }
}

/// How a state value converts into the RT float.
#[derive(Clone, Debug, PartialEq)]
pub enum Conv {
    Float,
    Bool,
    /// Built-in effect choice (value is the option string or an index).
    Choice(&'static [&'static str]),
    /// Patch enum param.
    Options(Vec<String>),
    /// Component `i` of a vector/color value.
    Component(usize),
}

/// `patch.<id>.payload.*` (published by the core with the trigger edge) → payload floats of a
/// dsp patch, in `TriggerPayload::floats` order. Bound before `.active`, so a sync that sees a
/// new edge delivers its payload first.
fn payload_binds(id: &str) -> Vec<(String, u8, Conv)> {
    use se_core::triggers::{PAYLOAD_COLOR, PAYLOAD_FIELDS};
    let mut v: Vec<(String, u8, Conv)> = PAYLOAD_FIELDS.iter().enumerate().map(|(k, f)| (format!("patch.{id}.payload.{f}"), k as u8, Conv::Float)).collect();
    for c in 0..4 {
        v.push((format!("patch.{id}.payload.user_color"), (PAYLOAD_COLOR + c) as u8, Conv::Component(c)));
    }
    v
}

impl Conv {
    pub fn apply(&self, v: &Value) -> Option<f32> {
        match self {
            Conv::Float => v.as_f32().or_else(|| matches!(v, Value::Bool(_)).then(|| if v.truthy() { 1.0 } else { 0.0 })),
            Conv::Bool => Some(if v.truthy() { 1.0 } else { 0.0 }),
            Conv::Choice(o) => match v {
                Value::Str(s) => o.iter().position(|x| x == s).map(|i| i as f32),
                _ => v.as_f32(),
            },
            Conv::Options(o) => match v {
                Value::Str(s) => o.iter().position(|x| x == s).map(|i| i as f32),
                _ => v.as_f32(),
            },
            Conv::Component(i) => v.as_list().and_then(|l| l.get(*i)).and_then(Value::as_f32),
        }
    }
}

/// `address` (state or signal) → RT target.
#[derive(Clone, Debug)]
pub struct ParamBind {
    pub addr: String,
    pub target: Target,
    pub conv: Conv,
    /// Used while the address doesn't exist yet (e.g. trigger `.env` before first fire).
    pub fallback: f32,
}

/// Effect enable = `!bypass && when`.
pub struct EnableRule {
    pub bypass: String,
    pub when: Option<se_expr::Expr>,
    pub target: Target,
}

/// Where a filter port connects.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum LinkTarget {
    /// Capture from a device source node channel (1-based).
    Source { target: String, channel: u32 },
    /// Playback into a device sink node channel (1-based).
    Sink { target: String, channel: u32 },
    /// Into one of our bus nodes (`se-<bus>`, channel 1 = FL, 2 = FR).
    Bus { node: String, channel: u32 },
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct LinkSpec {
    /// Our filter port name.
    pub port: String,
    pub input: bool,
    pub to: LinkTarget,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BusNode {
    pub bus: String,
    pub node: String,
    pub description: String,
}

/// A wasm effect instance in the graph (for hot reload + error reporting).
pub struct WasmUse {
    pub patch: String,
    pub chain: ChainRef,
    pub slot: u8,
    pub status: Arc<WasmStatus>,
}

/// Everything produced by one build.
pub struct Built {
    /// Always `Some` after [`build`]; taken when the graph is sent to the audio thread.
    pub graph: Option<Box<Graph>>,
    pub params: Vec<ParamBind>,
    pub enables: Vec<EnableRule>,
    pub declares: Vec<(String, Meta)>,
    pub triggers: Vec<(String, TriggerSpec)>,
    pub links: Vec<LinkSpec>,
    pub nodes: Vec<BusNode>,
    pub wasm: Vec<WasmUse>,
    pub info: Value,
    /// Pad names (index = pad) for `drums.<pad>` events.
    pub pads: Vec<String>,
    pub sounds: Vec<String>,
    /// Meter index per bus / input (same order as the config).
    pub bus_meters: Vec<usize>,
    pub input_meters: Vec<usize>,
    /// Flattened bus fx slots (for the `fx_active` stats) as `audio.bus.<b>.fx.<e>`.
    pub bus_fx: Vec<String>,
    pub errors: Vec<String>,
}

/// Meter indices: buses and inputs from 0, slots from [`SLOT_METER_BASE`].
pub const SLOT_METER_BASE: usize = 64;

pub fn node_name(bus: &str) -> String {
    format!("se-{bus}")
}

fn spec_meta(spec: &ParamSpec, base: f32) -> Meta {
    let m = match spec.kind {
        ParamKind::Float => Meta::float(base as f64, [spec.min as f64, spec.max as f64]),
        ParamKind::Bool => Meta::boolean(base >= 0.5),
        ParamKind::Choice(o) => Meta::enumeration(o.get(base.round() as usize).copied().unwrap_or(o[0]), o),
    };
    let m = if spec.unit.is_empty() { m } else { m.unit(spec.unit) };
    let m = if spec.description.is_empty() { m } else { m.describe(spec.description) };
    m.owner("audio")
}

fn db_meta(default: f32, lo: f64, hi: f64, what: &str) -> Meta {
    Meta::float(default as f64, [lo, hi]).unit("dB").describe(what).owner("audio")
}

struct Ctx<'a> {
    cfg: &'a AudioConfig,
    sr: f32,
    dsp: &'a DspPatches,
    out: Built,
}

impl Ctx<'_> {
    fn declare(&mut self, addr: String, meta: Meta) {
        self.out.declares.push((addr, meta));
    }

    fn bind(&mut self, addr: String, target: Target, conv: Conv, fallback: f32) {
        self.out.params.push(ParamBind { addr, target, conv, fallback });
    }

    /// Build one chain; `prefix` = `audio.bus.music` etc.
    fn chain(&mut self, defs: &[FxDef], prefix: &str, chain: ChainRef) -> (FxChain, Vec<Option<u8>>, Vec<Value>) {
        let mut out = FxChain::new();
        let mut keys = Vec::new();
        let mut info = Vec::new();
        for def in defs {
            let slot_idx = out.slots.len() as u8;
            let fxp = format!("{prefix}.fx.{}", def.name);
            let target = |what| Target::Fx { chain, slot: slot_idx, what };
            let (fx, specs_info): (Box<dyn se_dsp::Effect>, Vec<Value>) = match &def.kind {
                FxKind::Builtin(kind) => {
                    let Some(mut fx) = se_dsp::create(kind, self.sr) else {
                        self.out.errors.push(format!("{fxp}: unknown effect `{kind}`"));
                        continue;
                    };
                    let mut pinfo = Vec::new();
                    for (i, spec) in fx.params().iter().enumerate() {
                        let base = def.params.get(spec.name).and_then(|v| crate::config::param_value(spec, v).ok()).unwrap_or(spec.default);
                        fx.set_param(i, spec.clamp(base));
                        let addr = format!("{fxp}.{}", spec.name);
                        self.declare(addr.clone(), spec_meta(spec, base));
                        let conv = match spec.kind {
                            ParamKind::Choice(o) => Conv::Choice(o),
                            ParamKind::Bool => Conv::Bool,
                            ParamKind::Float => Conv::Float,
                        };
                        self.bind(addr, target(FxWhat::Param(i as u16)), conv, base);
                        pinfo.push(Value::Str(spec.name.to_string()));
                    }
                    (fx, pinfo)
                }
                FxKind::Patch(id) => match self.dsp.instantiate(id, self.sr, false) {
                    Ok((fx, status, slots)) => {
                        let mut pinfo = Vec::new();
                        for s in &slots {
                            self.bind(format!("patch.{id}.{}", s.param), target(FxWhat::Param(1 + s.slot as u16)), s.conv.clone(), s.default);
                            pinfo.push(Value::Str(format!("patch.{id}.{}", s.param)));
                        }
                        for (addr, k, conv) in payload_binds(id) {
                            self.bind(addr, target(FxWhat::Payload(k)), conv, 0.0);
                        }
                        let triggered = def.trigger || self.dsp.has_trigger(id);
                        self.bind(format!("patch.{id}.env"), target(FxWhat::Param(0)), Conv::Float, if triggered { 0.0 } else { 1.0 });
                        self.out.wasm.push(WasmUse { patch: id.clone(), chain, slot: slot_idx, status });
                        (Box::new(fx) as Box<dyn se_dsp::Effect>, pinfo)
                    }
                    Err(e) => {
                        self.out.errors.push(format!("{fxp}: dsp patch `{id}`: {e}"));
                        continue;
                    }
                },
            };
            // one-shot slots follow a trigger envelope: built-ins at their fx address, dsp
            // patches at `patch.<id>` (their manifest trigger)
            let triggered = match &def.kind {
                FxKind::Builtin(_) => def.trigger,
                FxKind::Patch(id) => def.trigger || self.dsp.has_trigger(id),
            };
            let mut slot = FxSlot::new(fx, self.sr, triggered);
            let (dw, dd) = match &def.kind {
                FxKind::Builtin(k) => se_dsp::registry::default_mix(k),
                FxKind::Patch(_) => (1.0, 0.0),
            };
            let wet = def.wet.unwrap_or(dw);
            let dry = def.dry.unwrap_or(dd);
            slot.set_wet(wet);
            slot.set_dry(dry);
            slot.set_enabled(!def.bypass && def.when.is_none());
            self.declare(format!("{fxp}.wet"), Meta::float(wet as f64, [0.0, 1.0]).describe("effect output level").owner("audio"));
            self.declare(format!("{fxp}.dry"), Meta::float(dry as f64, [0.0, 1.0]).describe("dry (input) level").owner("audio"));
            self.declare(format!("{fxp}.bypass"), Meta::boolean(def.bypass).describe("bypass with crossfade").owner("audio"));
            self.bind(format!("{fxp}.wet"), target(FxWhat::Wet), Conv::Float, wet);
            self.bind(format!("{fxp}.dry"), target(FxWhat::Dry), Conv::Float, dry);
            self.out.enables.push(EnableRule {
                bypass: format!("{fxp}.bypass"),
                when: def.when.as_deref().and_then(|w| se_expr::Expr::parse(w).ok()),
                target: target(FxWhat::Enabled),
            });
            if let (true, FxKind::Patch(id)) = (triggered, &def.kind) {
                if !self.dsp.has_trigger(id) {
                    self.out.triggers.push((format!("patch.{id}"), TriggerSpec::default()));
                }
                self.bind(format!("patch.{id}.env"), target(FxWhat::Env), Conv::Float, 0.0);
                self.bind(format!("patch.{id}.active"), target(FxWhat::Active), Conv::Bool, 0.0);
            } else if triggered {
                let spec = def.trigger_spec.as_ref().and_then(|v| TriggerSpec::from_value(&Value::from(v.clone())).ok()).unwrap_or(TriggerSpec {
                    attack_ms: 10,
                    hold_ms: Some(2000),
                    release_ms: 150,
                    ..Default::default()
                });
                self.out.triggers.push((fxp.clone(), spec));
                self.bind(format!("{fxp}.env"), target(FxWhat::Env), Conv::Float, 0.0);
                self.bind(format!("{fxp}.active"), target(FxWhat::Active), Conv::Bool, 0.0);
            }
            keys.push(def.key.as_deref().and_then(|k| self.cfg.bus_index(k)).map(|b| b as u8));
            info.push(
                Value::map()
                    .with("name", def.name.clone())
                    .with(
                        "kind",
                        match &def.kind {
                            FxKind::Builtin(k) => k.clone(),
                            FxKind::Patch(p) => format!("patch.{p}"),
                        },
                    )
                    .with("address", fxp.clone())
                    .with("trigger", def.trigger)
                    .with("when", def.when.clone().map(Value::Str).unwrap_or_default())
                    .with("latency", slot.latency() as i64)
                    .with("params", Value::List(specs_info)),
            );
            if matches!(chain, ChainRef::Bus(_)) {
                self.out.bus_fx.push(fxp.clone());
            }
            out.slots.push(slot);
        }
        (out, keys, info)
    }
}

fn buf() -> Vec<f32> {
    vec![0.0; MAX_BLOCK]
}

/// Build generation `generation` from `cfg`. `ports` grows with new port names.
pub fn build(cfg: &AudioConfig, generation: u32, ports: &mut PortTable, bank: SampleBank, dsp: &DspPatches) -> Built {
    let sr = cfg.rate as f32;
    let mut c = Ctx {
        cfg,
        sr,
        dsp,
        out: Built {
            graph: None,
            params: Vec::new(),
            enables: Vec::new(),
            declares: Vec::new(),
            triggers: Vec::new(),
            links: Vec::new(),
            nodes: Vec::new(),
            wasm: Vec::new(),
            info: Value::Null,
            pads: Vec::new(),
            sounds: Vec::new(),
            bus_meters: Vec::new(),
            input_meters: Vec::new(),
            bus_fx: Vec::new(),
            errors: Vec::new(),
        },
    };
    let max_delay = (cfg.max_delay_ms * 0.001 * sr) as usize;
    let mut meter = 0usize;

    // ---- inputs
    let mut inputs = Vec::new();
    let mut input_info = Vec::new();
    for (k, d) in cfg.inputs.iter().enumerate() {
        let pre = format!("audio.input.{}", d.name);
        let mut ports_ix = [None, None];
        for (ci, ch) in d.channels.iter().enumerate() {
            let pname = format!("in_{}_{}", d.name, ci + 1);
            ports_ix[ci] = Some(ports.input(&pname));
            c.out.links.push(LinkSpec { port: pname, input: true, to: LinkTarget::Source { target: d.target.clone(), channel: *ch } });
        }
        c.declare(format!("{pre}.gain"), db_meta(d.gain_db, -60.0, 24.0, "input gain"));
        c.declare(format!("{pre}.mute"), Meta::boolean(d.mute).owner("audio"));
        c.declare(format!("{pre}.delay_ms"), Meta::float(d.delay_ms as f64, [0.0, cfg.max_delay_ms as f64]).unit("ms").describe("A/V delay").owner("audio"));
        c.bind(format!("{pre}.gain"), Target::InputGain(k as u8), Conv::Float, d.gain_db);
        c.bind(format!("{pre}.mute"), Target::InputMute(k as u8), Conv::Bool, d.mute as u8 as f32);
        c.bind(format!("{pre}.delay_ms"), Target::InputDelay(k as u8), Conv::Float, d.delay_ms);
        let (chain, keys, fx_info) = c.chain(&d.fx, &pre, ChainRef::Input(k as u8));
        let mut delay = AvDelay::new(max_delay, (0.02 * sr) as u32);
        delay.set((d.delay_ms * 0.001 * sr) as usize);
        inputs.push(InputStrip {
            ports: ports_ix,
            bus: cfg.bus_index(&d.bus).unwrap_or(0) as u8,
            gain: Smoother::with_ms(db_to_gain(d.gain_db), 20.0, sr),
            mute: Smoother::with_ms(if d.mute { 0.0 } else { 1.0 }, 10.0, sr),
            delay,
            chain,
            keys,
            align: CompDelay::new(MAX_COMP),
            l: buf(),
            r: buf(),
            pre_l: buf(),
            pre_r: buf(),
            meter: 0,
        });
        input_info.push(
            Value::map()
                .with("name", d.name.clone())
                .with("label", d.label().to_string())
                .with("target", d.target.clone())
                .with("channels", Value::List(d.channels.iter().map(|c| Value::Int(*c as i64)).collect()))
                .with("bus", d.bus.clone())
                .with("address", pre)
                .with("fx", Value::List(fx_info)),
        );
    }

    // ---- buses
    let mut buses = Vec::new();
    let mut bus_info = Vec::new();
    for (k, d) in cfg.buses.iter().enumerate() {
        let pre = format!("audio.bus.{}", d.name);
        let is_program = d.name == "program";
        c.declare(format!("{pre}.gain"), db_meta(d.gain_db, -60.0, 12.0, "bus fader"));
        c.declare(format!("{pre}.mute"), Meta::boolean(d.mute).owner("audio"));
        c.declare(format!("{pre}.limiter"), Meta::boolean(d.limiter).describe("brickwall limiter").owner("audio"));
        c.declare(format!("{pre}.ceiling"), db_meta(d.ceiling_db, -24.0, 0.0, "limiter ceiling"));
        c.declare(format!("{pre}.level"), Meta::float(-100.0, [-100.0, 6.0]).unit("dBFS").readonly().owner("audio"));
        c.declare(format!("{pre}.peak"), Meta::float(-100.0, [-100.0, 6.0]).unit("dBFS").readonly().owner("audio"));
        c.bind(format!("{pre}.gain"), Target::BusGain(k as u8), Conv::Float, d.gain_db);
        c.bind(format!("{pre}.mute"), Target::BusMute(k as u8), Conv::Bool, d.mute as u8 as f32);
        c.bind(format!("{pre}.limiter"), Target::BusLimiter(k as u8), Conv::Bool, d.limiter as u8 as f32);
        c.bind(format!("{pre}.ceiling"), Target::BusCeiling(k as u8), Conv::Float, d.ceiling_db);
        let ducked = cfg.duck.targets.iter().any(|t| t == &d.name);
        if ducked {
            c.declare(format!("{pre}.ducked"), Meta::float(0.0, [-60.0, 0.0]).unit("dB").readonly().describe("current duck gain reduction").owner("audio"));
        }
        let (chain, keys, fx_info) = c.chain(&d.fx, &pre, ChainRef::Bus(k as u8));
        let (limiter, ceiling_idx) = match se_dsp::create("limiter", sr) {
            Some(fx) => {
                let idx = fx.params().iter().position(|p| p.name == "ceiling").unwrap_or(0);
                let mut s = FxSlot::new(fx, sr, false);
                s.set_wet(1.0);
                s.set_dry(0.0);
                s.set_param(idx, d.ceiling_db);
                s.set_enabled(d.limiter);
                (Some(s), idx as u16)
            }
            None => {
                c.out.errors.push("se-dsp has no `limiter` effect".into());
                (None, 0)
            }
        };
        let port_pair = if d.node {
            let node = node_name(&d.name);
            c.out.nodes.push(BusNode { bus: d.name.clone(), node: node.clone(), description: format!("Stream Engine: {}", d.label) });
            let pl = format!("out_{}_L", d.name);
            let pr = format!("out_{}_R", d.name);
            let ix = (ports.output(&pl), ports.output(&pr));
            c.out.links.push(LinkSpec { port: pl, input: false, to: LinkTarget::Bus { node: node.clone(), channel: 1 } });
            c.out.links.push(LinkSpec { port: pr, input: false, to: LinkTarget::Bus { node, channel: 2 } });
            Some(ix)
        } else {
            None
        };
        c.out.bus_meters.push(meter);
        buses.push(BusStrip {
            sum_l: buf(),
            sum_r: buf(),
            out_l: buf(),
            out_r: buf(),
            post_l: buf(),
            post_r: buf(),
            key: buf(),
            al_l: buf(),
            al_r: buf(),
            chain,
            keys,
            gain: Smoother::with_ms(db_to_gain(d.gain_db), 20.0, sr),
            mute: Smoother::with_ms(if d.mute { 0.0 } else { 1.0 }, 10.0, sr),
            limiter,
            ceiling_idx,
            ducked,
            align: CompDelay::new(MAX_COMP),
            ports: port_pair,
            to_program: d.to_program,
            meter,
            is_program,
        });
        meter += 1;
        bus_info.push(
            Value::map()
                .with("name", d.name.clone())
                .with("label", d.label.clone())
                .with("address", pre)
                .with("node", if d.node { Value::Str(node_name(&d.name)) } else { Value::Null })
                .with("to_program", d.to_program)
                .with("ducked", ducked)
                .with("fx", Value::List(fx_info)),
        );
    }
    for i in &mut inputs {
        i.meter = meter;
        c.out.input_meters.push(meter);
        meter += 1;
    }

    // ---- sounds / samplers
    let mut sound_bus = Vec::new();
    let mut sampler_buses: Vec<u8> = Vec::new();
    for s in &bank.sounds {
        let bus = cfg.sounds.iter().find(|x| x.name == s.name).and_then(|x| cfg.bus_index(&x.bus)).unwrap_or_else(|| cfg.bus_index("sfx").unwrap_or(0)) as u8;
        sound_bus.push(bus);
        if !sampler_buses.contains(&bus) {
            sampler_buses.push(bus);
        }
        c.out.sounds.push(s.name.clone());
    }
    let samplers = sampler_buses.into_iter().map(|bus| SamplerStrip { bus, sampler: Sampler::new(32, sr) }).collect();

    // ---- dsp sources
    let mut sources = Vec::new();
    for (k, s) in cfg.sources.iter().enumerate() {
        match dsp.instantiate(&s.patch, sr, true) {
            Ok((fx, status, slots)) => {
                let slot = FxSlot::new(Box::new(fx), sr, false);
                for p in &slots {
                    c.bind(
                        format!("patch.{}.{}", s.patch, p.param),
                        Target::Fx { chain: ChainRef::Source(k as u8), slot: 0, what: FxWhat::Param(1 + p.slot as u16) },
                        p.conv.clone(),
                        p.default,
                    );
                }
                for (addr, p, conv) in payload_binds(&s.patch) {
                    c.bind(addr, Target::Fx { chain: ChainRef::Source(k as u8), slot: 0, what: FxWhat::Payload(p) }, conv, 0.0);
                }
                c.bind(format!("patch.{}.env", s.patch), Target::Fx { chain: ChainRef::Source(k as u8), slot: 0, what: FxWhat::Param(0) }, Conv::Float, 1.0);
                c.declare(format!("audio.source.{}.gain", s.patch), db_meta(s.gain_db, -60.0, 12.0, "dsp source level"));
                c.bind(format!("audio.source.{}.gain", s.patch), Target::SourceGain(k as u8), Conv::Float, s.gain_db);
                c.out.wasm.push(WasmUse { patch: s.patch.clone(), chain: ChainRef::Source(k as u8), slot: 0, status });
                sources.push(SourceStrip {
                    fx: slot,
                    bus: cfg.bus_index(&s.bus).unwrap_or(0) as u8,
                    gain: Smoother::with_ms(db_to_gain(s.gain_db), 20.0, sr),
                    l: buf(),
                    r: buf(),
                });
            }
            Err(e) => c.out.errors.push(format!("sources.{}: {e}", s.patch)),
        }
    }

    // ---- monitor
    let monitor = cfg.monitor.as_ref().map(|m| {
        let mut ports_ix = [None, None];
        for (ci, ch) in m.channels.iter().enumerate() {
            let pname = format!("mon_{}", ci + 1);
            ports_ix[ci] = Some(ports.output(&pname));
            c.out.links.push(LinkSpec { port: pname, input: false, to: LinkTarget::Sink { target: m.target.clone(), channel: *ch } });
        }
        c.declare("audio.monitor.gain".into(), db_meta(m.gain_db, -60.0, 12.0, "monitor level"));
        c.bind("audio.monitor.gain".into(), Target::MonitorGain, Conv::Float, m.gain_db);
        c.declare(
            "audio.monitor.roundtrip_ms".into(),
            Meta::float(0.0, [0.0, 1000.0]).unit("ms").readonly().describe("measured monitor round trip").owner("audio"),
        );
        let (chain, _, _) = c.chain(&m.fx, "audio.monitor", ChainRef::Monitor);
        MonitorStrip {
            buses: m.buses.iter().filter_map(|b| cfg.bus_index(b)).map(|b| b as u8).collect(),
            inputs: m.inputs.iter().filter_map(|i| cfg.input_index(i)).map(|i| i as u8).collect(),
            chain,
            gain: Smoother::with_ms(db_to_gain(m.gain_db), 20.0, sr),
            ports: ports_ix,
            l: buf(),
            r: buf(),
        }
    });

    // ---- software buses → motherboard output → 16R. The 24c capture is not returned
    // to this output; the drummer's independent pre-fader monitor remains separate.
    let playback = cfg.playback.as_ref().map(|p| {
        let mut ports_ix = [None, None];
        for (ci, ch) in p.channels.iter().enumerate() {
            let pname = format!("play_{}", ci + 1);
            ports_ix[ci] = Some(ports.output(&pname));
            c.out.links.push(LinkSpec { port: pname, input: false, to: LinkTarget::Sink { target: p.target.clone(), channel: *ch } });
        }
        PlaybackStrip { buses: p.buses.iter().filter_map(|name| cfg.bus_index(name)).map(|index| index as u8).collect(), ports: ports_ix, l: buf(), r: buf() }
    });

    // ---- direct routes (slot → device channel)
    for d in &cfg.direct {
        let pname = format!("direct_{}", d.slot.replace('.', "_"));
        ports.output(&pname);
        c.out.links.push(LinkSpec { port: pname, input: false, to: LinkTarget::Sink { target: d.target.clone(), channel: d.channel } });
    }

    // ---- drum triggers (M12)
    let drums = if cfg.pads.is_empty() {
        None
    } else {
        let mut pads = Vec::new();
        let mut layers = Vec::new();
        let mut confs = Vec::new();
        for (k, p) in cfg.pads.iter().enumerate() {
            let inp = cfg.input_index(&p.input).unwrap_or(0);
            pads.push((inp as u8, p.channel as u8));
            let layer = p.layer.as_ref().and_then(|(s, g)| match bank.find(s) {
                Some(i) => Some((i as u16, *g)),
                None => {
                    c.out.errors.push(format!("drums.pads.{}: unknown sound `{s}` (not in [audio.sounds] or assets/sounds/)", p.name));
                    None
                }
            });
            layers.push(layer);
            confs.push(DrumPadConfig {
                name: p.name.clone(),
                band: p.band,
                threshold_db: p.threshold_db,
                retrigger_ms: p.retrigger_ms,
                scan_ms: p.scan_ms,
                bleed_ratio_db: p.bleed_db,
                velocity_curve: p.velocity_curve,
                velocity_range_db: p.velocity_range_db,
            });
            let a = format!("audio.drums.{}.threshold", p.name);
            c.declare(a.clone(), db_meta(p.threshold_db, -80.0, 0.0, "trigger threshold"));
            c.bind(a, Target::PadThreshold(k as u8), Conv::Float, p.threshold_db);
            c.out.pads.push(p.name.clone());
        }
        let n = pads.len().min(MAX_PADS);
        pads.truncate(n);
        layers.truncate(n);
        confs.truncate(n);
        Some(DrumStage { trig: DrumTriggers::new(sr, confs), pads, layers, bufs: (0..n).map(|_| buf()).collect() })
    };

    // ---- ducking
    let d = &cfg.duck;
    c.declare("audio.duck.active".into(), Meta::boolean(false).describe("manual/preset ducking").owner("audio"));
    c.declare("audio.duck.depth".into(), db_meta(d.depth_db, -60.0, 0.0, "duck depth"));
    c.declare("audio.duck.attack".into(), Meta::float(d.attack_ms as f64, [1.0, 2000.0]).unit("ms").owner("audio"));
    c.declare("audio.duck.release".into(), Meta::float(d.release_ms as f64, [1.0, 5000.0]).unit("ms").owner("audio"));
    c.bind("audio.duck.active".into(), Target::DuckActive, Conv::Bool, 0.0);
    c.bind("audio.duck.depth".into(), Target::DuckDepth, Conv::Float, d.depth_db);
    c.bind("audio.duck.attack".into(), Target::DuckAttack, Conv::Float, d.attack_ms);
    c.bind("audio.duck.release".into(), Target::DuckRelease, Conv::Float, d.release_ms);
    let duck = Ducker::new(
        d.targets.iter().filter_map(|t| cfg.bus_index(t)).map(|b| b as u8).collect(),
        d.keys.iter().filter_map(|t| cfg.bus_index(t)).map(|b| b as u8).collect(),
        d.depth_db,
        d.attack_ms,
        d.release_ms,
        d.threshold_db,
        (d.hold_ms * 0.001 * sr) as u32,
    );

    let info = Value::map()
        .with("rate", cfg.rate as i64)
        .with("quantum", cfg.quantum as i64)
        .with("buses", Value::List(bus_info))
        .with("inputs", Value::List(input_info))
        .with("sounds", Value::List(c.out.sounds.iter().cloned().map(Value::Str).collect()))
        .with("pads", Value::List(c.out.pads.iter().cloned().map(Value::Str).collect()))
        .with(
            "duck",
            Value::map()
                .with("targets", Value::List(d.targets.iter().cloned().map(Value::Str).collect()))
                .with("keys", Value::List(d.keys.iter().cloned().map(Value::Str).collect()))
                .with("signals", Value::List(d.signals.iter().cloned().map(Value::Str).collect())),
        )
        .with("monitor", cfg.monitor.as_ref().map(|m| Value::Str(m.target.clone())).unwrap_or_default())
        .with("playback", cfg.playback.as_ref().map(|p| Value::Str(p.target.clone())).unwrap_or_default());

    let graph = Graph::new(generation, sr, inputs, buses, samplers, sound_bus, Box::new(bank), sources, monitor, playback, drums, duck);
    c.out.info = info.with("latency", graph.latency as i64);
    c.out.graph = Some(Box::new(graph));
    c.out
}
