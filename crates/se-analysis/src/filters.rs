//! IIR building blocks. State is `f64` (the 20 Hz third-octave band has poles within 1e-3 of the
//! unit circle at 48 kHz, which `f32` cannot represent accurately) and is flushed to zero once per
//! hop so decaying tails never reach denormal range.

use rustfft::num_complex::Complex64;
use std::f64::consts::PI;

/// States below this magnitude are flushed to zero (far below any audible or measurable level).
pub(crate) const FLUSH: f64 = 1e-30;

#[inline]
pub(crate) fn flush(v: &mut f64) {
    if v.abs() < FLUSH {
        *v = 0.0;
    }
}

/// Coefficient of a one-pole smoother with time constant `tau_s` at `sr` (per-sample update
/// `y += a·(x − y)`).
pub(crate) fn one_pole(tau_s: f64, sr: f64) -> f64 {
    if tau_s <= 0.0 { 1.0 } else { 1.0 - (-1.0 / (tau_s * sr)).exp() }
}

/// Transposed direct form II biquad, `a0` normalised to 1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Biquad {
    pub b0: f64,
    pub b1: f64,
    pub b2: f64,
    pub a1: f64,
    pub a2: f64,
    z1: f64,
    z2: f64,
}

impl Biquad {
    pub const IDENTITY: Biquad = Biquad { b0: 1.0, b1: 0.0, b2: 0.0, a1: 0.0, a2: 0.0, z1: 0.0, z2: 0.0 };

    pub fn new(b0: f64, b1: f64, b2: f64, a1: f64, a2: f64) -> Self {
        Biquad { b0, b1, b2, a1, a2, z1: 0.0, z2: 0.0 }
    }

    /// Bilinear-transform (prewarped) second-order low-pass; `q = 1/√2` is Butterworth.
    pub fn lowpass(fc: f64, sr: f64, q: f64) -> Self {
        let (s, c) = (2.0 * PI * fc / sr).sin_cos();
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b0 = (1.0 - c) / 2.0 / a0;
        Biquad::new(b0, 2.0 * b0, b0, -2.0 * c / a0, (1.0 - alpha) / a0)
    }

    /// Bilinear-transform (prewarped) second-order high-pass; `q = 1/√2` is Butterworth.
    pub fn highpass(fc: f64, sr: f64, q: f64) -> Self {
        let (s, c) = (2.0 * PI * fc / sr).sin_cos();
        let alpha = s / (2.0 * q);
        let a0 = 1.0 + alpha;
        let b0 = (1.0 + c) / 2.0 / a0;
        Biquad::new(b0, -2.0 * b0, b0, -2.0 * c / a0, (1.0 - alpha) / a0)
    }

    #[inline]
    pub fn process(&mut self, x: f64) -> f64 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    pub fn flush(&mut self) {
        flush(&mut self.z1);
        flush(&mut self.z2);
    }

    /// Complex frequency response at `f` Hz.
    pub fn response(&self, f: f64, sr: f64) -> Complex64 {
        let w = 2.0 * PI * f / sr;
        let z1 = Complex64::from_polar(1.0, -w);
        let z2 = z1 * z1;
        (self.b0 + z1 * self.b1 + z2 * self.b2) / (1.0 + z1 * self.a1 + z2 * self.a2)
    }

    fn scale(&mut self, g: f64) {
        self.b0 *= g;
        self.b1 *= g;
        self.b2 *= g;
    }
}

/// Complex response of a cascade.
pub(crate) fn cascade_response(sections: &[Biquad], f: f64, sr: f64) -> Complex64 {
    sections.iter().fold(Complex64::new(1.0, 0.0), |acc, s| acc * s.response(f, sr))
}

/// Group delay of a cascade at `f` Hz, in seconds (numerical phase derivative).
pub(crate) fn cascade_group_delay(sections: &[Biquad], f: f64, sr: f64) -> f64 {
    let df = (f * 1e-3).max(0.01);
    let unwrap = |a: f64, b: f64| {
        let mut d = b - a;
        while d > PI {
            d -= 2.0 * PI;
        }
        while d < -PI {
            d += 2.0 * PI;
        }
        d
    };
    let mut total = 0.0;
    for s in sections {
        let p0 = s.response(f - df, sr).arg();
        let p1 = s.response(f + df, sr).arg();
        total += -unwrap(p0, p1) / (2.0 * PI * 2.0 * df);
    }
    total
}

/// Q values of the second-order sections of an `order`-th order (even) Butterworth filter.
pub(crate) fn butterworth_qs(order: usize) -> impl Iterator<Item = f64> {
    let n = order.max(2) / 2 * 2;
    (0..n / 2).map(move |k| 1.0 / (2.0 * (PI * (2 * k + 1) as f64 / (2 * n) as f64).cos()))
}

