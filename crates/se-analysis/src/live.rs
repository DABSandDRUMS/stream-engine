//! Live analysis of one tapped bus (PLAN §8.3): levels, loudness, bands, centroid, onsets per
//! drum class, beat, drops, sections and hype, one [`Features`] frame per hop.
//!
//! Per sample (all `f64` IIR, structure-of-arrays so the lane loops vectorise):
//! * K-weighting per channel (BS.1770-4) → per-hop sums for momentary (400 ms) and short-term
//!   (3 s) loudness over exact sliding windows of whole hops.
//! * 31 third-octave Butterworth band-passes (6th order, IEC 61260 style) on the mono mix, mean
//!   square with a per-band time constant (≈1.5 periods, 12–75 ms). An FFT cannot resolve the
//!   lowest third-octaves at a latency-compatible length (the 20 Hz band is 4.6 Hz wide), which is
//!   why the bands use a filter bank.
//! * Six onset bands (Scheirer-style filter bank, 30 Hz–16 kHz) whose energy envelopes give the
//!   onset-strength curve (`flux`): half-wave rectified amplitude increase per hop, summed and
//!   log-compressed relative to a slow peak reference (scale invariant).
//! * Three class bands (kick/snare/hat, 24 dB/oct skirts) with fast energy envelopes, kept per
//!   sample for two hops so an onset can be located to the sample.
//!
//! Per hop: a 1024-point Hann FFT for the spectral centroid; onset decisions (log-energy rise
//! over the last one/two hops against `threshold + sensitivity × median(rise)` with a relative
//! noise floor 30 dB under the band's recent peak, level acceptance against the class's recent
//! onset level, retrigger guard) that emit within `max_latency_ms` of the located onset; the
//! beat tracker; drop, section and hype detectors on decimated histories. Every 200 ms a slow
//! feature vector (4096-point FFT chroma, third-octave spectral shape, loudness) feeds a
//! checkerboard novelty (cross-block minus within-block distance of the last 2 s against the 6 s
//! before them).

use crate::beat::{BeatEvent, BeatTracker};
use crate::filters::{Bank, Biquad, butter_band_skirts, butter_bandpass, cascade_group_delay, flush, k_weighting, one_pole};
use crate::stats::{PercentileRing, SlidingMedian};
use realfft::{RealFftPlanner, RealToComplex};
use rustfft::num_complex::Complex;
use std::sync::Arc;

/// Number of third-octave bands (ISO 266 centres 20 Hz … 20 kHz).
pub const N_BANDS: usize = 31;

/// Exact third-octave centre frequency of band `i` (base-10 series, band 17 = 1 kHz).
pub fn band_center(i: usize) -> f32 {
    1000.0 * 10f32.powf((i as f32 - 17.0) / 10.0)
}

/// ISO 266 nominal label of band `i` in Hz (20, 25, 31.5, …, 20000).
pub const BAND_NOMINAL: [f32; N_BANDS] = [
    20.0, 25.0, 31.5, 40.0, 50.0, 63.0, 80.0, 100.0, 125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0, 630.0, 800.0, 1000.0, 1250.0, 1600.0, 2000.0, 2500.0,
    3150.0, 4000.0, 5000.0, 6300.0, 8000.0, 10000.0, 12500.0, 16000.0, 20000.0,
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OnsetClass {
    Kick,
    Snare,
    Hat,
}

impl OnsetClass {
    pub const ALL: [OnsetClass; 3] = [OnsetClass::Kick, OnsetClass::Snare, OnsetClass::Hat];

    pub fn name(self) -> &'static str {
        match self {
            OnsetClass::Kick => "kick",
            OnsetClass::Snare => "snare",
            OnsetClass::Hat => "hat",
        }
    }

    fn index(self) -> usize {
        self as usize
    }
}

/// Detector settings of one onset class.
#[derive(Clone, Debug, PartialEq)]
pub struct OnsetBand {
    /// Band-pass edges in Hz (4th-order Butterworth high-pass and low-pass).
    pub band: [f32; 2],
    /// Minimum log-energy rise (dB over one or two hops) for an onset.
    pub threshold_db: f32,
    /// Multiplier of the running median rise added to `threshold_db` (adapts to dense material).
    pub sensitivity: f32,
    /// Onsets whose level is more than this below the class's recent onset level are rejected
    /// (bleed from other instruments); strength spans this range.
    pub range_db: f32,
    /// Retrigger guard: minimum spacing of two onsets of this class.
    pub min_interval_ms: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct OnsetConfig {
    pub kick: OnsetBand,
    pub snare: OnsetBand,
    pub hat: OnsetBand,
    /// Window of the adaptive median over rises.
    pub median_window_ms: f32,
    /// Onsets are emitted no later than this after the located onset (the decision waits one hop
    /// for the peak level only when that stays within the bound).
    pub max_latency_ms: f32,
    /// Envelope (`Features::kick` …) decay: time to fall to 5 % (−26 dB) after an onset.
    pub envelope_decay_ms: f32,
}

impl OnsetConfig {
    pub fn class(&self, class: OnsetClass) -> &OnsetBand {
        match class {
            OnsetClass::Kick => &self.kick,
            OnsetClass::Snare => &self.snare,
            OnsetClass::Hat => &self.hat,
        }
    }

