//! Dynamics: `compressor`, `limiter`, `gate`, `transient`.

use super::defaults;
use crate::util::{DelayLine, EnvFollower, coef_ms, flush};
use crate::{Ctx, Effect, ParamSpec, Smoother, db_to_gain, gain_to_db};

const PARAM_MS: f32 = 20.0;

/// Detector input: the routed key when `sidechain` is on and a key is present, else the
/// stereo-linked peak of the input.
#[inline]
fn detect(sidechain: bool, key: &[f32], i: usize, l: f32, r: f32) -> f32 {
    match key.get(i) {
        Some(k) if sidechain => k.abs(),
        _ => l.abs().max(r.abs()),
    }
}

// ---------------------------------------------------------------------------------------------

pub const COMP_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("threshold", -60.0, 0.0, -18.0, "dB", "level where compression starts"),
    ParamSpec::float("ratio", 1.0, 20.0, 4.0, ":1", "compression ratio above the threshold"),
    ParamSpec::float("attack", 0.1, 100.0, 10.0, "ms", "gain-reduction attack time"),
    ParamSpec::float("release", 10.0, 2000.0, 150.0, "ms", "gain-reduction release time"),
    ParamSpec::float("knee", 0.0, 24.0, 6.0, "dB", "soft-knee width around the threshold"),
    ParamSpec::float("makeup", -12.0, 24.0, 0.0, "dB", "output gain after compression"),
    ParamSpec::boolean("sidechain", false, "detect from the routed key signal instead of the input"),
];

/// Static compression curve (Giannoulis/Massberg/Reiss soft knee): output level for input `x`.
#[inline]
fn comp_curve(x: f32, thr: f32, ratio: f32, knee: f32) -> f32 {
    let over = x - thr;
    if 2.0 * over < -knee {
        x
    } else if knee > 0.0 && 2.0 * over.abs() <= knee {
        let t = over + knee * 0.5;
        x + (1.0 / ratio - 1.0) * t * t / (2.0 * knee)
    } else {
        thr + over / ratio
    }
}

/// Feed-forward stereo-linked compressor with soft knee and optional sidechain key.
pub struct Compressor {
    sr: f32,
    thr: Smoother,
    ratio: Smoother,
    knee: Smoother,
    makeup: Smoother,
    att: f32,
    rel: f32,
    p_rel: f32,
    sidechain: bool,
    /// Current gain reduction (dB, ≥ 0).
    gr: f32,
}

impl Compressor {
    pub fn new(sr: f32) -> Compressor {
        let p: [f32; 7] = defaults(COMP_PARAMS);
        Compressor {
            sr,
            thr: Smoother::with_ms(p[0], PARAM_MS, sr),
            ratio: Smoother::with_ms(p[1], PARAM_MS, sr),
            att: coef_ms(p[2], sr),
            rel: coef_ms(p[3], sr),
            p_rel: p[3],
            knee: Smoother::with_ms(p[4], PARAM_MS, sr),
            makeup: Smoother::with_ms(p[5], PARAM_MS, sr),
            sidechain: p[6] >= 0.5,
            gr: 0.0,
        }
    }
}

impl Effect for Compressor {
    fn kind(&self) -> &'static str {
        "compressor"
    }

    fn params(&self) -> &'static [ParamSpec] {
        COMP_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.thr.set(v),
            1 => self.ratio.set(v.max(1.0)),
            2 => self.att = coef_ms(v, self.sr),
            3 => {
                self.p_rel = v;
                self.rel = coef_ms(v, self.sr);
            }
            4 => self.knee.set(v.max(0.0)),
            5 => self.makeup.set(v),
            6 => self.sidechain = v >= 0.5,
            _ => {}
        }
    }

    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        for i in 0..l.len().min(r.len()) {
            let (thr, ratio, knee, makeup) = (self.thr.next(), self.ratio.next(), self.knee.next(), self.makeup.next());
            let x = gain_to_db(detect(self.sidechain, ctx.key, i, l[i], r[i]));
            let target = x - comp_curve(x, thr, ratio, knee);
            let c = if target > self.gr { self.att } else { self.rel };
            self.gr = flush(self.gr + c * (target - self.gr));
            let g = db_to_gain(makeup - self.gr);
            l[i] *= g;
            r[i] *= g;
        }
    }

    fn tail(&self) -> usize {
        (self.p_rel * 0.001 * self.sr * 5.0) as usize
    }

    fn reset(&mut self) {
        self.gr = 0.0;
        for s in [&mut self.thr, &mut self.ratio, &mut self.knee, &mut self.makeup] {
            s.reset(s.target());
        }
    }
}

// ---------------------------------------------------------------------------------------------