/// Upper frequency limit used for filter edges (bilinear prewarping breaks down near Nyquist).
pub(crate) fn max_edge(sr: f64) -> f64 {
    0.45 * sr
}

/// Butterworth band-pass of total order `2·n` (n second-order sections), unity gain at the
/// geometric centre, designed by the analog low-pass→band-pass transform and the prewarped
/// bilinear transform. Returns `None` when the band lies above the usable range at `sr`.
pub(crate) fn butter_bandpass(lo: f64, hi: f64, sr: f64, n: usize) -> Option<Vec<Biquad>> {
    let top = max_edge(sr);
    if lo >= top || lo <= 0.0 || hi <= lo {
        return None;
    }
    let hi = hi.min(top);
    let k = 2.0 * sr;
    let w1 = k * (PI * lo / sr).tan();
    let w2 = k * (PI * hi / sr).tan();
    let bw = w2 - w1;
    let w0sq = w1 * w2;
    let mut sections = Vec::with_capacity(n);
    let mut reals = Vec::new();
    for i in 0..n {
        let theta = PI * (2 * i + n + 1) as f64 / (2 * n) as f64;
        let pb = Complex64::from_polar(1.0, theta) * bw;
        let disc = (pb * pb - 4.0 * w0sq).sqrt();
        for s in [(pb + disc) * 0.5, (pb - disc) * 0.5] {
            let z = (k + s) / (k - s);
            if z.im > 1e-12 {
                sections.push(Biquad::new(1.0, 0.0, -1.0, -2.0 * z.re, z.norm_sqr()));
            } else if z.im.abs() <= 1e-12 {
                reals.push(z.re);
            }
        }
    }
    for pair in reals.chunks(2) {
        let (r1, r2) = (pair[0], pair.get(1).copied().unwrap_or(0.0));
        sections.push(Biquad::new(1.0, 0.0, -1.0, -(r1 + r2), r1 * r2));
    }
    let fc = (w0sq.sqrt() / k).atan() * sr / PI;
    let g = cascade_response(&sections, fc, sr).norm();
    let per = (1.0 / g).powf(1.0 / sections.len() as f64);
    for s in &mut sections {
        s.scale(per);
    }
    Some(sections)
}

/// Butterworth high-pass of `order` (even) followed by Butterworth low-pass of `order`, i.e. a
/// band with `6·order` dB/octave skirts on both sides. The low-pass is omitted when `hi` is at or
/// above the usable range.
pub(crate) fn butter_band_skirts(lo: f64, hi: f64, sr: f64, order: usize) -> Vec<Biquad> {
    let mut v = Vec::with_capacity(order);
    if lo > 0.0 {
        v.extend(butterworth_qs(order).map(|q| Biquad::highpass(lo.min(max_edge(sr)), sr, q)));
    }
    if hi < max_edge(sr) {
        v.extend(butterworth_qs(order).map(|q| Biquad::lowpass(hi, sr, q)));
    }
    v
}

/// BS.1770-4 K-weighting (stage 1 high shelf, stage 2 RLB high-pass) for any sample rate, using
/// the analog prototype parameters that reproduce the standard's 48 kHz coefficients.
pub(crate) fn k_weighting(sr: f64) -> [Biquad; 2] {
    let f0 = 1681.974450955533;
    let g = 3.999843853973347;
    let q = 0.7071752369554196;
    let k = (PI * f0 / sr).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad::new(
        (vh + vb * k / q + k * k) / a0,
        2.0 * (k * k - vh) / a0,
        (vh - vb * k / q + k * k) / a0,
        2.0 * (k * k - 1.0) / a0,
        (1.0 - k / q + k * k) / a0,
    );
    let f0 = 38.13547087602444;
    let q = 0.5003270373238773;
    let k = (PI * f0 / sr).tan();
    let a0 = 1.0 + k / q + k * k;
    let rlb = Biquad::new(1.0, -2.0, 1.0, 2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0);
    [shelf, rlb]
}

/// Structure-of-arrays bank of `L` independent lanes, each a cascade of `S` biquads (unused
/// sections are identity). The inner loops run across lanes so they vectorise.
#[derive(Clone, Debug)]
pub(crate) struct Bank<const L: usize, const S: usize> {
    b0: [[f64; L]; S],
    b1: [[f64; L]; S],
    b2: [[f64; L]; S],
    a1: [[f64; L]; S],
    a2: [[f64; L]; S],
    z1: [[f64; L]; S],
    z2: [[f64; L]; S],
}

impl<const L: usize, const S: usize> Bank<L, S> {
    pub fn new() -> Self {
        Bank { b0: [[1.0; L]; S], b1: [[0.0; L]; S], b2: [[0.0; L]; S], a1: [[0.0; L]; S], a2: [[0.0; L]; S], z1: [[0.0; L]; S], z2: [[0.0; L]; S] }
    }