    pub fn class_mut(&mut self, class: OnsetClass) -> &mut OnsetBand {
        match class {
            OnsetClass::Kick => &mut self.kick,
            OnsetClass::Snare => &mut self.snare,
            OnsetClass::Hat => &mut self.hat,
        }
    }
}

impl Default for OnsetConfig {
    fn default() -> Self {
        OnsetConfig {
            kick: OnsetBand { band: [35.0, 160.0], threshold_db: 6.0, sensitivity: 1.0, range_db: 18.0, min_interval_ms: 70.0 },
            snare: OnsetBand { band: [400.0, 4000.0], threshold_db: 6.0, sensitivity: 1.0, range_db: 15.0, min_interval_ms: 70.0 },
            hat: OnsetBand { band: [7000.0, 16000.0], threshold_db: 6.0, sensitivity: 1.0, range_db: 15.0, min_interval_ms: 45.0 },
            median_window_ms: 1000.0,
            max_latency_ms: 11.0,
            envelope_decay_ms: 150.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct LiveConfig {
    pub sample_rate: f32,
    /// Analysis hop in samples (one [`Features`] per hop).
    pub hop: usize,
    pub onsets: OnsetConfig,
    /// Run the beat tracker (bpm/phase/beat events).
    pub beat: bool,
    /// Compute `hype` and emit hype spikes (mic buses).
    pub hype: bool,
}

impl Default for LiveConfig {
    fn default() -> Self {
        LiveConfig { sample_rate: 48000.0, hop: 256, onsets: OnsetConfig::default(), beat: true, hype: false }
    }
}

/// One analysis frame (per hop).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Features {
    /// Master-clock ns of the end of the analysed hop.
    pub ts: u64,
    /// RMS (50 ms) of both channels, linear (full-scale sine = 0.707).
    pub level: f32,
    /// Sample peak over the hop, linear.
    pub peak: f32,
    /// Momentary (400 ms) loudness, LUFS, floor −70.
    pub lufs_m: f32,
    /// Short-term (3 s) loudness, LUFS, floor −70.
    pub lufs_s: f32,
    /// Band RMS, linear: 20–250 Hz, 250 Hz–4 kHz, 4–20 kHz.
    pub bass: f32,
    pub mid: f32,
    pub high: f32,
    /// Third-octave band RMS, linear (mono mix).
    pub bands: [f32; N_BANDS],
    /// Spectral centroid, 0–1 log-mapped over 20 Hz–20 kHz (0 in silence).
    pub centroid: f32,
    pub centroid_hz: f32,
    /// Onset envelopes 0–1: jump to the onset strength, decay per `envelope_decay_ms`.
    pub kick: f32,
    pub snare: f32,
    pub hat: f32,
    /// Onset strength (novelty curve sample fed to the beat tracker), ≥ 0, typically 0–3.
    pub flux: f32,
    /// Section novelty 0–1 (refers to the moment 2 s before `ts`).
    pub novelty: f32,
    /// 0–1 loudness above the rolling baseline (when `LiveConfig::hype`).
    pub hype: f32,
    pub bpm: f32,
    /// Beat phase 0–1 at `ts`.
    pub phase: f32,
    pub beat_confidence: f32,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum AnalysisEvent {
    /// `strength` 0–1 velocity relative to recent onsets of the class; `ts` = estimated onset.
    Onset {
        class: OnsetClass,
        strength: f32,
        ts: u64,
    },
    Beat(BeatEvent),
    /// Energy + low-end jump after a lower-energy build/breakdown; `ts` = start of the jump.
    Drop {
        strength: f32,
        ts: u64,
    },
    /// Section boundary at `ts` (reported ~2 s after it).
    Section {
        novelty: f32,
        ts: u64,
    },
    HypeSpike {
        level: f32,
        ts: u64,
    },
}

// ---------------------------------------------------------------------------------------------
// Constants.

/// Relative noise floor of the onset bands (energy, −30 dB under the recent peak).
const KAPPA: f64 = 1e-3;
/// Absolute energy floor (−100 dBFS).
const ABS_FLOOR: f64 = 1e-10;
const PEAK_RELEASE_S: f64 = 4.0;
const REF_DECAY_DB_PER_S: f32 = 1.0;
const REF_ADAPT: f32 = 0.1;
/// First onset of a class (no reference yet) must be within this of the broadband energy.
const INIT_RANGE_DB: f32 = 20.0;
const LEVEL_TAU_S: f64 = 0.05;
const LUFS_FLOOR: f32 = -70.0;
const ODF_BANDS: [[f64; 2]; 6] = [[30.0, 110.0], [110.0, 300.0], [300.0, 1000.0], [1000.0, 3000.0], [3000.0, 7000.0], [7000.0, 16000.0]];
const ODF_GAIN: f64 = 10.0;
const ODF_REF_RELEASE_S: f64 = 10.0;
const FFT_N: usize = 1024;
const CHROMA_N: usize = 4096;
const SLOW_S: f64 = 0.2;
const NOV_A: usize = 10;
const NOV_B: usize = 30;
const NOV_DIM: usize = 21;
const SECTION_MIN: f32 = 0.4;
const SECTION_GAP_S: f64 = 8.0;
const DROP_TICK_S: f64 = 0.05;
const DROP_POST_S: f64 = 0.75;
const DROP_PRE_S: f64 = 6.0;
const DROP_LONG_S: f64 = 60.0;
const DROP_GAP_S: f64 = 15.0;
const HYPE_TICK_S: f64 = 0.5;
const HYPE_WINDOW_S: f64 = 60.0;
const HYPE_GATE_LUFS: f32 = -60.0;
const HYPE_MIN_ACTIVE: usize = 10;
const HYPE_ON: f32 = 0.7;
const HYPE_OFF: f32 = 0.3;
const HYPE_GAP_S: f64 = 3.0;
const MAX_EVENTS: usize = 16;
/// Onset level (velocity) is read from the class peak follower this long after the located onset
/// (so it doesn't depend on where the onset falls within a hop, and fits the latency budget).
const LEVEL_DELAY_S: f64 = 0.002;
/// Release of the per-class peak follower.
const PEAK_HOLD_S: f64 = 0.03;
/// Snare onsets weaker than this (relative to recent snares) that coincide with a kick are bleed.
const SNARE_BLEED_STRENGTH: f32 = 0.6;
const KICK_BLEED_NS: u64 = 12_000_000;

// Lanes of the envelope bank.
const L_ODF: usize = 0;
const L_CLASS: usize = 6;
const L_BASS: usize = 9;
const LANES: usize = 12;

/// Class envelope `delay` samples after onset `at`, if still in the two-hop ring (`n_end` = last
/// written sample).
fn onset_level(ring: &[f32], at: u64, n_end: u64, delay: u64) -> Option<f64> {
    let len = ring.len() as u64;
    let t = (at + delay).min(n_end);
    (t + len > n_end).then(|| ring[(t % len) as usize] as f64)
}

/// Largest class-envelope value from onset `at` to `n_end` still in the ring.
fn window_max(ring: &[f32], at: u64, n_end: u64) -> f64 {
    let len = ring.len() as u64;
    let from = at.max((n_end + 1).saturating_sub(len));
    (from..=n_end).map(|t| ring[(t % len) as usize] as f64).fold(0.0, f64::max)
}

fn db10(e: f64) -> f64 {
    10.0 * e.max(1e-30).log10()
}

// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
struct Pending {
    ts: u64,
    /// Located onset (sample index).
    at: u64,
    level: f64,
}

/// Onset decision state of one class.
#[derive(Clone, Debug)]
struct Detector {
    class: OnsetClass,
    cfg: OnsetBand,
    ring: Vec<f32>,
    /// Peak follower of the class band signal (instant attack), per sample for two hops: the
    /// onset velocity is read from it (the energy envelope is too slow for a level at low latency).
    pk_ring: Vec<f32>,
    pk: f64,
    pk_rel: f64,
    hop_max: f64,
    peak: f64,
    l1: f64,
    l2: f64,
    e1: f64,
    e2: f64,
    median: SlidingMedian,
    last_onset: Option<u64>,
    /// Located time (master ns) of the last detected onset.
    last_ts: Option<u64>,
    /// Retrigger state before the latest detection (restored if that onset is rejected).
    prev: (Option<u64>, Option<u64>),
    pending: Option<Pending>,
    reference: Option<f32>,
    comp_ns: f64,
    min_interval: u64,
    env: f32,
}

impl Detector {
    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.pk_ring.fill(0.0);
        self.pk = 0.0;
        self.hop_max = 0.0;
        self.peak = 0.0;
        self.l1 = db10(ABS_FLOOR);
        self.l2 = self.l1;
        self.e1 = 0.0;
        self.e2 = 0.0;
        self.median.clear();
        self.last_onset = None;
        self.last_ts = None;
        self.prev = (None, None);
        self.pending = None;
        self.reference = None;
        self.env = 0.0;
    }
}

struct Spectral {
    fft: Arc<dyn RealToComplex<f32>>,
    fft_chroma: Arc<dyn RealToComplex<f32>>,
    window: Vec<f32>,
    window_chroma: Vec<f32>,
    input: Vec<f32>,
    input_chroma: Vec<f32>,
    spec: Vec<Complex<f32>>,
    spec_chroma: Vec<Complex<f32>>,
    scratch: Vec<Complex<f32>>,
    scratch_chroma: Vec<Complex<f32>>,
    chroma_map: Vec<(u16, u8)>,
    ring: Vec<f32>,
    pos: usize,
}

fn hann(n: usize) -> Vec<f32> {
    (0..n).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()) as f32).collect()
}

impl Spectral {
    fn new(sr: f64) -> Self {
        let mut planner = RealFftPlanner::<f32>::new();
        let fft = planner.plan_fft_forward(FFT_N);
        let fft_chroma = planner.plan_fft_forward(CHROMA_N);
        let chroma_map = (1..CHROMA_N / 2)
            .filter_map(|k| {
                let f = k as f64 * sr / CHROMA_N as f64;
                if !(80.0..=5000.0).contains(&f) {
                    return None;
                }
                let pc = (12.0 * (f / 440.0).log2()).round() as i64 + 9; // A = 9 with C = 0
                Some((k as u16, pc.rem_euclid(12) as u8))
            })
            .collect();
        Spectral {
            input: fft.make_input_vec(),
            spec: fft.make_output_vec(),
            scratch: fft.make_scratch_vec(),
            input_chroma: fft_chroma.make_input_vec(),
            spec_chroma: fft_chroma.make_output_vec(),
            scratch_chroma: fft_chroma.make_scratch_vec(),
            fft,
            fft_chroma,
            window: hann(FFT_N),
            window_chroma: hann(CHROMA_N),
            chroma_map,
            ring: vec![0.0; CHROMA_N],
            pos: 0,
        }
    }

