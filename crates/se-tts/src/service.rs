//! The engine subsystem: request queue, synthesis worker thread, pacing player thread,
//! actions, state, events, health, query, and deletion sync (PLAN §12.1, §14.4).
//!
//! ```text
//! tts.say ─► queue ─► worker (espeak + Kokoro, one item ahead) ─► ready ─► player ─► `tts` slot
//!                                                                   (24→48 kHz, ≤ 0.15 s buffered)
//! ```
//! All three share one mutex (never taken on an audio thread: the player is a pacing thread
//! feeding a lock-free ring; the audio graph only reads the ring).

use crate::config::{TtsConfig, valid_voice_name};
use crate::kokoro::{self, SAMPLE_RATE, Synth};
use crate::phonemize::Espeak;
use crate::resample::Upsampler2x;
use crate::text;
use parking_lot::{Condvar, Mutex};
use se_hub::{Bus, EngineCtx, Hub};
use se_proto::{Event, Meta, Op, Origin, Value};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::AsyncBufReadExt;

const TARGET: &str = "tts";
const OUT_RATE: u32 = 48_000;
const RING_SECONDS: f32 = 0.5;
/// Ring fill the player keeps topped up to: the bound on skip latency.
const FILL_TARGET: usize = OUT_RATE as usize * 15 / 100;
const TICK: Duration = Duration::from_millis(5);
/// Fade-out on skip (48 kHz samples, 12 ms).
const FADE_OUT: usize = OUT_RATE as usize * 12 / 1000;
/// Volume changes glide with this time constant (no zipper noise).
const GAIN_TAU_S: f32 = 0.02;
/// No ring consumption for this long = nobody reads the `tts` slot.
const STALL_AFTER: Duration = Duration::from_secs(1);
/// ONNX Runtime intra-op threads for synthesis.
pub const INTRA_THREADS: usize = 4;

#[derive(Clone, Debug)]
struct Item {
    seq: u64,
    id: String,
    user: String,
    user_id: String,
    message_id: String,
    voice: String,
    speed: f32,
    lang: String,
    text: String,
    cancel: Arc<AtomicBool>,
}

impl Item {
    fn info(&self) -> Value {
        Value::map().with("id", self.id.as_str()).with("user", self.user.as_str()).with("voice", self.voice.as_str()).with("text", self.text.as_str())
    }
}

#[derive(Clone, Debug, PartialEq)]
enum ModelState {
    Disabled,
    Loading,
    Loaded { model: String },
    Failed(String),
}

struct Inner {
    cfg: Arc<TtsConfig>,
    /// Bumped when the worker must re-check the model (config change, fetch finished).
    generation: u64,
    queue: VecDeque<Item>,
    /// Synthesized audio of `queue`'s front, waiting for the player.
    ready: Option<(u64, Vec<f32>)>,
    current: Option<Item>,
    model: ModelState,
    voices: Vec<String>,
    espeak: Result<String, String>,
    /// Last line of a running/failed `tts.model.fetch`.
    fetch: Option<String>,
    fetching: bool,
    stalled: bool,
    shutdown: bool,
}

impl Inner {
    fn ready(&self) -> bool {
        self.cfg.enabled && self.espeak.is_ok() && matches!(self.model, ModelState::Loaded { .. }) && self.voices.contains(&self.cfg.voice)
    }

    fn model_status(&self) -> String {
        if let Some(f) = &self.fetch {
            return f.clone();
        }
        match &self.model {
            ModelState::Disabled => "disabled".into(),
            ModelState::Loading => "loading".into(),
            ModelState::Loaded { model } => format!("loaded {model} ({} voices)", self.voices.len()),
            ModelState::Failed(e) => format!("error: {e}"),
        }
    }

