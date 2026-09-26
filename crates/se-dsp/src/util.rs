//! Shared DSP building blocks for the effects and the sampler.
//!
//! Everything here allocates only in constructors; the per-sample methods are allocation-free,
//! lock-free, and flush decaying state to exact zero so silence never runs in denormal range.

use crate::Transport;
use std::f64::consts::PI;

/// Tempo divisions offered by every tempo-synced parameter (choice index = position).
/// `t` = triplet (×2/3), `d` = dotted (×1.5).
pub const DIVISIONS: &[&str] = &["1/1", "1/2", "1/4", "1/8", "1/16", "1/32", "1/2t", "1/4t", "1/8t", "1/16t", "1/4d", "1/8d", "1/16d"];

/// Length of division `idx` of [`DIVISIONS`] in beats (quarter note = 1 beat). Out-of-range
/// indices fall back to one beat.
pub fn division_beats(idx: usize) -> f64 {
    match idx {
        0 => 4.0,
        1 => 2.0,
        2 => 1.0,
        3 => 0.5,
        4 => 0.25,
        5 => 0.125,
        6 => 4.0 / 3.0,
        7 => 2.0 / 3.0,
        8 => 1.0 / 3.0,
        9 => 1.0 / 6.0,
        10 => 1.5,
        11 => 0.75,
        12 => 0.375,
        _ => 1.0,
    }
}

/// Length of division `idx` in samples at the transport's tempo.
pub fn division_samples(idx: usize, t: &Transport, sr: f32) -> f64 {
    division_beats(idx) * t.samples_per_beat(sr)
}

/// Milliseconds to samples.
#[inline]
pub fn ms_to_samples(ms: f32, sr: f32) -> f32 {
    ms * 0.001 * sr
}

/// One-pole coefficient for a time constant of `ms` (0 → instant).
#[inline]
pub fn coef_ms(ms: f32, sr: f32) -> f32 {
    let n = ms_to_samples(ms, sr);
    if n <= 1e-3 { 1.0 } else { 1.0 - (-1.0 / n).exp() }
}

/// One-pole coefficient for a cutoff of `hz`.
#[inline]
pub fn coef_hz(hz: f32, sr: f32) -> f32 {
    1.0 - (-2.0 * std::f32::consts::PI * hz / sr).exp()
}

/// Flush values below ~6e-26 to exactly 0 (branch-free: `(x + c) - c` with a tiny normal `c`).
/// Exact for every |x| ≥ 2e-11, i.e. inaudible elsewhere.
#[inline]
pub fn flush(x: f32) -> f32 {
    const C: f32 = 1e-18;
    (x + C) - C
}

/// `f64` variant of [`flush`].
#[inline]
pub fn flush64(x: f64) -> f64 {
    const C: f64 = 1e-30;
    (x + C) - C
}

/// Rational tanh approximation (Lambert continued fraction; max error ≈ 1e-4 near |x| = 5,
/// < 2e-6 for |x| ≤ 3; clamped to ±1 beyond 4.97).
#[inline]
pub fn fast_tanh(x: f32) -> f32 {
    let x = x.clamp(-4.97, 4.97);
    let x2 = x * x;
    let n = x * (135135.0 + x2 * (17325.0 + x2 * (378.0 + x2)));
    let d = 135135.0 + x2 * (62370.0 + x2 * (3150.0 + 28.0 * x2));
    (n / d).clamp(-1.0, 1.0)
}

/// Cubic smoothstep of `t` clamped to 0–1 (zero slope at both ends; used for click-free fades).
#[inline]
pub fn smoothstep(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// xorshift32 PRNG (deterministic, allocation-free).
#[derive(Clone, Copy, Debug)]
pub struct Rng(u32);

impl Rng {
    pub fn new(seed: u32) -> Rng {
        Rng(if seed == 0 { 0x9E37_79B9 } else { seed })
    }

    #[inline]
    pub fn next_u32(&mut self) -> u32 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 17;
        x ^= x << 5;
        self.0 = x;
        x
    }

    /// Uniform in [0, 1).
    #[inline]
    pub fn next_f32(&mut self) -> f32 {
        (self.next_u32() >> 8) as f32 * (1.0 / 16_777_216.0)
    }

    /// Uniform in [-1, 1).
    #[inline]
    pub fn bipolar(&mut self) -> f32 {
        self.next_f32() * 2.0 - 1.0
    }

    /// Uniform integer in `0..n` (`n > 0`).
    #[inline]
    pub fn below(&mut self, n: usize) -> usize {
        ((self.next_u32() as u64 * n as u64) >> 32) as usize
    }
}