    #[inline]
    fn push(&mut self, x: f32) {
        self.ring[self.pos] = x;
        self.pos = (self.pos + 1) & (CHROMA_N - 1);
    }

    fn fill(ring: &[f32], pos: usize, window: &[f32], out: &mut [f32]) {
        let n = out.len();
        let start = (pos + CHROMA_N - n) & (CHROMA_N - 1);
        for (i, (o, w)) in out.iter_mut().zip(window).enumerate() {
            *o = ring[(start + i) & (CHROMA_N - 1)] * w;
        }
    }

    /// (centroid Hz, total power) of the latest `FFT_N` samples.
    fn centroid(&mut self, sr: f64) -> (f64, f64) {
        Self::fill(&self.ring, self.pos, &self.window, &mut self.input);
        if self.fft.process_with_scratch(&mut self.input, &mut self.spec, &mut self.scratch).is_err() {
            return (0.0, 0.0);
        }
        let df = sr / FFT_N as f64;
        let top = 20000.0f64.min(sr / 2.0);
        let (mut num, mut den) = (0.0, 0.0);
        for (k, c) in self.spec.iter().enumerate().skip(1) {
            let f = k as f64 * df;
            if f < 20.0 {
                continue;
            }
            if f > top {
                break;
            }
            let p = c.norm_sqr() as f64;
            num += f * p;
            den += p;
        }
        (if den > 0.0 { num / den } else { 0.0 }, den)
    }

    /// L1-normalised 12-bin chroma (C = 0) of the latest `CHROMA_N` samples.
    fn chroma(&mut self, out: &mut [f32; 12]) {
        *out = [0.0; 12];
        Self::fill(&self.ring, self.pos, &self.window_chroma, &mut self.input_chroma);
        if self.fft_chroma.process_with_scratch(&mut self.input_chroma, &mut self.spec_chroma, &mut self.scratch_chroma).is_err() {
            return;
        }
        let mut total = 0.0;
        for &(k, pc) in &self.chroma_map {
            let m = self.spec_chroma[k as usize].norm();
            out[pc as usize] += m;
            total += m;
        }
        if total > 1e-6 {
            for v in out.iter_mut() {
                *v /= total;
            }
        }
    }

    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.pos = 0;
    }
}

/// Exact sliding-window loudness over whole hops.
#[derive(Clone, Debug)]
struct Loudness {
    ring: Vec<f64>,
    pos: usize,
    m_hops: usize,
    sum_m: f64,
    sum_s: f64,
    hop: usize,
}

impl Loudness {
    fn new(fr: f64, hop: usize) -> Self {
        let m_hops = (0.4 * fr).round().max(1.0) as usize;
        let s_hops = (3.0 * fr).round().max(m_hops as f64) as usize;
        Loudness { ring: vec![0.0; s_hops], pos: 0, m_hops, sum_m: 0.0, sum_s: 0.0, hop }
    }

    fn push(&mut self, hop_sum: f64) {
        let cap = self.ring.len();
        let leaving_m = self.ring[(self.pos + cap - self.m_hops) % cap];
        let leaving_s = self.ring[self.pos];
        self.ring[self.pos] = hop_sum;
        self.pos = (self.pos + 1) % cap;
        if self.pos == 0 {
            // Re-sum once per wrap so rounding never accumulates.
            self.sum_s = self.ring.iter().sum();
            self.sum_m = (0..self.m_hops).map(|i| self.ring[(cap - 1 - i) % cap]).sum();
        } else {
            self.sum_s += hop_sum - leaving_s;
            self.sum_m += hop_sum - leaving_m;
        }
    }

    fn lufs(sum: f64, samples: usize) -> f32 {
        let ms = sum / samples as f64;
        if ms <= 1e-12 { LUFS_FLOOR } else { ((-0.691 + 10.0 * ms.log10()) as f32).max(LUFS_FLOOR) }
    }

    fn momentary(&self) -> f32 {
        Self::lufs(self.sum_m, self.m_hops * self.hop)
    }

