//! Utility: `gain` (level, balance, stereo width, polarity).

use super::defaults;
use crate::{Ctx, Effect, ParamSpec, Smoother, db_to_gain};

const PARAM_MS: f32 = 20.0;
/// Polarity flips ramp through zero over this time (no click).
const FLIP_MS: f32 = 10.0;

pub const GAIN_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("gain", -60.0, 24.0, 0.0, "dB", "level"),
    ParamSpec::float("pan", -1.0, 1.0, 0.0, "", "stereo balance (−1 left … +1 right; center = unity)"),
    ParamSpec::float("width", 0.0, 2.0, 1.0, "", "stereo width (0 = mono, 1 = unchanged, 2 = wide)"),
    ParamSpec::boolean("invert_l", false, "invert left polarity"),
    ParamSpec::boolean("invert_r", false, "invert right polarity"),
];

/// Gain / balance / width / polarity utility. Bit-exact pass-through at its defaults.
pub struct Gain {
    gain: Smoother,
    pan: Smoother,
    width: Smoother,
    pol: [Smoother; 2],
}

impl Gain {
    pub fn new(sr: f32) -> Gain {
        let p: [f32; 5] = defaults(GAIN_PARAMS);
        let pol = |on: f32| Smoother::with_ms(if on >= 0.5 { -1.0 } else { 1.0 }, FLIP_MS, sr);
        Gain {
            gain: Smoother::with_ms(db_to_gain(p[0]), PARAM_MS, sr),
            pan: Smoother::with_ms(p[1], PARAM_MS, sr),
            width: Smoother::with_ms(p[2], PARAM_MS, sr),
            pol: [pol(p[3]), pol(p[4])],
        }
    }

    fn identity(&self) -> bool {
        let at = |s: &Smoother, v: f32| s.settled() && s.value() == v;
        at(&self.gain, 1.0) && at(&self.pan, 0.0) && at(&self.width, 1.0) && at(&self.pol[0], 1.0) && at(&self.pol[1], 1.0)
    }
}

impl Effect for Gain {
    fn kind(&self) -> &'static str {
        "gain"
    }

    fn params(&self) -> &'static [ParamSpec] {
        GAIN_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.gain.set(db_to_gain(v)),
            1 => self.pan.set(v.clamp(-1.0, 1.0)),
            2 => self.width.set(v.clamp(0.0, 2.0)),
            3 | 4 => self.pol[idx - 3].set(if v >= 0.5 { -1.0 } else { 1.0 }),
            _ => {}
        }
    }

    fn process(&mut self, _ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        if self.identity() {
            return;
        }
        for i in 0..l.len().min(r.len()) {
            let (g, pan, w) = (self.gain.next(), self.pan.next(), self.width.next());
            let (pl, pr) = (self.pol[0].next(), self.pol[1].next());
            let (mut a, mut b) = (l[i], r[i]);
            if w != 1.0 {
                let m = 0.5 * (a + b);
                let s = 0.5 * (a - b) * w;
                a = m + s;
                b = m - s;
            }
            l[i] = a * (1.0 - pan.max(0.0)) * g * pl;
            r[i] = b * (1.0 + pan.min(0.0)) * g * pr;
        }
    }

    fn reset(&mut self) {
        for s in [&mut self.gain, &mut self.pan, &mut self.width].into_iter().chain(self.pol.iter_mut()) {
            s.reset(s.target());
        }
    }
}
