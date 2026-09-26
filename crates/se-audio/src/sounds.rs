//! Sound loading for the sampler (`[audio.sounds]`): decode (WAV/FLAC/MP3/OGG via
//! `se_analysis::offline::decode_file`), resample to the graph rate with a windowed-sinc
//! kernel, and build the [`SampleBank`]. Decoded files are cached by path + mtime so config
//! reloads don't re-decode.

use crate::config::AudioConfig;
use se_dsp::db_to_gain;
use se_dsp::sampler::{LayerDef, Pick, SampleBank, SampleData, SoundDef};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

type Stereo = Arc<(Vec<f32>, Vec<f32>)>;

#[derive(Default)]
pub struct SoundCache {
    files: HashMap<(PathBuf, u32), (SystemTime, Stereo)>,
}

/// Longest accepted sound (seconds) — the sampler is for one-shots and layers.
pub const MAX_SECONDS: f64 = 60.0;

/// Windowed-sinc resampling of one channel from `from` Hz to `to` Hz.
pub fn resample(x: &[f32], from: f32, to: f32) -> Vec<f32> {
    if (from - to).abs() < 0.5 || x.is_empty() {
        return x.to_vec();
    }
    let ratio = to as f64 / from as f64;
    let out_len = ((x.len() as f64) * ratio).round() as usize;
    // cutoff below the lower Nyquist, 16 zero crossings each side, Blackman window
    let cutoff = ratio.min(1.0) * 0.94;
    let half = 16.0 / cutoff;
    let mut y = Vec::with_capacity(out_len);
    for j in 0..out_len {
        let t = j as f64 / ratio;
        let lo = (t - half).ceil().max(0.0) as usize;
        let hi = ((t + half).floor() as usize).min(x.len() - 1);
        let mut acc = 0.0f64;
        let mut wsum = 0.0f64;
        for (i, xi) in x.iter().enumerate().take(hi + 1).skip(lo) {
            let d = i as f64 - t;
            let arg = d * cutoff;
            let sinc = if arg.abs() < 1e-9 { 1.0 } else { (std::f64::consts::PI * arg).sin() / (std::f64::consts::PI * arg) };
            let w = 0.42 + 0.5 * (std::f64::consts::PI * d / half).cos() + 0.08 * (2.0 * std::f64::consts::PI * d / half).cos();
            let k = sinc * w.max(0.0);
            acc += *xi as f64 * k;
            wsum += k;
        }
        y.push(if wsum.abs() > 1e-9 { (acc / wsum) as f32 } else { 0.0 });
    }
    y
}

