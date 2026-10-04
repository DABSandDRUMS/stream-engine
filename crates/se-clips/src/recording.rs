//! App-owned recording lifecycle and durable per-session show metadata.
//!
//! Auto-record follows `show.mode`; a manual stop (or `recording.finalize`) inhibits it until
//! the next mode change or an explicit start. Failures never inhibit it: encoder children are
//! restarted by the capture supervisor, and a recording that cannot start is retried with
//! bounded backoff while the show stays in an auto mode.

use crate::{
    capture::{self, policy::Backoff},
    show::{RecordingConfig, Role, SourceKind, show_dir_from_meta},
};
use parking_lot::Mutex;
use se_hub::EngineCtx;
use se_proto::{Meta, Op, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};

const TARGET: &str = "recording";

struct State {
    config: RecordingConfig,
    config_error: Option<String>,
    selected: Option<RecordingConfig>,
    session: String,
    show_dir: Option<PathBuf>,
    /// Why the recording folder could not be used (the master records to the fallback).
    fallback: Option<String>,
    previous: Vec<Value>,
    /// Files of the running recording (every segment of every input).
    files: Vec<capture::Recording>,
    feeds: Option<capture::Status>,
    /// `begin` is preparing folders.
    preparing: bool,
    /// The capture supervisor is alive.
    running: bool,
    master_started: bool,
    stopping: bool,
    stop: Option<Arc<AtomicBool>>,
    error: Option<String>,
    retry: Backoff,
    retry_at: Option<Instant>,
    pending_meta: Option<Value>,
}

impl State {
    fn new(config: RecordingConfig) -> Self {
        Self {
            config,
            config_error: None,
            selected: None,
            session: String::new(),
            show_dir: None,
            fallback: None,
            previous: Vec::new(),
            files: Vec::new(),
            feeds: None,
            preparing: false,
            running: false,
            master_started: false,
            stopping: false,
            stop: None,
            error: None,
            retry: Backoff::default(),
            retry_at: None,
            pending_meta: None,
        }
    }
    fn busy(&self) -> bool {
        self.preparing || self.running || self.stopping
    }
    fn starting(&self) -> bool {
        self.preparing || (self.running && !self.master_started && !self.stopping)
    }
    fn active(&self) -> bool {
        self.running && self.master_started && !self.stopping
    }
    fn recordings(&self) -> Value {
        Value::List(self.previous.iter().cloned().chain(self.files.iter().map(capture::Recording::value)).collect())
    }
    fn meta(&self) -> Value {
        Value::map().with("recordings", self.recordings())
    }
}

#[derive(Default)]
struct AutoGate {
    mode: String,
    armed: bool,
    inhibited: bool,
}
impl AutoGate {
    fn mode(&mut self, mode: &str) -> bool {
        let offline = self.mode != mode && mode == "offline";
        if self.mode != mode {
            self.mode = mode.into();
            self.inhibited = false;
            if offline { self.armed = false; }
        }
        offline
    }
    fn wants(&self, config: &RecordingConfig) -> bool {
        config.auto && !self.inhibited && self.mode != "offline" && (self.armed || config.modes.contains(&self.mode))
    }
    /// Manual stop / finalize only. Failures are retried, never inhibit.
    fn stop(&mut self) { self.inhibited = true; self.armed = false; }
}

fn config(ctx: &EngineCtx) -> Result<RecordingConfig, String> {
    let mut cfg = RecordingConfig::from_section(ctx.project_section("recording").as_ref())?;
    if !cfg.dir_path().is_absolute() {
        return Err(format!("[recording] dir must be absolute (or start with ~/): {}", cfg.dir));
    }
    if !cfg.fallback_path().is_absolute() {
        return Err(format!("[recording] fallback_dir must be absolute (or start with ~/): {}", cfg.fallback_dir));
    }
    for mode in &cfg.modes {
        if !ctx.config.borrow().project.modes.contains(mode) || mode == "offline" {
            return Err(format!("[recording] modes contains `{mode}`; choose an on-air show mode"));
        }
    }
    if cfg.frames_socket.is_empty() {
        cfg.frames_socket = ctx.project_section("render").and_then(|v| v.get("frames_socket").and_then(toml::Value::as_str).map(str::to_owned)).unwrap_or_default();
    }
    Ok(cfg)
}

fn show_name(id: &str) -> String {
    // Session creation may be hours earlier than the first recording.
    show_name_at(id, &se_store::session::new_session_id())
}

fn show_name_at(id: &str, stamp: &str) -> String {
    format!("{}-{}-{} {}-{} ({id})", &stamp[..4], &stamp[4..6], &stamp[6..8], &stamp[9..11], &stamp[11..13])
}

