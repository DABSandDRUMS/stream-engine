//! Voice push-to-talk: `voice.ptt start|stop|toggle` (deck key, MIDI button, or a Hyprland
//! bind calling the CLI) → short ALSA capture → Whisper (whisper.cpp via `whisper-rs`, model
//! downloaded to `<data>/models/`) → fixed grammar → `voice.intent` event + command, with
//! spoken/deck confirmation for risky intents.

pub mod audio;
pub mod grammar;

use crate::Shared;
use crate::config::VoiceCfg;
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender};
use grammar::{Intent, Vocab};
use parking_lot::Mutex;
use se_proto::{Command, Event, Meta, Origin, Value};
use serde::Serialize;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Known ggml models: name → (SHA-1, approximate size in MB).
pub const MODELS: &[(&str, &str, u32)] = &[
    ("tiny.en", "c78c86eb1a8faa21b369bcd33207cc90d64ae9df", 75),
    ("base.en", "137c40403d78fd54d454da0f9bd998f78703390c", 142),
    ("small.en", "db8a495a91d927739e50b3fc1cc4c6b8f6c2d022", 466),
];

const MODEL_URL: &str = "https://huggingface.co/ggerganov/whisper.cpp/resolve/main";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ptt {
    Start,
    Stop,
    Toggle,
}

