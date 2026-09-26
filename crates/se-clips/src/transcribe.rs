//! Whisper transcripts with word timestamps (whisper.cpp via `whisper-rs`, CPU, niced).
//! The ggml model (default `small.en`) is downloaded once to
//! `~/.local/share/stream-engine/models/whisper/` and verified by SHA-256.

use crate::config::WhisperConfig;
use crate::ffmpeg;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Path, PathBuf};
use whisper_rs::{DtwMode, DtwModelPreset, DtwParameters, FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters};

pub const SAMPLE_RATE: u32 = 16_000;
const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

/// `(name, size, sha256)` of known ggml models (Hugging Face LFS object ids).
const MODELS: &[(&str, u64, &str)] = &[
    ("tiny.en", 77_704_715, "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f"),
    ("tiny.en-q5_1", 32_166_155, "c77c5766f1cef09b6b7d47f21b546cbddd4157886b3b5d6d4f709e91e66c7c2b"),
    ("base.en", 147_964_211, "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002"),
    ("base.en-q5_1", 59_721_011, "4baf70dd0d7c4247ba2b81fafd9c01005ac77c2f9ef064e00dcf195d0e2fdd2f"),
    ("small.en", 487_614_201, "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d"),
    ("small.en-q5_1", 190_098_681, "bfdff4894dcb76bbf647d56263ea2a96645423f1669176f4844a1bf8e478ad30"),
    ("small.en-q8_0", 264_477_561, "67a179f608ea6114bd3fdb9060e762b588a3fb3bd00c4387971be4d177958067"),
    ("medium.en", 1_533_774_781, "cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356"),
    ("medium.en-q5_0", 539_225_533, "76733e26ad8fe1c7a5bf7531a9d41917b2adc0f20f2e4f5531688a8c6cd88eb0"),
];

/// One transcribed word in recording time (seconds).
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Word {
    pub t0: f64,
    pub t1: f64,
    #[serde(rename = "w")]
    pub text: String,
    #[serde(default)]
    pub p: f32,
    /// Non-speech annotation such as `(laughing)` or `[MUSIC]` (never captioned).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub annotation: bool,
}

pub fn model_path(data_dir: &Path, model: &str) -> PathBuf {
    let p = Path::new(model);
    if p.is_absolute() { p.to_path_buf() } else { data_dir.join("models/whisper").join(format!("ggml-{model}.bin")) }
}

fn sha256_file(p: &Path) -> std::io::Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(h.finalize().iter().map(|b| format!("{b:02x}")).collect())
}

/// Model file status for preflight: `Ok(path)` when present and verified.
pub fn model_status(data_dir: &Path, model: &str) -> Result<PathBuf, String> {
    let path = model_path(data_dir, model);
    if !path.exists() {
        return Err(format!("{} missing (downloads on the first clip job)", path.display()));
    }
    Ok(path)
}

/// Make sure the model is on disk and intact, downloading it with `curl` when missing.
/// Verification is cached in `<model>.sha256` (size + hash).
pub fn ensure_model(data_dir: &Path, model: &str, mut progress: impl FnMut(&str)) -> Result<PathBuf, String> {
    let path = model_path(data_dir, model);
    let known = MODELS.iter().find(|(n, _, _)| *n == model);
    let stamp = path.with_extension("bin.sha256");
    if path.exists() {
        let Some((_, size, sha)) = known else { return Ok(path) };
        let len = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if std::fs::read_to_string(&stamp).ok().as_deref().map(str::trim) == Some(&format!("{len} {sha}")) {
            return Ok(path);
        }
        if len == *size {
            progress("verifying Whisper model");
            if sha256_file(&path).map_err(|e| e.to_string())? == *sha {
                let _ = std::fs::write(&stamp, format!("{len} {sha}\n"));
                return Ok(path);
            }
        }
        tracing::warn!("whisper model {} is damaged; downloading again", path.display());
        let _ = std::fs::remove_file(&path);
    }
    let Some((_, size, sha)) = known else {
        return Err(format!("Whisper model {} not found (unknown model names must be an existing file)", path.display()));
    };
    let dir = path.parent().ok_or("bad model path")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let part = path.with_extension("bin.part");
    progress(&format!("downloading Whisper {model} ({} MB)", size / 1_000_000));
    let url = format!("{MODEL_URL}/ggml-{model}.bin");
    let out = std::process::Command::new("curl")
        .args(["--fail", "--location", "--silent", "--show-error", "--retry", "3", "--continue-at", "-", "--output"])
        .arg(&part)
        .arg(&url)
        .output()
        .map_err(|e| format!("curl: {e} (install curl)"))?;
    let len = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
    // curl reports 416 when a complete .part is resumed; the hash below decides
    if !out.status.success() && len != *size {
        return Err(format!("download {url}: {}", String::from_utf8_lossy(&out.stderr).trim()));
    }
    progress("verifying Whisper model");
    let got = sha256_file(&part).map_err(|e| e.to_string())?;
    if got != *sha {
        let _ = std::fs::remove_file(&part);
        return Err(format!("Whisper model checksum mismatch ({got}); removed the download, retry later"));
    }
    std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
    let _ = std::fs::write(&stamp, format!("{len} {sha}\n"));
    Ok(path)
}

