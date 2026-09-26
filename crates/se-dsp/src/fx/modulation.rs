//! Time/modulation: `chorus`, `flanger`, `phaser`. Each outputs only the modulated signal;
//! the slot's default mix adds the dry signal back.

use super::{Lfo, defaults, sin_turns};
use crate::util::{DIVISIONS, DelayLine, coef_ms, division_beats, flush};
use crate::{Ctx, Effect, ParamSpec, Smoother};

const PARAM_MS: f32 = 20.0;
/// Smoothing of modulated delay times (turns LFO phase jumps, e.g. toggling `sync`, into glides).
const TAP_MS: f32 = 3.0;
const CHOICE_MS: f32 = 10.0;

/// LFO shared by the modulation effects: free-running at `rate` or locked to `division`, with
/// the right channel offset by `spread` × 180°.
struct Modulator {
    rate: f32,
    sync: bool,
    division: usize,
    spread: Smoother,
    lfo: Lfo,
}

impl Modulator {
    fn new(rate: f32, sync: f32, division: f32, spread: f32, sr: f32) -> Modulator {
        Modulator { rate, sync: sync >= 0.5, division: division as usize, spread: Smoother::with_ms(spread, PARAM_MS, sr), lfo: Lfo::default() }
    }

    /// Handles the common parameters (`rate`, `sync`, `division`, and `spread` at `spread_idx`).
    fn set(&mut self, idx: usize, v: f32, spread_idx: usize) -> bool {
        match idx {
            0 => self.rate = v.max(0.0),
            1 => self.sync = v >= 0.5,
            2 => self.division = (v as usize).min(DIVISIONS.len() - 1),
            i if i == spread_idx => self.spread.set(v.clamp(0.0, 1.0)),
            _ => return false,
        }
        true
    }

    /// LFO phases (left, right) in turns for frame `i` of the block.
    #[inline]
    fn tick(&mut self, ctx: &Ctx, spb: f64, i: usize, sr: f32) -> (f64, f64) {
        let sync = if self.sync { Some((ctx.transport.beat + i as f64 / spb, division_beats(self.division))) } else { None };
        let p = self.lfo.tick(self.rate, sr, sync);
        (p, p + 0.5 * self.spread.next() as f64)
    }

    fn reset(&mut self) {
        self.lfo.reset();
        self.spread.reset(self.spread.target());
    }
}

const fn rate_params(rate: f32) -> [ParamSpec; 3] {
    [
        ParamSpec::float("rate", 0.01, 10.0, rate, "Hz", "LFO rate (free-running)"),
        ParamSpec::boolean("sync", false, "LFO period from `division` (locked to the beat) instead of `rate`"),
        ParamSpec::choice("division", DIVISIONS, 0, "LFO period when synced"),
    ]
}

const SPREAD: ParamSpec = ParamSpec::float("spread", 0.0, 1.0, 0.5, "", "right-channel LFO phase offset (0 = mono, 1 = 180°)");

// ---------------------------------------------------------------------------------------------

/// Chorus modulation depth at `depth = 1` (ms, ± around the base delay).
const CHORUS_MOD_MS: f32 = 4.0;
const CHORUS_MAX_MS: f32 = 40.0;

pub const CHORUS_PARAMS: &[ParamSpec] = &{
    let r = rate_params(0.8);
    [
        r[0],
        r[1],
        r[2],
        ParamSpec::float("depth", 0.0, 1.0, 0.5, "", "delay modulation (±4 ms at 1)"),
        ParamSpec::float("delay", 5.0, 40.0, 15.0, "ms", "base delay of the voices"),
        SPREAD,
    ]
};

/// Two-voice-per-channel stereo chorus.
pub struct Chorus {
    sr: f32,
    m: Modulator,
    depth: Smoother,
    base: Smoother,
    dl: [DelayLine; 2],
    taps: [[f32; 2]; 2],
    tap_c: f32,
    snap: bool,
}

impl Chorus {
    pub fn new(sr: f32) -> Chorus {
        let p: [f32; 6] = defaults(CHORUS_PARAMS);
        let max = (((CHORUS_MAX_MS + CHORUS_MOD_MS) * 0.001 * sr) as usize) + 8;
        Chorus {
            sr,
            m: Modulator::new(p[0], p[1], p[2], p[5], sr),
            depth: Smoother::with_ms(p[3], PARAM_MS, sr),
            base: Smoother::with_ms(p[4] * 0.001 * sr, PARAM_MS, sr),
            dl: [DelayLine::new(max), DelayLine::new(max)],
            taps: [[0.0; 2]; 2],
            tap_c: coef_ms(TAP_MS, sr),
            snap: true,
        }
    }
}

