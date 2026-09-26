//! Kokoro-82M v1.0 inference with ONNX Runtime on the CPU, plus the voice style packs.
//!
//! Model contract (checked at load against the file, not assumed):
//! `input_ids` int64 `[1, N+2]` (phoneme ids with pad 0 at both ends), `style` f32 `[1, 256]`,
//! `speed` f32 `[1]` → one f32 waveform output at 24 kHz.
//! Voices are raw little-endian f32 `[510, 256]`: one style vector per utterance length; row
//! `tokens - 1` is used, as in hexgrad/kokoro's `KPipeline.infer` (`pack[len(ps)-1]`).

use crate::phonemize::{self, Espeak};
use crate::vocab;
use anyhow::{Context, Result, anyhow, bail};
use ort::session::Session;
use ort::session::builder::GraphOptimizationLevel;
use ort::value::{TensorElementType, TensorRef, ValueType};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const SAMPLE_RATE: u32 = 24_000;
pub const STYLE_DIM: usize = 256;
pub const STYLE_ROWS: usize = 510;
const VOICE_BYTES: u64 = (STYLE_ROWS * STYLE_DIM * 4) as u64;
/// Silence between chunks of one utterance (long texts split at clause boundaries).
const CHUNK_PAUSE_S: f32 = 0.12;
pub const SPEED_RANGE: (f32, f32) = (0.5, 2.0);

/// One voice's style pack.
pub struct Voice {
    pub name: String,
    data: Box<[f32]>,
}

impl Voice {
    pub fn load(path: &Path) -> Result<Voice> {
        let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        let bytes = std::fs::read(path).with_context(|| format!("read voice {}", path.display()))?;
        if bytes.len() as u64 != VOICE_BYTES {
            bail!("voice {} is {} bytes, expected {VOICE_BYTES} (510×256 f32)", path.display(), bytes.len());
        }
        let data: Box<[f32]> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
        if data.iter().any(|v| !v.is_finite()) {
            bail!("voice {} contains non-finite values", path.display());
        }
        Ok(Voice { name, data })
    }

    /// Style vector for an utterance of `tokens` phonemes.
    pub fn style(&self, tokens: usize) -> &[f32] {
        let row = tokens.clamp(1, STYLE_ROWS) - 1;
        &self.data[row * STYLE_DIM..(row + 1) * STYLE_DIM]
    }
}

/// Names of the valid voice packs (`<dir>/<name>.bin` of the right size), sorted.
pub fn list_voices(dir: &Path) -> Vec<String> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut v: Vec<String> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "bin") && e.metadata().is_ok_and(|m| m.is_file() && m.len() == VOICE_BYTES))
        .filter_map(|e| e.path().file_stem().and_then(|s| s.to_str()).map(str::to_string))
        .collect();
    v.sort();
    v
}

/// A loaded Kokoro ONNX session.
pub struct Kokoro {
    session: Session,
    output: String,
    pub path: PathBuf,
    signature: String,
}

fn builder_err(e: ort::Error<ort::session::builder::SessionBuilder>) -> anyhow::Error {
    anyhow!("onnx runtime: {e}")
}

fn tensor_of(t: &ValueType) -> Option<TensorElementType> {
    t.tensor_type()
}

impl Kokoro {
    /// Load and validate the model. `threads` = intra-op threads for inference.
    pub fn load(path: &Path, threads: usize) -> Result<Kokoro> {
        if !path.is_file() {
            bail!("model file {} is missing", path.display());
        }
        let session = Session::builder()
            .map_err(|e| anyhow!("onnx runtime: {e}"))?
            .with_optimization_level(GraphOptimizationLevel::Level3)
            .map_err(builder_err)?
            .with_intra_threads(threads.max(1))
            .map_err(builder_err)?
            .with_inter_threads(1)
            .map_err(builder_err)?
            // Synthesis is bursty: don't keep cores spinning between utterances.
            .with_intra_op_spinning(false)
            .map_err(builder_err)?
            .commit_from_file(path)
            .map_err(|e| anyhow!("load {}: {e}", path.display()))?;
        let signature = format!(
            "{} → {}",
            session.inputs().iter().map(|i| format!("{}: {}", i.name(), i.dtype())).collect::<Vec<_>>().join(", "),
            session.outputs().iter().map(|o| format!("{}: {}", o.name(), o.dtype())).collect::<Vec<_>>().join(", ")
        );
        let want = [("input_ids", TensorElementType::Int64), ("style", TensorElementType::Float32), ("speed", TensorElementType::Float32)];
        for (name, ty) in want {
            match session.inputs().iter().find(|i| i.name() == name) {
                Some(i) if tensor_of(i.dtype()) == Some(ty) => {}
                Some(i) => bail!("model input `{name}` is {}, expected a {ty:?} tensor ({signature})", i.dtype()),
                None => bail!("model has no `{name}` input ({signature})"),
            }
        }
        if session.inputs().len() != want.len() {
            bail!("unexpected model inputs ({signature})");
        }
        let output = match session.outputs() {
            [o] if tensor_of(o.dtype()) == Some(TensorElementType::Float32) => o.name().to_string(),
            _ => bail!("expected one f32 waveform output ({signature})"),
        };
        Ok(Kokoro { session, output, path: path.to_path_buf(), signature })
    }