/// Mono delay line (power-of-two ring) with integer taps and 4-point Hermite reads.
pub struct DelayLine {
    buf: Vec<f32>,
    mask: usize,
    w: usize,
}

impl DelayLine {
    /// Holds at least `max_delay` samples of history (plus interpolation headroom).
    pub fn new(max_delay: usize) -> DelayLine {
        let cap = (max_delay + 4).next_power_of_two();
        DelayLine { buf: vec![0.0; cap], mask: cap - 1, w: 0 }
    }

    /// Largest delay `read`/`tap` can reach.
    pub fn max_delay(&self) -> usize {
        self.mask - 3
    }

    pub fn clear(&mut self) {
        self.buf.fill(0.0);
    }

    /// Append one sample (it becomes `tap(0)`).
    #[inline]
    pub fn push(&mut self, x: f32) {
        self.w = (self.w + 1) & self.mask;
        self.buf[self.w] = x;
    }

    /// Sample written `d` pushes ago (`d = 0` → newest).
    #[inline]
    pub fn tap(&self, d: usize) -> f32 {
        self.buf[self.w.wrapping_sub(d) & self.mask]
    }

    /// Fractional read `d` samples back: linear between the two newest samples for `d < 1`,
    /// 4-point Hermite otherwise (both agree at `d = 1`, so gliding across it is continuous).
    #[inline]
    pub fn read(&self, d: f32) -> f32 {
        let d = d.clamp(0.0, self.max_delay() as f32);
        if d < 1.0 {
            let a = self.tap(0);
            return a + (self.tap(1) - a) * d;
        }
        let i = d as usize;
        let f = d - i as f32;
        let xm1 = self.tap(i - 1);
        let x0 = self.tap(i);
        let x1 = self.tap(i + 1);
        let x2 = self.tap(i + 2);
        hermite(xm1, x0, x1, x2, f)
    }
}

/// 4-point, 3rd-order Hermite interpolation between `x0` (t = 0) and `x1` (t = 1).
#[inline]
pub fn hermite(xm1: f32, x0: f32, x1: f32, x2: f32, t: f32) -> f32 {
    let c1 = 0.5 * (x1 - xm1);
    let c2 = xm1 - 2.5 * x0 + 2.0 * x1 - 0.5 * x2;
    let c3 = 0.5 * (x2 - xm1) + 1.5 * (x0 - x1);
    ((c3 * t + c2) * t + c1) * t + x0
}

/// Normalized biquad coefficients (a0 = 1), designed with the RBJ Audio EQ Cookbook formulas.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BiquadCoefs {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
}

impl BiquadCoefs {
    pub const IDENTITY: BiquadCoefs = BiquadCoefs { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0 };

    fn w0(f: f32, sr: f32) -> (f64, f64, f64) {
        let f = (f as f64).clamp(1.0, 0.49 * sr as f64);
        let w = 2.0 * PI * f / sr as f64;
        (w, w.cos(), w.sin())
    }