impl Effect for Chorus {
    fn kind(&self) -> &'static str {
        "chorus"
    }

    fn params(&self) -> &'static [ParamSpec] {
        CHORUS_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        if self.m.set(idx, v, 5) {
            return;
        }
        match idx {
            3 => self.depth.set(v.clamp(0.0, 1.0)),
            4 => self.base.set(v.clamp(1.0, CHORUS_MAX_MS) * 0.001 * self.sr),
            _ => {}
        }
    }

    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let spb = ctx.transport.samples_per_beat(self.sr);
        let mod_s = CHORUS_MOD_MS * 0.001 * self.sr;
        for i in 0..l.len().min(r.len()) {
            let (pl, pr) = self.m.tick(ctx, spb, i, self.sr);
            let (depth, base) = (self.depth.next() * mod_s, self.base.next());
            self.dl[0].push(l[i]);
            self.dl[1].push(r[i]);
            let mut out = [0.0f32; 2];
            for (c, p) in [pl, pr].into_iter().enumerate() {
                for v in 0..2 {
                    let target = base + depth * sin_turns(p + 0.5 * v as f64);
                    let t = &mut self.taps[c][v];
                    *t = if self.snap { target } else { *t + self.tap_c * (target - *t) };
                    out[c] += 0.5 * self.dl[c].read(*t);
                }
            }
            self.snap = false;
            l[i] = out[0];
            r[i] = out[1];
        }
    }

    fn tail(&self) -> usize {
        (self.base.target() + CHORUS_MOD_MS * 0.001 * self.sr) as usize
    }

    fn reset(&mut self) {
        self.dl[0].clear();
        self.dl[1].clear();
        self.m.reset();
        self.depth.reset(self.depth.target());
        self.base.reset(self.base.target());
        self.snap = true;
    }
}

// ---------------------------------------------------------------------------------------------

const FLANGER_MAX_MS: f32 = 10.0;
/// Sweep range at `depth = 1`: ±2 octaves of delay around the center.
const FLANGER_OCT: f32 = 2.0;

pub const FLANGER_PARAMS: &[ParamSpec] = &{
    let r = rate_params(0.25);
    [
        r[0],
        r[1],
        r[2],
        ParamSpec::float("depth", 0.0, 1.0, 0.7, "", "sweep width (±2 octaves of delay at 1)"),
        ParamSpec::float("delay", 0.1, 10.0, 1.0, "ms", "sweep center delay"),
        ParamSpec::float("feedback", -0.95, 0.95, 0.5, "", "resonance (negative = odd-harmonic comb)"),
        ParamSpec::float("spread", 0.0, 1.0, 0.25, "", "right-channel LFO phase offset (0 = mono, 1 = 180°)"),
    ]
};

/// Stereo flanger (swept short delay with feedback).
pub struct Flanger {
    sr: f32,
    m: Modulator,
    depth: Smoother,
    base: Smoother,
    fb: Smoother,
    dl: [DelayLine; 2],
    taps: [f32; 2],
    tap_c: f32,
    snap: bool,
}

impl Flanger {
    pub fn new(sr: f32) -> Flanger {
        let p: [f32; 7] = defaults(FLANGER_PARAMS);
        let max = ((FLANGER_MAX_MS * 4.0 * 0.001 * sr) as usize) + 8;
        Flanger {
            sr,
            m: Modulator::new(p[0], p[1], p[2], p[6], sr),
            depth: Smoother::with_ms(p[3], PARAM_MS, sr),
            base: Smoother::with_ms(p[4] * 0.001 * sr, PARAM_MS, sr),
            fb: Smoother::with_ms(p[5], PARAM_MS, sr),
            dl: [DelayLine::new(max), DelayLine::new(max)],
            taps: [0.0; 2],
            tap_c: coef_ms(TAP_MS, sr),
            snap: true,
        }
    }
}

impl Effect for Flanger {
    fn kind(&self) -> &'static str {
        "flanger"
    }

    fn params(&self) -> &'static [ParamSpec] {
        FLANGER_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        if self.m.set(idx, v, 6) {
            return;
        }
        match idx {
            3 => self.depth.set(v.clamp(0.0, 1.0)),
            4 => self.base.set(v.clamp(0.05, FLANGER_MAX_MS) * 0.001 * self.sr),
            5 => self.fb.set(v.clamp(-0.95, 0.95)),
            _ => {}
        }
    }

    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let spb = ctx.transport.samples_per_beat(self.sr);
        let max = self.dl[0].max_delay() as f32;
        for i in 0..l.len().min(r.len()) {
            let (pl, pr) = self.m.tick(ctx, spb, i, self.sr);
            let (depth, base, fb) = (self.depth.next() * FLANGER_OCT, self.base.next(), self.fb.next());
            for (c, (p, x)) in [(pl, &mut l[i]), (pr, &mut r[i])].into_iter().enumerate() {
                let target = (base * (depth * sin_turns(p)).exp2()).clamp(1.0, max);
                let t = &mut self.taps[c];
                *t = if self.snap { target } else { *t + self.tap_c * (target - *t) };
                // read before pushing this sample: `t − 1` back is `t` samples ago
                let y = self.dl[c].read(*t - 1.0);
                self.dl[c].push(flush(*x + fb * y));
                *x = y;
            }
            self.snap = false;
        }
    }

    fn tail(&self) -> usize {
        let fb = self.fb.target().abs();
        let passes = if fb < 1e-3 { 1.0 } else { 1.0 + (1e-3f32).ln() / fb.ln() };
        (self.base.target() * 4.0 * passes) as usize
    }

    fn reset(&mut self) {
        self.dl[0].clear();
        self.dl[1].clear();
        self.m.reset();
        for s in [&mut self.depth, &mut self.base, &mut self.fb] {
            s.reset(s.target());
        }
        self.snap = true;
    }
}

