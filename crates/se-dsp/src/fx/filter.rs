//! Filters and EQ: `svf` (TPT state-variable), `eq` (RBJ parametric), `djfilter`.

use super::defaults;
use crate::util::{Biquad, BiquadCoefs, flush, smoothstep};
use crate::{Ctx, Effect, ParamSpec, Smoother};

/// Cutoff/frequency glide time.
const FREQ_MS: f32 = 30.0;
/// Gain/Q/resonance glide time.
const PARAM_MS: f32 = 20.0;
/// Mode switch crossfade.
const MODE_MS: f32 = 10.0;
/// Coefficients are recomputed every this many samples while a parameter glides.
const CONTROL: usize = 16;

const MODES: &[&str] = &["lp", "hp", "bp", "notch"];

/// Resonance 0–1 → Q = 0.5 · 40^r (0.5 … 20; r ≈ 0.094 is Butterworth).
pub fn resonance_q(r: f32) -> f32 {
    0.5 * 40f32.powf(r.clamp(0.0, 1.0))
}

/// Approximate −60 dB ring time of a 2nd-order resonance at `hz` with quality `q`.
fn ring_samples(hz: f32, q: f32, sr: f32) -> usize {
    (6.91 * 2.0 * q / (std::f32::consts::TAU * hz.max(1.0)) * sr) as usize
}

/// Coefficients of the trapezoidal (TPT/Simper) state-variable filter.
#[derive(Clone, Copy, Debug)]
struct SvfCoefs {
    k: f32,
    a1: f32,
    a2: f32,
    a3: f32,
}

impl SvfCoefs {
    fn new(hz: f32, q: f32, sr: f32) -> SvfCoefs {
        let g = (std::f32::consts::PI * hz.clamp(1.0, 0.49 * sr) / sr).tan();
        let k = 1.0 / q.max(0.05);
        let a1 = 1.0 / (1.0 + g * (g + k));
        SvfCoefs { k, a1, a2: g * a1, a3: g * g * a1 }
    }
}

/// One channel of the state-variable filter. Returns (low, band, high).
#[derive(Clone, Copy, Debug, Default)]
struct SvfState {
    ic1: f32,
    ic2: f32,
}

impl SvfState {
    #[inline]
    fn tick(&mut self, c: &SvfCoefs, x: f32) -> (f32, f32, f32) {
        let v3 = x - self.ic2;
        let v1 = c.a1 * self.ic1 + c.a2 * v3;
        let v2 = self.ic2 + c.a2 * self.ic1 + c.a3 * v3;
        self.ic1 = flush(2.0 * v1 - self.ic1);
        self.ic2 = flush(2.0 * v2 - self.ic2);
        (v2, v1, x - c.k * v1 - v2)
    }
}

/// Response `mode` (index into `MODES`) from one SVF tick. Band-pass is normalized to 0 dB peak.
#[inline]
fn svf_out(mode: usize, x: f32, k: f32, (lo, band, hi): (f32, f32, f32)) -> f32 {
    match mode {
        0 => lo,
        1 => hi,
        2 => k * band,
        _ => x - k * band,
    }
}

pub const SVF_PARAMS: &[ParamSpec] = &[
    ParamSpec::choice("mode", MODES, 0, "response: low-pass, high-pass, band-pass (0 dB peak), notch"),
    ParamSpec::float("cutoff", 20.0, 20000.0, 1000.0, "Hz", "cutoff / center frequency (glides in log frequency)"),
    ParamSpec::float("resonance", 0.0, 1.0, 0.1, "", "0–1 → Q 0.5–20 (log scale; 0.094 = Butterworth)"),
];

/// Sweepable state-variable filter (zero-delay-feedback trapezoidal SVF).
pub struct Svf {
    sr: f32,
    mode: usize,
    prev_mode: usize,
    fade: Smoother,
    log_hz: Smoother,
    res: Smoother,
    c: SvfCoefs,
    st: [SvfState; 2],
}

