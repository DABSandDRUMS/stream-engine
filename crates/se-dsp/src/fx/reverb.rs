//! Time: `reverb` — 8-line feedback delay network (Hadamard mixing, per-line RT60 gains,
//! damping, gentle modulation), input diffusion, predelay, and stereo width. Wet only.

use super::{defaults, sin_turns};
use crate::util::{DelayLine, OnePole, flush};
use crate::{Ctx, Effect, ParamSpec, Smoother};

const LINES: usize = 8;
/// Line lengths (ms) at scale 1; roughly exponentially spread, mutually incommensurate.
const LINE_MS: [f32; LINES] = [31.3, 37.1, 41.9, 47.3, 53.9, 61.1, 68.7, 77.9];
/// size 0–1 → length scale.
const SCALE_MIN: f32 = 0.25;
const SCALE_MAX: f32 = 1.5;
const MAX_PREDELAY_MS: f32 = 250.0;
/// Modulation depth (ms) and rates (Hz) of the line lengths (smears metallic resonances).
const MOD_MS: f32 = 0.3;
const MOD_HZ: [f32; 2] = [0.53, 0.71];
/// Input diffusion allpasses (ms) per channel and their gain.
const DIFF_MS: [[f32; 2]; 2] = [[4.31, 1.61], [4.73, 1.87]];
const DIFF_G: f32 = 0.6;
const CONTROL: usize = 16;
const OUT_GAIN: f32 = 0.6;

pub const REVERB_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("size", 0.0, 1.0, 0.5, "", "room size (scales the network's delay lengths; glides)"),
    ParamSpec::float("decay", 0.1, 20.0, 2.0, "s", "RT60 decay time"),
    ParamSpec::float("damping", 0.0, 1.0, 0.5, "", "high-frequency damping per pass (0 = bright, 1 = dark)"),
    ParamSpec::float("predelay", 0.0, 250.0, 20.0, "ms", "delay before the reverb starts"),
    ParamSpec::float("width", 0.0, 1.0, 1.0, "", "stereo width of the tail (0 = mono)"),
];

/// Schroeder allpass (fixed length).
struct Allpass {
    dl: DelayLine,
    d: usize,
}

impl Allpass {
    fn new(ms: f32, sr: f32) -> Allpass {
        let d = ((ms * 0.001 * sr) as usize).max(1);
        Allpass { dl: DelayLine::new(d + 1), d }
    }

    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let del = self.dl.tap(self.d - 1);
        let v = flush(x + DIFF_G * del);
        self.dl.push(v);
        del - DIFF_G * v
    }
}

/// In-place normalized 8-point Walsh–Hadamard transform (orthogonal: lossless mixing).
#[inline]
fn hadamard8(y: &mut [f32; LINES]) {
    let mut h = 1;
    while h < LINES {
        let mut i = 0;
        while i < LINES {
            for j in i..i + h {
                let (a, b) = (y[j], y[j + h]);
                y[j] = a + b;
                y[j + h] = a - b;
            }
            i += 2 * h;
        }
        h *= 2;
    }
    let s = 1.0 / (LINES as f32).sqrt();
    for v in y.iter_mut() {
        *v *= s;
    }
}

/// FDN reverb.
pub struct Reverb {
    sr: f32,
    scale: Smoother,
    decay: Smoother,
    damping: Smoother,
    predelay: Smoother,
    width: Smoother,
    pre: [DelayLine; 2],
    diff: [[Allpass; 2]; 2],
    lines: [DelayLine; LINES],
    damp: [OnePole; LINES],
    gains: [f32; LINES],
    lens: [f32; LINES],
    mod_phase: [f64; 2],
}

impl Reverb {
    pub fn new(sr: f32) -> Reverb {
        let p: [f32; 5] = defaults(REVERB_PARAMS);
        let max_line = |ms: f32| ((ms * SCALE_MAX + 2.0 * MOD_MS) * 0.001 * sr) as usize + 4;
        let mut rv = Reverb {
            sr,
            scale: Smoother::with_ms(Self::scale_of(p[0]), 200.0, sr),
            decay: Smoother::with_ms(p[1], 50.0, sr),
            damping: Smoother::with_ms(p[2], 50.0, sr),
            predelay: Smoother::with_ms(p[3] * 0.001 * sr, 100.0, sr),
            width: Smoother::with_ms(p[4], 20.0, sr),
            pre: [DelayLine::new((MAX_PREDELAY_MS * 0.001 * sr) as usize + 4), DelayLine::new((MAX_PREDELAY_MS * 0.001 * sr) as usize + 4)],
            diff: [[Allpass::new(DIFF_MS[0][0], sr), Allpass::new(DIFF_MS[0][1], sr)], [Allpass::new(DIFF_MS[1][0], sr), Allpass::new(DIFF_MS[1][1], sr)]],
            lines: std::array::from_fn(|i| DelayLine::new(max_line(LINE_MS[i]))),
            damp: [OnePole::default(); LINES],
            gains: [0.0; LINES],
            lens: [0.0; LINES],
            mod_phase: [0.0, 0.25],
        };
        rv.update();
        rv
    }