pub const LIMITER_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("ceiling", -24.0, 0.0, -1.0, "dB", "output never exceeds this level"),
    ParamSpec::float("gain", 0.0, 24.0, 0.0, "dB", "drive into the limiter"),
    ParamSpec::float("release", 1.0, 1000.0, 100.0, "ms", "gain recovery time"),
    ParamSpec::float("lookahead", 0.5, 10.0, 3.0, "ms", "lookahead; latency = lookahead + 2 samples (changing it re-primes the detector)"),
];

/// Sliding-window minimum (monotonic deque in a fixed ring; O(1) amortized, allocation-free).
struct MinWindow {
    idx: Vec<u64>,
    val: Vec<f32>,
    mask: usize,
    head: usize,
    len: usize,
}

impl MinWindow {
    fn new(max_window: usize) -> MinWindow {
        let cap = (max_window + 2).next_power_of_two();
        MinWindow { idx: vec![0; cap], val: vec![0.0; cap], mask: cap - 1, head: 0, len: 0 }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    /// Push value `v` at sample `n`; returns the minimum over samples `n − w + 1 ..= n`.
    #[inline]
    fn push(&mut self, n: u64, v: f32, w: u64) -> f32 {
        while self.len > 0 && self.val[(self.head + self.len - 1) & self.mask] >= v {
            self.len -= 1;
        }
        let t = (self.head + self.len) & self.mask;
        self.idx[t] = n;
        self.val[t] = v;
        self.len += 1;
        while self.idx[self.head] + w <= n {
            self.head = (self.head + 1) & self.mask;
            self.len -= 1;
        }
        self.val[self.head]
    }
}

/// Lookahead brickwall limiter. The gain needed by every sample (including an inter-sample
/// peak estimate from the 4-point midpoints on both sides) is min-held over the lookahead and
/// then box-averaged over the same window, so the gain is already down when a peak leaves the
/// delay line: the output never exceeds the ceiling (plus a final safety clamp for rounding).
pub struct Limiter {
    sr: f32,
    ceil: Smoother,
    drive: Smoother,
    rel: f32,
    la: usize,
    dl: [DelayLine; 2],
    prev_mid: f32,
    mins: MinWindow,
    boxbuf: Vec<f32>,
    box_pos: usize,
    sum: f64,
    r: f32,
    n: u64,
}

/// Samples the peak estimate lags the input (needs two newer samples for the midpoints).
const PEAK_LAG: usize = 2;

impl Limiter {
    const MAX_LA_MS: f32 = 10.0;

    pub fn new(sr: f32) -> Limiter {
        let p: [f32; 4] = defaults(LIMITER_PARAMS);
        let max_la = (Self::MAX_LA_MS * 0.001 * sr).ceil() as usize + 1;
        let mut lim = Limiter {
            sr,
            ceil: Smoother::with_ms(db_to_gain(p[0]), PARAM_MS, sr),
            drive: Smoother::with_ms(db_to_gain(p[1]), PARAM_MS, sr),
            rel: coef_ms(p[2], sr),
            la: Self::la_samples(p[3], sr),
            dl: [DelayLine::new(max_la + PEAK_LAG + 4), DelayLine::new(max_la + PEAK_LAG + 4)],
            prev_mid: 0.0,
            mins: MinWindow::new(max_la + 1),
            boxbuf: vec![1.0; max_la + 1],
            box_pos: 0,
            sum: 0.0,
            r: 1.0,
            // far from 0 so re-priming can index samples before the first one
            n: 1 << 32,
        };
        lim.reset();
        lim
    }

    fn la_samples(ms: f32, sr: f32) -> usize {
        ((ms.clamp(0.5, Self::MAX_LA_MS) * 0.001 * sr).round() as usize).max(1)
    }

    /// Absolute peak of the midpoints between taps `d+1`/`d` of both channels (4-point
    /// Lagrange midpoint from taps `d+2 … d−1`).
    #[inline]
    fn mid_peak(&self, d: usize) -> f32 {
        let m = |dl: &DelayLine| ((9.0 * (dl.tap(d) + dl.tap(d + 1)) - dl.tap(d - 1) - dl.tap(d + 2)) * (1.0 / 16.0)).abs();
        m(&self.dl[0]).max(m(&self.dl[1]))
    }

    #[inline]
    fn required(&self, d: usize, prev_mid: f32, next_mid: f32) -> f32 {
        let peak = self.dl[0].tap(d).abs().max(self.dl[1].tap(d).abs()).max(prev_mid).max(next_mid);
        if peak > 1.0 { 1.0 / peak } else { 1.0 }
    }

