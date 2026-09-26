//! Offline song analysis (PLAN §8.4): decode, beat grid, downbeats, sections, chorus candidates.
//!
//! * **Onset curve:** 1024-point (≈23 ms) Hann STFT at a 5 ms hop, 40-band mel spectrum in dB
//!   (80 dB range), half-wave rectified first difference summed over bands, normalised by its
//!   standard deviation. A second curve over the mel bands below 150 Hz is the low-frequency
//!   accent used for bar phase.
//! * **Tempo:** autocorrelation of the onset curve scored with a comb over four multiples of each
//!   candidate period (0.05 BPM grid, default 70–180 BPM), octave resolved by metrical evenness
//!   (fastest candidate whose alternate beats carry comparable onset energy), refined by a joint
//!   period/phase comb over the whole song.
//! * **Beats:** dynamic-programming beat tracking (Ellis 2007): local score = onset curve smoothed
//!   with a Gaussian of σ = period/32, transition cost `−100·ln²(Δ/period)`, backtracked from the
//!   last strong cumulative-score peak, weak leading/trailing beats trimmed. The reported tempo is
//!   the least-squares fit of the beat times.
//! * **Downbeats (4/4):** the bar phase maximising the low-frequency onset accent plus the
//!   harmonic change (chroma distance to the previous beat) on its beats.
//! * **Sections:** bar-synchronous features (chroma, 12 MFCC-like cepstral coefficients of the
//!   log-mel spectrum, loudness), standardised, cosine self-similarity; checkerboard novelty with
//!   a 4-bar Gaussian-tapered kernel; peaks ≥ 4 bars apart are boundaries (on downbeats by
//!   construction). Segments are labelled greedily (same letter when their block similarity is
//!   high relative to their internal similarity).
//! * **Chorus:** starts of the most repeated label among the high-energy labels (within 3 dB of
//!   the loudest label).

use anyhow::{Context, Result};
use realfft::RealFftPlanner;
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::Cursor;
use std::path::Path;
use symphonia::core::codecs::audio::AudioDecoderOptions;
use symphonia::core::errors::Error as SymError;
use symphonia::core::formats::probe::Hint;
use symphonia::core::formats::{FormatOptions, TrackType};
use symphonia::core::io::{MediaSource, MediaSourceStream};
use symphonia::core::meta::MetadataOptions;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Section {
    pub start: f64,
    pub end: f64,
    pub label: String,
}

/// All times in seconds.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SongAnalysis {
    pub media: String,
    pub duration: f64,
    pub bpm: f64,
    pub beats: Vec<f64>,
    pub downbeats: Vec<f64>,
    pub sections: Vec<Section>,
    pub chorus: Vec<f64>,
    /// Max |sample| of the mono mix per 10 ms bin, 0–1 (waveform overview).
    #[serde(default)]
    pub peaks: Vec<f32>,
}

/// Decoded audio at its native rate.
#[derive(Clone, Debug, PartialEq)]
pub struct Decoded {
    pub sample_rate: f32,
    /// Planar channels, f32 in −1..1.
    pub channels: Vec<Vec<f32>>,
}

impl Decoded {
    pub fn frames(&self) -> usize {
        self.channels.iter().map(Vec::len).min().unwrap_or(0)
    }

    pub fn duration(&self) -> f64 {
        if self.sample_rate > 0.0 { self.frames() as f64 / self.sample_rate as f64 } else { 0.0 }
    }

    /// Average of all channels.
    pub fn mono(&self) -> Vec<f32> {
        let n = self.frames();
        let k = self.channels.len().max(1) as f32;
        (0..n).map(|i| self.channels.iter().map(|c| c[i]).sum::<f32>() / k).collect()
    }
}

/// `"file:"` + the first 16 hex characters of the BLAKE3 hash of the file bytes.
pub fn media_id(bytes: &[u8]) -> String {
    let hex = blake3::hash(bytes).to_hex();
    format!("file:{}", &hex.as_str()[..16])
}

fn extension(path: &Path) -> Option<String> {
    path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase)
}

/// Decode any supported file (WAV, AIFF, FLAC, MP3, Ogg Vorbis, AAC/M4A, MKV/WebM audio, CAF,
/// ALAC) to planar f32 at its native sample rate.
pub fn decode_file(path: &Path) -> Result<Decoded> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    decode_source(Box::new(file), extension(path).as_deref()).with_context(|| format!("decode {}", path.display()))
}