    fn short_term(&self) -> f32 {
        Self::lufs(self.sum_s, self.ring.len() * self.hop)
    }

    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.pos = 0;
        self.sum_m = 0.0;
        self.sum_s = 0.0;
    }
}

/// Checkerboard novelty on slow feature frames.
#[derive(Clone, Debug)]
struct Novelty {
    frames: Vec<[f32; NOV_DIM]>,
    pos: usize,
    count: usize,
    prev: f32,
    prev2: f32,
    last_event_tick: Option<u64>,
    ticks: u64,
}

impl Novelty {
    fn new() -> Self {
        Novelty { frames: vec![[0.0; NOV_DIM]; NOV_A + NOV_B], pos: 0, count: 0, prev: 0.0, prev2: 0.0, last_event_tick: None, ticks: 0 }
    }

    fn frame(&self, age: usize) -> &[f32; NOV_DIM] {
        let cap = self.frames.len();
        &self.frames[(self.pos + cap - 1 - age) % cap]
    }

    fn dist(a: &[f32; NOV_DIM], b: &[f32; NOV_DIM]) -> f32 {
        a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum::<f32>().sqrt()
    }

    /// Push a frame; returns (novelty now, Some(peak novelty) if the previous tick was a peak).
    fn push(&mut self, v: [f32; NOV_DIM]) -> (f32, Option<f32>) {
        let cap = self.frames.len();
        self.frames[self.pos] = v;
        self.pos = (self.pos + 1) % cap;
        self.count = (self.count + 1).min(cap);
        self.ticks += 1;
        if self.count < cap {
            return (0.0, None);
        }
        let mean_block = |a0: usize, a1: usize, b0: usize, b1: usize, same: bool| {
            let (mut s, mut n) = (0.0f32, 0u32);
            for i in a0..a1 {
                for j in b0..b1 {
                    if same && j <= i {
                        continue;
                    }
                    s += Self::dist(self.frame(i), self.frame(j));
                    n += 1;
                }
            }
            if n == 0 { 0.0 } else { s / n as f32 }
        };
        let d_ab = mean_block(0, NOV_A, NOV_A, NOV_A + NOV_B, false);
        let d_aa = mean_block(0, NOV_A, 0, NOV_A, true);
        let d_bb = mean_block(NOV_A, NOV_A + NOV_B, NOV_A, NOV_A + NOV_B, true);
        let nov = if d_ab > 1e-6 { ((d_ab - 0.5 * (d_aa + d_bb)) / d_ab).clamp(0.0, 1.0) } else { 0.0 };
        let peak = (self.prev >= SECTION_MIN && self.prev >= self.prev2 && self.prev > nov).then_some(self.prev);
        self.prev2 = self.prev;
        self.prev = nov;
        (nov, peak)
    }

    fn reset(&mut self) {
        self.frames.fill([0.0; NOV_DIM]);
        self.pos = 0;
        self.count = 0;
        self.prev = 0.0;
        self.prev2 = 0.0;
        self.last_event_tick = None;
        self.ticks = 0;
    }
}

/// Drop detector on decimated loudness/bass energy.
#[derive(Clone, Debug)]
struct DropDetector {
    loud: Vec<f64>,
    bass: Vec<f64>,
    pos: usize,
    count: usize,
    acc_l: f64,
    acc_b: f64,
    acc_n: usize,
    tick_hops: usize,
    long: Vec<f64>,
    long_pos: usize,
    long_acc: f64,
    long_n: usize,
    ticks_per_long: usize,
    last_drop_tick: Option<u64>,
    ticks: u64,
}

impl DropDetector {
    fn new(fr: f64) -> Self {
        let tick_hops = (DROP_TICK_S * fr).round().max(1.0) as usize;
        let tick_s = tick_hops as f64 / fr;
        let cap = ((DROP_PRE_S + DROP_POST_S + 1.0) / tick_s).ceil() as usize + 2;
        let ticks_per_long = (1.0 / tick_s).round().max(1.0) as usize;
        DropDetector {
            loud: vec![0.0; cap],
            bass: vec![0.0; cap],
            pos: 0,
            count: 0,
            acc_l: 0.0,
            acc_b: 0.0,
            acc_n: 0,
            tick_hops,
            long: vec![0.0; DROP_LONG_S as usize],
            long_pos: 0,
            long_acc: 0.0,
            long_n: 0,
            ticks_per_long,
            last_drop_tick: None,
            ticks: 0,
        }
    }

    fn tick_s(&self, fr: f64) -> f64 {
        self.tick_hops as f64 / fr
    }

    fn at(v: &[f64], pos: usize, age: usize) -> f64 {
        v[(pos + v.len() - 1 - age) % v.len()]
    }

    /// Feed one hop's mean-square loudness and bass energies. Returns (strength, age of the jump
    /// start in seconds before now) when a drop is detected.
    fn push(&mut self, loud_ms: f64, bass_ms: f64, fr: f64) -> Option<(f32, f64)> {
        self.acc_l += loud_ms;
        self.acc_b += bass_ms;
        self.acc_n += 1;
        if self.acc_n < self.tick_hops {
            return None;
        }
        let (l, b) = (self.acc_l / self.acc_n as f64, self.acc_b / self.acc_n as f64);
        self.acc_l = 0.0;
        self.acc_b = 0.0;
        self.acc_n = 0;
        let cap = self.loud.len();
        self.loud[self.pos] = l;
        self.bass[self.pos] = b;
        self.pos = (self.pos + 1) % cap;
        self.count = (self.count + 1).min(cap);
        self.ticks += 1;
        self.long_acc += l;
        self.long_n += 1;
        if self.long_n >= self.ticks_per_long {
            self.long[self.long_pos] = self.long_acc / self.long_n as f64;
            self.long_pos = (self.long_pos + 1) % self.long.len();
            self.long_acc = 0.0;
            self.long_n = 0;
        }
        let tick_s = self.tick_s(fr);
        let post = (DROP_POST_S / tick_s).round() as usize;
        let pre = (DROP_PRE_S / tick_s).round() as usize;
        if self.count < post + pre {
            return None;
        }
        if let Some(t) = self.last_drop_tick
            && ((self.ticks - t) as f64) * tick_s < DROP_GAP_S
        {
            return None;
        }
        let mean = |v: &[f64], a0: usize, a1: usize| (a0..a1).map(|a| Self::at(v, self.pos, a)).sum::<f64>() / (a1 - a0) as f64;
        let (lp, lq) = (db10(mean(&self.loud, 0, post)), db10(mean(&self.loud, post, post + pre)));
        let (bp, bq) = (db10(mean(&self.bass, 0, post)), db10(mean(&self.bass, post, post + pre)));
        let long_max = db10(self.long.iter().copied().fold(0.0, f64::max).max(mean(&self.loud, 0, post)));
        let (dl, dbass) = (lp - lq, bp - bq);
        let ok = dl >= 6.0 && dbass >= 9.0 && lp >= long_max - 3.0 && lq >= long_max - 30.0 && lq >= -60.0;
        if !ok {
            return None;
        }
        // Start of the jump: oldest tick of the contiguous run above the midpoint that ends now
        // (isolated loud hits during the build don't count).
        let mid = 10f64.powf(0.5 * (lp + lq) / 10.0);
        let span = (2 * post).min(self.count);
        let start_age = (0..span).find(|a| Self::at(&self.loud, self.pos, *a) < mid).unwrap_or(span).saturating_sub(1);
        self.last_drop_tick = Some(self.ticks);
        let strength = (0.5 * ((dl - 3.0) / 12.0) + 0.5 * ((dbass - 6.0) / 18.0)).clamp(0.1, 1.0) as f32;
        Some((strength, (start_age + 1) as f64 * tick_s))
    }