// ---------------------------------------------------------------------------------------------

const STAGE_NAMES: &[&str] = &["2", "4", "6", "8", "12"];
const STAGES: [usize; 5] = [2, 4, 6, 8, 12];
const MAX_STAGES: usize = 12;
/// Sweep range at `depth = 1`: ±2 octaves around the center.
const PHASER_OCT: f32 = 2.0;

pub const PHASER_PARAMS: &[ParamSpec] = &{
    let r = rate_params(0.3);
    [
        r[0],
        r[1],
        r[2],
        ParamSpec::float("depth", 0.0, 1.0, 0.7, "", "sweep width (±2 octaves at 1)"),
        ParamSpec::float("center", 100.0, 8000.0, 800.0, "Hz", "sweep center frequency"),
        ParamSpec::float("feedback", -0.95, 0.95, 0.5, "", "resonance of the notches"),
        ParamSpec::choice("stages", STAGE_NAMES, 1, "first-order allpass stages (notches = stages / 2)"),
        SPREAD,
    ]
};

/// Stereo allpass-chain phaser.
pub struct Phaser {
    sr: f32,
    m: Modulator,
    depth: Smoother,
    log_center: Smoother,
    fb: Smoother,
    stages: usize,
    prev_stages: usize,
    fade: Smoother,
    z: [[f32; MAX_STAGES]; 2],
    last: [f32; 2],
}

impl Phaser {
    pub fn new(sr: f32) -> Phaser {
        let p: [f32; 8] = defaults(PHASER_PARAMS);
        let st = STAGES[p[6] as usize];
        Phaser {
            sr,
            m: Modulator::new(p[0], p[1], p[2], p[7], sr),
            depth: Smoother::with_ms(p[3], PARAM_MS, sr),
            log_center: Smoother::with_ms(p[4].log2(), PARAM_MS, sr),
            fb: Smoother::with_ms(p[5], PARAM_MS, sr),
            stages: st,
            prev_stages: st,
            fade: Smoother::with_ms(1.0, CHOICE_MS, sr),
            z: [[0.0; MAX_STAGES]; 2],
            last: [0.0; 2],
        }
    }
}

impl Effect for Phaser {
    fn kind(&self) -> &'static str {
        "phaser"
    }

    fn params(&self) -> &'static [ParamSpec] {
        PHASER_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        if self.m.set(idx, v, 7) {
            return;
        }
        match idx {
            3 => self.depth.set(v.clamp(0.0, 1.0)),
            4 => self.log_center.set(v.clamp(20.0, 20000.0).log2()),
            5 => self.fb.set(v.clamp(-0.95, 0.95)),
            6 => {
                let st = STAGES[(v as usize).min(STAGES.len() - 1)];
                if st != self.stages {
                    self.prev_stages = self.stages;
                    self.stages = st;
                    self.fade.reset(0.0);
                    self.fade.set(1.0);
                }
            }
            _ => {}
        }
    }

    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let spb = ctx.transport.samples_per_beat(self.sr);
        let fmax = 0.45 * self.sr;
        for i in 0..l.len().min(r.len()) {
            let (pl, pr) = self.m.tick(ctx, spb, i, self.sr);
            let (depth, center, fb, fade) = (self.depth.next() * PHASER_OCT, self.log_center.next(), self.fb.next(), self.fade.next());
            let n = self.stages.max(self.prev_stages);
            for (c, (p, x)) in [(pl, &mut l[i]), (pr, &mut r[i])].into_iter().enumerate() {
                let f = (center + depth * sin_turns(p)).exp2().clamp(20.0, fmax);
                let t = (std::f32::consts::PI * f / self.sr).tan();
                let a = (t - 1.0) / (t + 1.0);
                let mut v = *x + fb * self.last[c];
                let (mut at_prev, mut at_cur) = (0.0, 0.0);
                for k in 0..n {
                    let y = a * v + self.z[c][k];
                    self.z[c][k] = flush(v - a * y);
                    v = y;
                    if k + 1 == self.prev_stages {
                        at_prev = v;
                    }
                    if k + 1 == self.stages {
                        at_cur = v;
                    }
                }
                let y = at_prev + (at_cur - at_prev) * fade;
                self.last[c] = flush(y);
                *x = y;
            }
            if self.prev_stages != self.stages && self.fade.settled() {
                self.prev_stages = self.stages;
            }
        }
    }

    fn reset(&mut self) {
        self.z = [[0.0; MAX_STAGES]; 2];
        self.last = [0.0; 2];
        self.m.reset();
        for s in [&mut self.depth, &mut self.log_center, &mut self.fb] {
            s.reset(s.target());
        }
        self.fade.reset(1.0);
    }
}