fn decode_source(source: Box<dyn MediaSource>, ext: Option<&str>) -> Result<Decoded> {
    let mss = MediaSourceStream::new(source, Default::default());
    let mut hint = Hint::new();
    if let Some(e) = ext {
        hint.with_extension(e);
    }
    let mut format =
        symphonia::default::get_probe().probe(&hint, mss, FormatOptions::default(), MetadataOptions::default()).context("unsupported container")?;
    let track = format.default_track(TrackType::Audio).context("no audio track")?;
    let track_id = track.id;
    let params = track.codec_params.as_ref().and_then(|p| p.audio()).context("track has no audio codec parameters")?.clone();
    let mut decoder = symphonia::default::get_codecs().make_audio_decoder(&params, &AudioDecoderOptions::default()).context("unsupported codec")?;
    let mut rate = params.sample_rate;
    let mut channels: Vec<Vec<f32>> = Vec::new();
    let mut planes: Vec<Vec<f32>> = Vec::new();
    loop {
        let packet = match format.next_packet() {
            Ok(Some(p)) => p,
            Ok(None) | Err(SymError::ResetRequired) => break,
            Err(SymError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e).context("demux"),
        };
        if packet.track_id != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(buf) => {
                if rate.is_none() {
                    rate = Some(buf.spec().rate());
                }
                buf.copy_to_vecs_planar::<f32>(&mut planes);
                if channels.len() < planes.len() {
                    let have = channels.first().map_or(0, Vec::len);
                    channels.resize(planes.len(), vec![0.0; have]);
                }
                for (c, p) in channels.iter_mut().zip(&planes) {
                    c.extend_from_slice(p);
                }
            }
            Err(SymError::DecodeError(_)) | Err(SymError::IoError(_)) => continue,
            Err(e) => return Err(e).context("decode"),
        }
    }
    let rate = rate.context("unknown sample rate")?;
    anyhow::ensure!(!channels.is_empty(), "no audio decoded");
    Ok(Decoded { sample_rate: rate as f32, channels })
}

/// Decode, downmix and analyse a file; `media` = [`media_id`] of the file bytes.
pub fn analyze_file(path: &Path) -> Result<SongAnalysis> {
    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let media = media_id(&bytes);
    let decoded = decode_source(Box::new(Cursor::new(bytes)), extension(path).as_deref()).with_context(|| format!("decode {}", path.display()))?;
    let mut a = analyze_samples(&decoded.mono(), decoded.sample_rate);
    a.media = media;
    Ok(a)
}

// ---------------------------------------------------------------------------------------------

const N_MEL: usize = 40;
const MIN_BPM: f64 = 70.0;
const MAX_BPM: f64 = 180.0;
const TIGHTNESS: f64 = 100.0;
const KERNEL_BARS: usize = 4;
const MIN_SECTION_BARS: usize = 4;
/// Onset-curve peaks lead the true onset (the log-energy jump is largest while the onset enters
/// the window); measured on synthetic drums, corrected here.
const ODF_LEAD_S: f64 = 0.0;

fn mel(f: f64) -> f64 {
    2595.0 * (1.0 + f / 700.0).log10()
}

fn mel_inv(m: f64) -> f64 {
    700.0 * (10f64.powf(m / 2595.0) - 1.0)
}

/// Triangular mel filterbank over an `n_fft` spectrum: per band, (bin, weight) pairs.
fn mel_bank(n_fft: usize, sr: f64, fmin: f64, fmax: f64) -> Vec<Vec<(usize, f32)>> {
    let (m0, m1) = (mel(fmin), mel(fmax.min(sr / 2.0)));
    let pts: Vec<f64> = (0..N_MEL + 2).map(|i| mel_inv(m0 + (m1 - m0) * i as f64 / (N_MEL + 1) as f64)).collect();
    let df = sr / n_fft as f64;
    (0..N_MEL)
        .map(|b| {
            let (lo, c, hi) = (pts[b], pts[b + 1], pts[b + 2]);
            let mut w: Vec<(usize, f32)> = (1..n_fft / 2)
                .filter_map(|k| {
                    let f = k as f64 * df;
                    let v = if f > lo && f <= c {
                        (f - lo) / (c - lo)
                    } else if f > c && f < hi {
                        (hi - f) / (hi - c)
                    } else {
                        0.0
                    };
                    (v > 0.0).then_some((k, v as f32))
                })
                .collect();
            if w.is_empty() {
                w.push((((c / df).round() as usize).clamp(1, n_fft / 2 - 1), 1.0));
            }
            w
        })
        .collect()
}

fn pow2_near(x: f64) -> usize {
    1usize << (x.max(64.0).log2().round() as u32)
}