impl Svf {
    pub fn new(sr: f32) -> Svf {
        let p: [f32; 3] = defaults(SVF_PARAMS);
        let hz = p[1];
        Svf {
            sr,
            mode: p[0] as usize,
            prev_mode: p[0] as usize,
            fade: Smoother::with_ms(1.0, MODE_MS, sr),
            log_hz: Smoother::with_ms(hz.log2(), FREQ_MS, sr),
            res: Smoother::with_ms(p[2], PARAM_MS, sr),
            c: SvfCoefs::new(hz, resonance_q(p[2]), sr),
            st: [SvfState::default(); 2],
        }
    }
}

impl Effect for Svf {
    fn kind(&self) -> &'static str {
        "svf"
    }

    fn params(&self) -> &'static [ParamSpec] {
        SVF_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => {
                let m = (v as usize).min(MODES.len() - 1);
                if m != self.mode {
                    self.prev_mode = self.mode;
                    self.mode = m;
                    self.fade.reset(0.0);
                    self.fade.set(1.0);
                }
            }
            1 => self.log_hz.set(v.clamp(20.0, 20000.0).log2()),
            2 => self.res.set(v.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    fn process(&mut self, _ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        for (xl, xr) in l.iter_mut().zip(r.iter_mut()) {
            if !self.log_hz.settled() || !self.res.settled() {
                let hz = self.log_hz.next().exp2();
                self.c = SvfCoefs::new(hz, resonance_q(self.res.next()), self.sr);
            }
            let c = self.c;
            let (il, ir) = (*xl, *xr);
            let tl = self.st[0].tick(&c, il);
            let tr = self.st[1].tick(&c, ir);
            let (ol, or) = (svf_out(self.mode, il, c.k, tl), svf_out(self.mode, ir, c.k, tr));
            if self.fade.settled() {
                *xl = ol;
                *xr = or;
            } else {
                let f = self.fade.next();
                let (pl, pr) = (svf_out(self.prev_mode, il, c.k, tl), svf_out(self.prev_mode, ir, c.k, tr));
                *xl = pl + (ol - pl) * f;
                *xr = pr + (or - pr) * f;
            }
        }
    }

    fn tail(&self) -> usize {
        ring_samples(self.log_hz.target().exp2(), resonance_q(self.res.target()), self.sr)
    }

    fn reset(&mut self) {
        self.st = [SvfState::default(); 2];
        self.log_hz.reset(self.log_hz.target());
        self.res.reset(self.res.target());
        self.fade.reset(1.0);
        self.c = SvfCoefs::new(self.log_hz.value().exp2(), resonance_q(self.res.value()), self.sr);
    }
}

// ---------------------------------------------------------------------------------------------

pub const EQ_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("low_freq", 20.0, 1000.0, 100.0, "Hz", "low shelf corner"),
    ParamSpec::float("low_gain", -24.0, 24.0, 0.0, "dB", "low shelf gain"),
    ParamSpec::float("low_q", 0.3, 2.0, 0.707, "", "low shelf slope (Q; 0.707 = no overshoot)"),
    ParamSpec::float("mid1_freq", 20.0, 20000.0, 250.0, "Hz", "peaking band 1 center"),
    ParamSpec::float("mid1_gain", -24.0, 24.0, 0.0, "dB", "peaking band 1 gain"),
    ParamSpec::float("mid1_q", 0.1, 18.0, 1.0, "", "peaking band 1 bandwidth (Q)"),
    ParamSpec::float("mid2_freq", 20.0, 20000.0, 1000.0, "Hz", "peaking band 2 center"),
    ParamSpec::float("mid2_gain", -24.0, 24.0, 0.0, "dB", "peaking band 2 gain"),
    ParamSpec::float("mid2_q", 0.1, 18.0, 1.0, "", "peaking band 2 bandwidth (Q)"),
    ParamSpec::float("mid3_freq", 20.0, 20000.0, 4000.0, "Hz", "peaking band 3 center"),
    ParamSpec::float("mid3_gain", -24.0, 24.0, 0.0, "dB", "peaking band 3 gain"),
    ParamSpec::float("mid3_q", 0.1, 18.0, 1.0, "", "peaking band 3 bandwidth (Q)"),
    ParamSpec::float("high_freq", 1000.0, 20000.0, 8000.0, "Hz", "high shelf corner"),
    ParamSpec::float("high_gain", -24.0, 24.0, 0.0, "dB", "high shelf gain"),
    ParamSpec::float("high_q", 0.3, 2.0, 0.707, "", "high shelf slope (Q; 0.707 = no overshoot)"),
    ParamSpec::float("output", -24.0, 24.0, 0.0, "dB", "output gain"),
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum BandKind {
    LowShelf,
    Peak,
    HighShelf,
}