    fn health(&self) -> (&'static str, String) {
        let fetch_hint = format!("run `stream do tts.model.fetch` (or scripts/fetch-tts-model.sh --dir {})", self.cfg.model_dir.display());
        if !self.cfg.enabled {
            return ("warn", "disabled in project.toml ([tts] enabled = false)".into());
        }
        if let Err(e) = &self.espeak {
            return ("fail", format!("{e}; install the espeak-ng package"));
        }
        match &self.model {
            ModelState::Failed(e) => return ("fail", format!("{e} — {fetch_hint}")),
            ModelState::Loading | ModelState::Disabled => return ("warn", "loading Kokoro model".into()),
            ModelState::Loaded { .. } => {}
        }
        if !self.voices.contains(&self.cfg.voice) {
            return ("fail", format!("voice `{}` missing in {} — {fetch_hint}", self.cfg.voice, self.cfg.voices_dir().display()));
        }
        let ModelState::Loaded { model } = &self.model else { unreachable!() };
        let espeak = self.espeak.as_deref().unwrap_or_default();
        if self.stalled {
            return ("warn", format!("Kokoro {model} loaded, {} voices; audio slot `tts` is not being consumed (audio graph not running?)", self.voices.len()));
        }
        ("pass", format!("Kokoro {model} loaded, {} voices, espeak-ng {espeak}", self.voices.len()))
    }
}

struct Shared {
    hub: Arc<Hub>,
    inner: Mutex<Inner>,
    /// Wakes the synthesis worker (queue, config, or ready slot changed).
    work: Condvar,
    /// Wakes the idle player (audio became ready).
    idle: Condvar,
    volume: AtomicU32,
    seq: AtomicU64,
}

impl Shared {
    fn publish(&self, g: &Inner) {
        let h = &self.hub;
        h.publish("tts.queue", Value::Int(g.queue.len() as i64));
        h.publish("tts.speaking", Value::Bool(g.current.is_some()));
        let cur = g.current.as_ref();
        h.publish("tts.current.id", cur.map(|c| c.id.as_str()).unwrap_or("").into());
        h.publish("tts.current.user", cur.map(|c| c.user.as_str()).unwrap_or("").into());
        h.publish("tts.current.voice", cur.map(|c| c.voice.as_str()).unwrap_or("").into());
        h.publish("tts.current.text", cur.map(|c| c.text.as_str()).unwrap_or("").into());
        h.publish("tts.model.status", g.model_status().into());
        h.publish("tts.ready", Value::Bool(g.ready()));
        let (status, detail) = g.health();
        h.publish("health.tts", Value::map().with("status", status).with("detail", detail));
    }

    fn finished(&self, id: &str, skipped: bool, error: Option<&str>) {
        let mut p = Value::map().with("id", id).with("skipped", skipped);
        if let Some(e) = error {
            p = p.with("error", e);
        }
        self.hub.emit(Event::new("tts.finished", Origin::System, p));
    }

    /// Remove queued items matching `pred` (cancelling their synthesis) and stop the current
    /// item if `current` says so. Emits `tts.finished {skipped}` for dropped queued items; the
    /// player emits it for the current one once its fade-out completes.
    fn drop_where(&self, pred: impl Fn(&Item) -> bool, current: bool, reason: &str) -> usize {
        let mut dropped = Vec::new();
        {
            let mut g = self.inner.lock();
            let mut keep = VecDeque::with_capacity(g.queue.len());
            for it in g.queue.drain(..) {
                if pred(&it) {
                    it.cancel.store(true, Ordering::Relaxed);
                    dropped.push(it.id.clone());
                } else {
                    keep.push_back(it);
                }
            }
            g.queue = keep;
            if let Some((seq, _)) = &g.ready
                && !g.queue.iter().any(|q| q.seq == *seq)
            {
                g.ready = None;
            }
            if current && let Some(c) = &g.current {
                c.cancel.store(true, Ordering::Relaxed);
            }
            self.publish(&g);
        }
        self.work.notify_all();
        for id in &dropped {
            self.finished(id, true, Some(reason));
        }
        dropped.len()
    }
}