    fn reset(&mut self) {
        self.loud.fill(0.0);
        self.bass.fill(0.0);
        self.long.fill(0.0);
        self.pos = 0;
        self.count = 0;
        self.acc_l = 0.0;
        self.acc_b = 0.0;
        self.acc_n = 0;
        self.long_pos = 0;
        self.long_acc = 0.0;
        self.long_n = 0;
        self.last_drop_tick = None;
        self.ticks = 0;
    }
}

#[derive(Clone, Debug)]
struct Hype {
    ring: PercentileRing,
    tick_hops: usize,
    hops: usize,
    armed: bool,
    last_spike_hop: Option<u64>,
    value: f32,
}

impl Hype {
    fn new(fr: f64) -> Self {
        let tick_hops = (HYPE_TICK_S * fr).round().max(1.0) as usize;
        let cap = (HYPE_WINDOW_S / (tick_hops as f64 / fr)).ceil() as usize;
        Hype { ring: PercentileRing::new(cap), tick_hops, hops: 0, armed: true, last_spike_hop: None, value: 0.0 }
    }

    /// Returns Some(level) for a spike.
    fn push(&mut self, lufs_m: f32, hop_index: u64, fr: f64) -> Option<f32> {
        self.hops += 1;
        if self.hops >= self.tick_hops {
            self.hops = 0;
            if lufs_m > HYPE_GATE_LUFS {
                self.ring.push(lufs_m);
            }
        }
        if self.ring.len() < HYPE_MIN_ACTIVE {
            self.value = 0.0;
            return None;
        }
        let base = self.ring.percentile(0.5).unwrap_or(LUFS_FLOOR);
        self.value = ((lufs_m - base - 3.0) / 12.0).clamp(0.0, 1.0);
        let cooled = self.last_spike_hop.is_none_or(|h| (hop_index - h) as f64 / fr >= HYPE_GAP_S);
        if self.value < HYPE_OFF && cooled {
            self.armed = true;
        }
        if self.armed && self.value >= HYPE_ON {
            self.armed = false;
            self.last_spike_hop = Some(hop_index);
            return Some(self.value);
        }
        None
    }

    fn reset(&mut self) {
        self.ring.clear();
        self.hops = 0;
        self.armed = true;
        self.last_spike_hop = None;
        self.value = 0.0;
    }
}

/// Fixed-capacity event list handed to the callback.
struct Events {
    buf: [AnalysisEvent; MAX_EVENTS],
    len: usize,
}

impl Events {
    fn push(&mut self, e: AnalysisEvent) {
        if self.len < MAX_EVENTS {
            self.buf[self.len] = e;
            self.len += 1;
        }
    }
}

/// Live analyzer of one stereo bus. See the module docs for the algorithms.
pub struct LiveAnalyzer {
    cfg: LiveConfig,
    sr: f64,
    hop: usize,
    fr: f64,
    ns_per_sample: f64,
    // Sample counters.
    n: u64,
    in_hop: usize,
    hops: u64,
    call_ts: u64,
    call_n0: u64,
    // Level and loudness.
    peak: f32,
    lvl_ms: f64,
    a_lvl: f64,
    kw: [[Biquad; 2]; 2],
    kw_sum: f64,
    loud: Loudness,
    // Filter banks.
    thirds: Bank<32, 3>,
    third_a: [f64; 32],
    third_ms: [f64; 32],
    env_bank: Bank<LANES, 4>,
    env_a: [f64; LANES],
    env1: [f64; LANES],
    env2: [f64; LANES],
    // Onsets.
    det: [Detector; 3],
    odf_prev: [f64; 6],
    odf_ref: f64,
    odf_ref_rel: f64,
    peak_rel: f64,
    env_decay: f32,
    odf_delay_ns: f64,
    // Spectra.
    spectral: Spectral,
    slow_hops: usize,
    slow_count: usize,
    novelty: Novelty,
    nov_value: f32,
    drop: DropDetector,
    hype: Hype,
    beat: Option<BeatTracker>,
    features: Features,
    have_features: bool,
    events: Events,
}