/// Mono 16 kHz f32 PCM of one audio stream between `from` and `to` (recording seconds).
pub fn extract_pcm(recording: &Path, stream: usize, from: f64, to: f64, nice: i32) -> Result<Vec<f32>, String> {
    let args = ffmpeg::pcm_args(recording, stream, from, (to - from).max(0.0));
    let bytes = ffmpeg::run_capture(&args, nice)?;
    Ok(bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect())
}

/// A loaded Whisper model (load once per job).
pub struct Transcriber {
    ctx: WhisperContext,
    cfg: WhisperConfig,
    /// DTW token alignment available (known model family).
    dtw: bool,
}

/// Seconds a word lasts after its last aligned token.
const WORD_TAIL: f64 = 0.3;

/// Alignment heads for DTW word timing by model family.
fn dtw_preset(model: &str) -> Option<DtwModelPreset> {
    let file = Path::new(model).file_name().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default();
    let name = file.strip_suffix(".bin").unwrap_or(&file).trim_start_matches("ggml-");
    let base = name.split('-').next().unwrap_or(name);
    Some(match base {
        "tiny.en" => DtwModelPreset::TinyEn,
        "tiny" => DtwModelPreset::Tiny,
        "base.en" => DtwModelPreset::BaseEn,
        "base" => DtwModelPreset::Base,
        "small.en" => DtwModelPreset::SmallEn,
        "small" => DtwModelPreset::Small,
        "medium.en" => DtwModelPreset::MediumEn,
        "medium" => DtwModelPreset::Medium,
        _ => return None,
    })
}

impl Transcriber {
    pub fn load(path: &Path, cfg: &WhisperConfig) -> Result<Transcriber, String> {
        whisper_rs::install_logging_hooks();
        let mut params = WhisperContextParameters::default();
        params.use_gpu(false);
        let preset = dtw_preset(&cfg.model).or_else(|| dtw_preset(&path.to_string_lossy()));
        let dtw = preset.is_some();
        if let Some(model_preset) = preset {
            params.dtw_parameters(DtwParameters { mode: DtwMode::ModelPreset { model_preset }, ..DtwParameters::default() });
        }
        let ctx = WhisperContext::new_with_params(path, params).map_err(|e| format!("load {}: {e}", path.display()))?;
        Ok(Transcriber { ctx, cfg: cfg.clone(), dtw })
    }

    /// Transcribe `pcm` (16 kHz mono) that starts at `offset` seconds into the recording.
    /// Only the speech regions are decoded: whisper's timestamps drift across long silences
    /// (and silence costs time), so each region starts right at its speech.
    pub fn words(&self, pcm: &[f32], offset: f64) -> Result<Vec<Word>, String> {
        let mut state = self.ctx.create_state().map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        let min = SAMPLE_RATE as usize * 11 / 10;
        let mut padded = Vec::new();
        for (a, b) in speech_regions(pcm) {
            let chunk = if b - a >= min {
                &pcm[a..b]
            } else {
                // whisper wants ≥ 1 s of input
                padded.clear();
                padded.extend_from_slice(&pcm[a..b]);
                padded.resize(min, 0.0);
                &padded[..]
            };
            let end = offset + b as f64 / SAMPLE_RATE as f64;
            let words = self.decode(&mut state, chunk, offset + a as f64 / SAMPLE_RATE as f64)?;
            out.extend(words.into_iter().filter(|w| w.t0 < end).map(|mut w| {
                w.t1 = w.t1.min(end);
                w
            }));
        }
        Ok(out)
    }

