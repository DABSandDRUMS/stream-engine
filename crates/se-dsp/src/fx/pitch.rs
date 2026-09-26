//! Pitch: `pitch` — dual-window granular pitch shifter (two read taps sweeping a delay line,
//! sin²-windowed half a window apart, so the windows always sum to one).

use super::defaults;
use crate::util::DelayLine;
use crate::{Ctx, Effect, ParamSpec, Smoother};

const MAX_WINDOW_MS: f32 = 200.0;
/// Minimum tap delay (keeps the Hermite read inside written history).
const DMIN: usize = 2;
const PITCH_MS: f32 = 30.0;
const CLEAN_MS: f32 = 20.0;

pub const PITCH_PARAMS: &[ParamSpec] = &[
    ParamSpec::float("semitones", -24.0, 24.0, 0.0, "st", "pitch shift (exactly 0 = clean delay by the reported latency)"),
    ParamSpec::float("window", 10.0, 200.0, 60.0, "ms", "grain window: longer is smoother on sustained tones; latency = window / 2"),
];

/// Granular pitch shifter. Latency is half a window (the taps' average delay); at 0 semitones
/// it crossfades to a clean tap at exactly that delay.
pub struct PitchShift {
    sr: f32,
    semis: Smoother,
    /// 1 = clean delayed input, 0 = granular.
    clean: Smoother,
    win: usize,
    dl: [DelayLine; 2],
    phase: f64,
}

impl PitchShift {
    pub fn new(sr: f32) -> PitchShift {
        let p: [f32; 2] = defaults(PITCH_PARAMS);
        let max = (MAX_WINDOW_MS * 0.001 * sr) as usize + DMIN + 8;
        PitchShift {
            sr,
            semis: Smoother::with_ms(p[0], PITCH_MS, sr),
            clean: Smoother::with_ms(if p[0] == 0.0 { 1.0 } else { 0.0 }, CLEAN_MS, sr),
            win: Self::win_samples(p[1], sr),
            dl: [DelayLine::new(max), DelayLine::new(max)],
            phase: 0.5,
        }
    }

    /// Even window length in samples.
    fn win_samples(ms: f32, sr: f32) -> usize {
        (((ms.clamp(10.0, MAX_WINDOW_MS) * 0.001 * sr) as usize) / 2 * 2).max(4)
    }
}

impl Effect for PitchShift {
    fn kind(&self) -> &'static str {
        "pitch"
    }

    fn params(&self) -> &'static [ParamSpec] {
        PITCH_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => {
                let v = v.clamp(-24.0, 24.0);
                self.semis.set(v);
                self.clean.set(if v == 0.0 { 1.0 } else { 0.0 });
            }
            1 => self.win = Self::win_samples(v, self.sr),
            _ => {}
        }
    }

    fn process(&mut self, _ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let w = self.win as f32;
        let lat = self.latency();
        for i in 0..l.len().min(r.len()) {
            self.dl[0].push(l[i]);
            self.dl[1].push(r[i]);
            let (cl, cr) = (self.dl[0].tap(lat), self.dl[1].tap(lat));
            let c = self.clean.next();
            let s = self.semis.next();
            if c >= 1.0 {
                // parked: tap 1 sits exactly on the clean delay with full weight
                self.phase = 0.5;
                l[i] = cl;
                r[i] = cr;
                continue;
            }
            let ratio = (s / 12.0).exp2() as f64;
            self.phase = (self.phase + (1.0 - ratio) / w as f64).rem_euclid(1.0);
            let p2 = (self.phase + 0.5).rem_euclid(1.0);
            let (d1, d2) = (DMIN as f32 + self.phase as f32 * w, DMIN as f32 + p2 as f32 * w);
            let w1 = {
                let s = (std::f64::consts::PI * self.phase).sin() as f32;
                s * s
            };
            let w2 = 1.0 - w1;
            let gl = w1 * self.dl[0].read(d1) + w2 * self.dl[0].read(d2);
            let gr = w1 * self.dl[1].read(d1) + w2 * self.dl[1].read(d2);
            l[i] = gl + (cl - gl) * c;
            r[i] = gr + (cr - gr) * c;
        }
    }

    fn latency(&self) -> usize {
        DMIN + self.win / 2
    }

    fn tail(&self) -> usize {
        DMIN + self.win
    }

    fn reset(&mut self) {
        self.dl[0].clear();
        self.dl[1].clear();
        self.semis.reset(self.semis.target());
        self.clean.reset(self.clean.target());
        self.phase = 0.5;
    }
}
