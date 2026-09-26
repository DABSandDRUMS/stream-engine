//! Drive: `saturation`, `distortion` (2× oversampled waveshapers), `bitcrush`.

use super::defaults;
use crate::util::{Biquad, BiquadCoefs, OS_LATENCY, OnePole, Oversampler2x, fast_tanh};
use crate::{Ctx, Effect, ParamSpec, Smoother, db_to_gain};

const PARAM_MS: f32 = 20.0;
const TYPE_MS: f32 = 10.0;
/// Tilt-EQ pivot.
const TILT_HZ: f32 = 1000.0;
/// DC blocker corner for the asymmetric shapes.
const DC_HZ: f32 = 8.0;

/// Waveshaper selected by index; `asym` shapes produce DC that is blocked after decimation.
struct ShapeSet {
    shape: fn(usize, f32) -> f32,
    asym: fn(usize) -> bool,
    count: usize,
}

/// Oversampled waveshaper chain shared by `saturation` and `distortion`:
/// drive → 2× up → shape (type crossfade) → 2× down → DC block (asymmetric types) → tilt → output.
struct Shaper {
    set: ShapeSet,
    os: [Oversampler2x; 2],
    drive: Smoother,
    out: Smoother,
    tone: Smoother,
    asym: Smoother,
    fade: Smoother,
    ty: usize,
    prev: usize,
    tilt: [OnePole; 2],
    dc: [OnePole; 2],
}

impl Shaper {
    fn new(set: ShapeSet, drive_db: f32, ty: usize, tone: f32, out_db: f32, sr: f32) -> Shaper {
        let asym = if (set.asym)(ty) { 1.0 } else { 0.0 };
        Shaper {
            set,
            os: [Oversampler2x::new(), Oversampler2x::new()],
            drive: Smoother::with_ms(db_to_gain(drive_db), PARAM_MS, sr),
            out: Smoother::with_ms(db_to_gain(out_db), PARAM_MS, sr),
            tone: Smoother::with_ms(tone, PARAM_MS, sr),
            asym: Smoother::with_ms(asym, TYPE_MS, sr),
            fade: Smoother::with_ms(1.0, TYPE_MS, sr),
            ty,
            prev: ty,
            tilt: [OnePole::new(TILT_HZ, sr), OnePole::new(TILT_HZ, sr)],
            dc: [OnePole::new(DC_HZ, sr), OnePole::new(DC_HZ, sr)],
        }
    }

    fn set_type(&mut self, v: f32) {
        let t = (v as usize).min(self.set.count - 1);
        if t != self.ty {
            self.prev = self.ty;
            self.ty = t;
            self.fade.reset(0.0);
            self.fade.set(1.0);
            self.asym.set(if (self.set.asym)(t) { 1.0 } else { 0.0 });
        }
    }

    #[inline]
    fn shape2(&self, f: f32, x: f32) -> f32 {
        let y = (self.set.shape)(self.ty, x);
        if f >= 1.0 {
            y
        } else {
            let p = (self.set.shape)(self.prev, x);
            p + (y - p) * f
        }
    }

    fn process(&mut self, l: &mut [f32], r: &mut [f32]) {
        for i in 0..l.len().min(r.len()) {
            let d = self.drive.next();
            let f = self.fade.next();
            let asym = self.asym.next();
            let tone = self.tone.next();
            let g = self.out.next();
            let (gl, gh) = if tone == 0.0 { (1.0, 1.0) } else { (db_to_gain(-6.0 * tone), db_to_gain(6.0 * tone)) };
            for (c, x) in [&mut l[i], &mut r[i]].into_iter().enumerate() {
                let (a, b) = self.os[c].up(*x * d);
                let (a, b) = (self.shape2(f, a), self.shape2(f, b));
                let mut y = self.os[c].down(a, b);
                let dc = self.dc[c].lp(y);
                if asym != 0.0 {
                    y -= dc * asym;
                }
                let lo = self.tilt[c].lp(y);
                if tone != 0.0 {
                    y = lo * gl + (y - lo) * gh;
                }
                *x = y * g;
            }
        }
    }

    fn reset(&mut self) {
        for o in &mut self.os {
            o.reset();
        }
        for p in self.tilt.iter_mut().chain(self.dc.iter_mut()) {
            p.reset();
        }
        for s in [&mut self.drive, &mut self.out, &mut self.tone, &mut self.asym] {
            s.reset(s.target());
        }
        self.fade.reset(1.0);
    }
}