/// Start the TTS subsystem (see the crate docs for config, actions, state, and events).
pub async fn start(ctx: EngineCtx) -> anyhow::Result<()> {
    let hub = ctx.hub.clone();
    declare(&hub);
    let cfg = match TtsConfig::parse(ctx.project_section("tts").as_ref(), &ctx.data_dir) {
        Ok(c) => c,
        Err(e) => {
            hub.log("error", TARGET, format!("{e} (using defaults)"));
            TtsConfig::defaults(&ctx.data_dir)
        }
    };
    let espeak = tokio::task::spawn_blocking(|| Espeak::default().version().map_err(|e| e.to_string())).await?;
    let shared = Arc::new(Shared {
        hub: hub.clone(),
        volume: AtomicU32::new(cfg.volume.to_bits()),
        inner: Mutex::new(Inner {
            voices: kokoro::list_voices(&cfg.voices_dir()),
            cfg: Arc::new(cfg),
            generation: 1,
            queue: VecDeque::new(),
            ready: None,
            current: None,
            model: ModelState::Loading,
            espeak,
            fetch: None,
            fetching: false,
            stalled: false,
            shutdown: false,
        }),
        work: Condvar::new(),
        idle: Condvar::new(),
        seq: AtomicU64::new(1),
    });
    shared.publish(&shared.inner.lock());

    let producer = hub.audio.register("tts", 1, OUT_RATE, RING_SECONDS);
    let s = shared.clone();
    std::thread::Builder::new().name("tts-synth".into()).spawn(move || worker(s))?;
    let s = shared.clone();
    std::thread::Builder::new().name("tts-player".into()).spawn(move || player(s, producer))?;

    let s = shared.clone();
    hub.register_query(
        "tts",
        Arc::new(move |_, _| {
            let v = query(&s);
            Box::pin(async move { Ok(v) })
        }),
    );

    let mut actions = hub.route_actions("tts");
    let s = shared.clone();
    let share_dir = ctx.share_dir.clone();
    tokio::spawn(async move {
        while let Some(cmd) = actions.recv().await {
            if let Op::Action { name, args } = cmd.op {
                action(&s, &share_dir, &name, &args);
            }
        }
    });

    let mut bus = hub.subscribe();
    let s = shared.clone();
    tokio::spawn(async move {
        loop {
            match bus.recv().await {
                Ok(msg) => on_bus(&s, &msg),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    s.hub.log("warn", TARGET, format!("bus lagged by {n} messages; deletion sync may have missed events"));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
        let mut g = s.inner.lock();
        g.shutdown = true;
        drop(g);
        s.work.notify_all();
        s.idle.notify_all();
    });

    let mut config = ctx.config.clone();
    let s = shared.clone();
    let data_dir = ctx.data_dir.clone();
    tokio::spawn(async move {
        while config.changed().await.is_ok() {
            let section = config.borrow().project.extra.get("tts").cloned();
            match TtsConfig::parse(section.as_ref(), &data_dir) {
                Ok(c) => apply_config(&s, c).await,
                Err(e) => s.hub.log("error", TARGET, format!("{e} (keeping the previous [tts] settings)")),
            }
        }
    });
    Ok(())
}

fn declare(hub: &Hub) {
    let own = |m: Meta, d: &str| m.owner("tts").describe(d);
    hub.declare("tts.enabled", own(Meta::boolean(true), "Speak TTS requests (off = stop and clear)"));
    hub.declare("tts.speaking", own(Meta::boolean(false), "TTS audio is playing").readonly());
    hub.declare("tts.queue", own(Meta::int(0, [0.0, 500.0]), "Waiting TTS requests").readonly());
    hub.declare("tts.current.id", own(Meta::string(""), "Id of the request being spoken").readonly());
    hub.declare("tts.current.user", own(Meta::string(""), "User of the request being spoken").readonly());
    hub.declare("tts.current.voice", own(Meta::string(""), "Voice being spoken").readonly());
    hub.declare("tts.current.text", own(Meta::string(""), "Text being spoken").readonly());
    hub.declare("tts.model.status", own(Meta::string(""), "Kokoro model status").readonly());
    hub.declare("tts.ready", own(Meta::boolean(false), "Model loaded and espeak-ng available").readonly());
}

async fn apply_config(s: &Arc<Shared>, c: TtsConfig) {
    let espeak = tokio::task::spawn_blocking(|| Espeak::default().version().map_err(|e| e.to_string())).await.unwrap_or_else(|e| Err(e.to_string()));
    let voices = kokoro::list_voices(&c.voices_dir());
    let disable = !c.enabled;
    {
        let mut g = s.inner.lock();
        let reload = g.cfg.model_path() != c.model_path() || g.cfg.enabled != c.enabled;
        s.volume.store(c.volume.to_bits(), Ordering::Relaxed);
        g.cfg = Arc::new(c);
        g.espeak = espeak;
        g.voices = voices;
        if reload {
            g.generation += 1;
        }
        s.publish(&g);
    }
    s.work.notify_all();
    if disable {
        s.drop_where(|_| true, true, "tts disabled");
    }
}

fn str_arg(args: &Value, key: &str) -> Option<String> {
    match args.get_path(key)? {
        Value::Null => None,
        Value::Str(s) => Some(s.clone()),
        other => Some(other.to_string()),
    }
}

/// Positional args (`tts.say hello there`) joined, else the named key.
fn text_arg(args: &Value, key: &str) -> Option<String> {
    str_arg(args, key).or_else(|| {
        let pos = args.get_path("args")?.as_list()?;
        let s: Vec<String> = pos.iter().map(|v| v.to_string()).collect();
        (!s.is_empty()).then(|| s.join(" "))
    })
}

fn user_enabled(hub: &Hub) -> bool {
    hub.snapshot.load().get("tts.enabled").is_none_or(Value::truthy)
}

fn action(s: &Arc<Shared>, share_dir: &std::path::Path, name: &str, args: &Value) {
    match name {
        "tts.say" => say(s, args),
        "tts.skip" => match text_arg(args, "id") {
            Some(id) => {
                let n = s.drop_where(|it| it.id == id, false, "skipped");
                let g = s.inner.lock();
                let cur = g.current.as_ref().filter(|c| c.id == id);
                if let Some(c) = cur {
                    c.cancel.store(true, Ordering::Relaxed);
                } else if n == 0 {
                    s.hub.log("info", TARGET, format!("tts.skip: no queued or playing item `{id}`"));
                }
            }
            None => {
                let g = s.inner.lock();
                if let Some(c) = &g.current {
                    c.cancel.store(true, Ordering::Relaxed);
                }
            }
        },
        "tts.clear" => {
            s.drop_where(|_| true, true, "cleared");
        }
        "tts.model.fetch" => fetch(s, share_dir),
        other => s.hub.log("warn", TARGET, format!("unknown action `{other}` (tts.say, tts.skip, tts.clear, tts.model.fetch)")),
    }
}

fn say(s: &Arc<Shared>, args: &Value) {
    let seq = s.seq.fetch_add(1, Ordering::Relaxed);
    let id = str_arg(args, "id").unwrap_or_else(|| format!("tts-{seq}"));
    let reject = |why: String| {
        s.hub.log("info", TARGET, format!("tts.say `{id}` dropped: {why}"));
        s.finished(&id, true, Some(&why));
    };
    let Some(raw) = text_arg(args, "text") else { return reject("no text".into()) };
    if !user_enabled(&s.hub) {
        return reject("tts.enabled is off".into());
    }
    let mut g = s.inner.lock();
    if !g.ready() {
        let why = format!("not ready ({})", g.health().1);
        drop(g);
        return reject(why);
    }
    if g.queue.len() >= g.cfg.max_queue {
        let why = format!("queue full ({} items)", g.cfg.max_queue);
        drop(g);
        return reject(why);
    }
    let text = text::normalize(&raw, g.cfg.max_chars);
    if !text.chars().any(char::is_alphanumeric) {
        drop(g);
        return reject("nothing speakable after normalization".into());
    }
    let explicit = str_arg(args, "voice").filter(|v| {
        let ok = valid_voice_name(v) && g.voices.contains(v);
        if !ok {
            s.hub.log("warn", TARGET, format!("tts.say `{id}`: voice `{v}` not installed; using the configured voice"));
        }
        ok
    });
    let mut sel = g.cfg.select(args, explicit.as_deref());
    if !g.voices.contains(&sel.voice) {
        s.hub.log("warn", TARGET, format!("voice `{}` (from [[tts.voices]]) not installed; using `{}`", sel.voice, g.cfg.voice));
        sel = g.cfg.select(args, Some(&g.cfg.voice.clone()));
    }
    g.queue.push_back(Item {
        seq,
        id,
        user: str_arg(args, "user").unwrap_or_default(),
        user_id: str_arg(args, "user_id").unwrap_or_default(),
        message_id: str_arg(args, "message_id").unwrap_or_default(),
        voice: sel.voice,
        speed: sel.speed,
        lang: sel.lang,
        text,
        cancel: Arc::new(AtomicBool::new(false)),
    });
    s.publish(&g);
    drop(g);
    s.work.notify_all();
}

fn on_bus(s: &Arc<Shared>, msg: &Bus) {
    match msg {
        Bus::Event(e) => match e.ty.as_str() {
            // every ban/timeout/clear-user also emits this (se-twitch normalize)
            "twitch.user.purge" => {
                let uid = str_arg(&e.payload, "user_id").unwrap_or_default();
                let user = str_arg(&e.payload, "user").unwrap_or_default();
                if uid.is_empty() && user.is_empty() {
                    return;
                }
                let matches = |it: &Item| (!uid.is_empty() && it.user_id == uid) || (!user.is_empty() && it.user.eq_ignore_ascii_case(&user));
                let hit_current = s.inner.lock().current.as_ref().is_some_and(matches);
                let n = s.drop_where(matches, hit_current, "user banned or timed out");
                if n > 0 || hit_current {
                    s.hub.log(
                        "info",
                        TARGET,
                        format!("deletion sync: dropped {n} queued item(s) of {user}{}", if hit_current { " and stopped the current one" } else { "" }),
                    );
                }
            }
            "twitch.chat.delete" => {
                let mid = str_arg(&e.payload, "message_id").unwrap_or_default();
                if !mid.is_empty() {
                    let hit_current = s.inner.lock().current.as_ref().is_some_and(|c| c.message_id == mid);
                    s.drop_where(|it| it.message_id == mid, hit_current, "message deleted");
                }
            }
            // chat cleared: drop the queue, the current item finishes
            "twitch.chat.clear" => {
                s.drop_where(|_| true, false, "chat cleared");
            }
            _ => {}
        },
        Bus::Changes(changes) => {
            if let Some((_, v)) = changes.iter().find(|(a, _)| a == "tts.enabled")
                && !v.truthy()
            {
                s.drop_where(|_| true, true, "tts disabled");
            }
        }
        _ => {}
    }
}

fn query(s: &Shared) -> Value {
    let g = s.inner.lock();
    Value::map()
        .with("ready", g.ready())
        .with("enabled", user_enabled(&s.hub) && g.cfg.enabled)
        .with("current", g.current.as_ref().map(Item::info).unwrap_or(Value::Null))
        .with("queue", Value::List(g.queue.iter().map(Item::info).collect()))
        .with("voices", Value::List(g.voices.iter().map(|v| Value::from(v.as_str())).collect()))
        .with("voice", g.cfg.voice.as_str())
        .with("model_status", g.model_status())
}

/// Run `<share>/scripts/fetch-tts-model.sh --dir <model_dir> --model <model>` in the
/// background; progress goes to `tts.model.status`, success reloads the model.
fn fetch(s: &Arc<Shared>, share_dir: &std::path::Path) {
    let script = share_dir.join("scripts").join("fetch-tts-model.sh");
    let (dir, model) = {
        let mut g = s.inner.lock();
        if g.fetching {
            s.hub.log("info", TARGET, "tts.model.fetch already running");
            return;
        }
        g.fetching = true;
        g.fetch = Some("fetching: starting".into());
        s.publish(&g);
        (g.cfg.model_dir.clone(), g.cfg.model.clone())
    };
    let s = s.clone();
    tokio::spawn(async move {
        let result = run_fetch(&s, &script, &dir, &model).await;
        let voices = kokoro::list_voices(&dir.join("voices"));
        {
            let mut g = s.inner.lock();
            g.fetching = false;
            g.voices = voices;
            match &result {
                Ok(()) => {
                    g.fetch = None;
                    g.generation += 1;
                }
                Err(e) => g.fetch = Some(format!("fetch failed: {e}")),
            }
            s.publish(&g);
        }
        s.work.notify_all();
        match result {
            Ok(()) => s.hub.log("info", TARGET, format!("Kokoro model files verified in {}", dir.display())),
            Err(e) => s.hub.log("error", TARGET, format!("tts.model.fetch failed: {e}")),
        }
    });
}

async fn run_fetch(s: &Arc<Shared>, script: &std::path::Path, dir: &std::path::Path, model: &str) -> Result<(), String> {
    if !script.is_file() {
        return Err(format!("{} not found", script.display()));
    }
    let mut child = tokio::process::Command::new("bash")
        .arg(script)
        .arg("--dir")
        .arg(dir)
        .arg("--model")
        .arg(model)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("cannot run {}: {e}", script.display()))?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let stderr = child.stderr.take().ok_or("no stderr")?;
    let err_task = tokio::spawn(async move {
        let mut lines = tokio::io::BufReader::new(stderr).lines();
        let mut last = String::new();
        while let Ok(Some(l)) = lines.next_line().await {
            if !l.trim().is_empty() {
                last = l;
            }
        }
        last
    });
    let mut lines = tokio::io::BufReader::new(stdout).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        let mut g = s.inner.lock();
        g.fetch = Some(format!("fetching: {line}"));
        s.hub.publish("tts.model.status", g.model_status().into());
    }
    let status = child.wait().await.map_err(|e| e.to_string())?;
    let last_err = err_task.await.unwrap_or_default();
    if status.success() {
        Ok(())
    } else if last_err.is_empty() {
        Err(format!("fetch script exited with {status}"))
    } else {
        Err(last_err.trim_start_matches("error: ").to_string())
    }
}