fn load(path: &Path, rate: u32) -> Result<(Vec<f32>, Vec<f32>), String> {
    let d = se_analysis::offline::decode_file(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if d.channels.is_empty() || d.channels[0].is_empty() {
        return Err(format!("{}: no audio", path.display()));
    }
    let frames = d.channels[0].len();
    if frames as f64 / d.sample_rate as f64 > MAX_SECONDS {
        return Err(format!("{}: longer than {MAX_SECONDS} s", path.display()));
    }
    let l = resample(&d.channels[0], d.sample_rate, rate as f32);
    let r = match d.channels.get(1) {
        Some(c) => resample(c, d.sample_rate, rate as f32),
        None => l.clone(),
    };
    Ok((l, r))
}

impl SoundCache {
    fn get(&mut self, path: &Path, rate: u32) -> Result<Stereo, String> {
        let mtime = std::fs::metadata(path).and_then(|m| m.modified()).map_err(|e| format!("{}: {e}", path.display()))?;
        let key = (path.to_path_buf(), rate);
        if let Some((t, s)) = self.files.get(&key)
            && *t == mtime
        {
            return Ok(s.clone());
        }
        let s = Arc::new(load(path, rate)?);
        self.files.insert(key, (mtime, s.clone()));
        Ok(s)
    }
}

/// Build the sample bank. Sounds whose files fail are skipped and reported.
pub fn build_bank(cfg: &AudioConfig, root: &Path, cache: &mut SoundCache) -> (SampleBank, Vec<String>) {
    let mut bank = SampleBank { samples: Vec::new(), sounds: Vec::new() };
    let mut errors = Vec::new();
    let mut index: HashMap<PathBuf, usize> = HashMap::new();
    'sounds: for s in &cfg.sounds {
        let mut layers = Vec::new();
        for l in &s.layers {
            let mut samples = Vec::new();
            for f in &l.files {
                let p = if Path::new(f).is_absolute() { PathBuf::from(f) } else { root.join(f) };
                let idx = match index.get(&p) {
                    Some(i) => *i,
                    None => match cache.get(&p, cfg.rate) {
                        Ok(data) => {
                            bank.samples.push(SampleData { name: f.clone(), l: data.0.clone(), r: data.1.clone() });
                            index.insert(p.clone(), bank.samples.len() - 1);
                            bank.samples.len() - 1
                        }
                        Err(e) => {
                            errors.push(format!("sounds.{}: {e}", s.name));
                            continue 'sounds;
                        }
                    },
                };
                samples.push(idx);
            }
            layers.push(LayerDef { vel: l.vel, samples });
        }
        bank.sounds.push(SoundDef {
            name: s.name.clone(),
            layers,
            pick: if s.random { Pick::Random } else { Pick::RoundRobin },
            gain: db_to_gain(s.gain_db),
            choke: s.choke,
            max_voices: s.max_voices,
            next: 0,
        });
    }
    // implicit sounds: assets/sounds/<name>.<ext> that no config entry claims
    let dir = root.join("assets/sounds");
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    files.sort();
    for p in files {
        let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
        if !AUDIO_EXTS.contains(&ext.as_str()) {
            continue;
        }
        let Some(stem) = p.file_stem().and_then(|s| s.to_str()) else { continue };
        if !se_proto::address::is_valid(stem, false) || stem.contains('.') || bank.sounds.iter().any(|s| s.name == stem) {
            continue;
        }
        match cache.get(&p, cfg.rate) {
            Ok(data) => {
                let idx = match index.get(&p) {
                    Some(i) => *i,
                    None => {
                        bank.samples.push(SampleData {
                            name: p.file_name().and_then(|f| f.to_str()).unwrap_or(stem).to_string(),
                            l: data.0.clone(),
                            r: data.1.clone(),
                        });
                        index.insert(p.clone(), bank.samples.len() - 1);
                        bank.samples.len() - 1
                    }
                };
                bank.sounds.push(SoundDef {
                    name: stem.to_string(),
                    layers: vec![LayerDef { vel: [0.0, 1.0], samples: vec![idx] }],
                    pick: Pick::RoundRobin,
                    gain: 1.0,
                    choke: None,
                    max_voices: 4,
                    next: 0,
                });
            }
            Err(e) => errors.push(format!("assets/sounds: {e}")),
        }
    }
    (bank, errors)
}

/// File types picked up from `assets/sounds/`.
pub const AUDIO_EXTS: &[&str] = &["wav", "flac", "ogg", "mp3"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resample_preserves_a_sine_and_length() {
        let x: Vec<f32> = (0..44100).map(|i| (std::f32::consts::TAU * 1000.0 * i as f32 / 44100.0).sin()).collect();
        let y = resample(&x, 44100.0, 48000.0);
        assert_eq!(y.len(), 48000);
        let err = (1000..47000).map(|i| (y[i] - (std::f32::consts::TAU * 1000.0 * i as f32 / 48000.0).sin()).abs()).fold(0.0, f32::max);
        assert!(err < 2e-3, "max error {err}");
    }

    #[test]
    fn resample_downsampling_rejects_aliases() {
        // 30 kHz at 96 kHz → 48 kHz must be removed (above the new Nyquist)
        let x: Vec<f32> = (0..9600).map(|i| (std::f32::consts::TAU * 30000.0 * i as f32 / 96000.0).sin()).collect();
        let y = resample(&x, 96000.0, 48000.0);
        let rms = (y[200..4600].iter().map(|v| v * v).sum::<f32>() / 4400.0).sqrt();
        assert!(rms < 0.01, "alias rms {rms}");
    }
}
