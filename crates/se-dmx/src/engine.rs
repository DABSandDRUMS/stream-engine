//! Frame computation for the DMX thread: resolved state (snapshot) → per-head attribute
//! values (fixture/cell/group merge) → effects → masters and caps → flash limiter →
//! DMX universes. Everything is preallocated when the plan changes; [`Engine::render`] does
//! not allocate.

use crate::effects::{self, BeatClock, EffectDef, EffectState};
use crate::limiter::{Limiter, OnsetWindow};
use crate::rig::{AttrKind, Enc, Rig, SLOTS, StrobePolicy, slot};
use se_hub::Snapshot;
use se_proto::Value;
use std::collections::HashMap;
use std::sync::Arc;

const NONE: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InKind {
    Scalar,
    Color,
    Bool,
}

#[derive(Clone, Debug)]
struct InputDef {
    addr: String,
    kind: InKind,
}

#[derive(Clone, Copy, Debug)]
enum EffectColor {
    Literal([f32; 3]),
    Input(u32),
}

/// How one head attribute is resolved each frame.
#[derive(Clone, Debug)]
struct Bind {
    head: u32,
    kind: AttrKind,
    slot: u16,
    raw: u32,
    own: u32,
    parent: u32,
    groups: (u32, u32),
    default: [f32; 3],
}

/// Everything the DMX thread needs, built on the control side whenever the rig or effects
/// change.
pub struct Plan {
    pub rig: Arc<Rig>,
    pub effects: Vec<(EffectDef, Vec<(usize, f32)>)>,
    inputs: Vec<InputDef>,
    binds: Vec<Bind>,
    srcs: Vec<u32>,
    masters: Vec<u32>,
    head_masters: Vec<(u32, u32)>,
    raw_base: Vec<u32>,
    raw_len: usize,
    grand: u32,
    blackout: u32,
    panic_latched: u32,
    fx_inputs: Vec<[u32; 3]>,
    fx_coverage: Vec<Vec<u32>>,
    fx_slots: Vec<u32>,
    fx_colors: Vec<Vec<EffectColor>>,
    /// Which heads have additive emitters (limiter luminance, virtual dimmer).
    emitters: Vec<u32>,
}

/// Emitter slot bits for [`Plan::emitters`].
fn emitter_bit(slot: usize) -> u32 {
    1 << slot
}

impl Plan {
    pub fn build(rig: Arc<Rig>, effects: Vec<EffectDef>) -> (Plan, Vec<String>) {
        let mut errors = Vec::new();
        let mut index: HashMap<String, u32> = HashMap::new();
        let mut inputs: Vec<InputDef> = Vec::new();
        let mut input = |addr: String, kind: InKind| -> u32 {
            if let Some(i) = index.get(&addr) {
                return *i;
            }
            let i = inputs.len() as u32;
            index.insert(addr.clone(), i);
            inputs.push(InputDef { addr, kind });
            i
        };
        let in_kind = |k: AttrKind| if k == AttrKind::Color { InKind::Color } else { InKind::Scalar };
        let n = rig.heads.len();
        // groups each head belongs to (directly, as a cell of a grouped fixture, or as a grouped cell)
        let head_groups: Vec<Vec<usize>> = (0..n)
            .map(|h| {
                let parent = rig.heads[h].parent;
                rig.groups
                    .iter()
                    .enumerate()
                    .filter(|(_, g)| g.roots.contains(&h) || g.leaves.contains(&h) || parent.is_some_and(|p| g.roots.contains(&p)))
                    .map(|(i, _)| i)
                    .collect()
            })
            .collect();
        let mut binds = Vec::new();
        let mut srcs = Vec::new();
        let mut raw_base = Vec::with_capacity(n);
        let mut raw_len = 0usize;
        for (h, head) in rig.heads.iter().enumerate() {
            raw_base.push(raw_len as u32);
            raw_len += head.raw_names.len();
            for a in &head.attrs {
                let own = input(head.addr(&a.name), in_kind(a.kind));
                let parent = match head.parent {
                    Some(p) if rig.heads[p].attr(&a.name).is_some() => input(rig.heads[p].addr(&a.name), in_kind(a.kind)),
                    _ => NONE,
                };
                let start = srcs.len() as u32;
                if a.kind != AttrKind::Raw {
                    for g in &head_groups[h] {
                        let grp = &rig.groups[*g];
                        if grp.attrs.iter().any(|x| x.name == a.name) {
                            srcs.push(input(format!("lights.group.{}.{}", grp.name, a.name), in_kind(a.kind)));
                        }
                    }
                }
                let default = match (a.kind, &a.default) {
                    (AttrKind::Color, v) => v.as_color().map(|c| [c[0], c[1], c[2]]).unwrap_or([1.0; 3]),
                    (_, v) => [v.as_f32().unwrap_or(0.0), 0.0, 0.0],
                };
                binds.push(Bind {
                    head: h as u32,
                    kind: a.kind,
                    slot: a.slot.unwrap_or(0) as u16,
                    raw: a.raw.map(|r| r as u32).unwrap_or(NONE),
                    own,
                    parent,
                    groups: (start, srcs.len() as u32),
                    default,
                });
            }
        }
        let mut masters = Vec::new();
        let mut head_masters = Vec::with_capacity(n);
        for hg in &head_groups {
            let s = masters.len() as u32;
            for g in hg {
                masters.push(input(format!("lights.group.{}.master", rig.groups[*g].name), InKind::Scalar));
            }
            head_masters.push((s, masters.len() as u32));
        }
        let grand = input("lights.master".into(), InKind::Scalar);
        let blackout = input("lights.blackout".into(), InKind::Bool);
        let panic_latched = input("lights.panic_latched".into(), InKind::Bool);
        let mut fx = Vec::new();
        let mut fx_inputs = Vec::new();
        let mut fx_coverage = Vec::new();
        let mut fx_slots = Vec::new();
        let mut fx_colors = Vec::new();
        for e in effects {
            match e.layout(&rig) {
                Ok(l) => {
                    fx_slots.push(match e.kind {
                        effects::Kind::ColorChase | effects::Kind::ColorWave | effects::Kind::Rainbow | effects::Kind::FollowColor => (1 << slot::RED) | (1 << slot::GREEN) | (1 << slot::BLUE),
                        effects::Kind::Circle => (1 << slot::PAN) | (1 << slot::TILT),
                        effects::Kind::DimmerSine | effects::Kind::DimmerTriangle | effects::Kind::DimmerSaw | effects::Kind::DimmerSquare | effects::Kind::Sparkle | effects::Kind::Follow => 1 << slot::INTENSITY,
                    });
                    fx_inputs.push([
                        input(format!("lights.effect.{}.active", e.name), InKind::Bool),
                        input(format!("lights.effect.{}.rate", e.name), InKind::Scalar),
                        input(format!("lights.effect.{}.size", e.name), InKind::Scalar),
                    ]);
                    fx_coverage.push(l.iter().map(|(h, _)| {
                        input(format!("lights.effect.{}.coverage.{}", e.name, rig.heads[*h].id), InKind::Bool)
                    }).collect());
                    fx_colors.push(e.colors.iter().map(|spec| match spec {
                        crate::palette::Spec::Literal(v) => {
                            let [r, g, b, _] = v.as_color().expect("effect parser validates RGB literals");
                            EffectColor::Literal([r, g, b])
                        }
                        crate::palette::Spec::Address(a) => EffectColor::Input(input(a.clone(), InKind::Color)),
                        crate::palette::Spec::Palette(_) => unreachable!("effect parser rejects fixture palettes"),
                    }).collect());
                    fx.push((e, l));
                }
                Err(err) => errors.push(format!("effect `{}`: {err}", e.name)),
            }
        }
        let mut emitters = vec![0u32; n];
        for c in &rig.chans {
            if let Enc::Emitter { slot } = c.enc {
                emitters[c.head] |= emitter_bit(slot);
            }
            if matches!(c.enc, Enc::Subtractive { .. } | Enc::Wheel { .. }) {
                emitters[c.head] |= emitter_bit(slot::RED) | emitter_bit(slot::GREEN) | emitter_bit(slot::BLUE);
            }
        }
        (Plan { rig, effects: fx, inputs, binds, srcs, masters, head_masters, raw_base, raw_len, grand, blackout, panic_latched, fx_inputs, fx_coverage, fx_slots, fx_colors, emitters }, errors)
    }