impl LiveAnalyzer {
    pub fn new(cfg: LiveConfig) -> Self {
        let sr = cfg.sample_rate.max(1000.0) as f64;
        let hop = cfg.hop.max(16);
        let fr = sr / hop as f64;
        let mut thirds = Bank::<32, 3>::new();
        let mut third_a = [0.0; 32];
        for (i, a) in third_a.iter_mut().enumerate().take(N_BANDS) {
            let fc = band_center(i) as f64;
            match butter_bandpass(fc * 10f64.powf(-0.05), fc * 10f64.powf(0.05), sr, 3) {
                Some(s) => thirds.set_lane(i, &s),
                None => thirds.set_lane(i, &[]),
            }
            *a = one_pole((1.5 / fc).clamp(0.012, 0.075), sr);
        }
        thirds.set_lane(31, &[]);
        third_a[31] = 1.0;

        let mut env_bank = Bank::<LANES, 4>::new();
        let mut env_a = [0.0; LANES];
        let tau_for = |lo: f64| (1.0 / (2.0 * std::f64::consts::PI * lo.max(1.0))).clamp(0.001, 0.005);
        for (b, [lo, hi]) in ODF_BANDS.iter().enumerate() {
            env_bank.set_lane(L_ODF + b, &butter_band_skirts(*lo, *hi, sr, 2));
            env_a[L_ODF + b] = one_pole(tau_for(*lo), sr);
        }
        let mut comp = [0.0; 3];
        for class in OnsetClass::ALL {
            let c = cfg.onsets.class(class);
            let (lo, hi) = (c.band[0].max(10.0) as f64, c.band[1].max(c.band[0] + 1.0) as f64);
            // kick: 12 dB/oct skirts keep the low-band group delay inside the latency budget
            let order = if class == OnsetClass::Kick { 2 } else { 4 };
            let sections = butter_band_skirts(lo, hi, sr, order);
            let centre = (lo * hi.min(0.45 * sr)).sqrt();
            let tau = tau_for(lo);
            // Band group delay + time for the two-pole envelope to reach 10 % of a step.
            comp[class.index()] = cascade_group_delay(&sections, centre, sr) + 0.53 * tau;
            env_bank.set_lane(L_CLASS + class.index(), &sections);
            env_a[L_CLASS + class.index()] = one_pole(tau, sr);
        }
        env_bank.set_lane(
            L_BASS,
            &[Biquad::highpass(20.0, sr, std::f64::consts::FRAC_1_SQRT_2)].iter().copied().chain(butter_band_skirts(0.0, 250.0, sr, 4)).collect::<Vec<_>>(),
        );
        env_bank.set_lane(L_BASS + 1, &butter_band_skirts(250.0, 4000.0, sr, 4));
        let high: Vec<Biquad> = butter_band_skirts(4000.0, 1e9, sr, 4)
            .into_iter()
            .chain((20000.0 < 0.45 * sr).then(|| Biquad::lowpass(20000.0, sr, std::f64::consts::FRAC_1_SQRT_2)))
            .collect();
        env_bank.set_lane(L_BASS + 2, &high);
        env_a[L_BASS] = one_pole(0.025, sr);
        env_a[L_BASS + 1] = one_pole(0.01, sr);
        env_a[L_BASS + 2] = one_pole(0.0075, sr);

        let median_len = ((cfg.onsets.median_window_ms as f64 / 1000.0) * fr).round().max(3.0) as usize;
        let det = OnsetClass::ALL.map(|class| {
            let c = cfg.onsets.class(class).clone();
            let mut d = Detector {
                class,
                min_interval: (c.min_interval_ms.max(0.0) as f64 * 1e-3 * sr).round() as u64,
                cfg: c,
                ring: vec![0.0; 2 * hop],
                pk_ring: vec![0.0; 2 * hop],
                pk: 0.0,
                pk_rel: (-1.0 / (PEAK_HOLD_S * sr)).exp(),
                hop_max: 0.0,
                peak: 0.0,
                l1: 0.0,
                l2: 0.0,
                e1: 0.0,
                e2: 0.0,
                median: SlidingMedian::new(median_len),
                last_onset: None,
                last_ts: None,
                prev: (None, None),
                pending: None,
                reference: None,
                comp_ns: comp[class.index()] * 1e9,
                env: 0.0,
            };
            d.reset();
            d
        });
        let hop_s = hop as f64 / sr;
        let env_tau = (cfg.onsets.envelope_decay_ms.max(1.0) as f64 / 1000.0) / 3.0;
        let beat = cfg.beat.then(|| BeatTracker::new(sr as f32, hop));
        LiveAnalyzer {
            sr,
            hop,
            fr,
            ns_per_sample: 1e9 / sr,
            n: 0,
            in_hop: 0,
            hops: 0,
            call_ts: 0,
            call_n0: 0,
            peak: 0.0,
            lvl_ms: 0.0,
            a_lvl: one_pole(LEVEL_TAU_S, sr),
            kw: [k_weighting(sr), k_weighting(sr)],
            kw_sum: 0.0,
            loud: Loudness::new(fr, hop),
            thirds,
            third_a,
            third_ms: [0.0; 32],
            env_bank,
            env_a,
            env1: [0.0; LANES],
            env2: [0.0; LANES],
            det,
            odf_prev: [0.0; 6],
            odf_ref: 0.0,
            odf_ref_rel: (-hop_s / ODF_REF_RELEASE_S).exp(),
            peak_rel: (-hop_s / PEAK_RELEASE_S).exp(),
            env_decay: (-hop_s / env_tau).exp() as f32,
            // Onset energy shows up over the hop that contains it and the envelopes add ~2 ms.
            odf_delay_ns: (0.5 * hop_s + 0.002) * 1e9,
            spectral: Spectral::new(sr),
            slow_hops: (SLOW_S * fr).round().max(1.0) as usize,
            slow_count: 0,
            novelty: Novelty::new(),
            nov_value: 0.0,
            drop: DropDetector::new(fr),
            hype: Hype::new(fr),
            beat,
            features: Features::default(),
            have_features: false,
            events: Events { buf: [AnalysisEvent::Section { novelty: 0.0, ts: 0 }; MAX_EVENTS], len: 0 },
            cfg,
        }
    }

    pub fn config(&self) -> &LiveConfig {
        &self.cfg
    }

    /// Feed stereo samples (any length; `r` may be empty for mono). `ts` = master-clock ns of
    /// `l[0]`. Calls `out` once per completed hop. Does not allocate.
    pub fn process(&mut self, l: &[f32], r: &[f32], ts: u64, out: &mut dyn FnMut(&Features, &[AnalysisEvent])) {
        let len = if r.is_empty() { l.len() } else { l.len().min(r.len()) };
        self.call_ts = ts;
        self.call_n0 = self.n;
        for i in 0..len {
            let xl = l[i];
            let xr = if r.is_empty() { xl } else { r[i] };
            let xl = if xl.is_finite() { xl } else { 0.0 };
            let xr = if xr.is_finite() { xr } else { 0.0 };
            self.sample(xl, xr);
            if self.in_hop == self.hop {
                self.in_hop = 0;
                self.end_hop(out);
            }
        }
    }

    #[inline]
    fn sample(&mut self, xl: f32, xr: f32) {
        let (l, r) = (xl as f64, xr as f64);
        self.peak = self.peak.max(xl.abs()).max(xr.abs());
        self.lvl_ms += self.a_lvl * (0.5 * (l * l + r * r) - self.lvl_ms);
        let kl = self.kw[0][0].process(l);
        let kl = self.kw[0][1].process(kl);
        let kr = self.kw[1][0].process(r);
        let kr = self.kw[1][1].process(kr);
        self.kw_sum += kl * kl + kr * kr;
        let m = 0.5 * (l + r);
        self.spectral.push(m as f32);

        let mut y = [0.0; 32];
        self.thirds.process(m, &mut y);
        for b in 0..32 {
            self.third_ms[b] += self.third_a[b] * (y[b] * y[b] - self.third_ms[b]);
        }
        let mut e = [0.0; LANES];
        self.env_bank.process(m, &mut e);
        for b in 0..LANES {
            self.env1[b] += self.env_a[b] * (e[b] * e[b] - self.env1[b]);
            self.env2[b] += self.env_a[b] * (self.env1[b] - self.env2[b]);
        }
        let slot = (self.n % (2 * self.hop) as u64) as usize;
        for d in &mut self.det {
            let v = self.env2[L_CLASS + d.class.index()];
            let x = e[L_CLASS + d.class.index()].abs();
            d.pk = if x > d.pk { x } else { d.pk * d.pk_rel };
            flush(&mut d.pk);
            d.pk_ring[slot] = d.pk as f32;
            d.ring[slot] = v as f32;
            d.hop_max = d.hop_max.max(v);
        }
        self.n += 1;
        self.in_hop += 1;
    }