pub enum VoiceCmd {
    Ptt(Ptt),
    Confirm,
    Cancel,
    /// Run a WAV file through the same path as a capture.
    File(PathBuf),
    Config(VoiceCfg, Vocab),
    Vocab(Vocab),
    Download,
    Stop,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Heard {
    pub text: String,
    pub intent: Option<String>,
    pub confidence: f32,
    pub ms: u64,
    pub executed: bool,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct VoiceStatus {
    pub enabled: bool,
    pub state: String,
    pub model: String,
    pub model_path: String,
    pub device: String,
    pub pending: Option<String>,
    pub error: Option<String>,
    pub download: Option<f32>,
    pub history: Vec<Heard>,
}

pub struct VoiceHandle {
    pub tx: Sender<VoiceCmd>,
    pub status: Arc<Mutex<VoiceStatus>>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl VoiceHandle {
    pub fn stop(mut self) {
        let _ = self.tx.send(VoiceCmd::Stop);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

/// Resolve a model name or path.
pub fn model_path(data_dir: &Path, model: &str) -> PathBuf {
    if model.contains('/') || model.ends_with(".bin") {
        PathBuf::from(model)
    } else {
        // shared with the clip pipeline (se-clips keeps its whisper models in the same directory)
        data_dir.join("models").join("whisper").join(format!("ggml-{model}.bin"))
    }
}

/// Make sure the model file exists, downloading known models (verified by SHA-1).
pub fn ensure_model(data_dir: &Path, model: &str, progress: impl Fn(f32)) -> Result<PathBuf, String> {
    let path = model_path(data_dir, model);
    if path.is_file() {
        return Ok(path);
    }
    let Some((_, sha, _)) = MODELS.iter().find(|(n, _, _)| *n == model) else {
        return Err(format!("model file {} not found (known downloadable models: tiny.en, base.en, small.en)", path.display()));
    };
    let dir = path.parent().ok_or("bad model path")?;
    std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let url = format!("{MODEL_URL}/ggml-{model}.bin");
    let resp = ureq::get(&url).call().map_err(|e| format!("download {url}: {e}"))?;
    let total = resp.body().content_length();
    let mut reader = resp.into_body().into_reader();
    let part = path.with_extension("bin.part");
    let mut out = std::fs::File::create(&part).map_err(|e| format!("{}: {e}", part.display()))?;
    let mut h = sha1_smol::Sha1::new();
    let mut buf = vec![0u8; 1 << 16];
    let mut done: u64 = 0;
    let mut last = Instant::now();
    loop {
        let n = reader.read(&mut buf).map_err(|e| format!("download {url}: {e}"))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
        out.write_all(&buf[..n]).map_err(|e| e.to_string())?;
        done += n as u64;
        if let Some(t) = total
            && last.elapsed() > Duration::from_millis(250)
        {
            last = Instant::now();
            progress(done as f32 / t as f32);
        }
    }
    out.sync_all().map_err(|e| e.to_string())?;
    let got = h.digest().to_string();
    if got != *sha {
        let _ = std::fs::remove_file(&part);
        return Err(format!("model checksum mismatch for {model}: got {got}, want {sha}"));
    }
    std::fs::rename(&part, &path).map_err(|e| e.to_string())?;
    progress(1.0);
    Ok(path)
}

/// A loaded Whisper model.
pub struct Transcriber {
    ctx: whisper_rs::WhisperContext,
    pub threads: i32,
}

impl Transcriber {
    pub fn load(path: &Path, threads: i32, gpu: bool) -> Result<Transcriber, String> {
        whisper_rs::install_logging_hooks();
        let mut p = whisper_rs::WhisperContextParameters::default();
        p.use_gpu(gpu);
        let ctx = whisper_rs::WhisperContext::new_with_params(path, p).map_err(|e| format!("load {}: {e}", path.display()))?;
        Ok(Transcriber { ctx, threads })
    }

    /// Transcribe 16 kHz mono audio. `prompt` biases the vocabulary.
    pub fn transcribe(&self, samples: &[f32], prompt: &str) -> Result<String, String> {
        let mut pcm = samples.to_vec();
        // whisper needs at least ~1 s; pad short clips with silence
        if pcm.len() < audio::RATE as usize + audio::RATE as usize / 10 {
            pcm.resize(audio::RATE as usize + audio::RATE as usize / 10, 0.0);
        }
        let mut state = self.ctx.create_state().map_err(|e| e.to_string())?;
        let mut p = whisper_rs::FullParams::new(whisper_rs::SamplingStrategy::Greedy { best_of: 1 });
        p.set_n_threads(self.threads);
        p.set_language(Some("en"));
        p.set_translate(false);
        p.set_no_context(true);
        p.set_single_segment(true);
        p.set_no_timestamps(true);
        p.set_print_progress(false);
        p.set_print_realtime(false);
        p.set_print_special(false);
        p.set_print_timestamps(false);
        p.set_suppress_blank(true);
        p.set_initial_prompt(prompt);
        // encode only as much context as the clip needs (short commands → much faster)
        let secs = pcm.len() as f32 / audio::RATE as f32;
        let ctx = ((secs / 30.0) * 1500.0).ceil() as i32 + 128;
        p.set_audio_ctx(ctx.clamp(256, 1500));
        state.full(p, &pcm).map_err(|e| e.to_string())?;
        let mut text = String::new();
        for seg in state.as_iter() {
            if let Ok(s) = seg.to_str_lossy() {
                text.push_str(&s);
            }
        }
        Ok(text.trim().to_string())
    }
}

pub fn spawn(sh: Arc<Shared>, cfg: VoiceCfg, vocab: Vocab) -> VoiceHandle {
    let (tx, rx) = crossbeam_channel::unbounded();
    let status = Arc::new(Mutex::new(VoiceStatus {
        enabled: cfg.enabled,
        state: "idle".into(),
        model: cfg.model.clone(),
        device: cfg.device.clone(),
        ..Default::default()
    }));
    let st2 = status.clone();
    let join = std::thread::Builder::new()
        .name("se-voice".into())
        .spawn(move || {
            let mut w = Worker { sh, cfg, vocab, rx, status: st2, model: None, model_err: None, capture: None, pending: None };
            w.run();
        })
        .ok();
    VoiceHandle { tx, status, join }
}

struct Capture {
    stop: Arc<AtomicBool>,
    join: std::thread::JoinHandle<Result<Vec<f32>, String>>,
    started: Instant,
}

struct Worker {
    sh: Arc<Shared>,
    cfg: VoiceCfg,
    vocab: Vocab,
    rx: Receiver<VoiceCmd>,
    status: Arc<Mutex<VoiceStatus>>,
    model: Option<Transcriber>,
    model_err: Option<String>,
    capture: Option<Capture>,
    pending: Option<(Intent, Instant, Option<se_proto::Id>)>,
}

impl Worker {
    fn set_state(&self, s: &str) {
        self.status.lock().state = s.to_string();
        self.sh.hub.publish("controllers.voice.state", Value::Str(s.to_string()));
        self.sh.hub.publish("controllers.voice.active", Value::Bool(s == "listening"));
        self.sh.health_dirty();
    }

    fn run(&mut self) {
        let h = &self.sh.hub;
        let own = |m: Meta| m.owner("controllers");
        h.declare(
            "controllers.voice.state",
            own(Meta::enumeration("idle", &["idle", "disabled", "downloading", "loading", "listening", "transcribing", "confirm", "error"])
                .readonly()
                .describe("Voice push-to-talk")),
        );
        h.declare("controllers.voice.active", own(Meta::boolean(false).readonly().describe("Push-to-talk is capturing")));
        h.declare("controllers.voice.text", own(Meta::string("").readonly().describe("Last transcript")));
        h.declare("controllers.voice.pending", own(Meta::string("").readonly().describe("Voice command awaiting confirmation")));
        if self.cfg.enabled {
            self.load_model();
        } else {
            self.set_state("disabled");
        }
        loop {
            let timeout = if self.pending.is_some() || self.capture.is_some() { Duration::from_millis(100) } else { Duration::from_secs(1) };
            match self.rx.recv_timeout(timeout) {
                Ok(VoiceCmd::Stop) | Err(RecvTimeoutError::Disconnected) => {
                    if let Some(c) = self.capture.take() {
                        c.stop.store(true, Ordering::Release);
                        let _ = c.join.join();
                    }
                    return;
                }
                Ok(c) => self.command(c),
                Err(RecvTimeoutError::Timeout) => {}
            }
            // auto-stop at the capture limit
            if let Some(c) = &self.capture
                && c.join.is_finished()
            {
                self.stop_capture();
            }
            if let Some((i, at, _)) = &self.pending
                && at.elapsed() > Duration::from_millis(self.cfg.confirm_timeout_ms)
            {
                self.sh.hub.log("info", "voice", format!("confirmation for `{}` timed out", i.describe()));
                self.pending = None;
                self.publish_pending();
                self.set_state("idle");
            }
        }
    }

    fn load_model(&mut self) {
        self.set_state("downloading");
        let st = self.status.clone();
        let hub = self.sh.hub.clone();
        let res = ensure_model(&self.sh.data_dir, &self.cfg.model, move |f| {
            st.lock().download = Some(f);
            hub.publish("controllers.voice.download", Value::Float(f as f64));
        });
        self.status.lock().download = None;
        match res.and_then(|p| {
            self.set_state("loading");
            let t0 = Instant::now();
            let t = Transcriber::load(&p, self.cfg.threads, self.cfg.gpu)?;
            self.sh.hub.log("info", "voice", format!("whisper model {} loaded in {} ms", p.display(), t0.elapsed().as_millis()));
            self.status.lock().model_path = p.display().to_string();
            Ok(t)
        }) {
            Ok(t) => {
                self.model = Some(t);
                self.model_err = None;
                self.status.lock().error = None;
                self.set_state("idle");
            }
            Err(e) => {
                self.sh.hub.log("error", "voice", format!("voice model unavailable: {e}"));
                self.status.lock().error = Some(e.clone());
                self.model_err = Some(e);
                self.set_state("error");
            }
        }
    }

    fn command(&mut self, c: VoiceCmd) {
        match c {
            VoiceCmd::Ptt(op) => {
                let listening = self.capture.is_some();
                match (op, listening) {
                    (Ptt::Start, false) | (Ptt::Toggle, false) => self.start_capture(),
                    (Ptt::Stop, true) | (Ptt::Toggle, true) => self.stop_capture(),
                    _ => {}
                }
            }
            VoiceCmd::Confirm => self.confirm(),
            VoiceCmd::Cancel => self.cancel(),
            VoiceCmd::File(p) => match std::fs::read(&p).map_err(|e| e.to_string()).and_then(|b| audio::read_wav(&b)) {
                Ok(samples) => self.recognize(&samples),
                Err(e) => self.sh.hub.log("error", "voice", format!("{}: {e}", p.display())),
            },
            VoiceCmd::Config(cfg, vocab) => {
                let reload = cfg.model != self.cfg.model || cfg.gpu != self.cfg.gpu || (cfg.enabled && !self.cfg.enabled);
                self.cfg = cfg;
                self.vocab = vocab;
                {
                    let mut s = self.status.lock();
                    s.enabled = self.cfg.enabled;
                    s.model = self.cfg.model.clone();
                    s.device = self.cfg.device.clone();
                }
                if !self.cfg.enabled {
                    self.model = None;
                    self.set_state("disabled");
                } else if reload || self.model.is_none() {
                    self.model = None;
                    self.load_model();
                }
            }
            VoiceCmd::Vocab(v) => self.vocab = v,
            VoiceCmd::Download => {
                if self.cfg.enabled && self.model.is_none() {
                    self.load_model();
                }
            }
            VoiceCmd::Stop => {}
        }
    }

    fn start_capture(&mut self) {
        if !self.cfg.enabled {
            self.sh.hub.log("warn", "voice", "voice is disabled ([voice] enabled = false)");
            return;
        }
        if self.model.is_none() {
            self.sh.hub.log("warn", "voice", format!("push-to-talk ignored: {}", self.model_err.clone().unwrap_or_else(|| "model not loaded yet".into())));
            return;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (s2, dev, max) = (stop.clone(), self.cfg.device.clone(), self.cfg.max_seconds);
        match std::thread::Builder::new().name("se-voice-cap".into()).spawn(move || audio::capture(&dev, s2, max)) {
            Ok(join) => {
                self.capture = Some(Capture { stop, join, started: Instant::now() });
                self.set_state("listening");
            }
            Err(e) => self.sh.hub.log("error", "voice", format!("capture thread: {e}")),
        }
    }

    fn stop_capture(&mut self) {
        let Some(c) = self.capture.take() else { return };
        c.stop.store(true, Ordering::Release);
        let held = c.started.elapsed();
        match c.join.join() {
            Ok(Ok(samples)) => {
                if held < Duration::from_millis(250) || audio::rms(&samples) < 0.002 {
                    self.sh.hub.log("info", "voice", "push-to-talk: nothing heard");
                    self.set_state(if self.pending.is_some() { "confirm" } else { "idle" });
                    return;
                }
                self.recognize(&samples);
            }
            Ok(Err(e)) => {
                self.sh.hub.log("error", "voice", format!("capture failed: {e}"));
                self.status.lock().error = Some(e);
                self.set_state("idle");
            }
            Err(_) => self.set_state("idle"),
        }
    }

    fn recognize(&mut self, samples: &[f32]) {
        let Some(t) = &self.model else {
            self.sh.hub.log("warn", "voice", "no model loaded");
            return;
        };
        self.set_state("transcribing");
        let t0 = Instant::now();
        let text = match t.transcribe(samples, &self.vocab.prompt()) {
            Ok(s) => s,
            Err(e) => {
                self.sh.hub.log("error", "voice", format!("transcription failed: {e}"));
                self.set_state("idle");
                return;
            }
        };
        let ms = t0.elapsed().as_millis() as u64;
        self.sh.hub.publish("controllers.voice.text", Value::Str(text.clone()));
        self.handle_text(&text, ms);
    }

    fn handle_text(&mut self, text: &str, ms: u64) {
        let parsed = grammar::parse(text, &self.vocab);
        let mut heard = Heard {
            text: text.to_string(),
            intent: parsed.as_ref().map(|(i, _)| i.describe()),
            confidence: parsed.as_ref().map(|p| p.1).unwrap_or(0.0),
            ms,
            executed: false,
        };
        let payload = |i: Option<&Intent>, conf: f32, confirm: bool, executed: bool| {
            Value::map()
                .with("text", text)
                .with("intent", i.map(|i| Value::Str(i.kind().into())).unwrap_or_default())
                .with("arg", i.and_then(|i| i.arg()).map(|a| Value::Str(a.into())).unwrap_or_default())
                .with("command", i.map(|i| Value::Str(i.describe())).unwrap_or_default())
                .with("confidence", conf as f64)
                .with("confirm", confirm)
                .with("executed", executed)
                .with("ms", ms as i64)
        };
        match parsed {
            None => {
                self.sh.hub.log("info", "voice", format!("not understood: \"{text}\""));
                self.sh.hub.emit(Event::new("voice.intent", Origin::Voice, payload(None, 0.0, false, false)));
                self.set_state(if self.pending.is_some() { "confirm" } else { "idle" });
            }
            Some((Intent::Confirm, c)) => {
                self.sh.hub.emit(Event::new("voice.intent", Origin::Voice, payload(Some(&Intent::Confirm), c, false, self.pending.is_some())));
                heard.executed = self.pending.is_some();
                self.confirm();
            }
            Some((Intent::Cancel, c)) => {
                self.sh.hub.emit(Event::new("voice.intent", Origin::Voice, payload(Some(&Intent::Cancel), c, false, self.pending.is_some())));
                self.cancel();
            }
            Some((intent, conf)) => {
                let risky = self.cfg.confirm.iter().any(|k| k == intent.kind())
                    || matches!(&intent, Intent::Preset { name } if self.sh.core_config().presets.get(name).is_some_and(|d| d.confirm));
                let ev = Event::new("voice.intent", Origin::Voice, payload(Some(&intent), conf, risky, !risky));
                let id = ev.id;
                self.sh.hub.emit(ev);
                if risky {
                    self.sh.hub.log(
                        "info",
                        "voice",
                        format!("\"{text}\" → {} — say \"confirm\" (or press YES) within {} s", intent.describe(), self.cfg.confirm_timeout_ms / 1000),
                    );
                    self.pending = Some((intent, Instant::now(), Some(id)));
                    self.publish_pending();
                    self.set_state("confirm");
                } else {
                    self.sh.hub.log("info", "voice", format!("\"{text}\" → {}", intent.describe()));
                    if let Some(op) = intent.op() {
                        self.sh.hub.command(Command::new(Origin::Voice, op).caused_by(Some(id)));
                    }
                    heard.executed = true;
                    self.set_state(if self.pending.is_some() { "confirm" } else { "idle" });
                }
            }
        }
        let mut s = self.status.lock();
        s.history.insert(0, heard);
        s.history.truncate(20);
    }

    fn confirm(&mut self) {
        match self.pending.take() {
            Some((i, _, causal)) => {
                self.sh.hub.log("info", "voice", format!("confirmed: {}", i.describe()));
                if let Some(op) = i.op() {
                    self.sh.hub.command(Command::new(Origin::Voice, op).caused_by(causal));
                }
            }
            None => self.sh.hub.log("info", "voice", "nothing to confirm"),
        }
        self.publish_pending();
        self.set_state("idle");
    }

    fn cancel(&mut self) {
        if let Some((i, _, _)) = self.pending.take() {
            self.sh.hub.log("info", "voice", format!("cancelled: {}", i.describe()));
        }
        self.publish_pending();
        self.set_state("idle");
    }

    fn publish_pending(&self) {
        let d = self.pending.as_ref().map(|(i, _, _)| i.describe());
        self.status.lock().pending = d.clone();
        self.sh.hub.publish("controllers.voice.pending", Value::Str(d.unwrap_or_default()));
    }
}

/// Vocabulary from the project (scenes, presets, modes, primary deck pages).
pub fn vocab(cfg: &se_core::Config, pages: &[(String, String)]) -> Vocab {
    Vocab {
        scenes: cfg.scenes.iter().map(|(n, s)| (n.clone(), Vocab::forms(n, s.label.as_deref()))).collect(),
        presets: cfg.presets.iter().map(|(n, p)| (n.clone(), Vocab::forms(n, p.label.as_deref()))).collect(),
        modes: cfg.modes().into_iter().map(|m| (m.clone(), Vocab::forms(&m, None))).collect(),
        pages: pages.iter().map(|(n, l)| (n.clone(), Vocab::forms(n, Some(l)))).collect(),
    }
}