    fn decode(&self, state: &mut whisper_rs::WhisperState, pcm: &[f32], offset: f64) -> Result<Vec<Word>, String> {
        let mut p = FullParams::new(SamplingStrategy::Greedy { best_of: 1 });
        let threads =
            if self.cfg.threads > 0 { self.cfg.threads as i32 } else { (std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4) / 2).max(1) as i32 };
        p.set_n_threads(threads);
        p.set_language(Some(&self.cfg.language));
        p.set_translate(false);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_special(false);
        p.set_print_timestamps(false);
        // one segment per word (whisper.cpp `-ml 1 -sow`)
        p.set_token_timestamps(true);
        p.set_max_len(1);
        p.set_split_on_word(true);
        if !self.cfg.prompt.is_empty() {
            p.set_initial_prompt(&self.cfg.prompt);
        }
        state.full(p, pcm).map_err(|e| format!("whisper: {e}"))?;
        let eot = self.ctx.token_eot();
        let mut out: Vec<Word> = Vec::new();
        // last aligned token time per word, for its end
        let mut last_tok: Vec<f64> = Vec::new();
        let mut depth = 0i32;
        let dur = pcm.len() as f64 / SAMPLE_RATE as f64;
        for seg in state.as_iter() {
            let text = seg.to_str_lossy().map_err(|e| e.to_string())?.trim().to_string();
            if text.is_empty() {
                continue;
            }
            let (mut psum, mut pn) = (0.0f32, 0u32);
            let (mut first_dtw, mut last_dtw) = (None, None);
            for i in 0..seg.n_tokens() {
                if let Some(t) = seg.get_token(i)
                    && t.token_id() < eot
                {
                    psum += t.token_probability();
                    pn += 1;
                    let d = t.token_data().t_dtw;
                    if d >= 0 {
                        first_dtw.get_or_insert(d);
                        last_dtw = Some(d);
                    }
                }
            }
            if pn == 0 {
                continue;
            }
            // `(upbeat music)` spans two word segments: track brackets across words
            let opens = text.starts_with(['(', '[', '*']);
            let annotation = depth > 0 || opens;
            depth += text.matches(['(', '[']).count() as i32 - text.matches([')', ']']).count() as i32;
            depth = depth.max(0);
            // DTW token alignment is accurate; the timestamp-token heuristic drifts across
            // silence, so it's only the fallback (custom models without alignment heads)
            let (t0, t1) = match (first_dtw, last_dtw) {
                (Some(a), Some(b)) if self.dtw => (a as f64 / 100.0, b as f64 / 100.0 + WORD_TAIL),
                _ => (seg.start_timestamp() as f64 / 100.0, seg.end_timestamp() as f64 / 100.0),
            };
            let t0 = t0.clamp(0.0, dur);
            last_tok.push(t1.clamp(t0, dur));
            out.push(Word { t0: offset + t0, t1: offset + t0, text, p: psum / pn as f32, annotation });
        }
        // a word ends at its last aligned token (+ tail) or where the next word starts
        let n = out.len();
        for i in 0..n {
            let next = if i + 1 < n { out[i + 1].t0 } else { f64::INFINITY };
            let end = (offset + last_tok[i]).min(next).max(out[i].t0 + 0.05);
            out[i].t1 = end;
        }
        Ok(out)
    }
}

const FRAME: usize = SAMPLE_RATE as usize / 50; // 20 ms
/// How far above the noise floor a frame counts as speech.
const CONTRAST_DB: f32 = 9.0;