    fn norm(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> BiquadCoefs {
        BiquadCoefs { b0: b0 / a0, b1: b1 / a0, b2: b2 / a0, a1: a1 / a0, a2: a2 / a0 }
    }

    pub fn lowpass(f: f32, q: f32, sr: f32) -> BiquadCoefs {
        let (_, c, s) = Self::w0(f, sr);
        let al = s / (2.0 * q as f64);
        Self::norm((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al)
    }

    pub fn highpass(f: f32, q: f32, sr: f32) -> BiquadCoefs {
        let (_, c, s) = Self::w0(f, sr);
        let al = s / (2.0 * q as f64);
        Self::norm((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al)
    }

    /// Band-pass with 0 dB peak gain.
    pub fn bandpass(f: f32, q: f32, sr: f32) -> BiquadCoefs {
        let (_, c, s) = Self::w0(f, sr);
        let al = s / (2.0 * q as f64);
        Self::norm(al, 0.0, -al, 1.0 + al, -2.0 * c, 1.0 - al)
    }

    pub fn notch(f: f32, q: f32, sr: f32) -> BiquadCoefs {
        let (_, c, s) = Self::w0(f, sr);
        let al = s / (2.0 * q as f64);
        Self::norm(1.0, -2.0 * c, 1.0, 1.0 + al, -2.0 * c, 1.0 - al)
    }

    pub fn peaking(f: f32, q: f32, gain_db: f32, sr: f32) -> BiquadCoefs {
        let (_, c, s) = Self::w0(f, sr);
        let a = 10f64.powf(gain_db as f64 / 40.0);
        let al = s / (2.0 * q as f64);
        Self::norm(1.0 + al * a, -2.0 * c, 1.0 - al * a, 1.0 + al / a, -2.0 * c, 1.0 - al / a)
    }

    pub fn low_shelf(f: f32, q: f32, gain_db: f32, sr: f32) -> BiquadCoefs {
        let (_, c, s) = Self::w0(f, sr);
        let a = 10f64.powf(gain_db as f64 / 40.0);
        let al = s / (2.0 * q as f64);
        let k = 2.0 * a.sqrt() * al;
        Self::norm(
            a * ((a + 1.0) - (a - 1.0) * c + k),
            2.0 * a * ((a - 1.0) - (a + 1.0) * c),
            a * ((a + 1.0) - (a - 1.0) * c - k),
            (a + 1.0) + (a - 1.0) * c + k,
            -2.0 * ((a - 1.0) + (a + 1.0) * c),
            (a + 1.0) + (a - 1.0) * c - k,
        )
    }

    pub fn high_shelf(f: f32, q: f32, gain_db: f32, sr: f32) -> BiquadCoefs {
        let (_, c, s) = Self::w0(f, sr);
        let a = 10f64.powf(gain_db as f64 / 40.0);
        let al = s / (2.0 * q as f64);
        let k = 2.0 * a.sqrt() * al;
        Self::norm(
            a * ((a + 1.0) + (a - 1.0) * c + k),
            -2.0 * a * ((a - 1.0) + (a + 1.0) * c),
            a * ((a + 1.0) + (a - 1.0) * c - k),
            (a + 1.0) - (a - 1.0) * c + k,
            2.0 * ((a - 1.0) - (a + 1.0) * c),
            (a + 1.0) - (a - 1.0) * c - k,
        )
    }
}

/// Mono biquad, transposed direct form II in `f64` (keeps low-frequency shelves precise).
#[derive(Clone, Copy, Debug)]
pub struct Biquad {
    pub c: BiquadCoefs,
    s1: f64,
    s2: f64,
}

impl Default for Biquad {
    fn default() -> Self {
        Biquad { c: BiquadCoefs::IDENTITY, s1: 0.0, s2: 0.0 }
    }
}

impl Biquad {
    pub fn new(c: BiquadCoefs) -> Biquad {
        Biquad { c, s1: 0.0, s2: 0.0 }
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let x = x as f64;
        let c = &self.c;
        let y = c.b0 * x + self.s1;
        self.s1 = flush64(c.b1 * x - c.a1 * y + self.s2);
        self.s2 = flush64(c.b2 * x - c.a2 * y);
        // a decaying f64 tail can still round to an f32 subnormal
        flush(y as f32)
    }

    pub fn reset(&mut self) {
        self.s1 = 0.0;
        self.s2 = 0.0;
    }
}

/// One-pole low-pass `y += a·(x − y)`; `x − lp(x)` is the matching high-pass.
#[derive(Clone, Copy, Debug, Default)]
pub struct OnePole {
    pub a: f32,
    pub y: f32,
}

impl OnePole {
    pub fn new(hz: f32, sr: f32) -> OnePole {
        OnePole { a: coef_hz(hz, sr), y: 0.0 }
    }

    pub fn set_hz(&mut self, hz: f32, sr: f32) {
        self.a = coef_hz(hz, sr);
    }

    #[inline]
    pub fn lp(&mut self, x: f32) -> f32 {
        self.y = flush(self.y + self.a * (x - self.y));
        self.y
    }

    #[inline]
    pub fn hp(&mut self, x: f32) -> f32 {
        x - self.lp(x)
    }

    pub fn reset(&mut self) {
        self.y = 0.0;
    }
}

/// Attack/release envelope follower on a rectified (≥ 0) input.
#[derive(Clone, Copy, Debug)]
pub struct EnvFollower {
    att: f32,
    rel: f32,
    pub env: f32,
}

impl EnvFollower {
    pub fn new(attack_ms: f32, release_ms: f32, sr: f32) -> EnvFollower {
        EnvFollower { att: coef_ms(attack_ms, sr), rel: coef_ms(release_ms, sr), env: 0.0 }
    }

    pub fn set_times(&mut self, attack_ms: f32, release_ms: f32, sr: f32) {
        self.att = coef_ms(attack_ms, sr);
        self.rel = coef_ms(release_ms, sr);
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let c = if x > self.env { self.att } else { self.rel };
        self.env = flush(self.env + c * (x - self.env));
        self.env
    }

    pub fn reset(&mut self) {
        self.env = 0.0;
    }
}

/// Center tap (and latency in base-rate samples) of the 2× halfband FIR.
pub const OS_LATENCY: usize = 15;
const OS_TAPS: usize = OS_LATENCY + 1;

/// Halfband coefficients of the 31-tap Kaiser-windowed (β = 7) 2× FIR: the 16 non-zero taps
/// at odd offsets −15, −13, …, 15 from the center (whose tap is 0.5), normalized so each
/// polyphase branch has unity DC gain.
fn halfband_taps() -> [f32; OS_TAPS] {
    fn i0(x: f64) -> f64 {
        let (mut sum, mut term, mut k) = (1.0, 1.0, 1.0);
        while term > 1e-12 * sum {
            term *= (x / (2.0 * k)) * (x / (2.0 * k));
            sum += term;
            k += 1.0;
        }
        sum
    }
    const BETA: f64 = 7.0;
    let half = (OS_LATENCY + 1) as f64;
    let mut h = [0f64; OS_TAPS];
    for (m, v) in h.iter_mut().enumerate() {
        let o = 2.0 * m as f64 - OS_LATENCY as f64;
        let t = o / 2.0;
        let sinc = (PI * t).sin() / (PI * t);
        let w = i0(BETA * (1.0 - (o / half) * (o / half)).sqrt()) / i0(BETA);
        *v = 0.5 * sinc * w;
    }
    let sum: f64 = h.iter().sum();
    h.map(|v| (v * 0.5 / sum) as f32)
}

/// 2× oversampler (mono): polyphase halfband up- and down-sampling around a nonlinearity.
/// Linear-phase, total latency [`OS_LATENCY`] base-rate samples.
pub struct Oversampler2x {
    h: [f32; OS_TAPS],
    x: [f32; 2 * OS_TAPS],
    xp: usize,
    e: [f32; 2 * OS_TAPS],
    ep: usize,
    o: [f32; OS_TAPS],
    op: usize,
}

impl Default for Oversampler2x {
    fn default() -> Self {
        Self::new()
    }
}

impl Oversampler2x {
    pub fn new() -> Oversampler2x {
        Oversampler2x { h: halfband_taps(), x: [0.0; 2 * OS_TAPS], xp: 0, e: [0.0; 2 * OS_TAPS], ep: 0, o: [0.0; OS_TAPS], op: 0 }
    }

    pub fn reset(&mut self) {
        self.x = [0.0; 2 * OS_TAPS];
        self.e = [0.0; 2 * OS_TAPS];
        self.o = [0.0; OS_TAPS];
    }

    #[inline]
    fn dot(h: &[f32; OS_TAPS], w: &[f32]) -> f32 {
        let mut acc = 0.0;
        for k in 0..OS_TAPS {
            acc += h[k] * w[k];
        }
        acc
    }

    /// Upsample one input sample into two (earlier, later) at the doubled rate.
    #[inline]
    pub fn up(&mut self, x: f32) -> (f32, f32) {
        self.xp = (self.xp + OS_TAPS - 1) % OS_TAPS;
        self.x[self.xp] = x;
        self.x[self.xp + OS_TAPS] = x;
        let w = &self.x[self.xp..self.xp + OS_TAPS];
        (2.0 * Self::dot(&self.h, w), w[OS_TAPS / 2 - 1])
    }

    /// Downsample two processed samples (earlier, later) into one output sample.
    #[inline]
    pub fn down(&mut self, a: f32, b: f32) -> f32 {
        self.ep = (self.ep + OS_TAPS - 1) % OS_TAPS;
        self.e[self.ep] = a;
        self.e[self.ep + OS_TAPS] = a;
        self.op = (self.op + 1) % OS_TAPS;
        self.o[self.op] = b;
        let odd = self.o[(self.op + OS_TAPS - OS_TAPS / 2) % OS_TAPS];
        Self::dot(&self.h, &self.e[self.ep..self.ep + OS_TAPS]) + 0.5 * odd
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fast_tanh_tracks_tanh() {
        let mut worst = 0f32;
        for i in -2000..=2000 {
            let x = i as f32 * 0.005;
            worst = worst.max((fast_tanh(x) - x.tanh()).abs());
        }
        assert!(worst < 1.5e-4, "max error {worst}");
    }

    #[test]
    fn divisions_cover_every_choice() {
        assert_eq!(DIVISIONS.len(), 13);
        let t = Transport { bpm: 120.0, beat: 0.0, beats_per_bar: 4 };
        assert_eq!(division_samples(3, &t, 48000.0), 12000.0); // 1/8 at 120 = 250 ms
        assert!((division_beats(7) * 3.0 - 2.0).abs() < 1e-12); // three 1/4t = one half note
        assert_eq!(division_beats(11), 0.75);
    }

    #[test]
    fn hermite_read_is_exact_on_integers_and_continuous_at_one() {
        let mut d = DelayLine::new(64);
        for i in 0..32 {
            d.push(i as f32 * i as f32);
        }
        assert_eq!(d.read(5.0), d.tap(5));
        let below = d.read(0.999_99);
        let above = d.read(1.000_01);
        assert!((below - above).abs() < 1e-2, "{below} vs {above}");
        // central-difference Hermite reproduces quadratics exactly
        assert!((d.read(3.5) - (31.0f32 - 3.5).powi(2)).abs() < 1e-3);
    }

    /// Frequency response of the composite halfband (center 0.5 + odd taps) at the 2× rate.
    fn halfband_db(f_norm: f64) -> f64 {
        let h = halfband_taps();
        let (mut re, mut im) = (0.5, 0.0);
        for (m, &c) in h.iter().enumerate() {
            let o = 2.0 * m as f64 - OS_LATENCY as f64;
            re += c as f64 * (2.0 * PI * f_norm * o).cos();
            im -= c as f64 * (2.0 * PI * f_norm * o).sin();
        }
        20.0 * (re * re + im * im).sqrt().log10()
    }

    #[test]
    fn halfband_passes_audio_band_and_rejects_images() {
        // normalized to the doubled rate: 0.17 → 16.3 kHz at 48 kHz, 0.33 → 31.7 kHz
        for f in [0.0, 0.05, 0.1, 0.15, 0.17] {
            assert!(halfband_db(f).abs() < 0.05, "passband {f}: {} dB", halfband_db(f));
        }
        for f in [0.33, 0.37, 0.42, 0.5] {
            assert!(halfband_db(f) < -65.0, "stopband {f}: {} dB", halfband_db(f));
        }
    }

    #[test]
    fn oversampler_round_trip_is_a_pure_delay_in_band() {
        let mut os = Oversampler2x::new();
        let w = 2.0 * std::f32::consts::PI * 1000.0 / 48000.0;
        let mut worst = 0f32;
        for n in 0..4000 {
            let (a, b) = os.up((w * n as f32).sin());
            let y = os.down(a, b);
            if n > 200 {
                let expect = (w * (n - OS_LATENCY) as f32).sin();
                worst = worst.max((y - expect).abs());
            }
        }
        assert!(worst < 1e-3, "worst {worst}");
    }
}
