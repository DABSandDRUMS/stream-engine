//! SMPTE 12M linear timecode (LTC): 80-bit frames sent LSB-first as biphase-mark audio.
//!
//! Frame layout (bit 0 first): frame units 0–3, user 1 4–7, frame tens 8–9, drop-frame 10,
//! color frame 11, user 2 12–15, second units 16–19, user 3 20–23, second tens 24–26,
//! flag 27, user 4 28–31, minute units 32–35, user 5 36–39, minute tens 40–42, flag 43,
//! user 6 44–47, hour units 48–51, user 7 52–55, hour tens 56–57, BGF1 58, flag 59,
//! user 8 60–63, sync word 64–79 = `0011 1111 1111 1101`.
//!
//! Flags: at 25 fps bit 27 = BGF0, 43 = BGF2, 59 = polarity correction; at 24/29.97/30 bit
//! 27 = polarity correction, 43 = BGF0, 59 = BGF2. The polarity bit makes the number of
//! ones (and so zeros) in the frame even, so every frame starts with the same edge polarity.
//!
//! Biphase mark: every bit cell starts with a transition; a `1` has another one mid-cell.

use super::{FrameRate, Timecode};

/// Sync word bits 64–79 in transmission order.
pub const SYNC: [u8; 16] = [0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0, 1];
const SYNC_MASK_FWD: u128 = 0xFFFF << 64;
const SYNC_FWD: u128 = sync_value(false);
const SYNC_MASK_REV: u128 = 0xFFFF;
const SYNC_REV: u128 = sync_value(true);

const fn sync_value(reverse: bool) -> u128 {
    let mut v = 0u128;
    let mut i = 0;
    while i < 16 {
        if SYNC[i] == 1 {
            v |= if reverse { 1u128 << (15 - i) } else { 1u128 << (64 + i) };
        }
        i += 1;
    }
    v
}

/// One decoded or to-be-encoded LTC frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct LtcFrame {
    pub tc: Timecode,
    /// User bits: nibble `i` (0 = bits 4–7 … 7 = bits 60–63) in bits `4i..4i+4`.
    pub user_bits: u32,
    pub color_frame: bool,
    /// Binary group flags BGF0, BGF1, BGF2 in bits 0–2.
    pub bgf: u8,
}

fn put(bits: &mut u128, pos: u32, len: u32, v: u32) {
    for i in 0..len {
        if (v >> i) & 1 == 1 {
            *bits |= 1u128 << (pos + i);
        }
    }
}

fn get(bits: u128, pos: u32, len: u32) -> u32 {
    ((bits >> pos) as u32) & ((1u32 << len) - 1)
}

const USER_POS: [u32; 8] = [4, 12, 20, 28, 36, 44, 52, 60];

impl LtcFrame {
    pub fn new(tc: Timecode) -> Self {
        LtcFrame { tc, ..Default::default() }
    }

    /// The 80 bits in transmission order (bit `i` of the result = bit `i` sent).
    pub fn to_bits(&self) -> u128 {
        let tc = &self.tc;
        let mut b = 0u128;
        put(&mut b, 0, 4, (tc.frames % 10) as u32);
        put(&mut b, 8, 2, (tc.frames / 10) as u32);
        put(&mut b, 10, 1, tc.rate.drop_frame() as u32);
        put(&mut b, 11, 1, self.color_frame as u32);
        put(&mut b, 16, 4, (tc.seconds % 10) as u32);
        put(&mut b, 24, 3, (tc.seconds / 10) as u32);
        put(&mut b, 32, 4, (tc.minutes % 10) as u32);
        put(&mut b, 40, 3, (tc.minutes / 10) as u32);
        put(&mut b, 48, 4, (tc.hours % 10) as u32);
        put(&mut b, 56, 2, (tc.hours / 10) as u32);
        for (i, p) in USER_POS.iter().enumerate() {
            put(&mut b, *p, 4, (self.user_bits >> (4 * i)) & 0xF);
        }
        let (bgf0, bgf2, parity) = flag_positions(tc.rate);
        put(&mut b, bgf0, 1, (self.bgf & 1) as u32);
        put(&mut b, 58, 1, ((self.bgf >> 1) & 1) as u32);
        put(&mut b, bgf2, 1, ((self.bgf >> 2) & 1) as u32);
        b |= SYNC_FWD;
        if b.count_ones() % 2 == 1 {
            b |= 1u128 << parity;
        }
        b
    }