    /// Every address the engine reads (for declarations/diagnostics).
    pub fn addresses(&self) -> impl Iterator<Item = &str> {
        self.inputs.iter().map(|i| i.addr.as_str())
    }
}

#[derive(Clone, Copy, Debug, Default)]
struct InputState {
    id: Option<usize>,
    val: [f32; 4],
    present: bool,
    stamp: u64,
    priority: u16,
}

/// What the visualizer shows per head.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct HeadView {
    pub intensity: f32,
    pub color: [f32; 3],
    pub pan: f32,
    pub tilt: f32,
    pub zoom: f32,
    pub strobe: f32,
    pub limited: bool,
}

/// One rendered frame.
#[derive(Clone, Debug, Default)]
pub struct Frame {
    /// Indexed like `Rig::universes`.
    pub universes: Vec<[u8; 512]>,
    pub heads: Vec<HeadView>,
    pub limited: u32,
    pub suppressed: u64,
    pub strobe_capped: u64,
}

pub struct Engine {
    plan: Arc<Plan>,
    inputs: Vec<InputState>,
    out: Vec<[f32; SLOTS]>,
    raw: Vec<f32>,
    lit: Vec<bool>,
    /// Intensity before dimmer modulation, then after masters and brightness caps.
    intended_intensity: Vec<f32>,
    head_universes: Vec<usize>,
    universe_intended: Vec<bool>,
    rehearsal_hold_allowed: bool,
    limiters: Vec<Limiter>,
    global: OnsetWindow,
    fx: Vec<EffectState>,
    colors: Vec<Vec<Option<[f32; 3]>>>,
    coverage: Vec<Vec<bool>>,
    priorities: Vec<[u16; SLOTS]>,
    protected: Vec<u32>,
    move_stages: Vec<Option<MoveStage>>,
    beat: BeatClock,
    pub frame: Frame,
    count: u64,
    index: Option<Arc<HashMap<String, usize>>>,
    generation: u64,
    signals: Option<Arc<HashMap<String, usize>>>,
    sig_phase: Option<usize>,
    sig_bpm: Option<usize>,
    sig_confidence: Option<usize>,
    sig_position: Option<usize>,
    fx_signals: Vec<Option<usize>>,
    time_ns: u64,
}

/// Authored positioning/wheel travel is hidden; continuous effect motion is not.
/// These conservative timings are presentation staging, not calibrated safe aiming.
#[derive(Clone, Copy, Default)]
struct MoveStage {
    target: [f32; 3],
    initialized: bool,
    quiet_s: f64,
    priority: u16,
}

impl MoveStage {
    fn update(&mut self, target: [f32; 3], priorities: [u16; 3], initial_priority: u16, dt: f64) {
        if !self.initialized || target != self.target {
            let priority = priorities.into_iter().enumerate()
                .filter(|(s, _)| !self.initialized || target[*s] != self.target[*s])
                .map(|(_, priority)| priority).max().unwrap_or(0);
            self.priority = if !self.initialized && priority == 0 { initial_priority } else { priority };
            self.target = target;
            self.initialized = true;
            self.quiet_s = 0.0;
        } else {
            self.quiet_s += dt;
        }
    }

    fn gain(&self) -> f32 {
        let p = ((self.quiet_s - 0.75) / 0.75).clamp(0.0, 1.0) as f32;
        p * p * (3.0 - 2.0 * p)
    }
}

/// Defaults of all fixed slots.
fn slot_defaults() -> [f32; SLOTS] {
    let mut d = [0.0; SLOTS];
    for k in crate::rig::KNOWN {
        d[k.slot] = k.default;
        if k.kind == AttrKind::Color {
            d[slot::GREEN] = k.default;
            d[slot::BLUE] = k.default;
        }
    }
    d
}

impl Engine {
    pub fn new(plan: Arc<Plan>) -> Engine {
        let mut e = Engine {
            plan: plan.clone(),
            inputs: Vec::new(),
            out: Vec::new(),
            raw: Vec::new(),
            lit: Vec::new(),
            intended_intensity: Vec::new(),
            head_universes: Vec::new(),
            universe_intended: Vec::new(),
            rehearsal_hold_allowed: false,
            limiters: Vec::new(),
            global: OnsetWindow::default(),
            fx: Vec::new(),
            colors: Vec::new(),
            coverage: Vec::new(),
            priorities: Vec::new(),
            protected: Vec::new(),
            move_stages: Vec::new(),
            beat: BeatClock::default(),
            frame: Frame::default(),
            count: 0,
            index: None,
            generation: u64::MAX,
            signals: None,
            sig_phase: None,
            sig_bpm: None,
            sig_confidence: None,
            sig_position: None,
            fx_signals: Vec::new(),
            time_ns: 0,
        };
        e.set_plan(plan);
        e
    }

    pub fn plan(&self) -> &Arc<Plan> {
        &self.plan
    }

    /// Adopt a new plan (config reload). Allocates; never called per frame.
    pub fn set_plan(&mut self, plan: Arc<Plan>) {
        let n = plan.rig.heads.len();
        self.inputs = vec![InputState::default(); plan.inputs.len()];
        self.out = vec![slot_defaults(); n];
        self.raw = vec![0.0; plan.raw_len];
        self.priorities = vec![[0; SLOTS]; n];
        self.protected = vec![0; n];
        self.move_stages = plan.rig.heads.iter().map(|h| {
            (h.attr("pan").is_some() && h.attr("tilt").is_some()).then(MoveStage::default)
        }).collect();
        self.lit = vec![false; n];
        self.intended_intensity = vec![0.0; n];
        self.head_universes = vec![usize::MAX; n];
        for c in &plan.rig.chans {
            self.head_universes[c.head] = c.universe;
        }
        self.universe_intended = vec![false; plan.rig.universes.len()];
        if self.limiters.len() != n {
            self.limiters = vec![Limiter::default(); n];
        }
        self.fx = plan
            .effects
            .iter()
            .enumerate()
            .map(|(i, (_, heads))| EffectState {
                phase: 0.0,
                rng: 0x9E37_79B9_7F4A_7C15 ^ (i as u64 + 1).wrapping_mul(0x2545_F491_4F6C_DD1D),
                env: vec![0.0; heads.len()],
                ..Default::default()
            })
            .collect();
        self.colors = plan.fx_colors.iter().map(|colors| vec![None; colors.len()]).collect();
        self.coverage = plan.effects.iter().map(|(_, heads)| vec![true; heads.len()]).collect();
        self.fx_signals = vec![None; plan.effects.len()];
        self.signals = None;
        self.frame.universes = vec![[0u8; 512]; plan.rig.universes.len()];
        self.frame.heads = vec![HeadView::default(); n];
        self.index = None;
        self.generation = u64::MAX;
        self.plan = plan;
    }

    fn bind_snapshot(&mut self, snap: &Snapshot) {
        let same = self.generation == snap.generation && self.index.as_ref().is_some_and(|i| Arc::ptr_eq(i, &snap.index));
        if !same {
            for (st, def) in self.inputs.iter_mut().zip(&self.plan.inputs) {
                st.id = snap.id(&def.addr);
            }
            self.index = Some(snap.index.clone());
            self.generation = snap.generation;
        }
        if !self.signals.as_ref().is_some_and(|s| Arc::ptr_eq(s, &snap.signal_index)) {
            self.sig_phase = snap.signal_index.get("beat.phase").copied();
            self.sig_bpm = snap.signal_index.get("beat.bpm").copied();
            self.sig_confidence = snap.signal_index.get("beat.confidence").copied();
            self.sig_position = snap.signal_index.get("beat.position").copied();
            for (id, (def, _)) in self.fx_signals.iter_mut().zip(&self.plan.effects) {
                *id = def.signal.as_ref().and_then(|name| snap.signal_index.get(name)).copied();
            }
            self.signals = Some(snap.signal_index.clone());
        }
    }

    fn read_inputs(&mut self, snap: &Snapshot) {
        let stamp = self.count;
        for (st, def) in self.inputs.iter_mut().zip(&self.plan.inputs) {
            let mut v = [0.0f32; 4];
            let present = match st.id.map(|i| snap.value(i)) {
                None | Some(Value::Null) => false,
                Some(val) => match def.kind {
                    InKind::Scalar => val.as_f64().map(|x| v[0] = x as f32).is_some(),
                    InKind::Color => val.as_color().map(|c| v = c).is_some(),
                    InKind::Bool => {
                        v[0] = if val.truthy() { 1.0 } else { 0.0 };
                        true
                    }
                },
            };
            let priority = st.id.and_then(|id| snap.priorities.get(id)).copied().unwrap_or(0);
            if present != st.present || v != st.val || st.priority != priority {
                st.stamp = stamp;
                st.present = present;
                st.val = v;
            }
            st.priority = priority;
        }
    }