    /// Rebuild the detector for a new lookahead from the history already in the delay lines.
    fn reprime(&mut self) {
        let w = self.la as u64 + 1;
        self.mins.clear();
        let mut h = 1.0f32;
        for j in (0..=self.la).rev() {
            // sample m − j sits at tap PEAK_LAG + j; its midpoints are at taps +1/−0 of it
            let d = PEAK_LAG + j;
            let req = self.required(d, self.mid_peak(d), self.mid_peak(d - 1));
            h = self.mins.push(self.n.wrapping_sub(j as u64), req, w);
        }
        let r0 = self.r.min(h);
        self.r = r0;
        self.boxbuf.fill(r0);
        self.box_pos = 0;
        self.sum = r0 as f64 * w as f64;
        self.prev_mid = self.mid_peak(PEAK_LAG);
    }
}

impl Effect for Limiter {
    fn kind(&self) -> &'static str {
        "limiter"
    }

    fn params(&self) -> &'static [ParamSpec] {
        LIMITER_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.ceil.set(db_to_gain(v.min(0.0))),
            1 => self.drive.set(db_to_gain(v)),
            2 => self.rel = coef_ms(v, self.sr),
            3 => {
                let la = Self::la_samples(v, self.sr);
                if la != self.la {
                    self.la = la;
                    self.reprime();
                }
            }
            _ => {}
        }
    }

    fn process(&mut self, _ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let w = self.la as u64 + 1;
        let wlen = self.la + 1;
        let inv_w = 1.0 / w as f64;
        for i in 0..l.len().min(r.len()) {
            let c = self.ceil.next();
            let s = self.drive.next() / c;
            self.dl[0].push(l[i] * s);
            self.dl[1].push(r[i] * s);
            self.n += 1;
            let next_mid = self.mid_peak(1);
            let req = self.required(PEAK_LAG, self.prev_mid, next_mid);
            self.prev_mid = next_mid;
            let h = self.mins.push(self.n, req, w);
            self.r = if h < self.r { h } else { self.r + (h - self.r) * self.rel };
            self.sum += (self.r - self.boxbuf[self.box_pos]) as f64;
            self.boxbuf[self.box_pos] = self.r;
            self.box_pos += 1;
            if self.box_pos == wlen {
                self.box_pos = 0;
                // drop accumulated rounding once per window
                self.sum = self.boxbuf[..wlen].iter().map(|&v| v as f64).sum();
            }
            let g = ((self.sum * inv_w) as f32).min(1.0);
            let d = self.la + PEAK_LAG;
            l[i] = (self.dl[0].tap(d) * g * c).clamp(-c, c);
            r[i] = (self.dl[1].tap(d) * g * c).clamp(-c, c);
        }
    }

    fn latency(&self) -> usize {
        self.la + PEAK_LAG
    }

    fn tail(&self) -> usize {
        self.la + PEAK_LAG
    }

    fn reset(&mut self) {
        self.dl[0].clear();
        self.dl[1].clear();
        self.ceil.reset(self.ceil.target());
        self.drive.reset(self.drive.target());
        self.mins.clear();
        self.boxbuf.fill(1.0);
        self.box_pos = 0;
        self.sum = (self.la + 1) as f64;
        self.r = 1.0;
        self.prev_mid = 0.0;
    }
}

// ---------------------------------------------------------------------------------------------

pub const GATE_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("threshold", -80.0, 0.0, -40.0, "dB", "opens above this level (closes 4 dB below it)"),
    ParamSpec::float("attack", 0.05, 50.0, 0.5, "ms", "opening time"),
    ParamSpec::float("hold", 0.0, 500.0, 50.0, "ms", "stays open this long after the level falls"),
    ParamSpec::float("release", 5.0, 2000.0, 150.0, "ms", "closing time"),
    ParamSpec::float("range", -80.0, 0.0, -80.0, "dB", "attenuation while closed"),
    ParamSpec::boolean("sidechain", false, "detect from the routed key signal instead of the input"),
];

/// Hysteresis between opening and closing thresholds.
const GATE_HYST: f32 = 0.630_957_3; // −4 dB

/// Noise gate / ducking gate with hysteresis, hold, and a floor (`range`).
pub struct Gate {
    sr: f32,
    thr: Smoother,
    floor: Smoother,
    att: f32,
    rel: f32,
    hold: u32,
    sidechain: bool,
    det: EnvFollower,
    open: bool,
    hold_left: u32,
    g: f32,
}