    /// Decode 80 bits in transmission order. `rate` decides the flag layout and frame-count
    /// base for non-drop-frame codes (a set drop-frame bit always means 29.97 DF).
    pub fn from_bits(b: u128, rate: FrameRate) -> Option<LtcFrame> {
        if b & SYNC_MASK_FWD != SYNC_FWD {
            return None;
        }
        let df = get(b, 10, 1) == 1;
        let rate = if df {
            FrameRate::Fps2997Df
        } else if rate.drop_frame() {
            FrameRate::Fps30
        } else {
            rate
        };
        let (fu, ft, su, st, mu, mt, hu, ht) =
            (get(b, 0, 4), get(b, 8, 2), get(b, 16, 4), get(b, 24, 3), get(b, 32, 4), get(b, 40, 3), get(b, 48, 4), get(b, 56, 2));
        if fu > 9 || su > 9 || mu > 9 || hu > 9 {
            return None;
        }
        let tc = Timecode { frames: (ft * 10 + fu) as u8, seconds: (st * 10 + su) as u8, minutes: (mt * 10 + mu) as u8, hours: (ht * 10 + hu) as u8, rate };
        // frame labels beyond the rate's base are validated by the caller's rate detection
        if tc.hours >= 24 || tc.minutes >= 60 || tc.seconds >= 60 || tc.frames >= 30 {
            return None;
        }
        let mut user = 0u32;
        for (i, p) in USER_POS.iter().enumerate() {
            user |= get(b, *p, 4) << (4 * i);
        }
        let (bgf0, bgf2, _) = flag_positions(rate);
        let bgf = (get(b, bgf0, 1) | (get(b, 58, 1) << 1) | (get(b, bgf2, 1) << 2)) as u8;
        Some(LtcFrame { tc, user_bits: user, color_frame: get(b, 11, 1) == 1, bgf })
    }
}

/// (BGF0 bit, BGF2 bit, polarity-correction bit) for a rate.
fn flag_positions(rate: FrameRate) -> (u32, u32, u32) {
    if rate == FrameRate::Fps25 { (27, 43, 59) } else { (43, 59, 27) }
}

/// Biphase-mark LTC audio encoder with a controlled rise time (SMPTE 12M: 40 ± 10 µs).
/// Frames are laid out on a continuous fractional-sample time line, so non-integer
/// samples-per-frame (29.97 fps, varispeed) never accumulate drift.
#[derive(Clone, Debug)]
pub struct LtcEncoder {
    sample_rate: f64,
    /// Peak output level (linear, 0–1).
    pub amplitude: f32,
    rise: f64,
    level: f32,
    prev: f32,
    last_edge: f64,
    /// Start time (samples) of the next frame.
    t: f64,
    /// Next output sample index.
    n: u64,
    edges: Vec<f64>,
}