fn snapshot_project(root: &Path, show: &Path) -> Result<(), String> {
    let dest = show.join("data/project");
    if dest.exists() {
        return Ok(());
    }
    let partial = show.join("data/.project.partial");
    if partial.exists() {
        std::fs::remove_dir_all(&partial).map_err(|e| format!("cannot clear interrupted project snapshot: {e}"))?;
    }
    std::fs::create_dir_all(&partial).map_err(|e| format!("cannot create project snapshot: {e}"))?;
    fn copy_text(root: &Path, dest: &Path, show: &Path, depth: usize) -> std::io::Result<()> {
        if depth > 8 {
            return Ok(());
        }
        for item in std::fs::read_dir(root)? {
            let item = item?;
            if item.path().starts_with(show) {
                continue;
            }
            let name = item.file_name();
            let name = name.to_string_lossy();
            let lower = name.to_ascii_lowercase();
            if name.starts_with('.')
                || matches!(lower.as_str(), "assets" | "sessions" | "secrets" | "credentials" | "private" | "node_modules" | "target")
                || [".env", "secret", "credential", "password", "token", "private_key"].iter().any(|s| lower.contains(s))
            {
                continue;
            }
            let ty = item.file_type()?;
            if ty.is_symlink() {
                continue;
            }
            let out = dest.join(item.file_name());
            if ty.is_dir() {
                copy_text(&item.path(), &out, show, depth + 1)?;
            } else if ty.is_file()
                && matches!(item.path().extension().and_then(|s| s.to_str()), Some("toml" | "lua" | "js" | "ts" | "json" | "css" | "html" | "wgsl"))
                && item.metadata()?.len() <= 1_000_000
            {
                let text = std::fs::read_to_string(item.path())?;
                let ext = item.path().extension().and_then(|e| e.to_str()).unwrap_or_default().to_owned();
                let sensitive = |line: &str| {
                    let lower = line.to_ascii_lowercase();
                    (lower.contains('=') || lower.contains(':'))
                        && ["secret", "password", "token", "credential", "private_key", "api_key"].iter().any(|key| lower.contains(key))
                };
                // A JSON field cannot be removed line-wise without invalidating the
                // document; exclude the whole file rather than snapshot credentials.
                if ext == "json" && text.lines().any(sensitive) {
                    continue;
                }
                let sanitized = text
                    .lines()
                    .map(|line| {
                        if sensitive(line) {
                            match ext.as_str() {
                                "toml" => "# [redacted sensitive setting]",
                                "lua" => "-- [redacted sensitive setting]",
                                "css" => "/* [redacted sensitive setting] */",
                                "html" => "<!-- [redacted sensitive setting] -->",
                                _ => "// [redacted sensitive setting]",
                            }
                        } else {
                            line
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                std::fs::create_dir_all(dest)?;
                std::fs::write(out, sanitized)?;
            }
        }
        Ok(())
    }
    if let Err(e) = copy_text(root, &partial, show, 0) {
        let _ = std::fs::remove_dir_all(&partial);
        return Err(format!("cannot snapshot project: {e}"));
    }
    std::fs::rename(&partial, &dest).map_err(|e| format!("cannot finish project snapshot: {e}"))?;
    Ok(())
}

/// Where a recording writes, checked at start.
#[derive(Debug, PartialEq)]
struct Target {
    /// The show folder (`data/`, clips, the master).
    show_dir: PathBuf,
    /// ISOs' folder; None in fallback mode (master only).
    iso_dir: Option<PathBuf>,
    /// Why `[recording] dir` could not be used.
    fallback: Option<String>,
    /// Non-fatal problems to log (project snapshot).
    notes: Vec<String>,
}

/// A recordings root usable for a show folder named `name`: present (or creatable when its
/// parent exists or it lies inside the home folder; a missing external drive is never
/// recreated on the system disk), writable, and with at least `min_free_gb` free.
fn ready(root: &Path, name: &str, may_create: bool, min_free_gb: f64) -> Result<PathBuf, String> {
    if !root.is_absolute() {
        return Err(format!("recording folder {} must be an absolute path", root.display()));
    }
    if !root.is_dir() {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let creatable = may_create || root.parent().is_some_and(Path::is_dir) || home.is_some_and(|h| h.is_dir() && root.starts_with(&h));
        if !creatable {
            return Err(format!("recording folder {} is missing (is the drive connected?)", root.display()));
        }
        std::fs::create_dir_all(root).map_err(|e| format!("cannot create recording folder {}: {e}", root.display()))?;
    }
    let root = root.canonicalize().map_err(|e| format!("cannot resolve recording folder {}: {e}", root.display()))?;
    let show = root.join(name);
    let unwritable = |e: std::io::Error| format!("recording folder {} is not writable: {e}", root.display());
    std::fs::create_dir_all(&show).map_err(unwritable)?;
    let probe = show.join(".write-test");
    std::fs::write(&probe, b"ok").map_err(unwritable)?;
    let _ = std::fs::remove_file(&probe);
    if let Some(free) = capture::free_gb(&show)
        && free < min_free_gb
    {
        return Err(format!("only {free:.0} GB free in {} (minimum {min_free_gb:.0} GB)", root.display()));
    }
    Ok(show)
}

fn prepare(root: &Path, session: &str, cfg: &RecordingConfig) -> Result<Target, String> {
    if session.is_empty() { return Err("no active session".into()); }
    let session_dir = root.join("sessions").join(session);
    let previous = show_dir_from_meta(&session_dir);
    let name = previous.as_ref().and_then(|d| d.file_name()).map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| show_name(session));
    let fallback_root = cfg.fallback_path();
    // Preserve the show's name, not its old drive: saved settings apply at each start.
    let primary_root = cfg.dir_path();
    let mut target = match ready(&primary_root, &name, false, cfg.min_free_gb) {
        Ok(show) => Target { show_dir: show.clone(), iso_dir: Some(show), fallback: None, notes: Vec::new() },
        Err(primary) => match ready(&fallback_root, &name, true, cfg.min_free_gb) {
            Ok(show) => Target { show_dir: show, iso_dir: None, fallback: Some(primary), notes: Vec::new() },
            Err(fallback) => return Err(format!("{primary}; the fallback folder cannot be used either: {fallback}")),
        },
    };
    // The project snapshot is a convenience; it never prevents recording.
    if cfg.snapshot_project && let Err(e) = snapshot_project(root, &target.show_dir) {
        target.notes.push(e);
    }
    Ok(target)
}

fn is_camera(cfg: &RecordingConfig, name: &str) -> bool {
    cfg.video.iter().any(|v| v.name == name && matches!(v.kind(), SourceKind::Camera(_)))
}

/// "main + 2 cameras + tall": what is being recorded right now.
fn tiers(cfg: &RecordingConfig, feeds: &[capture::FeedStatus]) -> String {
    let recording = |f: &&capture::FeedStatus| f.state == "recording";
    let mut parts: Vec<String> = feeds.iter().filter(|f| f.role == Role::Master).filter(recording).map(|f| f.name.clone()).collect();
    let cameras = feeds.iter().filter(|f| f.role == Role::Iso && is_camera(cfg, &f.name)).filter(recording).count();
    if cameras > 0 {
        parts.push(format!("{cameras} camera{}", if cameras == 1 { "" } else { "s" }));
    }
    parts.extend(feeds.iter().filter(|f| f.role == Role::Iso && !is_camera(cfg, &f.name)).filter(recording).map(|f| f.name.clone()));
    parts.join(" + ")
}

fn health(s: &State, selected: &RecordingConfig) -> (&'static str, String) {
    if let Some(error) = &s.config_error {
        return ("fail", error.clone());
    }
    if s.stopping {
        return ("warn", "Finalizing recording files and session metadata".into());
    }
    if s.running {
        let feeds = s.feeds.as_ref().map(|f| f.feeds.as_slice()).unwrap_or(&[]);
        let master = feeds.iter().find(|f| f.role == Role::Master);
        if let Some(m) = master.filter(|m| matches!(m.state, "retrying" | "low_disk")) {
            return ("fail", format!("Master recording {} is not recording: {}", m.name, m.detail));
        }
        if !s.master_started {
            return ("warn", "Opening selected capture sources and encoder".into());
        }
        let mut problems: Vec<String> = feeds
            .iter()
            .filter(|f| f.role == Role::Iso && f.state != "recording")
            .map(|f| {
                let kind = if is_camera(selected, &f.name) { "camera " } else { "" };
                format!("{kind}{} {}", f.name, if f.detail.is_empty() { f.state.replace('_', " ") } else { f.detail.clone() })
            })
            .collect();
        if let Some(reason) = &s.fallback {
            let dir = s.show_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default();
            problems.insert(0, format!("{reason}: recording the master only, to the fallback folder {dir}"));
        }
        if let Some(note) = master.filter(|m| m.state == "recording").map(|m| m.detail.as_str()).filter(|d| !d.is_empty()) {
            problems.push(note.to_string());
        }
        let what = format!("Recording {}", tiers(selected, feeds));
        return if problems.is_empty() {
            ("pass", format!("{what} ({} audio track{})", selected.audio.len(), if selected.audio.len() == 1 { "" } else { "s" }))
        } else {
            ("warn", format!("{what}; {}", problems.join("; ")))
        };
    }
    if s.preparing {
        return ("warn", "Opening selected capture sources and encoder".into());
    }
    if let Some(error) = &s.error {
        let wait = s.retry_at.map(|at| format!("; retrying in {} s", at.saturating_duration_since(Instant::now()).as_secs() + 1)).unwrap_or_default();
        return ("fail", format!("Cannot record: {error}{wait}"));
    }
    if s.config.auto && s.config.master_index().is_none() {
        return ("warn", "Auto-record is on but no video input is selected; choose a master video in Settings → Recording".into());
    }
    ("pass", "Not recording".into())
}

fn status(ctx: &EngineCtx, state: &Mutex<State>) -> Value {
    let s = state.lock();
    let dir = s.config.dir_path();
    let selected = s.selected.as_ref().filter(|_| s.busy()).unwrap_or(&s.config);
    let inputs = |items: &[crate::show::RecordingInput]| Value::List(items.iter().map(|i| capture::input_value(&i.name, &i.source, &i.format)).collect());
    let (level, detail) = health(&s, selected);
    let feeds = s.feeds.as_ref().filter(|_| s.running).map(|f| f.feeds.as_slice()).unwrap_or(&[]);
    let master = feeds.iter().find(|f| f.role == Role::Master);
    let free = if s.busy() {
        s.feeds.as_ref().filter(|_| s.running).and_then(|f| f.dir_free_gb).or_else(|| capture::free_gb(s.show_dir.as_deref().unwrap_or(&dir)))
    } else {
        capture::free_gb(&dir)
    };
    Value::map()
        .with("dir", dir.display().to_string())
        .with("active", s.active())
        .with("starting", s.starting())
        .with("stopping", s.stopping)
        .with("path", master.map(|m| m.path.clone()).unwrap_or_default())
        .with(
            "paths",
            Value::List(
                feeds
                    .iter()
                    .filter(|f| !f.path.is_empty())
                    .map(|f| Value::map().with("canvas", f.name.clone()).with("role", f.role.as_str()).with("path", f.path.clone()))
                    .collect(),
            ),
        )
        .with("health", Value::map().with("status", level).with("detail", detail))
        .with("tracks", capture::tracks(&selected.audio))
        .with("video", inputs(&selected.video))
        .with("audio", inputs(&selected.audio))
        .with("free_gb", free.map(Value::Float).unwrap_or_default())
        .with("dir_free_gb", free.map(Value::Float).unwrap_or_default())
        .with("fallback", s.running && s.fallback.is_some())
        .with("fallback_reason", s.fallback.clone().filter(|_| s.running).unwrap_or_default())
        .with("segment", master.map(|m| m.segments.saturating_sub(1) as i64).unwrap_or(0))
        .with("feeds", Value::List(feeds.iter().map(capture::FeedStatus::value).collect()))
        .with("session", if s.session.is_empty() { ctx.hub.info.read().session.clone() } else { s.session.clone() })
        .with("show_dir", s.show_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default())
}

fn publish(ctx: &EngineCtx, state: &Mutex<State>) {
    let value = status(ctx, state);
    for key in ["active", "starting", "stopping", "path", "dir_free_gb"] {
        ctx.hub.publish(&format!("recording.{key}"), value.get_path(key).cloned().unwrap_or_default());
    }
    ctx.hub.publish("health.recording", value.get_path("health").cloned().unwrap_or_default());
}

async fn persist(ctx: &EngineCtx, session: &str, values: Value) -> Result<(), String> {
    ctx.hub.query("session.persist", Value::map().with("session", session).with("values", values)).await.map(|_| ())
}

/// Persist the file list now; on failure keep it pending and retry on the next tick, so the
/// recording itself never stops because metadata could not be written yet.
async fn persist_files(ctx: &EngineCtx, state: &Mutex<State>) {
    let (session, values) = {
        let mut s = state.lock();
        let meta = s.meta();
        s.pending_meta = Some(meta.clone());
        (s.session.clone(), meta)
    };
    match persist(ctx, &session, values).await {
        Ok(()) => state.lock().pending_meta = None,
        Err(error) => ctx.hub.log("warn", TARGET, format!("cannot persist recording metadata yet ({error}); retrying")),
    }
}

fn request_stop(ctx: &EngineCtx, state: &Mutex<State>) {
    {
        let mut s = state.lock();
        if let Some(stop) = &s.stop {
            stop.store(true, Ordering::Relaxed);
            s.stopping = true;
        }
    }
    publish(ctx, state);
}

async fn begin(ctx: &EngineCtx, state: &Arc<Mutex<State>>, events: &mpsc::UnboundedSender<capture::Event>) -> Result<(), String> {
    let (cfg, session) = {
        let mut s = state.lock();
        if s.busy() { return Ok(()); }
        if let Some(error) = &s.config_error { return Err(error.clone()); }
        if s.pending_meta.is_some() { return Err("previous recording metadata has not been persisted yet".into()); }
        if s.config.master_index().is_none() { return Err("no video input is selected; choose a master video in Settings → Recording".into()); }
        s.preparing = true;
        s.error = None;
        s.selected = Some(s.config.clone());
        (s.config.clone(), ctx.hub.info.read().session.clone())
    };
    publish(ctx, state);
    let root = ctx.project_root.clone();
    let (prepare_cfg, prepare_session) = (cfg.clone(), session.clone());
    let target = tokio::task::spawn_blocking(move || prepare(&root, &prepare_session, &prepare_cfg)).await.map_err(|e| e.to_string())??;
    for note in &target.notes {
        ctx.hub.log("warn", TARGET, note);
    }
    if let Some(reason) = &target.fallback {
        ctx.hub.log("warn", TARGET, format!("{reason}; recording the master only to {}", target.show_dir.display()));
    }
    let dir = target.show_dir.clone();
    let show = Value::map().with("dir", dir.display().to_string()).with("name", dir.file_name().unwrap_or_default().to_string_lossy().to_string());
    persist(ctx, &session, Value::map().with("show", show)).await?;
    let previous = std::fs::read_to_string(ctx.project_root.join("sessions").join(&session).join("meta.toml"))
        .ok().and_then(|text| toml::from_str::<toml::Value>(&text).ok()).and_then(|v| v.get("recordings").cloned()).map(Value::from)
        .and_then(|v| v.as_list().map(<[Value]>::to_vec)).unwrap_or_default();
    let stop = Arc::new(AtomicBool::new(false));
    {
        let mut s = state.lock();
        s.session = session;
        s.show_dir = Some(dir.clone());
        s.fallback = target.fallback.clone();
        s.previous = previous;
        s.files.clear();
        s.feeds = None;
        s.master_started = false;
        s.stop = Some(stop.clone());
        s.preparing = false;
        s.running = true;
    }
    let plan = capture::Plan { config: cfg, master_dir: dir, iso_dir: target.iso_dir };
    let (events, failures, hub) = (events.clone(), events.clone(), ctx.hub.clone());
    let log = ctx.hub.clone();
    tokio::spawn(async move {
        let observe: Box<dyn capture::Observe> = Box::new(hub.clone());
        if let Err(error) = tokio::task::spawn_blocking(move || capture::run(plan, Some(hub), observe, stop, events)).await {
            // A supervisor bug: its children die with it (PDEATHSIG is per thread group, so
            // they are reaped by their Process guards). The lifecycle retries the recording.
            log.log("error", TARGET, format!("recording supervisor failed: {error}"));
            let _ = failures.send(capture::Event::Finished);
        }
    });
    Ok(())
}

async fn flush_pending(ctx: &EngineCtx, state: &Mutex<State>) -> Result<(), String> {
    let (session, pending) = { let s = state.lock(); (s.session.clone(), s.pending_meta.clone()) };
    if let Some(values) = pending {
        persist(ctx, &session, values).await?;
        state.lock().pending_meta = None;
    }
    Ok(())
}

/// Engine camera sources (`sources/<id>.toml` with a `device`) as `camera:<id>` video inputs.
fn camera_sources(root: &Path) -> Vec<Value> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root.join("sources")) else { return out };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "toml")).collect();
    paths.sort();
    for path in paths {
        let Some(id) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else { continue };
        let Some(doc) = std::fs::read_to_string(&path).ok().and_then(|t| toml::from_str::<toml::Table>(&t).ok()) else { continue };
        if doc.get("device").and_then(toml::Value::as_str).is_none() {
            continue;
        }
        let label = doc.get("label").and_then(toml::Value::as_str).unwrap_or(&id).to_string();
        out.push(capture::input_value(&label, &format!("camera:{id}"), ""));
    }
    out
}