enum Job {
    Reload(Arc<TtsConfig>, u64),
    Synth(Item),
}

/// Synthesis worker: owns the ONNX session; synthesizes the queue front one item ahead of
/// the player.
fn worker(s: Arc<Shared>) {
    let mut synth: Option<Synth> = None;
    let mut loaded: Option<PathBuf> = None;
    let mut seen_gen = 0;
    loop {
        let job = {
            let mut g = s.inner.lock();
            loop {
                if g.shutdown {
                    return;
                }
                if g.generation != seen_gen {
                    break Job::Reload(g.cfg.clone(), g.generation);
                }
                if synth.is_some()
                    && g.ready.is_none()
                    && let Some(front) = g.queue.front()
                {
                    break Job::Synth(front.clone());
                }
                s.work.wait(&mut g);
            }
        };
        match job {
            Job::Reload(cfg, generation) => {
                seen_gen = generation;
                if !cfg.enabled {
                    synth = None;
                    loaded = None;
                    set_model(&s, ModelState::Disabled);
                    continue;
                }
                let path = cfg.model_path();
                if synth.is_some() && loaded.as_ref() == Some(&path) {
                    continue;
                }
                synth = None;
                set_model(&s, ModelState::Loading);
                let t0 = Instant::now();
                match Synth::load(&path, &cfg.voices_dir(), INTRA_THREADS, Espeak::default()) {
                    Ok(sy) => {
                        s.hub.log("info", TARGET, format!("loaded {} in {:.1}s: {}", path.display(), t0.elapsed().as_secs_f32(), sy.model.signature()));
                        synth = Some(sy);
                        loaded = Some(path);
                        set_model(&s, ModelState::Loaded { model: cfg.model.clone() });
                    }
                    Err(e) => {
                        loaded = None;
                        s.hub.log("error", TARGET, format!("{e:#}"));
                        set_model(&s, ModelState::Failed(format!("{e:#}")));
                    }
                }
            }
            Job::Synth(item) => {
                let Some(sy) = synth.as_mut() else { continue };
                let t0 = Instant::now();
                let cancel = item.cancel.clone();
                let res = sy.speak(&item.text, &item.voice, &item.lang, item.speed, &|| cancel.load(Ordering::Relaxed));
                let mut g = s.inner.lock();
                let queued = g.queue.iter().any(|q| q.seq == item.seq);
                match res {
                    Ok(Some(speech)) if queued && !item.cancel.load(Ordering::Relaxed) => {
                        let secs = speech.samples.len() as f32 / SAMPLE_RATE as f32;
                        tracing::debug!(target: "tts", id = %item.id, audio_s = secs, rtf = t0.elapsed().as_secs_f32() / secs.max(1e-3), "synthesized");
                        if speech.samples.is_empty() {
                            g.queue.retain(|q| q.seq != item.seq);
                            s.publish(&g);
                            drop(g);
                            s.finished(&item.id, true, Some("no audio"));
                            continue;
                        }
                        g.ready = Some((item.seq, speech.samples));
                        s.idle.notify_all();
                    }
                    Ok(_) => {}
                    Err(e) => {
                        let queued_before = g.queue.len();
                        g.queue.retain(|q| q.seq != item.seq);
                        let removed = g.queue.len() != queued_before;
                        s.publish(&g);
                        drop(g);
                        s.hub.log("error", TARGET, format!("synthesis of `{}` failed: {e:#}", item.id));
                        if removed {
                            s.finished(&item.id, true, Some(&format!("{e:#}")));
                        }
                    }
                }
            }
        }
    }
}