// ---------------------------------------------------------------------------------------------

const SAT_TYPES: &[&str] = &["tanh", "tape", "tube"];

pub const SAT_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("drive", 0.0, 36.0, 6.0, "dB", "input gain into the shaper"),
    ParamSpec::choice("type", SAT_TYPES, 0, "tanh: symmetric soft clip; tape: softer exponential knee; tube: asymmetric (even harmonics)"),
    ParamSpec::float("tone", -1.0, 1.0, 0.0, "", "tilt EQ around 1 kHz after the shaper: −1 dark … +1 bright (±6 dB)"),
    ParamSpec::float("output", -24.0, 12.0, 0.0, "dB", "output gain"),
];

/// tube: shifted tanh, renormalized to unity small-signal gain.
const TUBE_BIAS: f32 = 0.25;

fn sat_shape(ty: usize, x: f32) -> f32 {
    match ty {
        0 => fast_tanh(x),
        1 => x.signum() * (1.0 - (-x.abs()).exp()),
        _ => {
            let t0 = fast_tanh(TUBE_BIAS);
            (fast_tanh(x + TUBE_BIAS) - t0) / (1.0 - t0 * t0)
        }
    }
}

/// Oversampled soft saturation.
pub struct Saturation {
    s: Shaper,
}

impl Saturation {
    pub fn new(sr: f32) -> Saturation {
        let p: [f32; 4] = defaults(SAT_PARAMS);
        let set = ShapeSet { shape: sat_shape, asym: |t| t == 2, count: SAT_TYPES.len() };
        Saturation { s: Shaper::new(set, p[0], p[1] as usize, p[2], p[3], sr) }
    }
}

impl Effect for Saturation {
    fn kind(&self) -> &'static str {
        "saturation"
    }

    fn params(&self) -> &'static [ParamSpec] {
        SAT_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.s.drive.set(db_to_gain(v)),
            1 => self.s.set_type(v),
            2 => self.s.tone.set(v.clamp(-1.0, 1.0)),
            3 => self.s.out.set(db_to_gain(v)),
            _ => {}
        }
    }

    fn process(&mut self, _ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        self.s.process(l, r);
    }

    fn latency(&self) -> usize {
        OS_LATENCY
    }

    fn reset(&mut self) {
        self.s.reset();
    }
}

// ---------------------------------------------------------------------------------------------

const DIST_TYPES: &[&str] = &["hard", "fold", "fuzz"];

pub const DIST_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("drive", 0.0, 48.0, 12.0, "dB", "input gain into the shaper"),
    ParamSpec::choice("type", DIST_TYPES, 0, "hard: hard clip at ±1; fold: sine wavefolder; fuzz: asymmetric clip"),
    ParamSpec::float("tone", -1.0, 1.0, 0.0, "", "tilt EQ around 1 kHz after the shaper: −1 dark … +1 bright (±6 dB)"),
    ParamSpec::float("output", -24.0, 12.0, -6.0, "dB", "output gain"),
];

fn dist_shape(ty: usize, x: f32) -> f32 {
    match ty {
        0 => x.clamp(-1.0, 1.0),
        1 => x.sin(),
        _ => {
            if x > 0.0 {
                0.5 * fast_tanh(2.0 * x)
            } else {
                fast_tanh(x)
            }
        }
    }
}

/// Oversampled hard distortion.
pub struct Distortion {
    s: Shaper,
}

impl Distortion {
    pub fn new(sr: f32) -> Distortion {
        let p: [f32; 4] = defaults(DIST_PARAMS);
        let set = ShapeSet { shape: dist_shape, asym: |t| t == 2, count: DIST_TYPES.len() };
        Distortion { s: Shaper::new(set, p[0], p[1] as usize, p[2], p[3], sr) }
    }
}

impl Effect for Distortion {
    fn kind(&self) -> &'static str {
        "distortion"
    }

    fn params(&self) -> &'static [ParamSpec] {
        DIST_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.s.drive.set(db_to_gain(v)),
            1 => self.s.set_type(v),
            2 => self.s.tone.set(v.clamp(-1.0, 1.0)),
            3 => self.s.out.set(db_to_gain(v)),
            _ => {}
        }
    }

    fn process(&mut self, _ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        self.s.process(l, r);
    }

    fn latency(&self) -> usize {
        OS_LATENCY
    }

    fn reset(&mut self) {
        self.s.reset();
    }
}