struct Band {
    kind: BandKind,
    log_hz: Smoother,
    gain: Smoother,
    q: Smoother,
    f: [Biquad; 2],
}

impl Band {
    fn new(kind: BandKind, hz: f32, gain: f32, q: f32, sr: f32) -> Band {
        let mut b = Band {
            kind,
            log_hz: Smoother::with_ms(hz.log2(), FREQ_MS, sr),
            gain: Smoother::with_ms(gain, PARAM_MS, sr),
            q: Smoother::with_ms(q, PARAM_MS, sr),
            f: [Biquad::default(); 2],
        };
        b.update(sr);
        b
    }

    fn settled(&self) -> bool {
        self.log_hz.settled() && self.gain.settled() && self.q.settled()
    }

    /// Gain settled at exactly 0 dB: the band is an identity (its state is then exactly zero).
    fn flat(&self) -> bool {
        self.gain.settled() && self.gain.value() == 0.0
    }

    fn update(&mut self, sr: f32) {
        let (hz, g, q) = (self.log_hz.value().exp2(), self.gain.value(), self.q.value());
        let c = match self.kind {
            BandKind::LowShelf => BiquadCoefs::low_shelf(hz, q, g, sr),
            BandKind::Peak => BiquadCoefs::peaking(hz, q, g, sr),
            BandKind::HighShelf => BiquadCoefs::high_shelf(hz, q, g, sr),
        };
        self.f[0].c = c;
        self.f[1].c = c;
    }

    fn snap(&mut self, sr: f32) {
        self.log_hz.reset(self.log_hz.target());
        self.gain.reset(self.gain.target());
        self.q.reset(self.q.target());
        self.update(sr);
        self.f[0].reset();
        self.f[1].reset();
    }
}

/// Parametric EQ: low shelf, three peaking bands, high shelf (RBJ biquads).
pub struct Eq {
    sr: f32,
    bands: [Band; 5],
    out: Smoother,
}

impl Eq {
    pub fn new(sr: f32) -> Eq {
        let p: [f32; 16] = defaults(EQ_PARAMS);
        let kinds = [BandKind::LowShelf, BandKind::Peak, BandKind::Peak, BandKind::Peak, BandKind::HighShelf];
        Eq { sr, bands: std::array::from_fn(|b| Band::new(kinds[b], p[3 * b], p[3 * b + 1], p[3 * b + 2], sr)), out: Smoother::with_ms(1.0, PARAM_MS, sr) }
    }
}

impl Effect for Eq {
    fn kind(&self) -> &'static str {
        "eq"
    }

    fn params(&self) -> &'static [ParamSpec] {
        EQ_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        if idx == 15 {
            self.out.set(crate::db_to_gain(v));
            return;
        }
        let Some(b) = self.bands.get_mut(idx / 3) else { return };
        match idx % 3 {
            0 => b.log_hz.set(v.max(1.0).log2()),
            1 => b.gain.set(v),
            _ => b.q.set(v.max(0.05)),
        }
    }

    fn process(&mut self, _ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        let mut start = 0;
        while start < n {
            let end = (start + CONTROL).min(n);
            let len = (end - start) as u32;
            for b in &mut self.bands {
                if b.flat() {
                    b.f[0].reset();
                    b.f[1].reset();
                    continue;
                }
                if !b.settled() {
                    b.log_hz.skip(len);
                    b.gain.skip(len);
                    b.q.skip(len);
                    b.update(self.sr);
                }
                for i in start..end {
                    l[i] = b.f[0].process(l[i]);
                    r[i] = b.f[1].process(r[i]);
                }
            }
            if !(self.out.settled() && self.out.value() == 1.0) {
                for i in start..end {
                    let g = self.out.next();
                    l[i] *= g;
                    r[i] *= g;
                }
            }
            start = end;
        }
    }

    fn tail(&self) -> usize {
        self.bands.iter().filter(|b| !b.flat()).map(|b| ring_samples(b.log_hz.target().exp2(), b.q.target().max(0.707), self.sr)).max().unwrap_or(0)
    }

    fn reset(&mut self) {
        for b in &mut self.bands {
            b.snap(self.sr);
        }
        self.out.reset(self.out.target());
    }
}