    fn input(&self, i: u32) -> Option<&InputState> {
        if i == NONE {
            return None;
        }
        let s = &self.inputs[i as usize];
        s.present.then_some(s)
    }

    /// Render one frame at master-clock time `now` (ns). No allocation.
    pub fn render(&mut self, snap: &Snapshot, now: u64) -> &Frame {
        let dt = if self.time_ns == 0 { 0.0 } else { now.saturating_sub(self.time_ns) as f64 / 1e9 };
        self.time_ns = now;
        self.count += 1;
        self.bind_snapshot(snap);
        self.read_inputs(snap);
        let plan = self.plan.clone();
        let rig = &plan.rig;
        let defaults = slot_defaults();
        for (o, intended) in self.out.iter_mut().zip(&mut self.intended_intensity) {
            *o = defaults;
            *intended = defaults[slot::INTENSITY];
        }
        let mut authored = false;
        // 1. merge fixture / parent / group addresses
        for b in &plan.binds {
            let h = b.head as usize;
            let srcs = &plan.srcs[b.groups.0 as usize..b.groups.1 as usize];
            match b.kind {
                AttrKind::Intensity => {
                    let own = self.input(b.own);
                    let mut v = own.map(|s| s.val[0]).unwrap_or(b.default[0]);
                    let mut priority = own.map_or(0, |s| s.priority);
                    if let Some(p) = self.input(b.parent) {
                        v = v.max(p.val[0]);
                        priority = priority.max(p.priority);
                    }
                    for g in srcs {
                        if let Some(s) = self.input(*g) {
                            v = v.max(s.val[0]);
                            priority = priority.max(s.priority);
                        }
                    }
                    self.out[h][slot::INTENSITY] = v.clamp(0.0, 1.0);
                    self.intended_intensity[h] = self.out[h][slot::INTENSITY];
                    self.priorities[h][slot::INTENSITY] = priority;
                    authored |= priority > 0 || v > 0.0;
                }
                kind => {
                    // Priority-LTP across fixture / parent / groups; latest changed value wins
                    // at equal priority, then the most specific source wins ties.
                    let mut best: Option<&InputState> = self.input(b.own);
                    for i in std::iter::once(b.parent).chain(srcs.iter().copied()) {
                        if let Some(s) = self.input(i)
                            && best.is_none_or(|x| (s.priority, s.stamp) > (x.priority, x.stamp))
                        {
                            best = Some(s);
                        }
                    }
                    let v = best.map(|s| s.val).unwrap_or([b.default[0], b.default[1], b.default[2], 1.0]);
                    let priority = best.map_or(0, |s| s.priority);
                    authored |= priority > 0;
                    if kind == AttrKind::Color {
                        for s in [slot::RED, slot::GREEN, slot::BLUE] {
                            self.priorities[h][s] = priority;
                        }
                    } else if kind != AttrKind::Raw {
                        self.priorities[h][b.slot as usize] = priority;
                    }
                    match kind {
                        AttrKind::Color => {
                            let o = &mut self.out[h];
                            o[slot::RED] = v[0].clamp(0.0, 1.0);
                            o[slot::GREEN] = v[1].clamp(0.0, 1.0);
                            o[slot::BLUE] = v[2].clamp(0.0, 1.0);
                        }
                        AttrKind::Raw => {
                            let base = plan.raw_base[h] as usize;
                            self.raw[base + b.raw as usize] = v[0].clamp(0.0, 255.0);
                        }
                        AttrKind::Index => self.out[h][b.slot as usize] = v[0].max(0.0),
                        _ => self.out[h][b.slot as usize] = v[0].clamp(0.0, 1.0),
                    }
                }
            }
        }
        // Capture only authored coordinates before effects: a steady circle may stay
        // illuminated once its initial travel has settled.
        for (h, stage) in self.move_stages.iter_mut().enumerate() {
            if let Some(stage) = stage {
                let priorities = [slot::PAN, slot::TILT, slot::GOBO].map(|s| self.priorities[h][s]);
                stage.update([self.out[h][slot::PAN], self.out[h][slot::TILT], self.out[h][slot::GOBO]],
                    priorities, self.priorities[h][slot::INTENSITY], dt);
            }
        }
        // 2. effects
        let panic_latched = self.input(plan.panic_latched).is_some_and(|s| s.val[0] != 0.0);
        let phase = self.sig_phase.map(|i| snap.signals[i]);
        let bpm = self.sig_bpm.map(|i| snap.signals[i]);
        let confidence = self.sig_confidence.map(|i| snap.signals[i]);
        let position = self.sig_position.map(|i| snap.signals[i]);
        let beat_pos = self.beat.update(dt, phase, bpm, confidence, position);
        for (k, (def, heads)) in plan.effects.iter().enumerate() {
            let [ia, ir, is] = plan.fx_inputs[k];
            let active = !panic_latched && self.input(ia).is_some_and(|s| s.val[0] != 0.0);
            authored |= active;
            let rate = self.input(ir).map(|s| s.val[0]).unwrap_or(def.rate);
            effects::advance(def, &mut self.fx[k], rate, dt, beat_pos);
            if active {
                if def.signal.is_some() {
                    let signal = self.fx_signals[k].and_then(|id| snap.signals.get(id)).copied().unwrap_or(0.0);
                    effects::follow(def, &mut self.fx[k], signal, dt);
                }
                let size = self.input(is).map(|s| s.val[0]).unwrap_or(def.size);
                if size <= 0.0 {
                    self.fx[k].env.fill(0.0);
                    continue;
                }
                for (color, source) in self.colors[k].iter_mut().zip(&plan.fx_colors[k]) {
                    *color = match source {
                        EffectColor::Literal(rgb) => Some(*rgb),
                        EffectColor::Input(id) => {
                            let state = &self.inputs[*id as usize];
                            state.present.then(|| [state.val[0].clamp(0.0, 1.0), state.val[1].clamp(0.0, 1.0), state.val[2].clamp(0.0, 1.0)])
                        }
                    };
                }
                for (mask, id) in self.coverage[k].iter_mut().zip(&plan.fx_coverage[k]) {
                    *mask = self.inputs[*id as usize].present.then_some(self.inputs[*id as usize].val[0] != 0.0).unwrap_or(true);
                }
                let priority = self.inputs[ia as usize].priority;
                for (h, _) in heads {
                    let mut slots = plan.fx_slots[k];
                    let mut protected = 0;
                    while slots != 0 {
                        let s = slots.trailing_zeros() as usize;
                        let bit = 1u32 << s;
                        if self.priorities[*h][s] > priority {
                            protected |= bit;
                        }
                        slots &= !bit;
                    }
                    self.protected[*h] = protected;
                }
                effects::apply(def, &mut self.fx[k], &self.colors[k], heads, &self.coverage[k], &self.protected, size, dt, rate, &mut self.out);
            } else {
                self.fx[k].env.fill(0.0);
                self.fx[k].follow = 0.0;
            }
        }
        // 3. masters, caps, limiter
        let grand = self.input(plan.grand).map(|s| s.val[0]).unwrap_or(1.0).clamp(0.0, 1.0);
        let blackout = self.input(plan.blackout).is_some_and(|s| s.val[0] != 0.0);
        // Ambient is a static fallback only: explicit dark/manual looks and active effects
        // still suppress it. Keep intended_intensity untouched so successful ambient DMX
        // never switches Main off. Global safety and all ordinary caps/limiters still apply.
        if !authored && !blackout && !panic_latched && let Some(idle) = &rig.idle {
            self.out.fill(defaults);
            self.raw.fill(0.0);
            for h in &idle.heads {
                self.out[*h][slot::INTENSITY] = idle.intensity;
                self.out[*h][slot::RED] = idle.color[0];
                self.out[*h][slot::GREEN] = idle.color[1];
                self.out[*h][slot::BLUE] = idle.color[2];
            }
        }
        let safety = &rig.safety;
        self.rehearsal_hold_allowed = !blackout && !panic_latched && grand > 0.0 && safety.max_intensity > 0.0;
        self.frame.limited = 0;
        self.universe_intended.fill(false);
        for l in self.lit.iter_mut() {
            *l = false;
        }
        // rig-wide gate: frames that start a flash anywhere (same-frame onsets count once)
        let room = self.global.room(now, safety.max_flash_hz);
        let mut frame_onset = false;
        for (h, head) in rig.heads.iter().enumerate() {
            let (ms, me) = plan.head_masters[h];
            let mut master = if blackout { 0.0 } else { grand };
            for m in &plan.masters[ms as usize..me as usize] {
                master *= self.input(*m).map(|s| s.val[0].clamp(0.0, 1.0)).unwrap_or(1.0);
            }
            let cap = head.max_intensity.min(safety.max_intensity);
            let stage = self.move_stages[h].as_ref().filter(|s| self.priorities[h][slot::INTENSITY] <= s.priority).map_or(1.0, MoveStage::gain);
            let i = (self.out[h][slot::INTENSITY] * master * stage).min(cap);
            let intended = (self.intended_intensity[h] * master).min(cap);
            self.out[h][slot::INTENSITY] = i;
            if !head.leaf {
                continue;
            }
            let universe = self.head_universes[h];
            // Positive underlying intensity owns the room even during intentional dimmer or
            // palette-black moments; only masters/blackout/brightness caps relinquish it.
            if universe != usize::MAX && intended > 0.0 {
                self.universe_intended[universe] = true;
            }
            let lum = self.luminance(h, i);
            let (allowed, onset) = self.limiters[h].process_gated(lum, now, safety.max_flash_hz, safety.flash_threshold, room || frame_onset);
            frame_onset |= onset;
            if self.limiters[h].limited && lum > 0.0 {
                self.out[h][slot::INTENSITY] = i * (allowed / lum);
                self.frame.limited += 1;
            }
        }
        if frame_onset {
            self.global.record(now);
        }
        for (h, head) in rig.heads.iter().enumerate() {
            if head.leaf
                && self.out[h][slot::INTENSITY] > 0.0
                && let Some(p) = head.parent
            {
                self.lit[p] = true;
            }
        }
        self.frame.suppressed = self.limiters.iter().map(|l| l.suppressed).sum::<u64>();
        // 4. encode
        for u in self.frame.universes.iter_mut() {
            u.fill(0);
        }
        for c in &rig.chans {
            let h = c.head;
            let head = &rig.heads[h];
            let o = &self.out[h];
            let i = o[slot::INTENSITY];
            let (mut r, mut g, mut b, mut w) = (o[slot::RED], o[slot::GREEN], o[slot::BLUE], o[slot::WHITE]);
            if head.white_mix == crate::profile::WhiteMix::Extract {
                let m = r.min(g).min(b);
                r -= m;
                g -= m;
                b -= m;
                w = (w + m).min(1.0);
            }
            let vd = if head.virtual_dimmer { i } else { 1.0 };
            let v: u8 = match &c.enc {
                Enc::Dimmer { fine } => byte16(i, *fine),
                Enc::MasterDimmer => {
                    if self.lit[h] {
                        255
                    } else {
                        0
                    }
                }
                Enc::Emitter { slot: s } => {
                    let x = match *s {
                        slot::RED => r,
                        slot::GREEN => g,
                        slot::BLUE => b,
                        slot::WHITE => w,
                        other => o[other],
                    };
                    byte(x * vd)
                }
                Enc::Subtractive { slot: s } => {
                    let x = match *s {
                        slot::RED => r,
                        slot::GREEN => g,
                        _ => b,
                    };
                    byte(1.0 - x * vd)
                }
                Enc::Wheel { values, colors } => {
                    let mut best = 0;
                    let mut bd = f32::MAX;
                    for (k, col) in colors.iter().enumerate() {
                        let d = (col[0] - o[slot::RED]).powi(2) + (col[1] - o[slot::GREEN]).powi(2) + (col[2] - o[slot::BLUE]).powi(2);
                        if d < bd {
                            bd = d;
                            best = k;
                        }
                    }
                    values[best]
                }
                Enc::Scalar { slot: s, fine, lo, hi } => {
                    let x = o[*s].clamp(0.0, 1.0);
                    if *lo == 0 && *hi == 255 { byte16(x, *fine) } else { lo.saturating_add((x * (*hi as f32 - *lo as f32)).round() as u8) }
                }
                Enc::Gobo { values } => values[(o[slot::GOBO].round() as usize).min(values.len().saturating_sub(1))],
                Enc::Strobe { lo, hi, hz, open, closed } => {
                    let s = o[slot::STROBE];
                    if i <= 0.001 && let Some(closed) = closed {
                        *closed
                    } else if s <= 0.001 {
                        *open
                    } else if safety.strobe == StrobePolicy::Block {
                        self.frame.strobe_capped += 1;
                        *open
                    } else {
                        match hz {
                            Some((a, bz)) => {
                                let want = a + s.min(1.0) * (bz - a);
                                let cap = want.min(safety.max_flash_hz);
                                if want > cap {
                                    self.frame.strobe_capped += 1;
                                }
                                if cap < *a - 1e-4 {
                                    *open
                                } else {
                                    let k = if (bz - a).abs() < 1e-6 { 0.0 } else { (cap - a) / (bz - a) };
                                    lo.saturating_add((k.clamp(0.0, 1.0) * (*hi as f32 - *lo as f32)).round() as u8)
                                }
                            }
                            None => {
                                // unknown rate: can't prove it's safe
                                self.frame.strobe_capped += 1;
                                *open
                            }
                        }
                    }
                }
                Enc::Raw { index } => self.raw[plan.raw_base[h] as usize + index].round() as u8,
                Enc::Fixed(v) => *v,
            };
            self.frame.universes[c.universe][c.channel] = if c.invert { 255 - v } else { v };
        }
        // 5. visualizer view
        for (h, view) in self.frame.heads.iter_mut().enumerate() {
            let o = &self.out[h];
            *view = HeadView {
                intensity: o[slot::INTENSITY],
                color: [o[slot::RED], o[slot::GREEN], o[slot::BLUE]],
                pan: o[slot::PAN],
                tilt: o[slot::TILT],
                zoom: o[slot::ZOOM],
                strobe: o[slot::STROBE],
                limited: self.limiters.get(h).is_some_and(|l| l.limited),
            };
        }
        &self.frame
    }