// ---------------------------------------------------------------------------------------------

pub const CRUSH_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("bits", 1.0, 24.0, 8.0, "bits", "quantizer resolution (fractional values glide smoothly)"),
    ParamSpec::float("rate", 100.0, 48000.0, 12000.0, "Hz", "sample-and-hold rate (at or above the graph rate = off)"),
    ParamSpec::boolean("antialias", false, "4th-order low-pass at 0.45 × rate before the rate reduction"),
    ParamSpec::float("output", -24.0, 12.0, 0.0, "dB", "output gain"),
];

/// Butterworth 4th order as two biquads.
const BUTTER4_Q: [f32; 2] = [0.541_196_1, 1.306_563];
const CONTROL: usize = 16;

/// Bit depth and sample-rate reduction.
pub struct Bitcrush {
    sr: f32,
    bits: Smoother,
    rate: Smoother,
    aa: Smoother,
    out: Smoother,
    phase: f32,
    held: [f32; 2],
    lp: [[Biquad; 2]; 2],
}

impl Bitcrush {
    pub fn new(sr: f32) -> Bitcrush {
        let p: [f32; 4] = defaults(CRUSH_PARAMS);
        let mut b = Bitcrush {
            sr,
            bits: Smoother::with_ms(p[0], PARAM_MS, sr),
            rate: Smoother::with_ms(p[1], PARAM_MS, sr),
            aa: Smoother::with_ms(p[2], TYPE_MS, sr),
            out: Smoother::with_ms(db_to_gain(p[3]), PARAM_MS, sr),
            phase: 1.0,
            held: [0.0; 2],
            lp: [[Biquad::default(); 2]; 2],
        };
        b.update_aa();
        b
    }

    fn update_aa(&mut self) {
        let fc = (0.45 * self.rate.value()).min(0.45 * self.sr);
        for (s, q) in BUTTER4_Q.iter().enumerate() {
            let c = BiquadCoefs::lowpass(fc, *q, self.sr);
            self.lp[0][s].c = c;
            self.lp[1][s].c = c;
        }
    }
}

impl Effect for Bitcrush {
    fn kind(&self) -> &'static str {
        "bitcrush"
    }

    fn params(&self) -> &'static [ParamSpec] {
        CRUSH_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.bits.set(v.clamp(1.0, 24.0)),
            1 => self.rate.set(v.max(1.0)),
            2 => self.aa.set(if v >= 0.5 { 1.0 } else { 0.0 }),
            3 => self.out.set(db_to_gain(v)),
            _ => {}
        }
    }

    fn process(&mut self, _ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        let mut levels = (self.bits.value() - 1.0).exp2();
        for i in 0..n {
            if i % CONTROL == 0 && !self.rate.settled() {
                self.rate.skip(CONTROL.min(n - i) as u32);
                self.update_aa();
            }
            if !self.bits.settled() {
                levels = (self.bits.next() - 1.0).exp2();
            }
            let aa = self.aa.next();
            let step = self.rate.value() / self.sr;
            let g = self.out.next();
            let mut x = [l[i], r[i]];
            if aa > 0.0 || !self.aa.settled() {
                for (c, v) in x.iter_mut().enumerate() {
                    let s1 = self.lp[c][0].process(*v);
                    let f = self.lp[c][1].process(s1);
                    *v += (f - *v) * aa;
                }
            } else {
                for f in self.lp.iter_mut().flatten() {
                    f.reset();
                }
            }
            if step >= 1.0 {
                self.held = x;
                self.phase = 1.0;
            } else {
                self.phase += step;
                if self.phase >= 1.0 {
                    self.phase -= 1.0;
                    self.held = x;
                }
            }
            l[i] = (self.held[0] * levels).round() / levels * g;
            r[i] = (self.held[1] * levels).round() / levels * g;
        }
    }

    fn reset(&mut self) {
        for s in [&mut self.bits, &mut self.rate, &mut self.aa, &mut self.out] {
            s.reset(s.target());
        }
        self.phase = 1.0;
        self.held = [0.0; 2];
        for f in self.lp.iter_mut().flatten() {
            f.reset();
        }
        self.update_aa();
    }
}