// ---------------------------------------------------------------------------------------------

/// Half-width of the flat zone around the center of the DJ filter knob.
pub const DJ_DEAD_ZONE: f32 = 0.05;
/// Fraction of the travel past the dead zone over which the filter fades in.
const DJ_FADE: f32 = 0.1;

pub const DJ_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("filter", -1.0, 1.0, 0.0, "", "−1…0 low-pass sweep (20 kHz → 40 Hz), 0…1 high-pass sweep (20 Hz → 10 kHz), flat near 0"),
    ParamSpec::float("resonance", 0.0, 1.0, 0.3, "", "0–1 → Q 0.5–20 (log scale)"),
];

/// DJ filter knob position → (cutoff Hz, filter blend 0–1, high-pass?). Inside the dead zone the
/// blend is 0 (output = input); past it the filter fades in over the first 10 % of travel while
/// the cutoff sweeps log-linearly.
pub fn dj_map(x: f32) -> (f32, f32, bool) {
    let x = x.clamp(-1.0, 1.0);
    let t = ((x.abs() - DJ_DEAD_ZONE) / (1.0 - DJ_DEAD_ZONE)).max(0.0);
    let blend = if t <= 0.0 { 0.0 } else { smoothstep(t / DJ_FADE) };
    if x < 0.0 { (20000.0 * (40.0f32 / 20000.0).powf(t), blend, false) } else { (20.0 * (10000.0f32 / 20.0).powf(t), blend, true) }
}

/// One-knob DJ filter (low-pass left of center, high-pass right of center).
pub struct DjFilter {
    sr: f32,
    pos: Smoother,
    res: Smoother,
    c: SvfCoefs,
    blend: f32,
    hp: bool,
    st: [SvfState; 2],
}

impl DjFilter {
    pub fn new(sr: f32) -> DjFilter {
        let p: [f32; 2] = defaults(DJ_PARAMS);
        let mut d = DjFilter {
            sr,
            pos: Smoother::with_ms(p[0], FREQ_MS, sr),
            res: Smoother::with_ms(p[1], PARAM_MS, sr),
            c: SvfCoefs::new(1000.0, 0.7, sr),
            blend: 0.0,
            hp: false,
            st: [SvfState::default(); 2],
        };
        d.update(p[0], p[1]);
        d
    }

    fn update(&mut self, pos: f32, res: f32) {
        let (hz, blend, hp) = dj_map(pos);
        self.c = SvfCoefs::new(hz, resonance_q(res), self.sr);
        self.blend = blend;
        self.hp = hp;
    }
}

impl Effect for DjFilter {
    fn kind(&self) -> &'static str {
        "djfilter"
    }

    fn params(&self) -> &'static [ParamSpec] {
        DJ_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.pos.set(v.clamp(-1.0, 1.0)),
            1 => self.res.set(v.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    fn process(&mut self, _ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        for (xl, xr) in l.iter_mut().zip(r.iter_mut()) {
            if !self.pos.settled() || !self.res.settled() {
                let (p, q) = (self.pos.next(), self.res.next());
                self.update(p, q);
            }
            let c = self.c;
            let (il, ir) = (*xl, *xr);
            let (tl, tr) = (self.st[0].tick(&c, il), self.st[1].tick(&c, ir));
            let mode = if self.hp { 1 } else { 0 };
            let (fl, fr) = (svf_out(mode, il, c.k, tl), svf_out(mode, ir, c.k, tr));
            *xl = il + (fl - il) * self.blend;
            *xr = ir + (fr - ir) * self.blend;
        }
    }

    fn tail(&self) -> usize {
        let (hz, blend, _) = dj_map(self.pos.target());
        if blend == 0.0 { 0 } else { ring_samples(hz, resonance_q(self.res.target()), self.sr) }
    }

    fn reset(&mut self) {
        self.pos.reset(self.pos.target());
        self.res.reset(self.res.target());
        self.st = [SvfState::default(); 2];
        self.update(self.pos.value(), self.res.value());
    }
}
