//! Time: `delay` (tempo-synced, ping-pong, filtered feedback).

use super::defaults;
use crate::util::{DIVISIONS, DelayLine, OnePole, division_samples, flush};
use crate::{Ctx, Effect, ParamSpec, Smoother};

/// Longest echo spacing (synced divisions are clamped to it).
pub const MAX_DELAY_S: f32 = 4.0;
/// Delay-time glide: exponential approach with this time constant …
const GLIDE_MS: f32 = 80.0;
/// … and at most this much delay change per sample (read speed stays within 0.5–1.5×).
const MAX_SLEW: f32 = 0.5;
const PARAM_MS: f32 = 20.0;

pub const DELAY_PARAMS: &[ParamSpec] = &[
    ParamSpec::boolean("sync", true, "echo spacing from `division` at the current tempo (else `time`)"),
    ParamSpec::choice("division", DIVISIONS, 11, "echo spacing when synced"),
    ParamSpec::float("time", 1.0, 2000.0, 375.0, "ms", "echo spacing when not synced"),
    ParamSpec::float("feedback", 0.0, 1.0, 0.4, "", "repeat level (soft-limited above −6 dBFS; 1 = endless)"),
    ParamSpec::boolean("ping_pong", false, "echoes alternate left/right (input summed to mono)"),
    ParamSpec::float("tone", 200.0, 20000.0, 6000.0, "Hz", "low-pass cutoff inside the feedback loop"),
];

/// Unity below 0.5, then a tanh knee towards ±1 (keeps endless feedback bounded).
#[inline]
fn soft_limit(x: f32) -> f32 {
    let a = x.abs();
    if a <= 0.5 { x } else { x.signum() * (0.5 + 0.5 * crate::util::fast_tanh((a - 0.5) * 2.0)) }
}

/// Stereo echo. Outputs the echoes only (the slot mixes the dry signal back in).
pub struct Delay {
    sr: f32,
    sync: bool,
    division: usize,
    time_ms: f32,
    fb: Smoother,
    pp: Smoother,
    tone: Smoother,
    lp: [OnePole; 2],
    dl: [DelayLine; 2],
    cur: f32,
    glide: f32,
    snap: bool,
}

impl Delay {
    pub fn new(sr: f32) -> Delay {
        let p: [f32; 6] = defaults(DELAY_PARAMS);
        let max = (MAX_DELAY_S * sr) as usize + 4;
        Delay {
            sr,
            sync: p[0] >= 0.5,
            division: p[1] as usize,
            time_ms: p[2],
            fb: Smoother::with_ms(p[3], PARAM_MS, sr),
            pp: Smoother::with_ms(p[4], PARAM_MS, sr),
            tone: Smoother::with_ms(p[5], PARAM_MS, sr),
            lp: [OnePole::new(p[5], sr), OnePole::new(p[5], sr)],
            dl: [DelayLine::new(max), DelayLine::new(max)],
            cur: 0.0,
            glide: 1.0 - (-1.0 / (GLIDE_MS * 0.001 * sr)).exp(),
            snap: true,
        }
    }

    fn target(&self, ctx: &Ctx) -> f32 {
        let d = if self.sync { division_samples(self.division, &ctx.transport, self.sr) as f32 } else { self.time_ms * 0.001 * self.sr };
        d.clamp(1.0, MAX_DELAY_S * self.sr)
    }
}

impl Effect for Delay {
    fn kind(&self) -> &'static str {
        "delay"
    }

    fn params(&self) -> &'static [ParamSpec] {
        DELAY_PARAMS
    }

    fn set_param(&mut self, idx: usize, v: f32) {
        match idx {
            0 => self.sync = v >= 0.5,
            1 => self.division = (v as usize).min(DIVISIONS.len() - 1),
            2 => self.time_ms = v.max(0.0),
            3 => self.fb.set(v.clamp(0.0, 1.0)),
            4 => self.pp.set(if v >= 0.5 { 1.0 } else { 0.0 }),
            5 => self.tone.set(v.clamp(20.0, 0.45 * self.sr)),
            _ => {}
        }
    }

    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let target = self.target(ctx);
        if self.snap {
            self.cur = target;
            self.snap = false;
        }
        for i in 0..l.len().min(r.len()) {
            if self.cur != target {
                let step = ((target - self.cur) * self.glide).clamp(-MAX_SLEW, MAX_SLEW);
                self.cur = if (target - self.cur).abs() < 1e-4 { target } else { self.cur + step };
            }
            if !self.tone.settled() {
                let hz = self.tone.next();
                self.lp[0].set_hz(hz, self.sr);
                self.lp[1].set_hz(hz, self.sr);
            }
            // read before this sample is pushed: `cur − 1` back is `cur` samples ago
            let (rl, rr) = (self.dl[0].read(self.cur - 1.0), self.dl[1].read(self.cur - 1.0));
            let fb = self.fb.next();
            let pp = self.pp.next();
            let fl = soft_limit(self.lp[0].lp(rl) * fb);
            let fr = soft_limit(self.lp[1].lp(rr) * fb);
            let (xl, xr) = (l[i], r[i]);
            let (wl, wr) = if pp == 0.0 {
                (xl + fl, xr + fr)
            } else {
                let m = 0.5 * (xl + xr);
                ((1.0 - pp) * (xl + fl) + pp * (m + fr), (1.0 - pp) * (xr + fr) + pp * fl)
            };
            self.dl[0].push(flush(wl));
            self.dl[1].push(flush(wr));
            l[i] = rl;
            r[i] = rr;
        }
    }

    fn tail(&self) -> usize {
        let d = if self.sync { self.cur.max(1.0) } else { self.time_ms * 0.001 * self.sr };
        let f = self.fb.target();
        let repeats = if f < 1e-3 {
            1.0
        } else if f >= 0.999 {
            f32::INFINITY
        } else {
            1.0 + (1e-3f32).ln() / f.ln()
        };
        (d * repeats).min(30.0 * self.sr) as usize
    }

    fn reset(&mut self) {
        self.dl[0].clear();
        self.dl[1].clear();
        self.lp[0].reset();
        self.lp[1].reset();
        for s in [&mut self.fb, &mut self.pp, &mut self.tone] {
            s.reset(s.target());
        }
        self.lp[0].set_hz(self.tone.value(), self.sr);
        self.lp[1].set_hz(self.tone.value(), self.sr);
        self.snap = true;
    }
}