    fn ts_of(&self, sample: u64) -> u64 {
        let d = (sample as f64 - self.call_n0 as f64) * self.ns_per_sample;
        (self.call_ts as f64 + d).max(0.0) as u64
    }

    fn end_hop(&mut self, out: &mut dyn FnMut(&Features, &[AnalysisEvent])) {
        self.hops += 1;
        self.events.len = 0;
        let ts = self.ts_of(self.n);
        let n_end = self.n - 1;
        let f = &mut self.features;
        f.ts = ts;
        f.peak = self.peak;
        self.peak = 0.0;
        f.level = self.lvl_ms.max(0.0).sqrt() as f32;
        let hop_ms = self.kw_sum / (2.0 * self.hop as f64);
        self.loud.push(self.kw_sum);
        self.kw_sum = 0.0;
        f.lufs_m = self.loud.momentary();
        f.lufs_s = self.loud.short_term();
        for (b, v) in f.bands.iter_mut().enumerate() {
            *v = self.third_ms[b].max(0.0).sqrt() as f32;
        }
        f.bass = self.env2[L_BASS].max(0.0).sqrt() as f32;
        f.mid = self.env2[L_BASS + 1].max(0.0).sqrt() as f32;
        f.high = self.env2[L_BASS + 2].max(0.0).sqrt() as f32;
        let (c_hz, power) = self.spectral.centroid(self.sr);
        if power > 1e-9 && c_hz > 0.0 {
            f.centroid_hz = c_hz as f32;
            f.centroid = ((c_hz / 20.0).ln() / 1000f64.ln()).clamp(0.0, 1.0) as f32;
        } else {
            f.centroid_hz = 0.0;
            f.centroid = 0.0;
        }

        // Onset-strength curve.
        let mut flux = 0.0;
        let mut total = 0.0;
        for b in 0..6 {
            let e = self.env2[L_ODF + b].max(0.0);
            let a = e.sqrt();
            flux += (a - self.odf_prev[b]).max(0.0);
            self.odf_prev[b] = a;
            total += e;
        }
        self.odf_ref = (self.odf_ref * self.odf_ref_rel).max(total.sqrt()).max(1e-5);
        let odf = (ODF_GAIN * flux / self.odf_ref).ln_1p();
        self.features.flux = odf as f32;

        // Onset decisions.
        let broadband_db = db10(total.max(ABS_FLOOR)) as f32;
        for c in 0..3 {
            self.detect(c, n_end, broadband_db);
        }
        let f = &mut self.features;
        for d in &mut self.det {
            d.env *= self.env_decay;
            if d.env < 1e-4 {
                d.env = 0.0;
            }
        }
        f.kick = self.det[0].env;
        f.snare = self.det[1].env;
        f.hat = self.det[2].env;

        // Beat.
        if let Some(bt) = &mut self.beat {
            let odf_ts = (ts as f64 - self.odf_delay_ns).max(0.0) as u64;
            if let Some(ev) = bt.push(odf as f32, odf_ts) {
                self.events.push(AnalysisEvent::Beat(ev));
            }
            f.bpm = bt.bpm();
            f.phase = bt.phase_at(ts);
            f.beat_confidence = bt.confidence();
        }

        // Drops.
        if let Some((strength, age_s)) = self.drop.push(hop_ms, self.env2[L_BASS].max(0.0), self.fr) {
            let t = (ts as f64 - age_s * 1e9).max(0.0) as u64;
            self.events.push(AnalysisEvent::Drop { strength, ts: t });
        }

        // Slow features → section novelty.
        self.slow_count += 1;
        if self.slow_count >= self.slow_hops {
            self.slow_count = 0;
            let v = self.slow_vector();
            let (nov, peak) = self.novelty.push(v);
            self.nov_value = nov;
            if let Some(p) = peak {
                let tick_s = self.slow_hops as f64 / self.fr;
                let gap_ok = self.novelty.last_event_tick.is_none_or(|t| (self.novelty.ticks - t) as f64 * tick_s >= SECTION_GAP_S);
                if gap_ok {
                    self.novelty.last_event_tick = Some(self.novelty.ticks);
                    // The peak was one tick ago; the boundary sits between blocks A and B.
                    let back = (NOV_A as f64 + 1.0) * tick_s;
                    let t = (ts as f64 - back * 1e9).max(0.0) as u64;
                    self.events.push(AnalysisEvent::Section { novelty: p, ts: t });
                }
            }
        }
        self.features.novelty = self.nov_value;

        // Hype.
        if self.cfg.hype {
            if let Some(level) = self.hype.push(self.features.lufs_m, self.hops, self.fr) {
                self.events.push(AnalysisEvent::HypeSpike { level, ts });
            }
            self.features.hype = self.hype.value;
        }

        self.thirds.flush();
        self.env_bank.flush();
        for v in self.third_ms.iter_mut().chain(&mut self.env1).chain(&mut self.env2) {
            flush(v);
        }
        flush(&mut self.lvl_ms);
        for k in &mut self.kw {
            k[0].flush();
            k[1].flush();
        }
        self.have_features = true;
        out(&self.features, &self.events.buf[..self.events.len]);
    }

    fn slow_vector(&mut self) -> [f32; NOV_DIM] {
        let mut v = [0.0f32; NOV_DIM];
        let mut chroma = [0.0f32; 12];
        self.spectral.chroma(&mut chroma);
        for (o, c) in v.iter_mut().zip(chroma) {
            *o = 2.0 * c;
        }
        let mut shape = [0.0f64; 8];
        for (g, s) in shape.iter_mut().enumerate() {
            let (a, b) = (g * 4, (g * 4 + 4).min(N_BANDS));
            let ms = self.third_ms[a..b].iter().sum::<f64>() / (b - a) as f64;
            *s = db10(ms + 1e-12);
        }
        let mean = shape.iter().sum::<f64>() / 8.0;
        for (g, s) in shape.iter().enumerate() {
            v[12 + g] = ((s - mean) / 6.0) as f32;
        }
        v[20] = self.features.lufs_m.max(LUFS_FLOOR) / 6.0;
        v
    }

