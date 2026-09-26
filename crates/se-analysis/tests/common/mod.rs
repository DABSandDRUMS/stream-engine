//! Synthesised test material (PLAN §23): drums, bass, pads, noise, sines.
#![allow(dead_code)]

use std::f64::consts::PI;

pub const SR: f32 = 48000.0;

/// xorshift64* — deterministic noise.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in [0, 1).
    pub fn uniform(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in [-1, 1).
    pub fn bipolar(&mut self) -> f32 {
        (self.uniform() * 2.0 - 1.0) as f32
    }
}

/// RBJ biquad for shaping test material (independent of the crate's filters).
#[derive(Clone, Copy)]
pub struct Bq {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Bq {
    fn from(b0: f64, b1: f64, b2: f64, a0: f64, a1: f64, a2: f64) -> Self {
        Bq { b: [b0 / a0, b1 / a0, b2 / a0], a: [a1 / a0, a2 / a0], z: [0.0; 2] }
    }

    pub fn hp(f: f64, q: f64) -> Self {
        let w = 2.0 * PI * f / SR as f64;
        let (s, c) = w.sin_cos();
        let al = s / (2.0 * q);
        Bq::from((1.0 + c) / 2.0, -(1.0 + c), (1.0 + c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al)
    }

    pub fn lp(f: f64, q: f64) -> Self {
        let w = 2.0 * PI * f / SR as f64;
        let (s, c) = w.sin_cos();
        let al = s / (2.0 * q);
        Bq::from((1.0 - c) / 2.0, 1.0 - c, (1.0 - c) / 2.0, 1.0 + al, -2.0 * c, 1.0 - al)
    }

    pub fn bp(f: f64, q: f64) -> Self {
        let w = 2.0 * PI * f / SR as f64;
        let (s, c) = w.sin_cos();
        let al = s / (2.0 * q);
        Bq::from(al, 0.0, -al, 1.0 + al, -2.0 * c, 1.0 - al)
    }

    pub fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Drum {
    Kick,
    Snare,
    Hat,
}

#[derive(Clone, Copy, Debug)]
pub struct Hit {
    /// True onset, seconds.
    pub t: f64,
    pub drum: Drum,
    /// Linear amplitude factor.
    pub vel: f32,
}

fn add(buf: &mut [f32], start: usize, v: impl Iterator<Item = f32>) {
    for (i, x) in v.enumerate() {
        if let Some(b) = buf.get_mut(start + i) {
            *b += x;
        }
    }
}

/// Pitched-down sine kick: 155 → 45 Hz, 250 ms decay, onset at `start` exactly.
pub fn kick(buf: &mut [f32], start: usize, amp: f32) {
    let n = (0.45 * SR as f64) as usize;
    let mut ph = 0.0f64;
    add(
        buf,
        start,
        (0..n).map(|i| {
            let t = i as f64 / SR as f64;
            let f = 45.0 + 110.0 * (-t / 0.03).exp();
            ph += 2.0 * PI * f / SR as f64;
            let env = (-t / 0.18).exp() * (t / 0.0005).min(1.0);
            (amp as f64 * env * ph.sin()) as f32
        }),
    );
}

/// Snare: 190 Hz tone + band-passed noise.
pub fn snare(buf: &mut [f32], start: usize, amp: f32, rng: &mut Rng) {
    let n = (0.3 * SR as f64) as usize;
    let mut bp = Bq::bp(2500.0, 0.6);
    let mut lp = Bq::lp(7000.0, 0.7);
    add(
        buf,
        start,
        (0..n).map(|i| {
            let t = i as f64 / SR as f64;
            let tone = 0.45 * (2.0 * PI * 190.0 * t).sin() * (-t / 0.07).exp();
            let noise = lp.run(bp.run(rng.bipolar() as f64)) * 1.6 * (-t / 0.08).exp();
            let atk = (t / 0.0005).min(1.0);
            (amp as f64 * atk * (tone + noise)) as f32
        }),
    );
}

/// Closed hi-hat: 4th-order high-passed noise, 30 ms decay.
pub fn hat(buf: &mut [f32], start: usize, amp: f32, rng: &mut Rng) {
    let n = (0.12 * SR as f64) as usize;
    let mut h1 = Bq::hp(7500.0, 0.54);
    let mut h2 = Bq::hp(7500.0, 1.31);
    add(
        buf,
        start,
        (0..n).map(|i| {
            let t = i as f64 / SR as f64;
            let v = h2.run(h1.run(rng.bipolar() as f64));
            (amp as f64 * 0.9 * v * (-t / 0.03).exp()) as f32
        }),
    );
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pattern {
    /// Kick 1 & 3, snare 2 & 4, hats on 8ths.
    Rock,
    /// Kick on every beat, snare 2 & 4, hats on 8ths.
    Four,
    /// Kick 1 & "3-and", snare 2 & 4, hats on 8ths (drum & bass two-step).
    TwoStep,
    /// Kick 1, snare 3, hats on 8ths (half-time feel).
    HalfTime,
}

pub struct Song {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    pub hits: Vec<Hit>,
    pub bpm: f64,
    /// True beat times (unhumanised grid), seconds.
    pub beats: Vec<f64>,
}

impl Song {
    pub fn scaled(&self, g: f32) -> (Vec<f32>, Vec<f32>) {
        (self.left.iter().map(|x| x * g).collect(), self.right.iter().map(|x| x * g).collect())
    }
}

/// Drum groove + bass line + pad at `bpm` for `secs`, humanised ±5 ms and ±2 dB.
pub fn groove(bpm: f64, secs: f64, pattern: Pattern, seed: u64) -> Song {
    groove_with(bpm, secs, pattern, seed, true)
}

pub fn groove_with(bpm: f64, secs: f64, pattern: Pattern, seed: u64, music: bool) -> Song {
    let n = (secs * SR as f64) as usize;
    let mut drums = vec![0f32; n];
    let mut rng = Rng::new(seed);
    let beat = 60.0 / bpm;
    let mut hits = Vec::new();
    let mut beats = Vec::new();
    let eighths = (secs / (beat / 2.0)).ceil() as usize;
    for e in 0..eighths {
        let grid = e as f64 * beat / 2.0;
        if e % 2 == 0 {
            beats.push(grid);
        }
        let pos = e % 8; // 8th position in the bar
        let (k, s) = match pattern {
            Pattern::Rock => (pos == 0 || pos == 4, pos == 2 || pos == 6),
            Pattern::Four => (pos % 2 == 0, pos == 2 || pos == 6),
            Pattern::TwoStep => (pos == 0 || pos == 5, pos == 2 || pos == 6),
            Pattern::HalfTime => (pos == 0, pos == 4),
        };
        let mut place = |drum: Drum, level: f32, rng: &mut Rng| {
            let t = grid + rng.bipolar() as f64 * 0.005;
            let vel = level * 10f32.powf(rng.bipolar() * 2.0 / 20.0);
            if t >= 0.0 && t < secs - 0.05 {
                hits.push(Hit { t, drum, vel });
            }
        };
        if k {
            place(Drum::Kick, 0.8, &mut rng);
        }
        if s {
            place(Drum::Snare, 0.55, &mut rng);
        }
        place(Drum::Hat, if pos % 2 == 0 { 0.22 } else { 0.16 }, &mut rng);
    }
    for h in &hits {
        let start = (h.t * SR as f64).round() as usize;
        match h.drum {
            Drum::Kick => kick(&mut drums, start, h.vel),
            Drum::Snare => snare(&mut drums, start, h.vel, &mut rng),
            Drum::Hat => hat(&mut drums, start, h.vel, &mut rng),
        }
    }
    let (mut left, mut right) = (drums.clone(), drums);
    if music {
        let bass = bass_line(bpm, n);
        let pad = pad(bpm, n, &[[57, 60, 64], [53, 57, 60], [48, 52, 55], [55, 59, 62]]);
        for i in 0..n {
            left[i] += bass[i] + pad[i];
            right[i] += bass[i] + 0.8 * pad[i];
        }
    }
    hits.sort_by(|a, b| a.t.total_cmp(&b.t));
    Song { left, right, hits, bpm, beats }
}

pub fn midi_hz(m: i32) -> f64 {
    440.0 * 2f64.powf((m - 69) as f64 / 12.0)
}

/// Legato sub bass, one note per beat (A1 C2 D2 E2 …), 10 ms glide-free crossfades.
pub fn bass_line(bpm: f64, n: usize) -> Vec<f32> {
    let notes = [33, 36, 38, 40, 33, 31, 36, 38];
    let beat = 60.0 / bpm;
    let mut ph = 0.0f64;
    (0..n)
        .map(|i| {
            let t = i as f64 / SR as f64;
            let b = (t / beat) as usize;
            let f = midi_hz(notes[b % notes.len()]);
            ph += 2.0 * PI * f / SR as f64;
            let within = t - b as f64 * beat;
            let edge = (within / 0.01).min((beat - within) / 0.01).clamp(0.3, 1.0);
            (0.12 * edge * (ph.sin() + 0.3 * (2.0 * ph).sin())) as f32
        })
        .collect()
}

/// Soft chord pad, chord changes every bar, 150 ms attack/release.
pub fn pad(bpm: f64, n: usize, chords: &[[i32; 3]]) -> Vec<f32> {
    let bar = 4.0 * 60.0 / bpm;
    let mut phases = [[0f64; 4]; 3];
    (0..n)
        .map(|i| {
            let t = i as f64 / SR as f64;
            let b = (t / bar) as usize;
            let chord = chords[b % chords.len()];
            let within = t - b as f64 * bar;
            let env = (within / 0.15).min((bar - within) / 0.15).clamp(0.0, 1.0);
            let mut v = 0.0;
            for (k, &m) in chord.iter().enumerate() {
                let f = midi_hz(m);
                for (h, ph) in phases[k].iter_mut().enumerate() {
                    *ph += 2.0 * PI * f * (h + 1) as f64 / SR as f64;
                    v += ph.sin() / (h + 1) as f64;
                }
            }
            (0.02 * env * v) as f32
        })
        .collect()
}

pub fn sine(freq: f64, amp: f64, secs: f64, sr: f32) -> Vec<f32> {
    let n = (secs * sr as f64) as usize;
    (0..n).map(|i| (amp * (2.0 * PI * freq * i as f64 / sr as f64).sin()) as f32).collect()
}

/// Pink noise (Paul Kellet's refined filter), unit-ish RMS before scaling.
pub fn pink(n: usize, rng: &mut Rng) -> Vec<f32> {
    let mut b = [0f64; 7];
    (0..n)
        .map(|_| {
            let w = rng.bipolar() as f64;
            b[0] = 0.99886 * b[0] + w * 0.0555179;
            b[1] = 0.99332 * b[1] + w * 0.0750759;
            b[2] = 0.96900 * b[2] + w * 0.1538520;
            b[3] = 0.86650 * b[3] + w * 0.3104856;
            b[4] = 0.55000 * b[4] + w * 0.5329522;
            b[5] = -0.7616 * b[5] - w * 0.0168980;
            let out = b.iter().sum::<f64>() + w * 0.5362;
            b[6] = w * 0.115926;
            (out * 0.2) as f32
        })
        .collect()
}

pub fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|&v| v as f64 * v as f64).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

/// Feed the analyzer in fixed blocks; returns every hop's features with its events.
pub fn run_live(
    an: &mut se_analysis::LiveAnalyzer,
    l: &[f32],
    r: &[f32],
    block: usize,
    t0_ns: u64,
) -> Vec<(se_analysis::Features, Vec<se_analysis::AnalysisEvent>)> {
    let sr = an.config().sample_rate as f64;
    let mut out = Vec::new();
    let mut i = 0;
    while i < l.len() {
        let j = (i + block).min(l.len());
        let ts = t0_ns + (i as f64 * 1e9 / sr).round() as u64;
        an.process(&l[i..j], &r[i..j], ts, &mut |f, ev| out.push((f.clone(), ev.to_vec())));
        i = j;
    }
    out
}

pub fn ns(s: f64) -> u64 {
    (s * 1e9).round() as u64
}

pub fn secs(ns: u64) -> f64 {
    ns as f64 * 1e-9
}

/// 16-bit PCM WAV writer (interleaves the given channels).
pub fn write_wav(path: &std::path::Path, sr: u32, channels: &[&[f32]]) -> std::io::Result<()> {
    let nch = channels.len() as u16;
    let frames = channels.iter().map(|c| c.len()).min().unwrap_or(0);
    let data_len = frames as u32 * nch as u32 * 2;
    let mut b = Vec::with_capacity(44 + data_len as usize);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&nch.to_le_bytes());
    b.extend_from_slice(&sr.to_le_bytes());
    b.extend_from_slice(&(sr * nch as u32 * 2).to_le_bytes());
    b.extend_from_slice(&(nch * 2).to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..frames {
        for c in channels {
            let v = (c[i].clamp(-1.0, 1.0) * 32767.0).round() as i16;
            b.extend_from_slice(&v.to_le_bytes());
        }
    }
    std::fs::write(path, b)
}

/// Unique scratch directory under the system temp dir.
pub fn scratch_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!(
        "se-analysis-{tag}-{}-{}",
        std::process::id(),
        Rng::new(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64).next_u64() % 1_000_000
    ));
    std::fs::create_dir_all(&d).unwrap();
    d
}