fn set_model(s: &Shared, m: ModelState) {
    let mut g = s.inner.lock();
    g.model = m;
    g.voices = kokoro::list_voices(&g.cfg.voices_dir());
    s.publish(&g);
}

struct Playback {
    item: Item,
    samples: Vec<f32>,
    pos: usize,
    /// Remaining fade-out samples (48 kHz) once a skip started.
    fade: Option<usize>,
    flushed: bool,
    /// Set once every sample is in the ring: when the listener has heard the end.
    done_at: Option<Instant>,
}

/// Pacing thread: keeps ≤ `FILL_TARGET` samples in the `tts` ring, resamples on the fly,
/// applies volume and the skip fade, and emits `tts.started` / `tts.finished`.
fn player(s: Arc<Shared>, mut prod: rtrb::Producer<f32>) {
    let cap = prod.buffer().capacity();
    let mut up = Upsampler2x::new();
    let mut play: Option<Playback> = None;
    let mut gain = f32::from_bits(s.volume.load(Ordering::Relaxed));
    let gain_a = 1.0 - (-1.0 / (GAIN_TAU_S * OUT_RATE as f32)).exp();
    let mut block = [0f32; 1024];
    let mut last_fill = 0usize;
    let mut last_drain = Instant::now();
    let mut last_tick = Instant::now();
    let mut stalled = false;
    loop {
        if play.is_none() {
            let mut g = s.inner.lock();
            if g.ready.is_none() && !g.shutdown {
                s.idle.wait_for(&mut g, Duration::from_millis(250));
            }
        } else {
            std::thread::sleep(TICK);
        }
        let now = Instant::now();
        let elapsed = now - last_tick;
        last_tick = now;
        let fill = cap - prod.slots();
        if fill < last_fill || fill == 0 {
            last_drain = now;
        }
        let is_stalled = prod.is_abandoned() || (fill > 0 && now - last_drain > STALL_AFTER);
        if is_stalled != stalled {
            stalled = is_stalled;
            let mut g = s.inner.lock();
            if g.shutdown {
                return;
            }
            g.stalled = stalled;
            s.publish(&g);
        }

        if play.is_none() {
            let mut g = s.inner.lock();
            if g.shutdown {
                return;
            }
            if let Some((seq, samples)) = g.ready.take()
                && let Some(i) = g.queue.iter().position(|q| q.seq == seq)
            {
                let item = g.queue.remove(i).expect("position is in range");
                g.current = Some(item.clone());
                s.publish(&g);
                drop(g);
                s.work.notify_all();
                let duration_ms = (samples.len() as u64 * 1000) / SAMPLE_RATE as u64;
                s.hub.emit(Event::new(
                    "tts.started",
                    Origin::System,
                    Value::map()
                        .with("id", item.id.as_str())
                        .with("user", item.user.as_str())
                        .with("voice", item.voice.as_str())
                        .with("duration_ms", duration_ms as i64),
                ));
                up.reset();
                last_tick = now;
                play = Some(Playback { item, samples, pos: 0, fade: None, flushed: false, done_at: None });
            }
        }
        let Some(p) = play.as_mut() else {
            last_fill = cap - prod.slots();
            continue;
        };

        if p.fade.is_none() && p.done_at.is_none() && p.item.cancel.load(Ordering::Relaxed) {
            p.fade = Some(FADE_OUT);
        }
        let volume = f32::from_bits(s.volume.load(Ordering::Relaxed));
        // Output samples to produce this tick: top the ring up, or (nobody reading) follow
        // the wall clock so items still take their real duration.
        let mut want = (if stalled { (elapsed.as_secs_f64() * OUT_RATE as f64) as usize } else { FILL_TARGET.saturating_sub(fill) }) & !1;
        while want > 0 && p.done_at.is_none() {
            let n = want.min(block.len());
            let mut produced = 0;
            while produced + 2 <= n {
                let x = if p.pos < p.samples.len() {
                    let v = p.samples[p.pos];
                    p.pos += 1;
                    v
                } else if !p.flushed && p.pos < p.samples.len() + Upsampler2x::FLUSH_INPUTS {
                    p.pos += 1;
                    0.0
                } else {
                    p.flushed = true;
                    break;
                };
                let pair = up.push(x);
                for v in pair {
                    let target = match p.fade.as_mut() {
                        Some(r) => {
                            *r = r.saturating_sub(1);
                            volume * (*r as f32 / FADE_OUT as f32)
                        }
                        None => volume,
                    };
                    gain += (target - gain) * gain_a;
                    let g = if p.fade.is_some() { gain.min(target) } else { gain };
                    block[produced] = v * g;
                    produced += 1;
                }
                if p.fade == Some(0) {
                    break;
                }
            }
            if !stalled && produced > 0 {
                let space = prod.slots().min(produced);
                if let Ok(chunk) = prod.write_chunk_uninit(space) {
                    chunk.fill_from_iter(block[..space].iter().copied());
                }
            }
            want -= produced.min(want);
            if p.flushed || p.fade == Some(0) {
                let buffered = if stalled { 0 } else { cap - prod.slots() };
                p.done_at = Some(now + Duration::from_secs_f64(buffered as f64 / OUT_RATE as f64));
            }
            if produced < n {
                break;
            }
        }
        if let Some(t) = p.done_at
            && now >= t
        {
            let skipped = p.fade.is_some();
            let id = p.item.id.clone();
            play = None;
            {
                let mut g = s.inner.lock();
                g.current = None;
                s.publish(&g);
            }
            s.work.notify_all();
            s.finished(&id, skipped, None);
        }
        last_fill = cap - prod.slots();
    }
}