    /// Install `sections` (at most `S`) on `lane`; an empty slice silences the lane.
    pub fn set_lane(&mut self, lane: usize, sections: &[Biquad]) {
        assert!(sections.len() <= S, "lane has more sections than the bank");
        for s in 0..S {
            let q = sections.get(s).copied().unwrap_or(Biquad::IDENTITY);
            self.b0[s][lane] = q.b0;
            self.b1[s][lane] = q.b1;
            self.b2[s][lane] = q.b2;
            self.a1[s][lane] = q.a1;
            self.a2[s][lane] = q.a2;
        }
        if sections.is_empty() {
            self.b0[0][lane] = 0.0;
        }
    }

    #[inline]
    pub fn process(&mut self, x: f64, out: &mut [f64; L]) {
        let mut v = [x; L];
        for s in 0..S {
            let (b0, b1, b2, a1, a2) = (&self.b0[s], &self.b1[s], &self.b2[s], &self.a1[s], &self.a2[s]);
            let (z1, z2) = (&mut self.z1[s], &mut self.z2[s]);
            for l in 0..L {
                let xin = v[l];
                let y = b0[l] * xin + z1[l];
                z1[l] = b1[l] * xin - a1[l] * y + z2[l];
                z2[l] = b2[l] * xin - a2[l] * y;
                v[l] = y;
            }
        }
        *out = v;
    }

    pub fn flush(&mut self) {
        for s in 0..S {
            for l in 0..L {
                flush(&mut self.z1[s][l]);
                flush(&mut self.z2[s][l]);
            }
        }
    }

    pub fn reset(&mut self) {
        self.z1 = [[0.0; L]; S];
        self.z2 = [[0.0; L]; S];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn k_weighting_matches_bs1770_48k_table() {
        let [shelf, rlb] = k_weighting(48000.0);
        let want = [1.53512485958697, -2.69169618940638, 1.19839281085285, -1.69065929318241, 0.73248077421585];
        let got = [shelf.b0, shelf.b1, shelf.b2, shelf.a1, shelf.a2];
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 1e-8, "shelf {got:?}");
        }
        let got = [rlb.b0, rlb.b1, rlb.b2, rlb.a1, rlb.a2];
        let want = [1.0, -2.0, 1.0, -1.99004745483398, 0.99007225036621];
        for (g, w) in got.iter().zip(want) {
            assert!((g - w).abs() < 1e-8, "rlb {got:?}");
        }
    }

    #[test]
    fn third_octave_bandpass_edges_are_3db_down() {
        let sr = 48000.0;
        for fc in [20.0, 63.0, 1000.0, 16000.0] {
            let (lo, hi) = (fc * 10f64.powf(-0.05), fc * 10f64.powf(0.05));
            let bp = butter_bandpass(lo, hi, sr, 3).unwrap();
            let db = |f: f64| 20.0 * cascade_response(&bp, f, sr).norm().log10();
            assert!(db(fc).abs() < 0.05, "centre {fc}: {}", db(fc));
            assert!((db(lo) + 3.01).abs() < 0.1, "lo edge {fc}: {}", db(lo));
            assert!((db(hi) + 3.01).abs() < 0.1, "hi edge {fc}: {}", db(hi));
            // 6th-order skirts: one octave away is far down.
            // (an octave above 16 kHz is past Nyquist at 48 kHz)
            assert!(db(fc / 2.0) < -30.0, "lower skirt {fc}");
            if fc * 2.0 < sr / 2.0 {
                assert!(db(fc * 2.0) < -30.0, "upper skirt {fc}");
            }
        }
        assert!(butter_bandpass(20000.0, 22000.0, 32000.0, 3).is_none());
    }

    #[test]
    fn bank_lanes_match_scalar_cascades() {
        let sr = 48000.0;
        let a = butter_band_skirts(100.0, 1000.0, sr, 4);
        let b = butter_bandpass(900.0, 1100.0, sr, 3).unwrap();
        let mut bank = Bank::<2, 4>::new();
        bank.set_lane(0, &a);
        bank.set_lane(1, &b);
        let (mut sa, mut sb) = (a.clone(), b.clone());
        let mut out = [0.0; 2];
        for i in 0..2000 {
            let x = ((i * 7919) % 97) as f64 / 48.5 - 1.0;
            bank.process(x, &mut out);
            let ya = sa.iter_mut().fold(x, |v, s| s.process(v));
            let yb = sb.iter_mut().fold(x, |v, s| s.process(v));
            assert!((out[0] - ya).abs() < 1e-12 && (out[1] - yb).abs() < 1e-12);
        }
    }
}