/// Speech regions `(start, end)` in samples by short-time energy: frames 9 dB above the noise
/// floor (10th percentile, at least −55 dBFS), gaps under 0.6 s bridged, 0.3 s of padding,
/// blips under 0.15 s dropped. A track that never gets quiet comes back as one region.
pub fn speech_regions(pcm: &[f32]) -> Vec<(usize, usize)> {
    let n = pcm.len() / FRAME;
    if n == 0 {
        return Vec::new();
    }
    let db: Vec<f32> = pcm
        .as_chunks::<FRAME>()
        .0
        .iter()
        .map(|f| {
            let ms = f.iter().map(|x| x * x).sum::<f32>() / FRAME as f32;
            10.0 * ms.max(1e-12).log10()
        })
        .collect();
    let mut sorted = db.clone();
    sorted.sort_by(f32::total_cmp);
    let floor = sorted[n / 10];
    let loud = sorted[n * 9 / 10];
    let threshold = (floor + CONTRAST_DB).max(-55.0);
    if loud < threshold {
        // no quiet/loud contrast: silence, or sound throughout (speech over steady bleed)
        return if loud > -55.0 { vec![(0, pcm.len())] } else { Vec::new() };
    }
    let frames = |secs: f32| (secs * 50.0).round() as usize;
    let (bridge, pad, min_len) = (frames(0.6), frames(0.3), frames(0.15));
    let mut regions: Vec<(usize, usize)> = Vec::new();
    let mut i = 0;
    while i < n {
        if db[i] < threshold {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i + 1;
        let mut quiet = 0;
        let mut j = i + 1;
        while j < n {
            if db[j] >= threshold {
                end = j + 1;
                quiet = 0;
            } else {
                quiet += 1;
                if quiet > bridge {
                    break;
                }
            }
            j += 1;
        }
        if end - start >= min_len {
            let a = start.saturating_sub(pad);
            let b = (end + pad).min(n);
            match regions.last_mut() {
                Some(last) if a <= last.1 => last.1 = b,
                _ => regions.push((a, b)),
            }
        }
        i = j.max(end);
    }
    regions.into_iter().map(|(a, b)| (a * FRAME, (b * FRAME).min(pcm.len()))).collect()
}

/// Run `f` on a fresh thread at a lower CPU priority (Whisper's worker threads inherit it),
/// so a clip job never competes with the show.
pub fn niced<T: Send + 'static>(nice: i32, f: impl FnOnce() -> T + Send + 'static) -> Result<T, String> {
    std::thread::Builder::new()
        .name("se-clips-work".into())
        .spawn(move || {
            // SAFETY: plain syscall; on Linux PRIO_PROCESS with who = 0 targets this thread.
            unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice) };
            f()
        })
        .map_err(|e| e.to_string())?
        .join()
        .map_err(|_| "clip worker panicked".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speech_regions_skip_silence_and_bridge_short_pauses() {
        let sr = SAMPLE_RATE as usize;
        let mut pcm = vec![0.0f32; sr * 30];
        // faint room noise everywhere
        for (i, x) in pcm.iter_mut().enumerate() {
            *x = 0.001 * ((i as f32 * 0.37).sin());
        }
        let tone = |pcm: &mut Vec<f32>, from: f32, to: f32| {
            let (a, b) = ((from * sr as f32) as usize, (to * sr as f32) as usize);
            for (i, x) in pcm.iter_mut().enumerate().take(b).skip(a) {
                *x += 0.3 * (i as f32 * 0.2).sin();
            }
        };
        tone(&mut pcm, 5.0, 7.0);
        tone(&mut pcm, 7.4, 8.0); // 0.4 s pause: same region
        tone(&mut pcm, 20.0, 21.0);
        tone(&mut pcm, 25.0, 25.05); // 50 ms click: dropped
        let r = speech_regions(&pcm);
        let secs: Vec<(f32, f32)> = r.iter().map(|(a, b)| (*a as f32 / sr as f32, *b as f32 / sr as f32)).collect();
        assert_eq!(secs.len(), 2, "{secs:?}");
        assert!((secs[0].0 - 4.7).abs() < 0.03 && (secs[0].1 - 8.3).abs() < 0.03, "{secs:?}");
        assert!((secs[1].0 - 19.7).abs() < 0.03 && (secs[1].1 - 21.3).abs() < 0.03, "{secs:?}");
        // all loud → one region; all silent → none
        assert_eq!(speech_regions(&vec![0.2; sr * 3]).len(), 1);
        assert!(speech_regions(&vec![0.0; sr * 3]).is_empty());
    }

    #[test]
    fn dtw_alignment_heads_follow_the_model_family() {
        assert!(matches!(dtw_preset("small.en"), Some(DtwModelPreset::SmallEn)));
        assert!(matches!(dtw_preset("small.en-q5_1"), Some(DtwModelPreset::SmallEn)));
        assert!(matches!(dtw_preset("/m/ggml-base.en.bin"), Some(DtwModelPreset::BaseEn)));
        assert!(dtw_preset("/m/my-finetune.bin").is_none());
    }

    #[test]
    fn model_paths_and_stamped_verification() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(model_path(d.path(), "small.en"), d.path().join("models/whisper/ggml-small.en.bin"));
        assert_eq!(model_path(d.path(), "/x/y.bin"), PathBuf::from("/x/y.bin"));
        // an unknown name must be an existing file, never a download
        assert!(ensure_model(d.path(), "my-finetune", |_| {}).unwrap_err().contains("not found"));
        let own = d.path().join("own.bin");
        std::fs::write(&own, b"x").unwrap();
        assert_eq!(ensure_model(d.path(), own.to_str().unwrap(), |_| {}).unwrap(), own);
        assert!(model_status(d.path(), "small.en").is_err());
    }
}