struct Frames {
    /// Frames per second of the onset curves.
    fr: f64,
    odf: Vec<f32>,
    odf_low: Vec<f32>,
    /// Timbre/harmony frames (every `slow` onset frames).
    slow: usize,
    chroma: Vec<[f32; 12]>,
    mfcc: Vec<[f32; 12]>,
    energy: Vec<f32>,
}

fn frames(mono: &[f32], sr: f64) -> Frames {
    let hop = (sr / 200.0).round().max(1.0) as usize;
    let n_fft = pow2_near(0.023 * sr);
    let n_slow = pow2_near(0.085 * sr);
    let slow = 4;
    let n = mono.len() / hop + 1;
    let mut planner = RealFftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(n_fft);
    let fft_slow = planner.plan_fft_forward(n_slow);
    let win: Vec<f32> = (0..n_fft).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n_fft as f64).cos()) as f32).collect();
    let win_slow: Vec<f32> = (0..n_slow).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n_slow as f64).cos()) as f32).collect();
    let bank = mel_bank(n_fft, sr, 30.0, 8000.0);
    let bank_slow = mel_bank(n_slow, sr, 30.0, 8000.0);
    let low_bands = (0..N_MEL).filter(|&b| {
        let (m0, m1) = (mel(30.0), mel(8000f64.min(sr / 2.0)));
        mel_inv(m0 + (m1 - m0) * (b + 1) as f64 / (N_MEL + 1) as f64) < 150.0
    });
    let low_bands: Vec<usize> = low_bands.collect();
    let chroma_map: Vec<(usize, usize)> = (1..n_slow / 2)
        .filter_map(|k| {
            let f = k as f64 * sr / n_slow as f64;
            (80.0..=5000.0).contains(&f).then(|| (k, ((12.0 * (f / 440.0).log2()).round() as i64 + 9).rem_euclid(12) as usize))
        })
        .collect();
    let (mut input, mut spec, mut scratch) = (fft.make_input_vec(), fft.make_output_vec(), fft.make_scratch_vec());
    let (mut input_s, mut spec_s, mut scratch_s) = (fft_slow.make_input_vec(), fft_slow.make_output_vec(), fft_slow.make_scratch_vec());
    let frame = |center: usize, win: &[f32], out: &mut [f32]| {
        let half = win.len() / 2;
        for (i, (o, w)) in out.iter_mut().zip(win).enumerate() {
            let s = (center + i).checked_sub(half);
            *o = s.and_then(|s| mono.get(s)).map_or(0.0, |x| x * w);
        }
    };
    let mut logmel = vec![[0f32; N_MEL]; n];
    let mut chroma = Vec::with_capacity(n / slow + 1);
    let mut slow_mel = Vec::with_capacity(n / slow + 1);
    let mut energy = Vec::with_capacity(n / slow + 1);
    for t in 0..n {
        frame(t * hop, &win, &mut input);
        let _ = fft.process_with_scratch(&mut input, &mut spec, &mut scratch);
        for (b, w) in bank.iter().enumerate() {
            let p: f32 = w.iter().map(|&(k, g)| spec[k].norm_sqr() * g).sum();
            logmel[t][b] = 10.0 * (p + 1e-10).log10();
        }
        if t % slow == 0 {
            frame(t * hop, &win_slow, &mut input_s);
            let _ = fft_slow.process_with_scratch(&mut input_s, &mut spec_s, &mut scratch_s);
            let mut c = [0f32; 12];
            for &(k, pc) in &chroma_map {
                c[pc] += spec_s[k].norm();
            }
            let norm = c.iter().map(|v| v * v).sum::<f32>().sqrt();
            if norm > 1e-9 {
                c.iter_mut().for_each(|v| *v /= norm);
            }
            chroma.push(c);
            let mut m = [0f32; N_MEL];
            for (b, w) in bank_slow.iter().enumerate() {
                m[b] = 10.0 * (w.iter().map(|&(k, g)| spec_s[k].norm_sqr() * g).sum::<f32>() + 1e-10).log10();
            }
            slow_mel.push(m);
            let e: f32 = spec_s.iter().map(|c| c.norm_sqr()).sum::<f32>() / (n_slow as f32 * n_slow as f32 * 0.375);
            energy.push(e);
        }
    }
    // 80 dB dynamic range below the loudest mel value.
    let top = logmel.iter().flat_map(|m| m.iter().copied()).fold(f32::MIN, f32::max);
    let floor = top - 80.0;
    for m in &mut logmel {
        m.iter_mut().for_each(|v| *v = v.max(floor));
    }
    let top_s = slow_mel.iter().flat_map(|m: &[f32; N_MEL]| m.iter().copied()).fold(f32::MIN, f32::max);
    let mut odf = vec![0f32; n];
    let mut odf_low = vec![0f32; n];
    for t in 1..n {
        odf[t] = (0..N_MEL).map(|b| (logmel[t][b] - logmel[t - 1][b]).max(0.0)).sum::<f32>() / N_MEL as f32;
        odf_low[t] = low_bands.iter().map(|&b| (logmel[t][b] - logmel[t - 1][b]).max(0.0)).sum::<f32>() / low_bands.len().max(1) as f32;
    }
    let mfcc = slow_mel
        .iter()
        .map(|m| {
            let mut c = [0f32; 12];
            for (i, ci) in c.iter_mut().enumerate() {
                let k = (i + 1) as f32;
                *ci = m
                    .iter()
                    .enumerate()
                    .map(|(b, v)| (v.max(top_s - 80.0) / 10.0) * (std::f32::consts::PI * k * (b as f32 + 0.5) / N_MEL as f32).cos())
                    .sum::<f32>();
            }
            c
        })
        .collect();
    Frames { fr: sr / hop as f64, odf, odf_low, slow, chroma, mfcc, energy }
}