    fn detect(&mut self, c: usize, n_end: u64, broadband_db: f32) {
        let hop = self.hop as u64;
        let ring_len = 2 * hop;
        let sr = self.sr;
        let hop_s = self.hop as f64 / sr;
        let max_lat = (self.cfg.onsets.max_latency_ms.max(0.0) as f64 * 1e-3 * sr) as u64;
        let peak_rel = self.peak_rel;
        let e_now = self.env2[L_CLASS + c].max(0.0);
        if n_end + 1 < ring_len {
            // Not two hops of history yet: only prime the state.
            let d = &mut self.det[c];
            d.peak = (d.peak * peak_rel).max(d.hop_max);
            d.l2 = d.l1;
            d.l1 = db10(e_now + (d.peak * KAPPA).max(ABS_FLOOR));
            d.e2 = d.e1;
            d.e1 = e_now;
            d.hop_max = 0.0;
            return;
        }
        let ts_first = self.ts_of(n_end + 1 - ring_len);
        let ns = self.ns_per_sample;
        let kick_ts = self.det[OnsetClass::Kick.index()].last_ts;
        let level_delay = (LEVEL_DELAY_S * sr) as u64;
        let d = &mut self.det[c];
        d.peak = (d.peak * peak_rel).max(d.hop_max);
        let eps = (d.peak * KAPPA).max(ABS_FLOOR);
        let l = db10(e_now + eps);
        let rise = (l - d.l1).max(l - d.l2).max(0.0);
        let thr = d.cfg.threshold_db as f64 + d.cfg.sensitivity as f64 * d.median.median() as f64;
        d.median.push(rise as f32);
        if let Some(r) = &mut d.reference {
            *r -= REF_DECAY_DB_PER_S * hop_s as f32;
        }
        let mut emit: Option<(u64, f64, u64)> = None;
        if let Some(p) = d.pending.take() {
            emit = Some((p.ts, onset_level(&d.pk_ring, p.at, n_end, level_delay).map(|a| a * a).unwrap_or(p.level.max(d.hop_max)), p.at));
        } else if rise >= thr && e_now > ABS_FLOOR * 10.0 {
            // Locate the onset: first sample of the last two hops above 10 % of the rise.
            let base = d.e2.min(e_now);
            let target = base + 0.1 * (d.hop_max - base);
            let start = ((n_end + 1 - ring_len) % ring_len) as usize;
            let mut j_on = ring_len as usize - 1;
            for j in 0..ring_len as usize {
                if d.ring[(start + j) % ring_len as usize] as f64 >= target {
                    j_on = j;
                    break;
                }
            }
            let s_on = n_end + 1 - ring_len + j_on as u64;
            let spaced = d.last_onset.is_none_or(|last| s_on >= last + d.min_interval);
            if spaced {
                d.prev = (d.last_onset, d.last_ts);
                d.last_onset = Some(s_on);
                let ts_on = (ts_first as f64 + j_on as f64 * ns - d.comp_ns).max(0.0) as u64;
                d.last_ts = Some(ts_on);
                // wait one more hop for the peak level only if the event still leaves within
                // `max_latency_ms` of the true onset (band group delay included)
                let comp_samples = (d.comp_ns / ns) as u64;
                if n_end + hop - s_on + comp_samples <= max_lat {
                    d.pending = Some(Pending { ts: ts_on, at: s_on, level: d.hop_max });
                } else {
                    emit = Some((ts_on, onset_level(&d.pk_ring, s_on, n_end, level_delay).map(|a| a * a).unwrap_or(d.hop_max), s_on));
                }
            }
        }
        d.l2 = d.l1;
        d.l1 = l;
        d.e2 = d.e1;
        d.e1 = e_now;
        d.hop_max = 0.0;
        if let Some((ts_on, level, at)) = emit {
            let level_db = db10(level.max(ABS_FLOOR)) as f32;
            let accept = match d.reference {
                Some(r) => level_db >= r - d.cfg.range_db,
                None => level_db >= broadband_db - INIT_RANGE_DB,
            };
            let r = d.reference.unwrap_or(level_db);
            let strength = if level_db >= r { 1.0 } else { 10f32.powf((level_db - r) / 20.0) };
            // Kick bleed: a weak snare-band rise at the same moment as a kick is the kick's
            // beater click (and coincident hats), not a snare. Strong unison hits still pass.
            // (judged on the loudest level since the onset: a snare slightly behind the kick
            // arrives after the click that triggered the detection)
            let wmax = window_max(&d.pk_ring, at, n_end);
            let window_db = db10((wmax * wmax).max(level).max(ABS_FLOOR)) as f32;
            let bleed = d.class == OnsetClass::Snare
                && 10f32.powf((window_db - r) / 20.0) < SNARE_BLEED_STRENGTH
                && kick_ts.is_some_and(|k| (k as i64 - ts_on as i64).unsigned_abs() <= KICK_BLEED_NS);
            if accept && !bleed {
                d.reference = Some(if level_db > r { level_db } else { r + REF_ADAPT * (level_db - r) });
                d.env = d.env.max(strength);
                let class = d.class;
                self.events.push(AnalysisEvent::Onset { class, strength, ts: ts_on });
            } else {
                // rejected (bleed or far below the class level): it must not hold off the
                // real onset that may follow within the retrigger window
                (d.last_onset, d.last_ts) = d.prev;
            }
        }
    }

    /// Tap tempo (forwarded to the beat tracker).
    pub fn tap(&mut self, ts: u64) {
        if let Some(b) = &mut self.beat {
            b.tap(ts);
        }
    }

    pub fn clear_tap(&mut self) {
        if let Some(b) = &mut self.beat {
            b.clear_tap();
        }
    }

    pub fn beat(&self) -> Option<&BeatTracker> {
        self.beat.as_ref()
    }

    pub fn beat_mut(&mut self) -> Option<&mut BeatTracker> {
        self.beat.as_mut()
    }

    /// Features of the latest completed hop.
    pub fn last_features(&self) -> Option<&Features> {
        self.have_features.then_some(&self.features)
    }

    /// Clear all signal history (keeps configuration and the beat range). Does not allocate.
    pub fn reset(&mut self) {
        self.n = 0;
        self.in_hop = 0;
        self.hops = 0;
        self.peak = 0.0;
        self.lvl_ms = 0.0;
        for k in &mut self.kw {
            k[0].reset();
            k[1].reset();
        }
        self.kw_sum = 0.0;
        self.loud.reset();
        self.thirds.reset();
        self.third_ms = [0.0; 32];
        self.env_bank.reset();
        self.env1 = [0.0; LANES];
        self.env2 = [0.0; LANES];
        for d in &mut self.det {
            d.reset();
        }
        self.odf_prev = [0.0; 6];
        self.odf_ref = 0.0;
        self.spectral.reset();
        self.slow_count = 0;
        self.novelty.reset();
        self.nov_value = 0.0;
        self.drop.reset();
        self.hype.reset();
        if let Some(b) = &mut self.beat {
            b.reset();
        }
        self.features = Features::default();
        self.have_features = false;
    }
}