    fn scale_of(size: f32) -> f32 {
        SCALE_MIN + (SCALE_MAX - SCALE_MIN) * size.clamp(0.0, 1.0)
    }

    /// Recompute line lengths, RT60 gains, and damping from the current smoothed values.
    fn update(&mut self) {
        let (scale, rt60) = (self.scale.value(), self.decay.value().max(0.05));
        let hz = 20000.0 * (1000.0f32 / 20000.0).powf(self.damping.value());
        for i in 0..LINES {
            let len = LINE_MS[i] * scale * 0.001 * self.sr;
            self.lens[i] = len;
            self.gains[i] = 10f32.powf(-3.0 * len / (rt60 * self.sr));
            self.damp[i].set_hz(hz.min(0.45 * self.sr), self.sr);
        }
    }

    fn settled(&self) -> bool {
        self.scale.settled() && self.decay.settled() && self.damping.settled()
    }
}

impl Effect for Reverb {
    fn kind(&self) -> &'static str {
        "reverb"
    }

    fn params(&self) -> &'static [ParamSpec] {
        REVERB_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.scale.set(Self::scale_of(v)),
            1 => self.decay.set(v.max(0.05)),
            2 => self.damping.set(v.clamp(0.0, 1.0)),
            3 => self.predelay.set(v.clamp(0.0, MAX_PREDELAY_MS) * 0.001 * self.sr),
            4 => self.width.set(v.clamp(0.0, 1.0)),
            _ => {}
        }
    }

    fn process(&mut self, _ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        let mod_depth = MOD_MS * 0.001 * self.sr;
        let mod_inc = [MOD_HZ[0] as f64 / self.sr as f64, MOD_HZ[1] as f64 / self.sr as f64];
        for i in 0..n {
            if i % CONTROL == 0 && !self.settled() {
                let k = CONTROL.min(n - i) as u32;
                self.scale.skip(k);
                self.decay.skip(k);
                self.damping.skip(k);
                self.update();
            }
            let pd = self.predelay.next();
            self.pre[0].push(l[i]);
            self.pre[1].push(r[i]);
            let dl = self.diff[0][0].process(self.pre[0].read(pd));
            let il = self.diff[0][1].process(dl);
            let dr = self.diff[1][0].process(self.pre[1].read(pd));
            let ir = self.diff[1][1].process(dr);

            for (p, inc) in self.mod_phase.iter_mut().zip(mod_inc) {
                *p += inc;
                if *p >= 1.0 {
                    *p -= 1.0;
                }
            }
            let m = [sin_turns(self.mod_phase[0]) * mod_depth, sin_turns(self.mod_phase[1]) * mod_depth];
            let mut y = [0.0f32; LINES];
            for k in 0..LINES {
                let wobble = match k {
                    0 => m[0],
                    3 => -m[1],
                    5 => m[1],
                    6 => -m[0],
                    _ => 0.0,
                };
                // read before this sample's push: `len − 1` back is `len` samples ago
                let v = self.lines[k].read(self.lens[k] + wobble - 1.0);
                y[k] = self.damp[k].lp(v) * self.gains[k];
            }
            let out_l = (y[0] - y[2] + y[4] - y[6]) * OUT_GAIN;
            let out_r = (y[1] - y[3] + y[5] - y[7]) * OUT_GAIN;
            hadamard8(&mut y);
            for k in 0..LINES {
                let inj = if k % 2 == 0 { il } else { ir };
                let sign = if k & 2 == 0 { 1.0 } else { -1.0 };
                self.lines[k].push(flush(y[k] + sign * inj));
            }
            let w = self.width.next();
            let mid = 0.5 * (out_l + out_r);
            let side = 0.5 * (out_l - out_r) * w;
            l[i] = mid + side;
            r[i] = mid - side;
        }
    }

    fn tail(&self) -> usize {
        (self.decay.target() * self.sr + self.predelay.target()) as usize
    }

    fn reset(&mut self) {
        for d in self.pre.iter_mut().chain(self.lines.iter_mut()) {
            d.clear();
        }
        for a in self.diff.iter_mut().flatten() {
            a.dl.clear();
        }
        for p in &mut self.damp {
            p.reset();
        }
        for s in [&mut self.scale, &mut self.decay, &mut self.damping, &mut self.predelay, &mut self.width] {
            s.reset(s.target());
        }
        self.mod_phase = [0.0, 0.25];
        self.update();
    }
}
