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
    fx_inputs: Vec<[u32; 3]>,
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
        let mut fx = Vec::new();
        let mut fx_inputs = Vec::new();
        for e in effects {
            match e.layout(&rig) {
                Ok(l) => {
                    fx_inputs.push([
                        input(format!("lights.effect.{}.active", e.name), InKind::Bool),
                        input(format!("lights.effect.{}.rate", e.name), InKind::Scalar),
                        input(format!("lights.effect.{}.size", e.name), InKind::Scalar),
                    ]);
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
        (Plan { rig, effects: fx, inputs, binds, srcs, masters, head_masters, raw_base, raw_len, grand, blackout, fx_inputs, emitters }, errors)
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
    limiters: Vec<Limiter>,
    global: OnsetWindow,
    fx: Vec<EffectState>,
    beat: BeatClock,
    pub frame: Frame,
    count: u64,
    index: Option<Arc<HashMap<String, usize>>>,
    generation: u64,
    signals: Option<Arc<HashMap<String, usize>>>,
    sig_phase: Option<usize>,
    sig_bpm: Option<usize>,
    time_ns: u64,
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
            limiters: Vec::new(),
            global: OnsetWindow::default(),
            fx: Vec::new(),
            beat: BeatClock::default(),
            frame: Frame::default(),
            count: 0,
            index: None,
            generation: u64::MAX,
            signals: None,
            sig_phase: None,
            sig_bpm: None,
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
        self.lit = vec![false; n];
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
            })
            .collect();
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
            if present != st.present || v != st.val {
                st.stamp = stamp;
                st.present = present;
                st.val = v;
            }
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
        for o in self.out.iter_mut() {
            *o = defaults;
        }
        // 1. merge fixture / parent / group addresses
        for b in &plan.binds {
            let h = b.head as usize;
            let srcs = &plan.srcs[b.groups.0 as usize..b.groups.1 as usize];
            match b.kind {
                AttrKind::Intensity => {
                    let mut v = self.input(b.own).map(|s| s.val[0]).unwrap_or(b.default[0]);
                    if let Some(p) = self.input(b.parent) {
                        v = v.max(p.val[0]);
                    }
                    for g in srcs {
                        if let Some(s) = self.input(*g) {
                            v = v.max(s.val[0]);
                        }
                    }
                    self.out[h][slot::INTENSITY] = v.clamp(0.0, 1.0);
                }
                kind => {
                    // LTP across the fixture address and its parent/groups: the most recently
                    // changed present value wins; ties go to the most specific source.
                    let mut best: Option<&InputState> = self.input(b.own);
                    for i in std::iter::once(b.parent).chain(srcs.iter().copied()) {
                        if let Some(s) = self.input(i)
                            && best.is_none_or(|x| s.stamp > x.stamp)
                        {
                            best = Some(s);
                        }
                    }
                    let v = best.map(|s| s.val).unwrap_or([b.default[0], b.default[1], b.default[2], 1.0]);
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
        // 2. effects
        let phase = self.sig_phase.map(|i| snap.signals[i]);
        let bpm = self.sig_bpm.map(|i| snap.signals[i]);
        let beat_pos = self.beat.update(dt, phase, bpm);
        for (k, (def, heads)) in plan.effects.iter().enumerate() {
            let [ia, ir, is] = plan.fx_inputs[k];
            let active = self.input(ia).is_some_and(|s| s.val[0] != 0.0);
            let rate = self.input(ir).map(|s| s.val[0]).unwrap_or(def.rate);
            let size = self.input(is).map(|s| s.val[0]).unwrap_or(def.size);
            let st = &mut self.fx[k];
            effects::advance(def, st, rate, dt, beat_pos);
            if active {
                effects::apply(def, st, heads, size, dt, rate, &mut self.out);
            } else {
                for e in st.env.iter_mut() {
                    *e = 0.0;
                }
            }
        }
        // 3. masters, caps, limiter
        let grand = self.input(plan.grand).map(|s| s.val[0]).unwrap_or(1.0).clamp(0.0, 1.0);
        let blackout = self.input(plan.blackout).is_some_and(|s| s.val[0] != 0.0);
        let safety = &rig.safety;
        self.frame.limited = 0;
        for l in self.lit.iter_mut() {
            *l = false;
        }
        // rig-wide gate: frames that start a flash anywhere (same-frame onsets count once)
        let room = self.global.room(now, safety.max_flash_hz);
        let mut frame_onset = false;
        for (h, head) in rig.heads.iter().enumerate() {
            let (ms, me) = plan.head_masters[h];
            let mut i = self.out[h][slot::INTENSITY];
            for m in &plan.masters[ms as usize..me as usize] {
                i *= self.input(*m).map(|s| s.val[0].clamp(0.0, 1.0)).unwrap_or(1.0);
            }
            i *= if blackout { 0.0 } else { grand };
            i = i.min(head.max_intensity).min(safety.max_intensity);
            self.out[h][slot::INTENSITY] = i;
            if !head.leaf {
                continue;
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
                Enc::Strobe { lo, hi, hz, open } => {
                    let s = o[slot::STROBE];
                    if s <= 0.001 {
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

    const RGB3: &str = "[fixtures.par1]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\n[fixtures.par2]\nprofile = \"generic_rgb\"\nmode = \"4ch\"\naddress = 10\nposition = [0.9, 0.5]\n[groups]\nfront = [\"par1\", \"par2\"]";

    fn engine(src: &str, fx: Vec<EffectDef>) -> Engine {
        let (p, errs) = Plan::build(rig(src), fx);
        assert!(errs.is_empty(), "{errs:?}");
        Engine::new(Arc::new(p))
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
        let u = e.render(&s, 1).universes[0];
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
    fn render_does_not_allocate() {
        let fx = EffectDef::parse("sp", &toml::from_str("kind = \"sparkle\"\nrate = 5").unwrap()).unwrap();
        let fx2 = EffectDef::parse("rb", &toml::from_str("kind = \"rainbow\"\nunit = \"beats\"").unwrap()).unwrap();
        let mut e = engine(RGB3, vec![fx, fx2]);
        let s = snapshot(
            &[
                ("lights.par1.intensity", Value::Float(1.0)),
                ("lights.group.front.color", Value::from([0.2f32, 0.3, 0.4, 1.0])),
                ("lights.effect.sp.active", Value::Bool(true)),
                ("lights.effect.rb.active", Value::Bool(true)),
            ],
            &[("beat.phase", 0.3)],
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