impl LtcEncoder {
    pub fn new(sample_rate: u32, amplitude: f32) -> Self {
        let sr = sample_rate as f64;
        LtcEncoder {
            sample_rate: sr,
            amplitude,
            // a linear ramp of 50 µs has a 10–90 % rise time of 40 µs
            rise: (50e-6 * sr).max(0.5),
            level: 1.0,
            prev: 1.0,
            last_edge: f64::NEG_INFINITY,
            t: 0.0,
            n: 0,
            edges: Vec::with_capacity(160),
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate as u32
    }

    /// Samples written so far.
    pub fn samples_written(&self) -> u64 {
        self.n
    }

    /// Samples one frame occupies at `fps` real frames per second (speed included).
    pub fn samples_per_frame(&self, fps: f64) -> f64 {
        self.sample_rate / fps
    }

    /// Append one frame played at `fps` frames per second (e.g. `rate.fps() * speed`).
    pub fn encode_frame(&mut self, frame: &LtcFrame, fps: f64, out: &mut Vec<f32>) {
        let half = self.sample_rate / (fps * 160.0);
        if self.t < self.n as f64 - 1.0 {
            self.t = self.n as f64;
        }
        let bits = frame.to_bits();
        self.edges.clear();
        let mut t = self.t;
        for i in 0..80 {
            self.edges.push(t);
            if (bits >> i) & 1 == 1 {
                self.edges.push(t + half);
            }
            t += 2.0 * half;
        }
        let end = t;
        let mut e = 0;
        while (self.n as f64) < end {
            let x = self.n as f64;
            while e < self.edges.len() && self.edges[e] <= x {
                self.prev = self.level;
                self.level = -self.level;
                self.last_edge = self.edges[e];
                e += 1;
            }
            let since = x - self.last_edge;
            let v = if since < self.rise { self.prev + (self.level - self.prev) * (since / self.rise) as f32 } else { self.level };
            out.push(v * self.amplitude);
            self.n += 1;
        }
        self.t = end;
    }

    /// Append `n` samples of silence (source stopped); the next frame starts after it.
    pub fn silence(&mut self, n: usize, out: &mut Vec<f32>) {
        out.extend(std::iter::repeat_n(0.0, n));
        self.n += n as u64;
        self.t = self.n as f64;
        self.prev = self.level;
        self.last_edge = f64::NEG_INFINITY;
    }
}

/// A frame found by the decoder.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LtcDecoded {
    pub frame: LtcFrame,
    /// Sample position (from stream start, fractional) of the first received bit edge.
    pub start: f64,
    /// Sample position of the edge that ended the last received bit.
    pub end: f64,
    /// Received back to front (tape/transport running backwards).
    pub reverse: bool,
}

impl LtcDecoded {
    /// Sample at which the source was exactly at `frame.tc` (the frame's bit-0 edge).
    pub fn position_sample(&self) -> f64 {
        if self.reverse { self.end } else { self.start }
    }
    /// Playback speed relative to the frame rate (1.0 = nominal, negative = reverse).
    pub fn speed(&self, sample_rate: u32) -> f64 {
        let dur = (self.end - self.start).max(1.0);
        let s = sample_rate as f64 / dur / self.frame.tc.rate.fps();
        if self.reverse { -s } else { s }
    }
}

/// Streaming LTC decoder: DC-blocked, hysteresis zero-crossing detection with sub-sample
/// edge timing, adaptive bit-period tracking (follows ±10 % varispeed and beyond), sync
/// word search in both directions, and frame-rate detection from frame-number wraps.
#[derive(Clone, Debug)]
pub struct LtcDecoder {
    sr: f64,
    /// Rate assumed until the stream reveals its own (DF flag or a frame-number wrap).
    pub rate_hint: FrameRate,
    detected: Option<FrameRate>,
    n: u64,
    dc: f32,
    dc_coef: f32,
    env: f32,
    env_decay: f32,
    /// Minimum hysteresis threshold (≈ −50 dBFS): quieter signals are treated as silence.
    pub floor: f32,
    high: bool,
    prev_v: f32,
    last_zero: f64,
    last_edge: Option<f64>,
    period: f64,
    half: Option<(f64, f64)>,
    too_long: u32,
    too_short: u32,
    reg: u128,
    starts: [f64; 80],
    nbits: u32,
    /// The pending half cell already completed a frame (its final `1`); swallow the bit.
    spec: bool,
    last: Option<LtcFrame>,
}