fn std_normalise(v: &mut [f32]) {
    let n = v.len().max(1) as f64;
    let mean = v.iter().map(|&x| x as f64).sum::<f64>() / n;
    let var = v.iter().map(|&x| (x as f64 - mean).powi(2)).sum::<f64>() / n;
    let sd = var.sqrt().max(1e-9) as f32;
    v.iter_mut().for_each(|x| *x /= sd);
}

fn interp(v: &[f32], x: f64) -> f64 {
    if x < 0.0 {
        return 0.0;
    }
    let i = x.floor() as usize;
    if i + 1 >= v.len() {
        return if i < v.len() { v[i] as f64 } else { 0.0 };
    }
    let f = x - i as f64;
    v[i] as f64 * (1.0 - f) + v[i + 1] as f64 * f
}

/// Autocorrelation (biased, mean removed) up to `max_lag` via FFT.
fn autocorr(x: &[f32], max_lag: usize) -> Vec<f64> {
    let n = x.len();
    let mean = x.iter().map(|&v| v as f64).sum::<f64>() / n.max(1) as f64;
    let m = (2 * n).next_power_of_two().max(2);
    let mut planner = RealFftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(m);
    let inv = planner.plan_fft_inverse(m);
    let mut buf = fwd.make_input_vec();
    for (b, &v) in buf.iter_mut().zip(x) {
        *b = v as f64 - mean;
    }
    let mut spec = fwd.make_output_vec();
    let _ = fwd.process(&mut buf, &mut spec);
    for c in &mut spec {
        *c = rustfft::num_complex::Complex::new(c.norm_sqr(), 0.0);
    }
    let _ = inv.process(&mut spec, &mut buf);
    buf.truncate(max_lag.min(n.saturating_sub(1)) + 1);
    buf.iter_mut().for_each(|v| *v /= m as f64);
    buf
}

fn comb_salience(acf: &[f64], p: f64) -> f64 {
    let (mut s, mut k) = (0.0, 0);
    for m in 1..=4 {
        let lag = p * m as f64;
        let i = lag.floor() as usize;
        if i + 1 >= acf.len() {
            break;
        }
        let f = lag - i as f64;
        s += acf[i] * (1.0 - f) + acf[i + 1] * f;
        k += 1;
    }
    if k == 0 || acf[0] <= 0.0 { 0.0 } else { s / k as f64 / acf[0] }
}

/// Phase comb over the whole curve: (best phase, onset sums on even and odd beats, score).
fn phase_comb(odf: &[f32], p: f64) -> (f64, f64, f64, f64) {
    let steps = p.ceil() as usize;
    let (mut best, mut best_phi) = (f64::MIN, 0.0);
    for i in 0..steps {
        let phi = i as f64;
        let mut s = 0.0;
        let mut t = phi;
        while t < odf.len() as f64 {
            s += interp(odf, t);
            t += p;
        }
        if s > best {
            best = s;
            best_phi = phi;
        }
    }
    let (mut even, mut odd) = (0.0, 0.0);
    let mut t = best_phi;
    let mut j = 0;
    while t < odf.len() as f64 {
        if j % 2 == 0 {
            even += interp(odf, t)
        } else {
            odd += interp(odf, t)
        }
        t += p;
        j += 1;
    }
    (best_phi, even, odd, best)
}

