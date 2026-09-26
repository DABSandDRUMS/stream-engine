//! 24 kHz → 48 kHz: 2× polyphase windowed-sinc interpolator (Kaiser, 128 taps).
//!
//! Cutoff 11 kHz with β = 8 (≈ 80 dB stopband, ≈ 1.9 kHz transition): every image of the
//! 0–12 kHz band (landing in 12–24 kHz) falls in the stopband. Each phase is normalized to
//! unity DC gain. Streaming with a fixed history; no allocation after construction.

const TAPS: usize = 128;
const PHASE_TAPS: usize = TAPS / 2;
const CUTOFF_HZ: f64 = 11_000.0;
const OUT_RATE: f64 = 48_000.0;
const BETA: f64 = 8.0;

pub struct Upsampler2x {
    phases: [[f32; PHASE_TAPS]; 2],
    /// History, stored twice so the newest `PHASE_TAPS` inputs are one contiguous slice.
    hist: [f32; 2 * PHASE_TAPS],
    pos: usize,
}

impl Default for Upsampler2x {
    fn default() -> Self {
        Self::new()
    }
}

impl Upsampler2x {
    /// Zero inputs to push after the last sample to drain the filter's tail.
    pub const FLUSH_INPUTS: usize = PHASE_TAPS / 2;

    pub fn new() -> Self {
        let m = (TAPS - 1) as f64;
        let fc = CUTOFF_HZ / OUT_RATE;
        let i0_beta = bessel_i0(BETA);
        let mut proto = [0f64; TAPS];
        for (n, h) in proto.iter_mut().enumerate() {
            let t = n as f64 - m / 2.0;
            let x = 2.0 * fc * t;
            let sinc = if x.abs() < 1e-12 { 1.0 } else { (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x) };
            let r = 2.0 * n as f64 / m - 1.0;
            let w = bessel_i0(BETA * (1.0 - r * r).max(0.0).sqrt()) / i0_beta;
            *h = 2.0 * fc * sinc * w;
        }
        let mut phases = [[0f32; PHASE_TAPS]; 2];
        for (p, phase) in phases.iter_mut().enumerate() {
            let sum: f64 = (0..PHASE_TAPS).map(|k| proto[2 * k + p]).sum();
            for (k, tap) in phase.iter_mut().enumerate() {
                *tap = (proto[2 * k + p] / sum) as f32;
            }
        }
        Upsampler2x { phases, hist: [0.0; 2 * PHASE_TAPS], pos: 0 }
    }

    pub fn reset(&mut self) {
        self.hist = [0.0; 2 * PHASE_TAPS];
        self.pos = 0;
    }

    /// One 24 kHz sample in, two 48 kHz samples out.
    #[inline]
    pub fn push(&mut self, x: f32) -> [f32; 2] {
        self.pos = (self.pos + PHASE_TAPS - 1) % PHASE_TAPS;
        self.hist[self.pos] = x;
        self.hist[self.pos + PHASE_TAPS] = x;
        let h = &self.hist[self.pos..self.pos + PHASE_TAPS];
        [dot(&self.phases[0], h), dot(&self.phases[1], h)]
    }

    /// `out.len()` must be `2 * input.len()`.
    pub fn process(&mut self, input: &[f32], out: &mut [f32]) {
        assert_eq!(out.len(), input.len() * 2, "output must hold two samples per input");
        for (x, o) in input.iter().zip(out.as_chunks_mut::<2>().0) {
            *o = self.push(*x);
        }
    }

    /// Whole-buffer convenience (offline tools): output aligned to the input (filter delay
    /// removed) and exactly twice as long.
    pub fn upsample(input: &[f32]) -> Vec<f32> {
        let mut r = Upsampler2x::new();
        let mut out = Vec::with_capacity(input.len() * 2 + TAPS);
        for &x in input.iter().chain(std::iter::repeat_n(&0.0, Self::FLUSH_INPUTS)) {
            out.extend_from_slice(&r.push(x));
        }
        let delay = Self::delay();
        out.drain(..delay);
        out.truncate(input.len() * 2);
        out
    }

    /// Group delay in output samples (integer part; the filter is linear phase).
    pub const fn delay() -> usize {
        (TAPS - 1) / 2
    }
}