/// Actions: recording.start / recording.stop. Queries: recording.status,
/// recording.sources, recording.finalize. Finalize returns only after child reaping
/// and the session writer has acknowledged durable metadata, including empty capture.
pub async fn start(ctx: EngineCtx) -> anyhow::Result<()> {
    let initial = config(&ctx);
    let mut initial_state = State::new(initial.clone().unwrap_or_default());
    initial_state.config_error = initial.err();
    let state = Arc::new(Mutex::new(initial_state));
    for key in ["active", "starting", "stopping"] {
        ctx.hub.declare(&format!("recording.{key}"), Meta::boolean(false).readonly().owner(TARGET));
    }
    ctx.hub.declare("recording.path", Meta::string("").readonly().owner(TARGET));
    ctx.hub.declare("recording.dir_free_gb", Meta::float(0.0, [0.0, 1e6]).unit("GB").readonly().owner(TARGET));
    let mut actions = ctx.hub.route_actions("recording");
    let (control, mut controls) = mpsc::unbounded_channel::<oneshot::Sender<Result<Value, String>>>();
    let (event_tx, mut events) = mpsc::unbounded_channel();
    {
        let (query_ctx, query_state) = (ctx.clone(), state.clone());
        ctx.hub.register_query("recording", Arc::new(move |name, _| {
            let (ctx, state, control) = (query_ctx.clone(), query_state.clone(), control.clone());
            Box::pin(async move {
                match name.as_str() {
                    "recording.status" => Ok(status(&ctx, &state)),
                    "recording.sources" => {
                        let root = ctx.project_root.clone();
                        tokio::task::spawn_blocking(move || {
                            let all = capture::sources();
                            let mut video = all.get_path("video").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
                            let at = video.len().min(2);
                            video.splice(at..at, camera_sources(&root));
                            all.with("video", Value::List(video))
                        })
                        .await
                        .map_err(|e| e.to_string())
                    }
                    "recording.finalize" => {
                        let (tx, rx) = oneshot::channel();
                        control.send(tx).map_err(|_| "recording service stopped".to_string())?;
                        rx.await.map_err(|_| "recording finalization worker stopped".to_string())?
                    }
                    _ => Err(format!("unknown recording query {name}")),
                }
            })
        }));
    }
    publish(&ctx, &state);
    tokio::spawn(async move {
        let mut cfg_rx = ctx.config.clone();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut gate = AutoGate::default();
        gate.mode(ctx.hub.snapshot.load().str("show.mode").unwrap_or("offline"));
        let mut waiters: Vec<oneshot::Sender<Result<Value, String>>> = Vec::new();
        let mut next_publish = Instant::now();
        loop {
            let mut start_requested = false;
            tokio::select! {
                Some(done) = controls.recv() => {
                    gate.stop();
                    request_stop(&ctx, &state);
                    if state.lock().stop.is_some() { waiters.push(done); }
                    else {
                        let result = flush_pending(&ctx, &state).await;
                        if result.is_ok() { state.lock().stopping = false; }
                        publish(&ctx, &state);
                        let _ = done.send(result.map(|_| status(&ctx, &state)));
                    }
                }
                Some(command) = actions.recv() => {
                    if let Op::Action { name, .. } = command.op {
                        match name.as_str() {
                            "recording.start" => {
                                gate.inhibited = false;
                                state.lock().retry_at = None;
                                start_requested = true;
                            }
                            "recording.stop" => { gate.stop(); request_stop(&ctx, &state); }
                            _ => {}
                        }
                    }
                }
                Some(event) = events.recv() => {
                    match event {
                        capture::Event::Started(record) => {
                            {
                                let mut s = state.lock();
                                if record.role == Role::Master {
                                    s.master_started = true;
                                    s.retry = Backoff::default();
                                }
                                s.files.push(record);
                            }
                            persist_files(&ctx, &state).await;
                        }
                        capture::Event::Ended { path, recording, error } => {
                            let master = {
                                let mut s = state.lock();
                                let master = recording.as_ref().map(|r| r.role == Role::Master)
                                    .or_else(|| s.files.iter().find(|f| f.path == path).map(|f| f.role == Role::Master))
                                    .unwrap_or(false);
                                match recording {
                                    Some(done) => match s.files.iter_mut().find(|f| f.path == path) {
                                        Some(file) => *file = done,
                                        None => s.files.push(done),
                                    },
                                    None => s.files.retain(|f| f.path != path),
                                }
                                master
                            };
                            if let Some(error) = error {
                                ctx.hub.log(if master { "error" } else { "warn" }, TARGET, format!("{}: {error}", path.display()));
                            }
                            persist_files(&ctx, &state).await;
                        }
                        capture::Event::Status(feeds) => {
                            state.lock().feeds = Some(feeds);
                        }
                        capture::Event::Finished => {
                            {
                                let mut s = state.lock();
                                s.running = false;
                                s.stopping = true;
                                s.stop = None;
                                s.pending_meta = Some(s.meta());
                            }
                            publish(&ctx, &state);
                            let result = flush_pending(&ctx, &state).await;
                            {
                                let mut s = state.lock();
                                s.stopping = result.is_err();
                                if let Err(error) = &result {
                                    let error = format!("cannot finalize recording metadata: {error}");
                                    ctx.hub.log("error", TARGET, &error);
                                    s.error = Some(error);
                                }
                            }
                            for done in waiters.drain(..) { let _ = done.send(result.clone().map(|_| status(&ctx, &state))); }
                        }
                    }
                    publish(&ctx, &state);
                }
                result = cfg_rx.changed() => {
                    if result.is_err() {
                        gate.stop();
                        request_stop(&ctx, &state);
                        // Keep processing the worker's final event; shutdown callers
                        // use recording.finalize before dropping the runtime.
                        if !state.lock().busy() { break; }
                    } else {
                        match config(&ctx) {
                            Ok(cfg) => {
                                let mut s = state.lock();
                                if cfg.auto && !s.config.auto { gate.inhibited = false; }
                                if cfg != s.config { s.retry_at = None; }
                                s.config = cfg;
                                s.config_error = None;
                            }
                            Err(error) => { state.lock().config_error = Some(error); }
                        }
                    }
                    publish(&ctx, &state);
                }
                _ = tick.tick() => {
                    let mode = ctx.hub.snapshot.load().str("show.mode").unwrap_or("offline").to_owned();
                    if gate.mode(&mode) {
                        request_stop(&ctx, &state);
                        let mut s = state.lock();
                        s.retry_at = None;
                        if !s.busy() { s.error = None; }
                    }
                    let current_session = ctx.hub.info.read().session.clone();
                    let (flush, now) = {
                        let mut s = state.lock();
                        if !s.busy() && s.pending_meta.is_none() && s.session != current_session {
                            s.session = current_session;
                            s.show_dir = show_dir_from_meta(&ctx.project_root.join("sessions").join(&s.session));
                            s.previous.clear(); s.files.clear(); s.feeds = None; s.error = None; s.fallback = None;
                        }
                        let now = Instant::now();
                        start_requested = !s.busy() && s.config_error.is_none() && s.pending_meta.is_none()
                            && s.config.master_index().is_some() && gate.wants(&s.config)
                            && s.retry_at.is_none_or(|at| at <= now);
                        (s.pending_meta.is_some() && !s.stopping, now)
                    };
                    if flush && flush_pending(&ctx, &state).await.is_ok() {
                        ctx.hub.log("info", TARGET, "recording metadata persisted after a retry");
                    }
                    if now >= next_publish {
                        next_publish = now + Duration::from_secs(5);
                        publish(&ctx, &state);
                    }
                }
            }
            if start_requested {
                if let Err(error) = begin(&ctx, &state, &event_tx).await {
                    {
                        let mut s = state.lock();
                        s.preparing = false;
                        s.running = false;
                        let delay = s.retry.fail(Duration::ZERO);
                        s.retry_at = Some(Instant::now() + delay);
                        s.error = Some(error.clone());
                    }
                    ctx.hub.log("error", TARGET, format!("cannot start recording: {error}; retrying"));
                } else { gate.armed = true; }
                publish(&ctx, &state);
            }
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn show_folder_name_uses_recording_time_not_old_session_time() {
        let name = show_name_at("20200101-000000", "20260926-123456");
        assert_eq!(name, "2026-09-26 12-34 (20200101-000000)");
    }

    #[test]
    fn manual_stop_inhibits_auto_until_mode_change_or_rearm() {
        let config = RecordingConfig::default();
        let mut gate = AutoGate::default();
        gate.mode("brb");
        assert!(!gate.wants(&config));
        gate.mode("preshow");
        assert!(gate.wants(&config));
        gate.armed = true;
        for mode in ["live", "brb", "ad_break", "outro"] {
            gate.mode(mode);
            assert!(gate.wants(&config));
        }
        gate.mode("live");
        gate.stop();
        gate.mode("live");
        assert!(!gate.wants(&config));
        gate.inhibited = false;
        assert!(gate.wants(&config));
        gate.stop();
        gate.mode("preshow");
        assert!(gate.wants(&config));
        assert!(gate.mode("offline"));
        assert!(!gate.wants(&config));
    }

    #[test]
    fn project_snapshot_is_at_start_and_excludes_secrets_and_recordings() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        std::fs::write(root.join("project.toml"), "title = \"original\"\npassword = \"private\"\nauth = { credential = \"danger\" }\n").unwrap();
        std::fs::create_dir_all(root.join("assets")).unwrap();
        std::fs::write(root.join("assets/a.toml"), "not copied").unwrap();
        std::fs::write(root.join("access_token.toml"), "not copied").unwrap();
        std::fs::write(root.join("settings.json"), "{\"token\":\"secret-value\"}").unwrap();
        std::fs::write(root.join("layout.json"), "{\"canvas\":\"wide\"}").unwrap();
        let show = root.join("shows/one");
        std::fs::create_dir_all(&show).unwrap();
        std::fs::create_dir_all(show.join("data/.project.partial")).unwrap();
        std::fs::write(show.join("data/.project.partial/stale.toml"), "old snapshot").unwrap();
        snapshot_project(root, &show).unwrap();
        std::fs::write(root.join("project.toml"), "title = \"new\"\n").unwrap();
        snapshot_project(root, &show).unwrap();
        let text = std::fs::read_to_string(show.join("data/project/project.toml")).unwrap();
        assert!(text.contains("title = \"original\""));
        assert!(!text.contains("private"));
        assert!(!text.contains("danger"));
        assert!(!show.join("data/project/settings.json").exists());
        assert!(show.join("data/project/layout.json").is_file());
        assert!(!show.join("data/project/assets/a.toml").exists());
        assert!(!show.join("data/project/access_token.toml").exists());
        assert!(!show.join("data/project/shows/one").exists());
        assert!(!show.join("data/project/stale.toml").exists());
        assert!(!show.join("data/.project.partial").exists());
    }

    #[test]
    fn preparing_session_creates_single_show_folder() {
        let d = tempfile::tempdir().unwrap();
        let cfg = RecordingConfig { dir: d.path().join("recordings").to_string_lossy().into_owned(), min_free_gb: 0.0, ..RecordingConfig::default() };
        let session = "20260926-123456";
        std::fs::write(d.path().join("project.toml"), "title = \"before\"\n").unwrap();
        let target = prepare(d.path(), session, &cfg).unwrap();
        let path = target.show_dir.clone();
        assert!(path.is_dir());
        assert_eq!((target.iso_dir.as_ref(), target.fallback.as_ref()), (Some(&path), None));
        assert!(path.join("data/project/project.toml").is_file());
        assert!(path.to_string_lossy().ends_with("(20260926-123456)"));
        assert!(!path.join(".write-test").exists());
        let session_dir = d.path().join("sessions").join(session);
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("meta.toml"), format!("[show]\ndir = {:?}\n", path.display().to_string())).unwrap();
        assert_eq!(prepare(d.path(), session, &cfg).unwrap().show_dir, path);
    }

    #[test]
    fn saved_destination_applies_to_next_recording_in_same_session() {
        let project = tempfile::tempdir().unwrap();
        let session = "20260926-123456";
        let old_root = project.path().join("old-drive");
        let new_root = project.path().join("selected-drive");
        std::fs::create_dir_all(&new_root).unwrap();
        let fallback = project.path().join("fallback");
        let cfg = RecordingConfig {
            dir: old_root.to_string_lossy().into_owned(),
            fallback_dir: fallback.to_string_lossy().into_owned(),
            min_free_gb: 0.0,
            snapshot_project: false,
            ..RecordingConfig::default()
        };
        let old_show = prepare(project.path(), session, &cfg).unwrap().show_dir;
        let old_recording = old_show.join("main-original.mkv");
        std::fs::write(&old_recording, b"existing recording").unwrap();
        let session_dir = project.path().join("sessions").join(session);
        std::fs::create_dir_all(&session_dir).unwrap();
        let meta = format!("show = {{ dir = {:?} }}\nrecordings = [{{ path = {:?} }}]\n", old_show.display().to_string(), old_recording.display().to_string());
        std::fs::write(session_dir.join("meta.toml"), &meta).unwrap();

        let saved = RecordingConfig { dir: new_root.to_string_lossy().into_owned(), ..cfg };
        let next = prepare(project.path(), session, &saved).unwrap();
        assert_eq!(next.show_dir.parent(), Some(new_root.canonicalize().unwrap().as_path()));
        assert_eq!(next.show_dir.file_name(), old_show.file_name());
        assert_eq!(next.iso_dir.as_ref(), Some(&next.show_dir));
        assert_eq!(next.fallback, None);
        assert_eq!(std::fs::read(&old_recording).unwrap(), b"existing recording");
        assert_eq!(std::fs::read_to_string(session_dir.join("meta.toml")).unwrap(), meta);
    }

    #[test]
    fn missing_or_full_recording_folder_records_master_only_to_fallback() {
        let d = tempfile::tempdir().unwrap();
        let fallback = d.path().join("fallback");
        let missing_drive = d.path().join("mnt/drive/Stream Engine");
        let cfg = RecordingConfig {
            dir: missing_drive.to_string_lossy().into_owned(),
            fallback_dir: fallback.to_string_lossy().into_owned(),
            min_free_gb: 0.0,
            snapshot_project: false,
            ..RecordingConfig::default()
        };
        let target = prepare(d.path(), "20260926-123456", &cfg).unwrap();
        assert!(target.show_dir.starts_with(fallback.canonicalize().unwrap()));
        assert_eq!(target.iso_dir, None, "cameras are not recorded in fallback mode");
        assert!(target.fallback.as_deref().is_some_and(|r| r.contains("is missing (is the drive connected?)")), "{target:?}");
        assert!(!missing_drive.exists(), "a missing drive's folder is never recreated on the system disk");
        // Not enough free space anywhere: refuse with both reasons (the lifecycle retries).
        let full = RecordingConfig { dir: d.path().join("rec").to_string_lossy().into_owned(), min_free_gb: 1e9, ..cfg };
        let error = prepare(d.path(), "20260926-123456", &full).unwrap_err();
        assert!(error.contains("GB free") && error.contains("fallback folder cannot be used either"), "{error}");
    }

    #[test]
    fn health_names_tiers_and_fails_only_when_the_master_is_down() {
        let cfg: RecordingConfig = toml::from_str(
            r#"video = [{ name = "main", source = "canvas:wide" }, { name = "kit", source = "camera:cam_kit", height = 1080 }, { name = "kick", source = "camera:cam_kick", height = 720 }]
               audio = [{ name = "Mix", source = "se-band", format = "pulse" }]"#,
        )
        .unwrap();
        let feed = |name: &str, role: Role, state: &'static str, detail: &str| capture::FeedStatus {
            name: name.into(),
            role,
            source: String::new(),
            state,
            path: String::new(),
            bytes: 0,
            dropped: 0,
            segments: 1,
            detail: detail.into(),
        };
        let mut s = State::new(cfg.clone());
        s.running = true;
        s.master_started = true;
        s.feeds = Some(capture::Status {
            feeds: vec![feed("main", Role::Master, "recording", ""), feed("kit", Role::Iso, "recording", ""), feed("kick", Role::Iso, "recording", "")],
            dir_free_gb: Some(500.0),
        });
        assert_eq!(health(&s, &cfg), ("pass", "Recording main + 2 cameras (1 audio track)".to_string()));
        s.feeds.as_mut().unwrap().feeds[2] = feed("kick", Role::Iso, "shed", "dropped: encoder busy; retrying in 30 s");
        assert_eq!(health(&s, &cfg), ("warn", "Recording main + 1 camera; camera kick dropped: encoder busy; retrying in 30 s".to_string()));
        s.feeds.as_mut().unwrap().feeds[0] = feed("main", Role::Master, "retrying", "encoder exited; retrying in 4 s");
        let (level, detail) = health(&s, &cfg);
        assert_eq!(level, "fail");
        assert!(detail.contains("Master recording main is not recording: encoder exited"), "{detail}");
        s.feeds.as_mut().unwrap().feeds[0] = feed("main", Role::Master, "recording", "");
        s.fallback = Some("recording folder /mnt/x is missing (is the drive connected?)".into());
        assert!(health(&s, &cfg).1.contains("recording the master only, to the fallback folder"));
        let idle = State { error: Some("only 3 GB free".into()), retry_at: Some(Instant::now() + Duration::from_secs(8)), ..State::new(cfg.clone()) };
        let (level, detail) = health(&idle, &cfg);
        assert_eq!(level, "fail");
        assert!(detail.starts_with("Cannot record: only 3 GB free; retrying in "), "{detail}");
    }
}