/// Global beat period in frames.
fn global_period(odf: &[f32], fr: f64, lo: f64, hi: f64) -> Option<f64> {
    let max_lag = (4.0 * 60.0 * fr / lo).ceil() as usize + 2;
    let acf = autocorr(odf, max_lag);
    if acf.len() < 3 {
        return None;
    }
    let to_p = |bpm: f64| 60.0 * fr / bpm;
    let steps = ((hi - lo) / 0.05).round() as usize;
    let (mut best, mut best_i) = (f64::MIN, 0);
    for i in 0..=steps {
        let s = comb_salience(&acf, to_p(lo + i as f64 * 0.05));
        if s > best {
            best = s;
            best_i = i;
        }
    }
    if best <= 0.0 {
        return None;
    }
    let p_star = to_p(lo + best_i as f64 * 0.05);
    let (p_min, p_max) = (to_p(hi), to_p(lo));
    let mut chosen = p_star;
    for j in (-3..=3).rev() {
        let p = p_star * 2f64.powi(-j);
        if p < p_min * 0.999 || p > p_max * 1.001 || (j != 0 && comb_salience(&acf, p) < 0.2 * best) {
            continue;
        }
        let (_, even, odd, _) = phase_comb(odf, p);
        let hi_eo = even.max(odd);
        if hi_eo <= 0.0 || even.min(odd) / hi_eo >= 0.45 {
            chosen = p;
            break;
        }
    }
    // Joint period/phase refinement (±1 %, 0.02 % steps).
    let (mut best_s, mut best_p) = (f64::MIN, chosen);
    for i in -50..=50 {
        let p = chosen * (1.0 + i as f64 * 0.0002);
        let (_, _, _, s) = phase_comb(odf, p);
        if s > best_s {
            best_s = s;
            best_p = p;
        }
    }
    Some(best_p)
}

fn dp_beats(odf: &[f32], period: f64) -> Vec<usize> {
    let n = odf.len();
    if n == 0 {
        return Vec::new();
    }
    // Local score: Gaussian smoothing, σ = period/32, support ±period.
    let half = period.round() as i64;
    let sigma = (period / 32.0).max(0.5);
    let kernel: Vec<f64> = (-half..=half).map(|d| (-0.5 * (d as f64 / sigma).powi(2)).exp()).collect();
    let local: Vec<f64> = (0..n as i64)
        .map(|t| {
            kernel
                .iter()
                .enumerate()
                .map(|(k, w)| {
                    let s = t + k as i64 - half;
                    if s >= 0 && (s as usize) < n { w * odf[s as usize] as f64 } else { 0.0 }
                })
                .sum()
        })
        .collect();
    let d_lo = (period / 2.0).round().max(1.0) as usize;
    let d_hi = (2.0 * period).round() as usize;
    let tx: Vec<f64> = (0..=d_hi).map(|d| if d == 0 { f64::MIN } else { -TIGHTNESS * (d as f64 / period).ln().powi(2) }).collect();
    let local_max = local.iter().copied().fold(0.0, f64::max);
    let mut cum = vec![0.0f64; n];
    let mut back = vec![-1i64; n];
    let mut first = true;
    for t in 0..n {
        let (mut best, mut arg) = (f64::MIN, -1i64);
        for d in d_lo..=d_hi {
            let prev = t as i64 - d as i64;
            let c = if prev >= 0 { cum[prev as usize] } else { 0.0 } + tx[d];
            if c > best {
                best = c;
                arg = prev;
            }
        }
        cum[t] = local[t] + best;
        if first && local[t] < 0.01 * local_max {
            back[t] = -1;
        } else {
            back[t] = arg;
            first = false;
        }
    }
    // Last beat: latest local maximum of the cumulative score above half the median peak.
    let maxima: Vec<usize> = (1..n.saturating_sub(1)).filter(|&t| cum[t] > cum[t - 1] && cum[t] >= cum[t + 1]).collect();
    if maxima.is_empty() {
        return Vec::new();
    }
    let mut vals: Vec<f64> = maxima.iter().map(|&t| cum[t]).collect();
    vals.sort_by(f64::total_cmp);
    let med = vals[vals.len() / 2];
    let last = maxima.iter().rev().copied().find(|&t| 2.0 * cum[t] > med).unwrap_or(maxima[maxima.len() - 1]);
    let mut beats = vec![last];
    let mut t = back[last];
    while t >= 0 {
        beats.push(t as usize);
        t = back[t as usize];
    }
    beats.reverse();
    // Trim weak beats at the edges.
    let strength: Vec<f64> = beats.iter().map(|&b| local[b]).collect();
    let smooth: Vec<f64> = (0..strength.len())
        .map(|i| {
            let g = |j: i64| if j >= 0 && (j as usize) < strength.len() { strength[j as usize] } else { 0.0 };
            0.5 * g(i as i64 - 1) + g(i as i64) + 0.5 * g(i as i64 + 1)
        })
        .collect();
    let thr = 0.5 * (smooth.iter().map(|v| v * v).sum::<f64>() / smooth.len().max(1) as f64).sqrt();
    let first_ok = smooth.iter().position(|&v| v > thr);
    let last_ok = smooth.iter().rposition(|&v| v > thr);
    match (first_ok, last_ok) {
        (Some(a), Some(b)) if a <= b => beats[a..=b].to_vec(),
        _ => Vec::new(),
    }
}