#[inline]
fn dot(a: &[f32; PHASE_TAPS], b: &[f32]) -> f32 {
    let mut acc = [0f32; 4];
    for (ca, cb) in a.as_chunks::<4>().0.iter().zip(b.as_chunks::<4>().0) {
        for i in 0..4 {
            acc[i] += ca[i] * cb[i];
        }
    }
    (acc[0] + acc[1]) + (acc[2] + acc[3])
}

fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let q = x * x / 4.0;
    for k in 1..64 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-17 {
            break;
        }
    }
    sum
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(freq: f64, rate: f64, n: usize, amp: f32) -> Vec<f32> {
        (0..n).map(|i| amp * (2.0 * std::f64::consts::PI * freq * i as f64 / rate).sin() as f32).collect()
    }

    fn rms(x: &[f32]) -> f64 {
        (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len() as f64).sqrt()
    }

    /// Magnitude of one frequency (Goertzel), normalized to amplitude.
    fn tone(x: &[f32], freq: f64, rate: f64) -> f64 {
        let w = 2.0 * std::f64::consts::PI * freq / rate;
        let (mut s1, mut s2) = (0.0f64, 0.0f64);
        for &v in x {
            let s = v as f64 + 2.0 * w.cos() * s1 - s2;
            s2 = s1;
            s1 = s;
        }
        (s1 * s1 + s2 * s2 - 2.0 * w.cos() * s1 * s2).sqrt() * 2.0 / x.len() as f64
    }

    #[test]
    fn length_and_streaming_equivalence() {
        let x = sine(440.0, 24_000.0, 1000, 0.5);
        let mut a = Upsampler2x::new();
        let mut whole = vec![0.0; 2000];
        a.process(&x, &mut whole);
        // the same signal pushed in odd-sized blocks gives identical output
        let mut b = Upsampler2x::new();
        let mut pieces = Vec::new();
        for block in x.chunks(37) {
            let mut o = vec![0.0; block.len() * 2];
            b.process(block, &mut o);
            pieces.extend(o);
        }
        assert_eq!(whole, pieces);
        assert_eq!(Upsampler2x::upsample(&x).len(), 2000);
        assert!(Upsampler2x::upsample(&[]).is_empty());
    }

    #[test]
    fn dc_passes_at_unity() {
        let mut r = Upsampler2x::new();
        let mut out = vec![0.0; 1000];
        r.process(&[0.25; 500], &mut out);
        for v in &out[TAPS..] {
            assert!((v - 0.25).abs() < 1e-5, "{v}");
        }
    }

    #[test]
    fn preserves_passband_energy() {
        for f in [100.0, 1_000.0, 4_000.0, 8_000.0] {
            let x = sine(f, 24_000.0, 24_000, 0.8);
            let y = Upsampler2x::upsample(&x);
            let (rx, ry) = (rms(&x[1000..23_000]), rms(&y[2000..46_000]));
            assert!((ry / rx - 1.0).abs() < 0.005, "{f} Hz: in {rx} out {ry}");
        }
    }

    #[test]
    fn rejects_images() {
        // a 9 kHz tone's image lands at 15 kHz; 11.5 kHz → 12.5 kHz
        for (f, image) in [(9_000.0, 15_000.0), (11_500.0, 12_500.0), (3_000.0, 21_000.0)] {
            let x = sine(f, 24_000.0, 24_000, 0.8);
            let y = Upsampler2x::upsample(&x);
            let img = tone(&y[2000..46_000], image, 48_000.0);
            let db = 20.0 * (img / 0.8).log10();
            assert!(db < -60.0, "{f} Hz image at {image} Hz: {db:.1} dB");
        }
    }

    #[test]
    fn linear_phase_half_sample_delay() {
        // after removing the integer delay, output n sits at input time (n - 0.5) / 48 kHz: the
        // interpolated samples follow the underlying continuous sine
        let (f, amp) = (500.0, 0.9);
        let x = sine(f, 24_000.0, 4000, amp);
        let y = Upsampler2x::upsample(&x);
        let err =
            (400..7600).map(|n| (y[n] as f64 - amp as f64 * (2.0 * std::f64::consts::PI * f * (n as f64 - 0.5) / 48_000.0).sin()).abs()).fold(0.0f64, f64::max);
        assert!(err < 1e-3, "max deviation {err}");
    }
}
