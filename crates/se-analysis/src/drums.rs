//! Per-drum triggers on close mics (PLAN §8.6, M12). Runs on the real-time audio thread: no
//! allocation after [`DrumTriggers::new`], O(pads) work per sample (a decision additionally scans
//! the pads' short amplitude rings once per detected hit).
//!
//! Per pad: 2nd-order Butterworth high-pass + low-pass (the pad's band), a two-pole energy
//! envelope (τ ≈ one period of the band's low edge, 1–5 ms). A hit starts when the envelope
//! amplitude reaches `threshold_db` **and** the energy has risen ≥ 6 dB over the last 2τ (so a
//! decaying tail above the threshold never retriggers, while a new stroke on top of it does),
//! outside the retrigger window of the previous onset. The decision is made `scan_ms` later:
//! velocity from the band's sample peak inside the scan window (dBFS mapped over
//! `velocity_range_db`, then `^velocity_curve`), and the hit is dropped as bleed when another
//! pad's band peak over the same window exceeds ours by `bleed_ratio_db`.

use crate::filters::{Biquad, butter_band_skirts, flush, one_pole};

#[derive(Clone, Debug, PartialEq)]
pub struct DrumPadConfig {
    pub name: String,
    /// Band-pass edges in Hz.
    pub band: [f32; 2],
    /// Trigger threshold on the band amplitude, dBFS.
    pub threshold_db: f32,
    /// Minimum spacing of two onsets (flams closer than this are one hit). At least `scan_ms`.
    pub retrigger_ms: f32,
    /// Window after the trigger over which the velocity peak is measured (= decision latency).
    pub scan_ms: f32,
    /// Reject the hit when another pad's band peak within the scan exceeds ours by this (dB).
    pub bleed_ratio_db: f32,
    /// Velocity exponent (1 = linear in dB, < 1 boosts soft hits).
    pub velocity_curve: f32,
    /// Band peak (dBFS) mapped to velocity 0 and 1.
    pub velocity_range_db: [f32; 2],
}

impl DrumPadConfig {
    /// Typical settings for a close mic on a drum with energy in `band`.
    pub fn new(name: impl Into<String>, band: [f32; 2]) -> Self {
        DrumPadConfig {
            name: name.into(),
            band,
            threshold_db: -40.0,
            retrigger_ms: 30.0,
            scan_ms: 3.0,
            bleed_ratio_db: 12.0,
            velocity_curve: 1.0,
            velocity_range_db: [-40.0, 0.0],
        }
    }

    pub fn kick() -> Self {
        DrumPadConfig::new("kick", [40.0, 150.0])
    }