    /// `name: type` of every input and output (for logs).
    pub fn signature(&self) -> &str {
        &self.signature
    }

    /// Run one chunk: `tokens` without pads (1..=510 of them).
    pub fn infer(&mut self, tokens: &[i64], style: &[f32], speed: f32) -> Result<Vec<f32>> {
        if tokens.is_empty() || tokens.len() > vocab::MAX_TOKENS {
            bail!("chunk of {} tokens (1..={} allowed)", tokens.len(), vocab::MAX_TOKENS);
        }
        if style.len() != STYLE_DIM {
            bail!("style vector of {} values, expected {STYLE_DIM}", style.len());
        }
        let mut ids = Vec::with_capacity(tokens.len() + 2);
        ids.push(vocab::PAD);
        ids.extend_from_slice(tokens);
        ids.push(vocab::PAD);
        let speed = [speed.clamp(SPEED_RANGE.0, SPEED_RANGE.1)];
        let inputs = ort::inputs![
            "input_ids" => TensorRef::from_array_view(([1usize, ids.len()], ids.as_slice()))?,
            "style" => TensorRef::from_array_view(([1usize, STYLE_DIM], style))?,
            "speed" => TensorRef::from_array_view(([1usize], speed.as_slice()))?,
        ];
        let out = self.session.run(inputs)?;
        let (_, wav) = out[self.output.as_str()].try_extract_tensor::<f32>()?;
        Ok(wav.to_vec())
    }
}

/// Phonemizer + model + voice cache: text in, 24 kHz mono samples out.
pub struct Synth {
    pub espeak: Espeak,
    pub model: Kokoro,
    voices_dir: PathBuf,
    voices: HashMap<String, Arc<Voice>>,
}

/// Result of [`Synth::speak`].
pub struct Speech {
    pub samples: Vec<f32>,
    /// Phoneme chunks that were synthesized.
    pub phonemes: Vec<String>,
}

impl Synth {
    pub fn load(model: &Path, voices_dir: &Path, threads: usize, espeak: Espeak) -> Result<Synth> {
        let model = Kokoro::load(model, threads)?;
        Ok(Synth { espeak, model, voices_dir: voices_dir.to_path_buf(), voices: HashMap::new() })
    }

    pub fn voices_dir(&self) -> &Path {
        &self.voices_dir
    }

    /// Load (once) and return a voice pack.
    pub fn voice(&mut self, name: &str) -> Result<Arc<Voice>> {
        if let Some(v) = self.voices.get(name) {
            return Ok(v.clone());
        }
        if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
            bail!("invalid voice name `{name}`");
        }
        let v = Arc::new(Voice::load(&self.voices_dir.join(format!("{name}.bin")))?);
        self.voices.insert(name.to_string(), v.clone());
        Ok(v)
    }

    /// Phonemize already-normalized `text` into ≤ 510-token chunks.
    pub fn phonemes(&self, text: &str, lang: &str) -> Result<Vec<String>> {
        let ps = phonemize::phonemize(&self.espeak, text, lang)?;
        Ok(phonemize::chunk(&ps, vocab::MAX_TOKENS))
    }

    /// Synthesize normalized text. `cancelled` is polled between chunks; `Ok(None)` = cancelled.
    pub fn speak(&mut self, text: &str, voice: &str, lang: &str, speed: f32, cancelled: &dyn Fn() -> bool) -> Result<Option<Speech>> {
        let voice = self.voice(voice)?;
        let chunks = self.phonemes(text, lang)?;
        let pause = vec![0.0f32; (SAMPLE_RATE as f32 * CHUNK_PAUSE_S) as usize];
        let mut samples = Vec::new();
        for (i, ps) in chunks.iter().enumerate() {
            if cancelled() {
                return Ok(None);
            }
            let tokens = phonemize::tokens(ps);
            let wav = self.model.infer(&tokens, voice.style(tokens.len()), speed)?;
            if i > 0 {
                samples.extend_from_slice(&pause);
            }
            samples.extend(wav);
        }
        Ok(Some(Speech { samples, phonemes: chunks }))
    }
}