fn zscore(v: &[f64]) -> Vec<f64> {
    let n = v.len().max(1) as f64;
    let mean = v.iter().sum::<f64>() / n;
    let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / n).sqrt();
    v.iter().map(|x| if sd > 1e-12 { (x - mean) / sd } else { 0.0 }).collect()
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let (mut ab, mut aa, mut bb) = (0.0, 0.0, 0.0);
    for (x, y) in a.iter().zip(b) {
        ab += x * y;
        aa += x * x;
        bb += y * y;
    }
    if aa <= 1e-18 || bb <= 1e-18 { 0.0 } else { ab / (aa * bb).sqrt() }
}

/// Mean of slow-frame rows over the onset-frame span `[a, b)`.
fn span_mean<const N: usize>(rows: &[[f32; N]], slow: usize, a: usize, b: usize) -> [f64; N] {
    let (i0, i1) = (a / slow, b.div_ceil(slow).min(rows.len()));
    let mut out = [0.0; N];
    if i1 <= i0 {
        if let Some(r) = rows.get(i0.min(rows.len().saturating_sub(1))) {
            for (o, v) in out.iter_mut().zip(r) {
                *o = *v as f64;
            }
        }
        return out;
    }
    for r in &rows[i0..i1] {
        for (o, v) in out.iter_mut().zip(r) {
            *o += *v as f64;
        }
    }
    out.iter_mut().for_each(|o| *o /= (i1 - i0) as f64);
    out
}