    pub fn snare() -> Self {
        DrumPadConfig::new("snare", [150.0, 8000.0])
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct DrumHit {
    pub pad: usize,
    /// 0–1.
    pub velocity: f32,
    /// Absolute sample index of the onset.
    pub frame: u64,
}

#[derive(Clone, Copy, Debug)]
struct PendingHit {
    onset: u64,
    decide_at: u64,
}

#[derive(Clone, Debug)]
struct Pad {
    cfg: DrumPadConfig,
    filt: [Biquad; 2],
    a_env: f64,
    e1: f64,
    e2: f64,
    thr_e: f64,
    delta: u64,
    lag: u64,
    retrigger: u64,
    scan: u64,
    bleed_ratio: f32,
    env: Vec<f32>,
    amp: Vec<f32>,
    last_onset: Option<u64>,
    pending: Option<PendingHit>,
}

/// Energy rise (×4 = 6 dB over 2τ) that marks a new stroke.
const RISE: f64 = 4.0;
/// Bleed look-back before our onset (the other drum's transient may reach its mic first).
const PRE_MS: f64 = 1.5;

/// Multi-pad drum trigger (see module docs).
pub struct DrumTriggers {
    sr: f64,
    pads: Vec<Pad>,
    ring: usize,
    pre: u64,
}

impl DrumTriggers {
    pub fn new(sample_rate: f32, pads: Vec<DrumPadConfig>) -> Self {
        let sr = sample_rate.max(1000.0) as f64;
        let pre = (PRE_MS * 1e-3 * sr).round() as u64;
        let mut built: Vec<Pad> = pads
            .into_iter()
            .map(|cfg| {
                let lo = cfg.band[0].max(10.0) as f64;
                let hi = (cfg.band[1] as f64).max(lo + 1.0);
                let sections = butter_band_skirts(lo, hi, sr, 2);
                let mut filt = [Biquad::IDENTITY; 2];
                for (f, s) in filt.iter_mut().zip(sections) {
                    *f = s;
                }
                let tau = (1.0 / (2.0 * std::f64::consts::PI * lo)).clamp(0.001, 0.005);
                let delta = (2.0 * tau * sr).round().max(1.0) as u64;
                let scan = (cfg.scan_ms.max(0.0) as f64 * 1e-3 * sr).round() as u64;
                let retrigger = ((cfg.retrigger_ms.max(0.0) as f64 * 1e-3 * sr).round() as u64).max(scan + 1);
                let mut p = Pad {
                    thr_e: 0.0,
                    a_env: one_pole(tau, sr),
                    e1: 0.0,
                    e2: 0.0,
                    delta,
                    lag: (0.53 * tau * sr).round() as u64,
                    retrigger,
                    scan,
                    bleed_ratio: 10f32.powf(cfg.bleed_ratio_db / 20.0),
                    filt,
                    env: Vec::new(),
                    amp: Vec::new(),
                    last_onset: None,
                    pending: None,
                    cfg,
                };
                p.set_threshold(p.cfg.threshold_db);
                p
            })
            .collect();
        let ring = built.iter().map(|p| (p.scan + p.delta + pre) as usize + 2).max().unwrap_or(2).max(2);
        for p in &mut built {
            p.env = vec![0.0; ring];
            p.amp = vec![0.0; ring];
        }
        DrumTriggers { sr, pads: built, ring, pre }
    }

    pub fn sample_rate(&self) -> f32 {
        self.sr as f32
    }

    pub fn pads(&self) -> impl Iterator<Item = &DrumPadConfig> {
        self.pads.iter().map(|p| &p.cfg)
    }

    /// Real-time safe.
    pub fn set_threshold_db(&mut self, pad: usize, db: f32) {
        if let Some(p) = self.pads.get_mut(pad) {
            p.set_threshold(db);
        }
    }

    /// `inputs[i]` = block of pad `i` (missing pads read silence; the block length is the shortest
    /// input); `frame` = absolute sample index of `inputs[..][0]`. Hits are reported
    /// `scan_ms` after their trigger with `frame` = located onset. Real-time safe.
    pub fn process(&mut self, inputs: &[&[f32]], frame: u64, on_hit: &mut dyn FnMut(DrumHit)) {
        let len = inputs.iter().map(|b| b.len()).min().unwrap_or(0);
        let ring = self.ring as u64;
        for i in 0..len {
            let t = frame + i as u64;
            let slot = (t % ring) as usize;
            for (pi, pad) in self.pads.iter_mut().enumerate() {
                let x = inputs.get(pi).map_or(0.0, |b| b[i]);
                let x = if x.is_finite() { x as f64 } else { 0.0 };
                let y = pad.filt[0].process(x);
                let y = pad.filt[1].process(y);
                pad.e1 += pad.a_env * (y * y - pad.e1);
                pad.e2 += pad.a_env * (pad.e1 - pad.e2);
                pad.env[slot] = pad.e2 as f32;
                pad.amp[slot] = y.abs() as f32;
                if pad.pending.is_some() || pad.last_onset.is_some_and(|lo| t < lo + pad.retrigger) || pad.e2 < pad.thr_e {
                    continue;
                }
                let back = t.saturating_sub(pad.delta);
                let base = if t >= pad.delta { pad.env[(back % ring) as usize] as f64 } else { 0.0 };
                if pad.e2 < RISE * base {
                    continue;
                }
                // Onset: first sample of the rise window above 10 % of the rise, minus the
                // envelope's own lag to that level.
                let target = base + 0.1 * (pad.e2 - base);
                let mut on = t;
                for s in back + 1..=t {
                    if pad.env[(s % ring) as usize] as f64 >= target {
                        on = s;
                        break;
                    }
                }
                let on = on.saturating_sub(pad.lag).max(back);
                if pad.last_onset.is_some_and(|lo| on < lo + pad.retrigger) {
                    continue;
                }
                pad.last_onset = Some(on);
                pad.pending = Some(PendingHit { onset: on, decide_at: t + pad.scan });
            }
            for pi in 0..self.pads.len() {
                let Some(p) = self.pads[pi].pending else { continue };
                if p.decide_at != t {
                    continue;
                }
                self.pads[pi].pending = None;
                if let Some(hit) = self.decide(pi, p, t) {
                    on_hit(hit);
                }
            }
        }
        for p in &mut self.pads {
            p.filt[0].flush();
            p.filt[1].flush();
            flush(&mut p.e1);
            flush(&mut p.e2);
        }
    }

    fn window_peak(&self, pad: usize, from: u64, to: u64) -> f32 {
        let ring = self.ring as u64;
        let from = from.max(to.saturating_sub(ring - 1));
        (from..=to).map(|s| self.pads[pad].amp[(s % ring) as usize]).fold(0.0, f32::max)
    }

    fn decide(&self, pi: usize, p: PendingHit, t: u64) -> Option<DrumHit> {
        let pad = &self.pads[pi];
        let own = self.window_peak(pi, p.onset, t);
        if own <= 0.0 {
            return None;
        }
        let from = p.onset.saturating_sub(self.pre);
        for q in 0..self.pads.len() {
            if q != pi && self.window_peak(q, from, t) >= own * pad.bleed_ratio {
                return None;
            }
        }
        let [lo, hi] = pad.cfg.velocity_range_db;
        let db = 20.0 * own.log10();
        let lin = if hi > lo { ((db - lo) / (hi - lo)).clamp(0.0, 1.0) } else { 1.0 };
        let velocity = lin.powf(pad.cfg.velocity_curve.max(0.01));
        Some(DrumHit { pad: pi, velocity, frame: p.onset })
    }
}

impl Pad {
    fn set_threshold(&mut self, db: f32) {
        self.cfg.threshold_db = db;
        let a = 10f64.powf(db as f64 / 20.0);
        // Envelope of a sine with peak `a` is a²/2.
        self.thr_e = a * a / 2.0;
    }
}