impl Gate {
    pub fn new(sr: f32) -> Gate {
        let p: [f32; 6] = defaults(GATE_PARAMS);
        Gate {
            sr,
            thr: Smoother::with_ms(db_to_gain(p[0]), PARAM_MS, sr),
            att: coef_ms(p[1], sr),
            hold: (p[2] * 0.001 * sr) as u32,
            rel: coef_ms(p[3], sr),
            floor: Smoother::with_ms(db_to_gain(p[4]), PARAM_MS, sr),
            sidechain: p[5] >= 0.5,
            det: EnvFollower::new(0.0, 5.0, sr),
            open: false,
            hold_left: 0,
            g: db_to_gain(p[4]),
        }
    }
}

impl Effect for Gate {
    fn kind(&self) -> &'static str {
        "gate"
    }

    fn params(&self) -> &'static [ParamSpec] {
        GATE_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.thr.set(db_to_gain(v)),
            1 => self.att = coef_ms(v, self.sr),
            2 => self.hold = (v.max(0.0) * 0.001 * self.sr) as u32,
            3 => self.rel = coef_ms(v, self.sr),
            4 => self.floor.set(db_to_gain(v.min(0.0))),
            5 => self.sidechain = v >= 0.5,
            _ => {}
        }
    }

    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        for i in 0..l.len().min(r.len()) {
            let thr = self.thr.next();
            let floor = self.floor.next();
            let lvl = self.det.process(detect(self.sidechain, ctx.key, i, l[i], r[i]));
            if lvl >= thr || (self.open && lvl >= thr * GATE_HYST) {
                self.open = true;
                self.hold_left = self.hold;
            } else if self.hold_left > 0 {
                self.hold_left -= 1;
            } else {
                self.open = false;
            }
            let (target, c) = if self.open { (1.0, self.att) } else { (floor, self.rel) };
            self.g = flush(self.g + c * (target - self.g));
            l[i] *= self.g;
            r[i] *= self.g;
        }
    }

    fn tail(&self) -> usize {
        self.hold as usize
    }

    fn reset(&mut self) {
        self.thr.reset(self.thr.target());
        self.floor.reset(self.floor.target());
        self.det.reset();
        self.open = false;
        self.hold_left = 0;
        self.g = self.floor.value();
    }
}

// ---------------------------------------------------------------------------------------------

pub const TRANSIENT_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("attack", -24.0, 24.0, 0.0, "dB", "boost/cut of transients (fast vs slow envelope)"),
    ParamSpec::float("sustain", -24.0, 24.0, 0.0, "dB", "boost/cut of the sustain/decay portion"),
    ParamSpec::float("output", -24.0, 24.0, 0.0, "dB", "output gain"),
];

/// Level-independent transient shaper: the relative difference between a fast and a slow
/// envelope (attack) and between a long and a short release (sustain) scales the gain in dB.
pub struct Transient {
    attack: Smoother,
    sustain: Smoother,
    out: Smoother,
    fast: EnvFollower,
    slow: EnvFollower,
    short: EnvFollower,
    long: EnvFollower,
}

impl Transient {
    pub fn new(sr: f32) -> Transient {
        let p: [f32; 3] = defaults(TRANSIENT_PARAMS);
        Transient {
            attack: Smoother::with_ms(p[0], PARAM_MS, sr),
            sustain: Smoother::with_ms(p[1], PARAM_MS, sr),
            out: Smoother::with_ms(p[2], PARAM_MS, sr),
            fast: EnvFollower::new(1.0, 150.0, sr),
            slow: EnvFollower::new(25.0, 150.0, sr),
            short: EnvFollower::new(1.0, 20.0, sr),
            long: EnvFollower::new(1.0, 250.0, sr),
        }
    }
}

impl Effect for Transient {
    fn kind(&self) -> &'static str {
        "transient"
    }

    fn params(&self) -> &'static [ParamSpec] {
        TRANSIENT_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.attack.set(v),
            1 => self.sustain.set(v),
            2 => self.out.set(v),
            _ => {}
        }
    }

    fn process(&mut self, _ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        for i in 0..l.len().min(r.len()) {
            let x = l[i].abs().max(r[i].abs());
            let (f, s) = (self.fast.process(x), self.slow.process(x));
            let (sh, lo) = (self.short.process(x), self.long.process(x));
            let a = if f > 1e-9 { ((f - s) / f).clamp(0.0, 1.0) } else { 0.0 };
            let su = if lo > 1e-9 { ((lo - sh) / lo).clamp(0.0, 1.0) } else { 0.0 };
            let g = db_to_gain(self.attack.next() * a + self.sustain.next() * su + self.out.next());
            l[i] *= g;
            r[i] *= g;
        }
    }

    fn reset(&mut self) {
        for s in [&mut self.attack, &mut self.sustain, &mut self.out] {
            s.reset(s.target());
        }
        for e in [&mut self.fast, &mut self.slow, &mut self.short, &mut self.long] {
            e.reset();
        }
    }
}