/// Analyse a mono buffer. `media` is left empty.
pub fn analyze_samples(mono: &[f32], sample_rate: f32) -> SongAnalysis {
    let sr = sample_rate.max(1.0) as f64;
    let duration = mono.len() as f64 / sr;
    let bin = (0.01 * sr).round().max(1.0) as usize;
    let peaks: Vec<f32> = mono.chunks(bin).map(|c| c.iter().fold(0f32, |m, x| if x.is_finite() { m.max(x.abs()) } else { m }).min(1.0)).collect();
    let mut out = SongAnalysis {
        media: String::new(),
        duration,
        bpm: 0.0,
        beats: Vec::new(),
        downbeats: Vec::new(),
        sections: vec![Section { start: 0.0, end: duration, label: "A".into() }],
        chorus: Vec::new(),
        peaks,
    };
    if sr < 4000.0 || duration < 3.0 {
        return out;
    }
    let clean: Vec<f32> = mono.iter().map(|x| if x.is_finite() { *x } else { 0.0 }).collect();
    let mut fr = frames(&clean, sr);
    std_normalise(&mut fr.odf);
    std_normalise(&mut fr.odf_low);
    let Some(period) = global_period(&fr.odf, fr.fr, MIN_BPM, MAX_BPM) else { return out };
    let beat_frames = dp_beats(&fr.odf, period);
    if beat_frames.len() < 2 {
        out.bpm = 60.0 * fr.fr / period;
        return out;
    }
    let to_s = |f: f64| (f / fr.fr + ODF_LEAD_S).max(0.0);
    out.beats = beat_frames.iter().map(|&b| to_s(b as f64)).collect();
    out.bpm = 60.0 * fr.fr / period;
    if out.beats.len() >= 8 {
        // Least-squares tempo of the beat grid.
        let n = out.beats.len() as f64;
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
        for (i, &t) in out.beats.iter().enumerate() {
            let x = i as f64;
            sx += x;
            sy += t;
            sxx += x * x;
            sxy += x * t;
        }
        let slope = (n * sxy - sx * sy) / (n * sxx - sx * sx);
        let fit = 60.0 / slope;
        if (fit / out.bpm - 1.0).abs() < 0.03 {
            out.bpm = fit;
        }
    }

    // Beat-synchronous accents for the bar phase.
    let nb = beat_frames.len();
    let win = (period / 8.0).round().max(2.0) as usize;
    let low: Vec<f64> =
        beat_frames.iter().map(|&b| (b.saturating_sub(win)..=(b + win).min(fr.odf_low.len() - 1)).map(|t| fr.odf_low[t] as f64).fold(0.0, f64::max)).collect();
    let spans: Vec<(usize, usize)> = (0..nb)
        .map(|i| {
            let a = beat_frames[i];
            let b = beat_frames.get(i + 1).copied().unwrap_or(a + period.round() as usize);
            (a, b.max(a + 1))
        })
        .collect();
    let beat_chroma: Vec<[f64; 12]> = spans.iter().map(|&(a, b)| span_mean(&fr.chroma, fr.slow, a, b)).collect();
    let change: Vec<f64> = (0..nb).map(|i| if i == 0 { 0.0 } else { 1.0 - cosine(&beat_chroma[i], &beat_chroma[i - 1]) }).collect();
    let (zl, zc) = (zscore(&low), zscore(&change));
    let phase = (0..4)
        .map(|p| {
            let idx: Vec<usize> = (p..nb).step_by(4).collect();
            let s = idx.iter().map(|&i| zl[i] + zc[i]).sum::<f64>() / idx.len().max(1) as f64;
            (p, s)
        })
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map_or(0, |(p, _)| p);
    let down_idx: Vec<usize> = (phase..nb).step_by(4).collect();
    out.downbeats = down_idx.iter().map(|&i| out.beats[i]).collect();
    if down_idx.len() < 2 * KERNEL_BARS {
        return out;
    }

    // Bar-synchronous features: bar j spans beats down_idx[j] .. down_idx[j+1]; the pickup
    // before the first downbeat joins bar 0 and the tail joins the last bar.
    let nbar = down_idx.len();
    let bar_span = |j: usize| {
        let a = if j == 0 { beat_frames[0] } else { beat_frames[down_idx[j]] };
        let b = if j + 1 < nbar { beat_frames[down_idx[j + 1]] } else { spans[nb - 1].1 };
        (a, b.max(a + 1))
    };
    let energy_rows: Vec<[f32; 1]> = fr.energy.iter().map(|&e| [e]).collect();
    let mut feats: Vec<Vec<f64>> = (0..nbar)
        .map(|j| {
            let (a, b) = bar_span(j);
            let mut v = Vec::with_capacity(25);
            v.extend(span_mean(&fr.chroma, fr.slow, a, b));
            v.extend(span_mean(&fr.mfcc, fr.slow, a, b));
            let e = span_mean(&energy_rows, fr.slow, a, b)[0];
            v.push(10.0 * (e + 1e-12).log10());
            v
        })
        .collect();
    let loud: Vec<f64> = feats.iter().map(|v| v[24]).collect();
    // Standardise each dimension, then balance the chroma / cepstrum / loudness blocks.
    for d in 0..25 {
        let col: Vec<f64> = feats.iter().map(|v| v[d]).collect();
        let z = zscore(&col);
        let w = if d < 24 { 1.0 / 12f64.sqrt() } else { 0.5 };
        for (v, zv) in feats.iter_mut().zip(z) {
            v[d] = zv * w;
        }
    }
    let ssm: Vec<Vec<f64>> = (0..nbar).map(|i| (0..nbar).map(|j| cosine(&feats[i], &feats[j])).collect()).collect();

    // Checkerboard novelty at each bar boundary j (between bar j−1 and j).
    let w = KERNEL_BARS as i64;
    let g = |d: i64| (-0.5 * ((d as f64 + 0.5) / (w as f64 * 0.5)).powi(2)).exp();
    let novelty: Vec<f64> = (0..nbar)
        .map(|j| {
            if j == 0 {
                return 0.0;
            }
            let (mut within, mut wsum, mut cross, mut csum) = (0.0, 0.0, 0.0, 0.0);
            for a in -w..w {
                for b in -w..w {
                    let (i1, i2) = (j as i64 + a, j as i64 + b);
                    if i1 < 0 || i2 < 0 || i1 >= nbar as i64 || i2 >= nbar as i64 || i1 == i2 {
                        continue;
                    }
                    let k = g(if a < 0 { -a - 1 } else { a }) * g(if b < 0 { -b - 1 } else { b });
                    let s = ssm[i1 as usize][i2 as usize];
                    if (a < 0) == (b < 0) {
                        within += k * s;
                        wsum += k;
                    } else {
                        cross += k * s;
                        csum += k;
                    }
                }
            }
            if wsum <= 0.0 || csum <= 0.0 { 0.0 } else { within / wsum - cross / csum }
        })
        .collect();
    let nmax = novelty.iter().copied().fold(0.0, f64::max);
    let thr = (0.3 * nmax).max(0.25);
    let mut candidates: Vec<usize> = (1..nbar)
        .filter(|&j| {
            let v = novelty[j];
            v >= thr && (j.saturating_sub(2)..=(j + 2).min(nbar - 1)).all(|k| k == j || novelty[k] <= v)
        })
        .collect();
    // Enforce the minimum section length, strongest first.
    candidates.sort_by(|&a, &b| novelty[b].total_cmp(&novelty[a]));
    let mut bounds: Vec<usize> = Vec::new();
    for c in candidates {
        if c >= MIN_SECTION_BARS.min(nbar) && nbar - c >= MIN_SECTION_BARS.min(nbar) && bounds.iter().all(|&b| b.abs_diff(c) >= MIN_SECTION_BARS) {
            bounds.push(c);
        }
    }
    bounds.sort_unstable();
    let mut segs: Vec<(usize, usize)> = Vec::new();
    let mut s = 0;
    for &b in &bounds {
        segs.push((s, b));
        s = b;
    }
    segs.push((s, nbar));

    // Label: greedy, by block similarity relative to internal similarity.
    let block = |(a0, a1): (usize, usize), (b0, b1): (usize, usize)| {
        let (mut sum, mut n) = (0.0, 0);
        for i in a0..a1 {
            for j in b0..b1 {
                if i != j {
                    sum += ssm[i][j];
                    n += 1;
                }
            }
        }
        if n == 0 { 1.0 } else { sum / n as f64 }
    };
    let mut labels: Vec<usize> = Vec::with_capacity(segs.len());
    let mut n_labels = 0;
    for (i, &si) in segs.iter().enumerate() {
        let self_i = block(si, si);
        let mut best: Option<(usize, f64)> = None;
        for l in 0..n_labels {
            let members: Vec<usize> = (0..i).filter(|&k| labels[k] == l).collect();
            let sim = members.iter().map(|&k| block(si, segs[k])).sum::<f64>() / members.len() as f64;
            let self_l = members.iter().map(|&k| block(segs[k], segs[k])).sum::<f64>() / members.len() as f64;
            if sim >= 0.3f64.max(0.5 * self_i.min(self_l)) && best.is_none_or(|(_, b)| sim > b) {
                best = Some((l, sim));
            }
        }
        labels.push(match best {
            Some((l, _)) => l,
            None => {
                n_labels += 1;
                n_labels - 1
            }
        });
    }
    let label_name = |l: usize| if l < 26 { ((b'A' + l as u8) as char).to_string() } else { format!("S{}", l + 1) };
    let bar_start = |j: usize| if j == 0 { 0.0 } else { out.beats[down_idx[j]] };
    out.sections = segs
        .iter()
        .zip(&labels)
        .map(|(&(a, b), &l)| Section { start: bar_start(a), end: if b >= nbar { duration } else { bar_start(b) }, label: label_name(l) })
        .collect();

    // Chorus: most repeated label among the loud ones.
    let seg_loud: Vec<f64> =
        segs.iter().map(|&(a, b)| 10.0 * (loud[a..b].iter().map(|db| 10f64.powf(db / 10.0)).sum::<f64>() / (b - a) as f64).log10()).collect();
    let mut stats: Vec<(usize, usize, f64)> = (0..n_labels)
        .map(|l| {
            let idx: Vec<usize> = (0..segs.len()).filter(|&k| labels[k] == l).collect();
            let e = idx.iter().map(|&k| seg_loud[k]).sum::<f64>() / idx.len() as f64;
            (l, idx.len(), e)
        })
        .collect();
    let loudest = stats.iter().map(|s| s.2).fold(f64::MIN, f64::max);
    stats.retain(|s| s.1 >= 2 && s.2 >= loudest - 3.0);
    if let Some(&(l, _, _)) = stats.iter().max_by(|a, b| a.1.cmp(&b.1).then(a.2.total_cmp(&b.2))) {
        out.chorus = segs.iter().zip(&labels).filter(|(_, x)| **x == l).map(|(&(a, _), _)| bar_start(a)).collect();
    }
    out
}