    /// Stable underlying intensity by universe, independent of dimmer and color dark ticks.
    /// Only a transport's successful writes may commit this preview intention.
    pub fn intended(&self) -> &[bool] {
        &self.universe_intended
    }

    /// Global safety controls bypass rehearsal's held physical output.
    pub fn rehearsal_hold_allowed(&self) -> bool { self.rehearsal_hold_allowed }

    /// Visible level of a head at intensity `i`: brightest additive emitter (1 for heads
    /// without colour mixing).
    fn luminance(&self, h: usize, i: f32) -> f32 {
        let mask = self.plan.emitters[h];
        if mask == 0 {
            return i;
        }
        let o = &self.out[h];
        let mut m = 0.0f32;
        for s in [slot::RED, slot::GREEN, slot::BLUE, slot::WHITE, slot::AMBER, slot::UV, slot::LIME] {
            if mask & emitter_bit(s) != 0 {
                m = m.max(o[s]);
            }
        }
        i * m
    }

    /// Beat position of the effect clock (diagnostics).
    pub fn beat_position(&self) -> f64 {
        self.beat.position()
    }
}

fn byte(x: f32) -> u8 {
    (x.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// Coarse or fine byte of a 16-bit value.
fn byte16(x: f32, fine: bool) -> u8 {
    let v = (x.clamp(0.0, 1.0) * 65535.0).round() as u32;
    if fine { (v & 0xff) as u8 } else { (v >> 8) as u8 }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use se_hub::Snapshot;

    pub fn snapshot(values: &[(&str, Value)], signals: &[(&str, f32)]) -> Snapshot {
        Snapshot {
            tick: 1,
            now: 0,
            generation: 1,
            index: Arc::new(values.iter().enumerate().map(|(i, (a, _))| (a.to_string(), i)).collect()),
            values: values.iter().map(|(_, v)| v.clone()).collect(),
            priorities: Vec::new(),
            signal_names: Arc::new(signals.iter().map(|(n, _)| n.to_string()).collect()),
            signal_index: Arc::new(signals.iter().enumerate().map(|(i, (n, _))| (n.to_string(), i)).collect()),
            signals: signals.iter().map(|(_, v)| *v).collect(),
        }
    }

    pub fn rig(src: &str) -> Arc<Rig> {
        let mut e = Vec::new();
        let lib = crate::profile::library(&Default::default(), &mut e);
        let r = Rig::compile(&toml::from_str(src).unwrap(), lib, e).unwrap();
        assert!(r.errors.is_empty(), "{:?}", r.errors);
        Arc::new(r)
    }

    const RGB3: &str = "[fixtures.par1]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\nlayout_verified = true\n[fixtures.par2]\nprofile = \"generic_rgb\"\nmode = \"4ch\"\naddress = 10\nposition = [0.9, 0.5]\nlayout_verified = true\n[groups]\nfront = [\"par1\", \"par2\"]";

    fn engine(src: &str, fx: Vec<EffectDef>) -> Engine {
        let (p, errs) = Plan::build(rig(src), fx);
        assert!(errs.is_empty(), "{errs:?}");
        Engine::new(Arc::new(p))
    }

    #[test]
    fn idle_is_bounded_static_and_never_claims_show_intention() {
        let src = format!("{RGB3}\n[idle]\ntarget = \"par1\"\ncolor = \"#ff69b4\"\nintensity = 0.3\n[safety]\nmax_intensity = 0.35\nstrobe = \"block\"");
        let mut e = engine(&src, vec![]);
        let idle = snapshot(&[], &[]);
        let u = e.render(&idle, 1_000_000_000).universes[0];
        assert_eq!(&u[..3], &[77, 32, 54]);
        assert_eq!(u[9], 0, "other physical dimmer stays dark");
        assert_eq!(e.intended(), &[false]);

        let show = snapshot(&[
            ("lights.par2.intensity", Value::Float(1.0)),
            ("lights.par2.color", Value::from("#0000ff")),
        ], &[]);
        let u = e.render(&show, 2_000_000_000).universes[0];
        assert_eq!(&u[..3], &[0, 0, 0], "fallback is not mixed into the show");
        assert_eq!(u[9], 89, "safety cap applies to authored light");
        assert_eq!(e.intended(), &[true]);
        assert_eq!(&e.render(&idle, 3_000_000_000).universes[0][..3], &[77, 32, 54]);
        assert_eq!(e.intended(), &[false]);

        for address in ["lights.blackout", "lights.panic_latched"] {
            let s = snapshot(&[(address, Value::Bool(true))], &[]);
            assert_eq!(&e.render(&s, 4_000_000_000).universes[0][..3], &[0; 3]);
            assert_eq!(e.intended(), &[false]);
        }
        let mut dark = snapshot(&[("lights.par1.intensity", Value::Float(0.0))], &[]);
        dark.priorities = vec![se_proto::PRIORITY_MANUAL];
        assert_eq!(&e.render(&dark, 5_000_000_000).universes[0][..3], &[0; 3], "intentional manual darkness suppresses ambient");
        let master = snapshot(&[("lights.master", Value::Float(0.5))], &[]);
        assert_eq!(&e.render(&master, 6_000_000_000).universes[0][..3], &[38, 16, 27]);
        let mut capped = engine(&src.replace("max_intensity = 0.35", "max_intensity = 0.1"), vec![]);
        capped.render(&idle, 7_000_000_000);
        assert_eq!(capped.frame.heads[0].intensity, 0.1, "idle cannot bypass the rig brightness cap");
    }

    #[test]
    fn virtual_and_physical_dimmers_encode() {
        let mut e = engine(RGB3, vec![]);
        let s = snapshot(
            &[
                ("lights.par1.intensity", Value::Float(0.5)),
                ("lights.par1.color", Value::from([1.0f32, 0.5, 0.0, 1.0])),
                ("lights.par2.intensity", Value::Float(1.0)),
                ("lights.par2.color", Value::Str("#0000ff".into())),
            ],
            &[],
        );
        let f = e.render(&s, 1_000_000);
        let u = &f.universes[0];
        assert_eq!(&u[0..3], &[128, 64, 0], "3ch: virtual dimmer scales RGB");
        assert_eq!(&u[9..13], &[255, 0, 0, 255], "4ch: dimmer + RGB");
    }

    #[test]
    fn group_ltp_by_latest_change_and_htp_intensity() {
        let mut e = engine(RGB3, vec![]);
        let mut vals = vec![
            ("lights.par1.intensity", Value::Float(0.2)),
            ("lights.par1.color", Value::from([1.0f32, 0.0, 0.0, 1.0])),
            ("lights.group.front.intensity", Value::Float(0.0)),
            ("lights.group.front.color", Value::Null),
            ("lights.group.front.master", Value::Float(1.0)),
        ];
        let f = e.render(&snapshot(&vals, &[]), 1).universes[0];
        assert_eq!(&f[0..3], &[51, 0, 0]);
        // group colour set later → wins; group intensity higher → HTP
        vals[3].1 = Value::from([0.0f32, 1.0, 0.0, 1.0]);
        vals[2].1 = Value::Float(1.0);
        let f = e.render(&snapshot(&vals, &[]), 2).universes[0];
        assert_eq!(&f[0..3], &[0, 255, 0]);
        // fixture colour changes afterwards → fixture wins again
        vals[1].1 = Value::from([0.0f32, 0.0, 1.0, 1.0]);
        let f = e.render(&snapshot(&vals, &[]), 3).universes[0];
        assert_eq!(&f[0..3], &[0, 0, 255]);
        // group master halves everything
        vals[4].1 = Value::Float(0.5);
        let f = e.render(&snapshot(&vals, &[]), 4).universes[0];
        assert_eq!(&f[0..3], &[0, 0, 128]);
        // released group colour (null) and grand master 0 blacks out
        vals.push(("lights.master", Value::Float(0.0)));
        let f = e.render(&snapshot(&vals, &[]), 5).universes[0];
        assert_eq!(&f[0..3], &[0, 0, 0]);
    }

    #[test]
    fn moving_head_16bit_wheel_gobo_and_strobe_cap() {
        let src = "[fixtures.mh]\nprofile = \"generic_moving_head\"\naddress = 1\n";
        let r = rig(src);
        let mode = r.profiles["generic_moving_head"].mode(None).unwrap().clone();
        let pos = |role: crate::profile::Role| mode.channels.iter().position(|c| c.role == role).unwrap();
        let (p, _) = Plan::build(r.clone(), vec![]);
        let mut e = Engine::new(Arc::new(p));
        let s = snapshot(
            &[
                ("lights.mh.intensity", Value::Float(1.0)),
                ("lights.mh.pan", Value::Float(0.5)),
                ("lights.mh.tilt", Value::Float(1.0)),
                ("lights.mh.color", Value::Str("#ff0000".into())),
                ("lights.mh.gobo", Value::Int(2)),
                ("lights.mh.strobe", Value::Float(1.0)),
            ],
            &[],
        );
        e.render(&s, 1);
        let u = e.render(&s, 2_000_000_001).universes[0];
        use crate::profile::Role;
        assert_eq!((u[pos(Role::Pan)], u[pos(Role::PanFine)]), (128, 0), "pan 0.5 = 32768");
        assert_eq!((u[pos(Role::Tilt)], u[pos(Role::TiltFine)]), (255, 255));
        let wheel = &mode.channels[pos(Role::ColorWheel)];
        let red = wheel.slots.iter().find(|s| s.color == Some([1.0, 0.0, 0.0])).unwrap().value;
        assert_eq!(u[pos(Role::ColorWheel)], red);
        let gobo = &mode.channels[pos(Role::Gobo)];
        assert_eq!(u[pos(Role::Gobo)], gobo.slots[2].value);
        // strobe 1.0 = 20 Hz requested → capped at 3 Hz within the shutter's strobe range
        let sh = mode.channels.iter().find(|c| matches!(c.role, Role::Shutter | Role::Strobe)).unwrap();
        let (lo, hi) = sh.range.unwrap();
        let (a, b) = sh.hz.unwrap();
        let v = u[mode.channels.iter().position(|c| matches!(c.role, Role::Shutter | Role::Strobe)).unwrap()];
        let hz = a + (v - lo) as f32 / (hi - lo) as f32 * (b - a);
        assert!(hz <= 3.05, "strobe channel {v} ≈ {hz} Hz");
        assert!(e.frame.strobe_capped > 0);
    }

    #[test]
    fn steady_circle_stays_illuminated_and_operator_intensity_bypasses_authored_travel() {
        let fx = EffectDef::parse("orbit", &toml::from_str("kind='circle'\nrate=0.125\nsize=0.4\norder='index'").unwrap()).unwrap();
        let mut e = engine("[fixtures.mh]\nprofile='generic_moving_head'\naddress=1", vec![fx]);
        let mut values = vec![
            ("lights.mh.intensity", Value::Float(0.8)),
            ("lights.mh.pan", Value::Float(0.5)),
            ("lights.mh.tilt", Value::Float(0.5)),
            ("lights.mh.gobo", Value::Int(0)),
            ("lights.effect.orbit.active", Value::Bool(true)),
        ];
        let mut s = snapshot(&values, &[]);
        s.priorities = vec![200; values.len()];
        assert_eq!(e.render(&s, 1_000_000_000).heads[0].intensity, 0.0);
        let first = e.render(&s, 3_000_000_000).heads[0];
        let second = e.render(&s, 3_500_000_000).heads[0];
        assert!((first.pan - second.pan).abs() > 0.03, "circle must actually move");
        assert!((first.intensity - 0.8).abs() < 1e-6 && (second.intensity - 0.8).abs() < 1e-6, "circle output changes must not retrigger blanking");
        values[1].1 = Value::Float(0.7);
        s = snapshot(&values, &[]);
        s.priorities = vec![200; values.len()];
        assert_eq!(e.render(&s, 4_000_000_000).heads[0].intensity, 0.0, "authored travel closes the shutter");
        s.priorities[0] = 300;
        assert!((e.render(&s, 5_000_000_000).heads[0].intensity - 0.8).abs() < 1e-6, "higher-priority operator intensity stays authoritative");
    }

    #[test]
    fn led_bar_cells_follow_root_and_master_dimmer() {
        let mut e = engine("[fixtures.bar]\nprofile = \"generic_led_bar\"\nmode = \"26ch\"\naddress = 1\n", vec![]);
        let mut vals = vec![("lights.bar.intensity", Value::Float(1.0)), ("lights.bar.color", Value::from([1.0f32, 0.0, 0.0, 1.0]))];
        let u = e.render(&snapshot(&vals, &[]), 1).universes[0];
        assert_eq!(u[0], 255, "master dimmer open while cells are lit");
        for c in 0..8 {
            assert_eq!(&u[2 + c * 3..5 + c * 3], &[255, 0, 0], "cell {c}");
        }
        vals.push(("lights.bar_3.color", Value::from([0.0f32, 0.0, 1.0, 1.0])));
        let u = e.render(&snapshot(&vals, &[]), 2).universes[0];
        assert_eq!(&u[2 + 2 * 3..5 + 2 * 3], &[0, 0, 255], "a cell address overrides the root after it changes");
        vals[0].1 = Value::Float(0.0);
        let u = e.render(&snapshot(&vals, &[]), 3).universes[0];
        assert_eq!(u[0], 0);
    }

    #[test]
    fn scanner_blackout_closes_shutter_without_losing_aim_or_wheel() {
        let mut e = engine("[fixtures.scan]\nprofile = \"adj_inno_pocket_scan\"\nmode = \"6ch\"\naddress = 15\n", vec![]);
        let mut values = vec![
            ("lights.scan.intensity", Value::Float(0.5)),
            ("lights.scan.pan", Value::Float(0.25)),
            ("lights.scan.tilt", Value::Float(0.75)),
            ("lights.scan.gobo", Value::Int(2)),
            ("lights.scan.strobe", Value::Float(1.0)),
            ("lights.blackout", Value::Bool(false)),
        ];
        e.render(&snapshot(&values, &[]), 1);
        let lit = e.render(&snapshot(&values, &[]), 2_000_000_000).universes[0];
        assert_eq!(&lit[14..20], &[64, 191, 8, 15, 128, 0], "uncalibrated strobe remains open, wheel stays in a discrete fixed slot");
        values[5].1 = Value::Bool(true);
        let dark = e.render(&snapshot(&values, &[]), 2_020_000_000).universes[0];
        assert_eq!(&dark[14..20], &[64, 191, 0, 15, 0, 0], "blackout closes shutter and zeros dimmer, without sweeping mirror or wheel");
        values[5].1 = Value::Bool(false);
        let restored = e.render(&snapshot(&values, &[]), 2_040_000_000).universes[0];
        assert_eq!(&restored[14..20], &lit[14..20]);
    }

    #[test]
    fn direct_mode_blackout_zeros_pixels_and_preserves_colorstrip_gate() {
        let mut e = engine(
            "[fixtures.bars]\nprofile = \"chauvet_colorstrip\"\naddress = 1\n\
             [fixtures.stick]\nprofile = \"chauvet_freedom_stick\"\naddress = 300\n",
            vec![],
        );
        let values = [
            ("lights.bars.intensity", Value::Float(0.5)),
            ("lights.bars.color", Value::from([1.0f32, 0.0, 0.0, 1.0])),
            ("lights.stick_16.intensity", Value::Float(0.5)),
            ("lights.stick_16.color", Value::from([0.0f32, 0.0, 1.0, 1.0])),
        ];
        let lit = e.render(&snapshot(&values, &[]), 1).universes[0];
        assert_eq!(&lit[0..4], &[210, 128, 0, 0]);
        assert_eq!(&lit[344..349], &[0, 0, 128, 0, 255], "last logical pixel opens the supplementary-profile master without requesting strobe");
        let mut dark_values = values.to_vec();
        dark_values.push(("lights.blackout", Value::Bool(true)));
        let dark = e.render(&snapshot(&dark_values, &[]), 2).universes[0];
        assert_eq!(&dark[0..4], &[210, 0, 0, 0], "blackout preserves direct RGB mode");
        assert!(dark[299..349].iter().all(|byte| *byte == 0), "all pixels zero; blackout never relies solely on disputed channel 50");
    }

    #[test]
    fn lighting_intention_survives_dimmer_dark_ticks_but_not_zero_levels() {
        let src = "[fixtures.par1]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\n";
        for kind in ["dimmer_triangle", "dimmer_square", "sparkle"] {
            let fx = EffectDef::parse("pulse", &toml::from_str(&format!(
                "kind = \"{kind}\"\nunit = \"hz\"\nrate = 1\nsize = 1\nspread = 0\norder = \"index\"\ntargets = [\"par1\"]"
            )).unwrap()).unwrap();
            let mut e = engine(src, vec![fx]);
            let values = vec![
                ("lights.par1.intensity", Value::Float(0.5)),
                ("lights.effect.pulse.active", Value::Bool(true)),
            ];
            // Triangle starts at its trough; square is off in the second half-cycle;
            // sparkle has no envelope until an event occurs (dt = 0 on its first frame).
            e.render(&snapshot(&values, &[]), 1);
            if kind == "dimmer_square" {
                e.render(&snapshot(&values, &[]), 750_000_001);
            }
            assert_eq!(&e.frame.universes[0][..3], &[0; 3], "{kind} is physically dark");
            assert_eq!(e.intended(), &[true], "{kind} still has underlying show lighting");
            for (address, value) in [
                ("lights.blackout", Value::Bool(true)),
                ("lights.master", Value::Float(0.0)),
                ("lights.par1.intensity", Value::Float(0.0)),
            ] {
                let mut off = values.clone();
                if address == "lights.par1.intensity" {
                    off[0].1 = value;
                } else {
                    off.push((address, value));
                }
                e.render(&snapshot(&off, &[]), 800_000_001);
                assert_eq!(e.intended(), &[false], "{kind}: {address}");
            }
            e.render(&snapshot(&values, &[]), 900_000_001);
            assert_eq!(e.intended(), &[true], "releasing the zero level restores intention");
        }
    }

    #[test]
    fn lighting_intention_survives_black_solid_and_color_wave_palette_ticks() {
        let fx = EffectDef::parse("color", &toml::from_str(
            "kind = \"color_wave\"\nunit = \"hz\"\nrate = 1\nsize = 1\nspread = 0\norder = \"index\"\n\
             colors = [\"#ff0000\", \"#000000\"]\ntargets = [\"par1\"]"
        ).unwrap()).unwrap();
        let mut e = engine(
            "[fixtures.par1]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\n",
            vec![fx],
        );
        let mut values = [
            ("lights.par1.intensity", Value::Float(0.5)),
            ("lights.par1.color", Value::from("#000000")),
            ("lights.effect.color.active", Value::Bool(false)),
        ];
        e.render(&snapshot(&values, &[]), 1);
        assert_eq!(&e.frame.universes[0][..3], &[0; 3]);
        assert_eq!(e.intended(), &[true], "black solid with positive intensity is show intention");
        values[2].1 = Value::Bool(true);
        e.render(&snapshot(&values, &[]), 250_000_001);
        assert!(e.frame.universes[0][0] > 0, "color wave lights the black starting solid");
        assert_eq!(e.intended(), &[true]);
        e.render(&snapshot(&values, &[]), 500_000_001);
        assert_eq!(&e.frame.universes[0][..3], &[0; 3], "the wave reaches its black palette entry");
        assert_eq!(e.intended(), &[true], "intentional color-wave darkness cannot restore Main");
        values[0].1 = Value::Float(0.0);
        e.render(&snapshot(&values, &[]), 750_000_001);
        assert_eq!(e.intended(), &[false], "zero underlying intensity still relinquishes the room");
    }

    #[test]
    fn lighting_intention_respects_group_and_final_brightness_caps_per_universe() {
        let src = "[fixtures.par1]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\nuniverse = 1\n\
                   [fixtures.par2]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\nuniverse = 2\n\
                   [groups]\nfront = [\"par1\"]\n";
        let values = [
            ("lights.par1.intensity", Value::Float(1.0)),
            ("lights.par2.intensity", Value::Float(1.0)),
            ("lights.group.front.master", Value::Float(0.0)),
        ];
        let mut e = engine(src, vec![]);
        e.render(&snapshot(&values, &[]), 1);
        assert_eq!(e.intended(), &[false, true]);
        let mut capped = (*rig(src)).clone();
        capped.safety.max_intensity = 0.0;
        let (p, errors) = Plan::build(Arc::new(capped), vec![]);
        assert!(errors.is_empty());
        e.set_plan(Arc::new(p));
        e.render(&snapshot(&values, &[]), 2);
        assert_eq!(e.intended(), &[false, false], "a final zero brightness cap defeats intention");
    }

    #[test]
    fn panic_latch_prevents_legacy_effects_from_modulating_safe_levels() {
        let fx = EffectDef::parse("pulse", &toml::from_str(
            "kind = \"dimmer_sine\"\nunit = \"beats\"\nrate = 1\nsize = 1\nspread = 0\ntargets = [\"par1\"]"
        ).unwrap()).unwrap();
        let mut e = engine(RGB3, vec![fx]);
        let mut values = vec![
            ("lights.par1.intensity", Value::Float(0.25)),
            ("lights.par1.color", Value::from("#ff0000")),
            ("lights.effect.pulse.active", Value::Bool(true)),
            ("lights.panic_latched", Value::Bool(false)),
        ];
        let signals = [("beat.phase", 0.0), ("beat.position", 0.0), ("beat.bpm", 120.0), ("beat.confidence", 1.0)];
        let animated = e.render(&snapshot(&values, &signals), 1).universes[0];
        assert!(animated[0] < 64, "effect modulates the base before panic");
        values[3].1 = Value::Bool(true);
        let safe = e.render(&snapshot(&values, &signals), 2).universes[0];
        assert_eq!(&safe[..3], &[64, 0, 0], "latched safe look is not animated by a later legacy start");
    }

    #[test]
    fn beat_synced_chase_follows_beat_phase() {
        let fx = EffectDef::parse(
            "chase",
            &toml::from_str("kind = \"color_chase\"\nunit = \"beats\"\nrate = 2\nspread = 0\ncolors = [\"#ff0000\", \"#0000ff\"]\ntargets = [\"front\"]")
                .unwrap(),
        )
        .unwrap();
        let mut e = engine(RGB3, vec![fx]);
        let mut seen = Vec::new();
        // 100 BPM synthetic beat signal, sampled at 44 Hz for 6 beats
        let frame_ns = 22_727_273u64;
        for k in 1..=(44.0 * 3.6) as u64 {
            let t = k * frame_ns;
            let beats = t as f64 / 1e9 * 100.0 / 60.0;
            let s = snapshot(
                &[("lights.par1.intensity", Value::Float(1.0)), ("lights.effect.chase.active", Value::Bool(true))],
                &[("beat.phase", beats.fract() as f32), ("beat.bpm", 100.0)],
            );
            let u = e.render(&s, t).universes[0];
            let red = u[0] == 255 && u[2] == 0;
            let blue = u[0] == 0 && u[2] == 255;
            assert!(red || blue, "frame {k}: {:?}", &u[0..3]);
            // colour index = floor(beat position / 1 beat) mod 2 (2 colours over 2 beats)
            let expect_red = (beats.floor() as u64).is_multiple_of(2);
            // allow the first frame after a beat boundary (phase sampled in the same frame)
            if (beats.fract()) > 0.05 {
                assert_eq!(red, expect_red, "frame {k} at beat {beats:.3}");
            }
            seen.push(red);
        }
        let switches = seen.windows(2).filter(|w| w[0] != w[1]).count();
        assert_eq!(switches, 5, "6 beats → 5 colour changes");
    }

    #[test]
    fn limiter_caps_a_fast_cue_strobe_on_the_output() {
        let mut e = engine(RGB3, vec![]);
        let frame_ns = 22_727_273u64;
        let mut level = Vec::new();
        for k in 1..=440u64 {
            let t = k * frame_ns;
            let on = (t / 50_000_000).is_multiple_of(2); // 10 Hz from the control layer
            let s = snapshot(&[("lights.par1.intensity", Value::Float(if on { 1.0 } else { 0.0 }))], &[]);
            level.push((t, e.render(&s, t).universes[0][0]));
        }
        // count onsets on the DMX output (rise ≥ 20 % from a dark state)
        let mut onsets = Vec::new();
        let (mut low, mut peak, mut high) = (0u8, 0u8, false);
        for &(t, v) in &level {
            if high {
                peak = peak.max(v);
                if peak.saturating_sub(v) >= 51 {
                    high = false;
                    low = v;
                }
            } else {
                low = low.min(v);
                if v.saturating_sub(low) >= 51 && low < 204 {
                    onsets.push(t);
                    high = true;
                    peak = v;
                }
            }
        }
        let worst = onsets.iter().map(|t0| onsets.iter().filter(|t| **t >= *t0 && **t - *t0 < 1_000_000_000).count()).max().unwrap();
        assert!(worst <= 3, "{worst} flashes within 1 s on the DMX output");
        assert!(onsets.len() >= 25, "still flashing about 3/s ({})", onsets.len());
    }

    #[test]
    fn rig_wide_gate_stops_a_fast_chase_across_many_heads() {
        let src: String = (0..8).map(|k| format!("[fixtures.d{k}]\nprofile = \"generic_dimmer\"\nmode = \"1ch\"\naddress = {}\n", k + 1)).collect();
        let mut e = engine(&src, vec![]);
        let frame_ns = 22_727_273u64;
        let mut prev = [0u8; 8];
        let mut onset_frames = Vec::new();
        let mut lows = [0u8; 8];
        let mut high = [false; 8];
        let mut peak = [0u8; 8];
        for k in 1..=440u64 {
            let t = k * frame_ns;
            // each head blinks at 1 Hz, phases spread → 8 flashes/s rig-wide
            let vals: Vec<(String, Value)> = (0..8)
                .map(|h| {
                    let ph = (t as f64 / 1e9 + h as f64 / 8.0).fract();
                    (format!("lights.d{h}.intensity"), Value::Float(if ph < 0.1 { 1.0 } else { 0.0 }))
                })
                .collect();
            let refs: Vec<(&str, Value)> = vals.iter().map(|(a, v)| (a.as_str(), v.clone())).collect();
            let u = e.render(&snapshot(&refs, &[]), t).universes[0];
            let mut any = false;
            for h in 0..8 {
                let v = u[h];
                if high[h] {
                    peak[h] = peak[h].max(v);
                    if peak[h].saturating_sub(v) >= 51 {
                        high[h] = false;
                        lows[h] = v;
                    }
                } else {
                    lows[h] = lows[h].min(v);
                    if v.saturating_sub(lows[h]) >= 51 && lows[h] < 204 {
                        high[h] = true;
                        peak[h] = v;
                        any = true;
                    }
                }
            }
            prev.copy_from_slice(&u[0..8]);
            if any {
                onset_frames.push(t);
            }
        }
        let worst = onset_frames.iter().map(|t0| onset_frames.iter().filter(|t| **t >= *t0 && **t - *t0 < 1_000_000_000).count()).max().unwrap();
        assert!(worst <= 3, "{worst} flash frames within 1 s rig-wide");
        assert!(onset_frames.len() >= 25, "{}", onset_frames.len());
    }

    #[test]
    fn triangle_cascade_addresses_every_stick_cell_and_preserves_coverage_and_caps() {
        let fx = EffectDef::parse("cascade", &toml::from_str(
            "kind = \"dimmer_triangle\"\nunit = \"beats\"\nrate = 1\norder = \"index\"\nspread = 1\ntargets = [\"stick\"]"
        ).unwrap()).unwrap();
        let mut e = engine("[fixtures.stick]\nprofile = \"chauvet_freedom_stick\"\nmode = \"50ch\"\naddress = 1\nmax_intensity = 0.15", vec![fx]);
        let values = [
            ("lights.stick.intensity", Value::Float(0.2)),
            ("lights.stick.color", Value::from("#ffffff")),
            ("lights.effect.cascade.active", Value::Bool(true)),
            ("lights.effect.cascade.size", Value::Float(1.0)),
            ("lights.effect.cascade.coverage.stick_16", Value::Bool(false)),
        ];
        let signals = [("beat.position", 0.5), ("beat.bpm", 120.0), ("beat.confidence", 1.0)];
        let snap = snapshot(&values, &signals);
        let frame = e.render(&snap, 1);
        for cell in 0..16 {
            let phase = (0.5f32 - cell as f32 / 16.0).rem_euclid(1.0);
            let intensity = if cell == 15 { 0.15 } else { (0.2 * (1.0 - (2.0 * phase - 1.0).abs())).min(0.15) };
            let expected = byte(intensity);
            assert_eq!(&frame.universes[0][cell * 3..cell * 3 + 3], &[expected; 3], "Stick cell {}", cell + 1);
        }
        assert_eq!(frame.universes[0][48], 0, "triangle does not touch hardware strobe");
        assert_eq!(frame.universes[0][49], 255, "lit cells retain the shared master");
    }

    #[test]
    fn live_palette_recolors_stepped_and_smooth_sequences_without_restarting() {
        for kind in ["color_chase", "color_wave"] {
            let fx = EffectDef::parse("sequence", &toml::from_str(&format!(
                "kind = \"{kind}\"\nunit = \"beats\"\nrate = 4\nspread = 0\norder = \"index\"\ntargets = [\"par1\"]\ncolors = [\"stream:accent\", \"stream:background\"]"
            )).unwrap()).unwrap();
            let mut e = engine(RGB3, vec![fx]);
            let mut snap = snapshot(&[
                ("lights.par1.intensity", Value::Float(0.2)),
                ("lights.par1.color", Value::from("#00ffff")),
                ("lights.effect.sequence.active", Value::Bool(true)),
                ("palette.accent", Value::from("#ff0000")),
                ("palette.background", Value::from("#0000ff")),
            ], &[("beat.position", 1.0), ("beat.bpm", 120.0), ("beat.confidence", 1.0)]);
            let initial = e.render(&snap, 1).heads[0].color;
            assert_eq!(initial, if kind == "color_chase" { [1.0, 0.0, 0.0] } else { [0.5, 0.0, 0.5] });
            snap.values[3] = Value::from("#00ff00");
            snap.signals[0] = 1.5;
            let recolored = e.render(&snap, 250_000_001).heads[0].color;
            let expected = if kind == "color_chase" { [0.0, 1.0, 0.0] } else { [0.0, 0.15625, 0.84375] };
            assert_eq!(recolored, expected, "{kind} keeps advancing while following the live endpoint");
            assert_eq!(e.fx[0].phase, 0.375);
            snap.values[3] = Value::Null;
            let missing = e.render(&snap, 250_000_002).heads[0].color;
            assert_eq!(missing, [0.0, 1.0, 1.0], "{kind} releases color while an endpoint is unavailable");
            snap.values[3] = Value::from("#ffffff");
            snap.priorities = vec![100, 300, 200, 100, 100];
            let protected = e.render(&snap, 250_000_003).heads[0].color;
            assert_eq!(protected, [0.0, 1.0, 1.0], "{kind} cannot overwrite programmer-owned color");
        }
    }

    #[test]
    fn follow_uses_live_signal_ids_after_reindex_and_plan_reload() {
        let fx = EffectDef::parse("hit", &toml::from_str("kind='follow'\nsignal='band.kick'\norder='index'\nattack=0\nrelease=0").unwrap()).unwrap();
        let mut e = engine(RGB3, vec![fx]);
        let values = [("lights.par1.intensity", Value::Float(0.1)), ("lights.effect.hit.active", Value::Bool(true))];
        let a = snapshot(&values, &[("band.kick", 0.5)]);
        assert_eq!(e.render(&a, 1).universes[0][0], 13);
        let b = snapshot(&values, &[("other", 0.9), ("band.kick", 0.25)]);
        assert_eq!(e.render(&b, 22_000_001).universes[0][0], 6);
        e.set_plan(e.plan().clone());
        assert_eq!(e.render(&b, 44_000_001).universes[0][0], 6, "plan replacement must invalidate cached signal ids too");
    }

    #[test]
    fn fast_kick_follow_remains_flash_limited_on_encoded_output() {
        let fx = EffectDef::parse("hit", &toml::from_str("kind='follow'\nsignal='band.kick'\norder='index'\nattack=0\nrelease=0").unwrap()).unwrap();
        let mut e = engine(RGB3, vec![fx]);
        let mut snap = snapshot(&[("lights.par1.intensity", Value::Float(1.0)), ("lights.effect.hit.active", Value::Bool(true))], &[("band.kick", 0.0)]);
        let (mut low, mut peak, mut high) = (0u8, 0u8, false);
        let mut onsets = Vec::new();
        for k in 1..=440u64 {
            let t = k * 22_727_273;
            snap.signals[0] = if (t / 50_000_000).is_multiple_of(2) { 1.0 } else { 0.0 };
            let v = e.render(&snap, t).universes[0][0];
            if high {
                peak = peak.max(v);
                if peak.saturating_sub(v) >= 51 { high = false; low = v; }
            } else {
                low = low.min(v);
                if v.saturating_sub(low) >= 51 && low < 204 { onsets.push(t); high = true; peak = v; }
            }
        }
        assert!(onsets.iter().all(|start| onsets.iter().filter(|t| **t >= *start && **t - *start < 1_000_000_000).count() <= 3));
        assert!(onsets.len() >= 25, "limiting preserves allowed musical hits rather than suppressing the effect");
    }

    #[test]
    fn render_does_not_allocate() {
        let fx = EffectDef::parse("sp", &toml::from_str("kind = \"sparkle\"\nrate = 5").unwrap()).unwrap();
        let fx2 = EffectDef::parse("rb", &toml::from_str("kind = \"rainbow\"\nunit = \"beats\"").unwrap()).unwrap();
        let fx3 = EffectDef::parse("wave", &toml::from_str("kind = \"color_wave\"\ncolors = [\"stream:accent\", \"stream:background\"]").unwrap()).unwrap();
        let follow = EffectDef::parse("follow", &toml::from_str("kind='follow'\nsignal='band.kick'\nattack='10ms'\nrelease='180ms'").unwrap()).unwrap();
        let color = EffectDef::parse("color", &toml::from_str("kind='follow_color'\nsignal='band.snare'\ncolors=['@lx.color.a']").unwrap()).unwrap();
        let mut e = engine(RGB3, vec![fx, fx2, fx3, follow, color]);
        let s = snapshot(
            &[
                ("lights.par1.intensity", Value::Float(1.0)),
                ("lights.group.front.color", Value::from([0.2f32, 0.3, 0.4, 1.0])),
                ("lights.effect.sp.active", Value::Bool(true)),
                ("lights.effect.rb.active", Value::Bool(true)),
                ("lights.effect.wave.active", Value::Bool(true)),
                ("palette.accent", Value::from("#ffaa88")),
                ("palette.background", Value::from("#001144")),
                ("lights.effect.follow.active", Value::Bool(true)),
                ("lights.effect.color.active", Value::Bool(true)),
                ("lx.color.a", Value::from("#ff0000")),
            ],
            &[("beat.phase", 0.3), ("band.kick", 0.7), ("band.snare", 0.4)],
        );
        e.render(&s, 1);
        assert!(se_alloc::installed());
        let scope = se_alloc::Scope::begin();
        for k in 2..200u64 {
            e.render(&s, k * 22_000_000);
        }
        assert_eq!(scope.allocs(), 0, "render allocated");
    }
}