impl LtcDecoder {
    pub fn new(sample_rate: u32, rate_hint: FrameRate) -> Self {
        let sr = sample_rate as f64;
        LtcDecoder {
            sr,
            rate_hint,
            detected: None,
            n: 0,
            dc: 0.0,
            dc_coef: (1.0 / (0.05 * sr)) as f32,
            env: 0.0,
            env_decay: (-1.0 / (0.2 * sr)).exp() as f32,
            floor: 0.003,
            high: false,
            prev_v: 0.0,
            last_zero: 0.0,
            last_edge: None,
            // centre of 21.6–33 fps (24 fps −10 % … 30 fps +10 %)
            period: sr / (80.0 * 26.7),
            half: None,
            too_long: 0,
            too_short: 0,
            reg: 0,
            starts: [0.0; 80],
            nbits: 0,
            spec: false,
            last: None,
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sr as u32
    }

    /// Samples consumed so far (the stream position of the next sample).
    pub fn position(&self) -> u64 {
        self.n
    }

    /// The frame rate in use (detected, else the hint).
    pub fn rate(&self) -> FrameRate {
        self.detected.unwrap_or(self.rate_hint)
    }

    /// Current bit-period estimate in samples.
    pub fn bit_period(&self) -> f64 {
        self.period
    }

    /// Feed mono samples; `out` receives every decoded frame.
    pub fn feed(&mut self, samples: &[f32], mut out: impl FnMut(LtcDecoded)) {
        for &x in samples {
            let n = self.n as f64;
            self.dc += (x - self.dc) * self.dc_coef;
            let v = x - self.dc;
            let a = v.abs();
            self.env = if a > self.env { a } else { self.env * self.env_decay };
            let th = (self.env * 0.25).max(self.floor);
            if (v >= 0.0) != (self.prev_v >= 0.0) {
                let frac = (self.prev_v / (self.prev_v - v)) as f64;
                self.last_zero = n - 1.0 + frac.clamp(0.0, 1.0);
            }
            if self.high && v < -th {
                self.high = false;
                let t = self.last_zero;
                self.edge(t, &mut out);
            } else if !self.high && v > th {
                self.high = true;
                let t = self.last_zero;
                self.edge(t, &mut out);
            }
            self.prev_v = v;
            self.n += 1;
        }
    }

    fn desync(&mut self) {
        self.half = None;
        self.spec = false;
        self.nbits = 0;
    }

    fn edge(&mut self, t: f64, out: &mut impl FnMut(LtcDecoded)) {
        let Some(prev) = self.last_edge.replace(t) else { return };
        let d = t - prev;
        let p = self.period;
        if d > 1.75 * p {
            // silence/dropout, or the period estimate is far too short: re-acquire after a few
            self.desync();
            self.too_long += 1;
            if self.too_long >= 6 {
                self.period = d;
                self.too_long = 0;
            }
            return;
        }
        if d < 0.3 * p {
            self.desync();
            self.too_short += 1;
            if self.too_short >= 6 {
                self.period = 2.0 * d;
                self.too_short = 0;
            }
            return;
        }
        self.too_long = 0;
        self.too_short = 0;
        if d > 0.75 * p {
            if self.half.take().is_some() {
                // a lone half cell before a full one: we were out of phase
                self.nbits = 0;
                self.spec = false;
            }
            self.period = 0.75 * p + 0.25 * d;
            self.push_bit(false, prev, t, out);
        } else if let Some((start, d0)) = self.half.take() {
            self.period = 0.75 * p + 0.25 * (d0 + d);
            if std::mem::take(&mut self.spec) {
                return;
            }
            self.push_bit(true, start, t, out);
        } else {
            self.half = Some((prev, d));
            // The last bit of a forward frame is always a `1`: its first half is enough to
            // complete the frame (so the final frame before a stop is not lost).
            if self.nbits == 79 && ((self.reg >> 1) | (1u128 << 79)) & SYNC_MASK_FWD == SYNC_FWD {
                self.push_bit(true, prev, t + d, out);
                self.spec = self.nbits == 0;
            }
        }
    }

    fn push_bit(&mut self, bit: bool, start: f64, end: f64, out: &mut impl FnMut(LtcDecoded)) {
        self.reg = (self.reg >> 1) | ((bit as u128) << 79);
        self.starts.copy_within(1..80, 0);
        self.starts[79] = start;
        self.nbits = (self.nbits + 1).min(80);
        if self.nbits < 80 {
            return;
        }
        let (bits, reverse) = if self.reg & SYNC_MASK_FWD == SYNC_FWD {
            (self.reg, false)
        } else if self.reg & SYNC_MASK_REV == SYNC_REV {
            (self.reg.reverse_bits() >> 48, true)
        } else {
            return;
        };
        let Some(mut frame) = LtcFrame::from_bits(bits, self.rate()) else { return };
        self.nbits = 0;
        self.learn_rate(&mut frame, reverse);
        out(LtcDecoded { frame, start: self.starts[0], end, reverse });
    }

    /// Frame-rate detection: a set DF bit means 29.97 DF; otherwise the frame number just
    /// before a wrap to 0 reveals the base (23 → 24, 24 → 25, 29 → 30), and any frame number
    /// beyond the current base raises it.
    fn learn_rate(&mut self, f: &mut LtcFrame, reverse: bool) {
        if f.tc.rate.drop_frame() {
            self.detected = Some(FrameRate::Fps2997Df);
        } else {
            let mut rate = self.rate();
            if rate.drop_frame() {
                rate = FrameRate::Fps30;
            }
            if f.tc.frames as u32 >= rate.nominal() {
                rate = if f.tc.frames >= 25 { FrameRate::Fps30 } else { FrameRate::Fps25 };
            }
            if let Some(l) = self.last {
                let (before, after) = if reverse { (f.tc, l.tc) } else { (l.tc, f.tc) };
                if after.frames == 0 && before.frames >= 23 && (after.seconds as u32 == (before.seconds as u32 + 1) % 60) {
                    rate = match before.frames {
                        23 => FrameRate::Fps24,
                        24 => FrameRate::Fps25,
                        _ => FrameRate::Fps30,
                    };
                }
            }
            self.detected = Some(rate);
        }
        f.tc.rate = self.rate();
        self.last = Some(*f);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bits_roundtrip_with_even_parity_all_rates() {
        for r in FrameRate::ALL {
            for f in [0u64, 1, 1799, 17_982, 107_891, 1_000_000] {
                let tc = Timecode::from_frames(f, r);
                let fr = LtcFrame { tc, user_bits: 0xA5C3_0F81, color_frame: f % 2 == 0, bgf: (f % 8) as u8 };
                let b = fr.to_bits();
                assert_eq!(b.count_ones() % 2, 0, "{r} {tc}: even parity");
                assert_eq!(b >> 80, 0);
                assert_eq!(LtcFrame::from_bits(b, r), Some(fr), "{r} {tc}");
            }
        }
    }

    #[test]
    fn drop_frame_bit_is_bit_10() {
        let tc = Timecode::parse("00:01:00;02", FrameRate::Fps2997Df).unwrap();
        let b = LtcFrame::new(tc).to_bits();
        assert_eq!((b >> 10) & 1, 1);
        let back = LtcFrame::from_bits(b, FrameRate::Fps30).unwrap();
        assert_eq!(back.tc.rate, FrameRate::Fps2997Df);
        assert_eq!(back.tc.to_string(), "00:01:00;02");
    }

    fn render(frames: &[LtcFrame], sr: u32, fps: f64) -> Vec<f32> {
        let mut e = LtcEncoder::new(sr, 0.5);
        let mut out = Vec::new();
        for f in frames {
            e.encode_frame(f, fps, &mut out);
        }
        out
    }

    fn run(r: FrameRate, sr: u32, speed: f64, count: u64) -> (Vec<LtcDecoded>, Vec<LtcFrame>) {
        let start = Timecode::from_frames(107_800, r);
        let frames: Vec<LtcFrame> = (0..count).map(|i| LtcFrame::new(Timecode::from_frames(start.to_frames() + i, r))).collect();
        let audio = render(&frames, sr, r.fps() * speed);
        let mut d = LtcDecoder::new(sr, FrameRate::Fps25);
        let mut got = Vec::new();
        d.feed(&audio, |f| got.push(f));
        (got, frames)
    }

    #[test]
    fn audio_roundtrip_every_rate_and_speed() {
        for r in FrameRate::ALL {
            for speed in [0.9, 1.0, 1.1] {
                for sr in [44_100, 48_000, 96_000] {
                    let (got, sent) = run(r, sr, speed, 120);
                    assert!(got.len() >= 117, "{r} x{speed} @{sr}: {} of {}", got.len(), sent.len());
                    // labels are consecutive at the true rate (frames decoded before the rate
                    // was learned carry the hint's rate)
                    let at = |d: &LtcDecoded| Timecode { rate: r, ..d.frame.tc }.to_frames();
                    for w in got.windows(2) {
                        assert_eq!(at(&w[1]), at(&w[0]) + 1, "{r} x{speed} @{sr}");
                    }
                    let last = got.last().unwrap();
                    assert_eq!(last.frame.tc.rate, r, "rate detected");
                    assert_eq!(last.frame.tc, sent.last().unwrap().tc);
                    // edge timing: frame k starts at k * samples-per-frame
                    let spf = sr as f64 / (r.fps() * speed);
                    let k = (last.frame.tc.to_frames() - sent[0].tc.to_frames()) as f64;
                    // zero crossings sit half a rise time after the edge
                    let tol = 25e-6 * sr as f64 + 1.0;
                    assert!((last.start - k * spf).abs() < tol, "{r} x{speed} @{sr}: {} vs {}", last.start, k * spf);
                    assert!((last.speed(sr) - speed).abs() < 0.01);
                }
            }
        }
    }

    #[test]
    fn reverse_playback_decodes() {
        let r = FrameRate::Fps25;
        let frames: Vec<LtcFrame> = (0..40).map(|i| LtcFrame::new(Timecode::from_frames(5000 + i, r))).collect();
        let mut audio = render(&frames, 48_000, 25.0);
        audio.reverse();
        let mut d = LtcDecoder::new(48_000, r);
        let mut got = Vec::new();
        d.feed(&audio, |f| got.push(f));
        assert!(got.len() >= 38, "{}", got.len());
        assert!(got.iter().all(|g| g.reverse));
        for w in got.windows(2) {
            assert_eq!(w[1].frame.tc.to_frames() + 1, w[0].frame.tc.to_frames());
        }
        assert!(got[5].speed(48_000) < -0.99);
    }

    #[test]
    fn survives_noise_dc_offset_inversion_and_dropouts() {
        let r = FrameRate::Fps30;
        let frames: Vec<LtcFrame> = (0..90).map(|i| LtcFrame::new(Timecode::from_frames(i, r))).collect();
        let mut audio = render(&frames, 48_000, 30.0);
        // inverted polarity, DC offset, deterministic noise, 100 ms dropout in the middle
        let mut s = 1u32;
        for (i, x) in audio.iter_mut().enumerate() {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let noise = ((s >> 8) as f32 / (1u32 << 24) as f32 - 0.5) * 0.04;
            *x = -*x * 0.3 + 0.05 + noise;
            if (72_000..76_800).contains(&i) {
                *x = 0.05 + noise * 0.1;
            }
        }
        let mut d = LtcDecoder::new(48_000, r);
        let mut got = Vec::new();
        d.feed(&audio, |f| got.push(f.frame.tc.frames as u64 + 30 * f.frame.tc.seconds as u64));
        assert!(got.len() >= 80, "{}", got.len());
        assert!(got.windows(2).all(|w| w[1] > w[0]), "monotonic, gap only at the dropout");
        assert!(got.contains(&89));
    }

    #[test]
    fn silence_then_resume_keeps_decoding() {
        let r = FrameRate::Fps24;
        let mut e = LtcEncoder::new(48_000, 0.5);
        let mut audio = Vec::new();
        for i in 0..24 {
            e.encode_frame(&LtcFrame::new(Timecode::from_frames(i, r)), 24.0, &mut audio);
        }
        e.silence(24_000, &mut audio);
        for i in 100..148 {
            e.encode_frame(&LtcFrame::new(Timecode::from_frames(i, r)), 24.0, &mut audio);
        }
        let mut d = LtcDecoder::new(48_000, r);
        let mut got = Vec::new();
        d.feed(&audio, |f| got.push(f.frame.tc.to_frames()));
        assert!(got.contains(&23) && got.contains(&147), "{got:?}");
    }
}
