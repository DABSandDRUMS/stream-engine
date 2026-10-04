//! App-owned ffmpeg capture. No OBS state, devices inferred from names, or shell commands.
//!
//! One supervisor per recording owns one ffmpeg child per video input: the **master** (with
//! every audio track) and video-only **ISOs**. Children never share fate: a failing or stalled
//! ISO never stops the master. Any child that fails is restarted into a new file segment after
//! a bounded backoff, for as long as the recording runs (no permanent give-up). Free disk
//! space is polled every 10 s (ISOs stop under 1.5 × `min_free_gb`, the master under 1 ×);
//! render or master-encoder pressure sheds ISOs lowest priority first, with hysteresis.

mod camera;
mod canvas;
mod encode;
mod mkv;
pub(crate) mod policy;

use crate::show::{EncoderPref, RecordingConfig, RecordingInput, Role};
use encode::{Encoder, Job, Live, Outcome};
use policy::{Backoff, Busy, DiskGuard, Load, Rate, Shedder};
use se_hub::Hub;
use se_proto::Value;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
pub struct Recording {
    /// Configured input name.
    pub canvas: String,
    pub role: Role,
    pub source: String,
    pub path: PathBuf,
    pub start_ns: u64,
    pub end_ns: Option<u64>,
    pub tracks: Value,
}

impl Recording {
    pub fn value(&self) -> Value {
        let mut value = Value::map()
            .with("canvas", self.canvas.clone())
            .with("role", self.role.as_str())
            .with("source", self.source.clone())
            .with("path", self.path.display().to_string())
            .with("start_ns", self.start_ns as i64)
            .with("tracks", self.tracks.clone());
        if let Some(end) = self.end_ns {
            value = value.with("end_ns", end as i64);
        }
        value
    }
}

/// What one video input is doing right now.
#[derive(Clone, Debug, PartialEq)]
pub struct FeedStatus {
    pub name: String,
    pub role: Role,
    pub source: String,
    /// `starting` | `recording` | `retrying` | `shed` | `low_disk` | `stopped`
    pub state: &'static str,
    pub path: String,
    pub bytes: u64,
    pub dropped: u64,
    /// Files started for this input in this recording (restarts make new segments).
    pub segments: u32,
    pub detail: String,
}

impl FeedStatus {
    pub fn value(&self) -> Value {
        Value::map()
            .with("name", self.name.clone())
            .with("role", self.role.as_str())
            .with("source", self.source.clone())
            .with("state", self.state)
            .with("active", self.state == "recording")
            .with("path", self.path.clone())
            .with("bytes", self.bytes as i64)
            .with("dropped", self.dropped as i64)
            .with("segments", self.segments as i64)
            .with("detail", self.detail.clone())
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub feeds: Vec<FeedStatus>,
    /// Free space where the master records.
    pub dir_free_gb: Option<f64>,
}

#[derive(Debug)]
pub enum Event {
    /// A file began receiving media (the first segment or a restart).
    Started(Recording),
    /// A file closed: `recording` is the finalized, probed file, None when unusable.
    Ended { path: PathBuf, recording: Option<Recording>, error: Option<String> },
    Status(Status),
    /// Every child exited and was reaped after `stop`.
    Finished,
}

/// Where one recording writes, decided (and checked) at start.
#[derive(Clone, Debug)]
pub struct Plan {
    pub config: RecordingConfig,
    pub master_dir: PathBuf,
    /// None in fallback mode: only the master is recorded.
    pub iso_dir: Option<PathBuf>,
}

pub fn tracks(audio: &[RecordingInput]) -> Value {
    Value::List(
        audio
            .iter()
            .enumerate()
            .map(|(index, input)| {
                Value::map()
                    .with("index", index as i64)
                    .with("name", input.name.clone())
                    .with("sources", vec![input.source.clone()])
                    .with("devices", vec![input.source.clone()])
            })
            .collect(),
    )
}

// Drop is the last-resort panic/error guard. The normal path first asks ffmpeg to
// finalize, then escalates, always wait()ing even after SIGKILL.
struct Process(Child);
impl Drop for Process {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

/// Why the supervisor asked a running child to stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Hold {
    Stop,
    LowDisk,
    Shed,
    Busy,
}

/// Time a newly started encoder gets (first picture, encoder init) before its speed and
/// drops count as load: judging it earlier shed every camera at the start of each show.
const WARMUP: Duration = Duration::from_secs(10);

struct Running {
    handle: JoinHandle<Outcome>,
    stop: Arc<AtomicBool>,
    live: Arc<Live>,
    started_at: Option<Instant>,
    asked: Option<Hold>,
}

struct Feed {
    index: usize,
    input: RecordingInput,
    role: Role,
    encoder: Encoder,
    run: Option<Running>,
    backoff: Backoff,
    retry_at: Option<Instant>,
    /// Never runs in this recording (ISOs in fallback mode).
    disabled: Option<String>,
    error: Option<String>,
    note: Option<String>,
    path: String,
    segments: u32,
    dropped: u64,
    drop_rate: Rate,
    /// Encoded pictures per second, observed once a second (the windowed speed).
    frame_rate: Rate,
    busy: Busy,
    busy_until: Option<Instant>,
}

impl Feed {
    fn state(&self, now: Instant, stopping: bool, disk: (bool, bool), shed: bool) -> (&'static str, String) {
        let held_disk = if self.role == Role::Master { disk.0 } else { disk.1 };
        if let Some(reason) = &self.disabled {
            return ("stopped", reason.clone());
        }
        if let Some(run) = &self.run {
            return match (run.asked, run.started_at.is_some()) {
                (Some(_), _) => ("stopped", "finalizing".into()),
                (None, true) => ("recording", self.note.clone().unwrap_or_default()),
                (None, false) => ("starting", self.note.clone().unwrap_or_else(|| "opening the source and encoder".into())),
            };
        }
        if stopping {
            return ("stopped", String::new());
        }
        if held_disk {
            return ("low_disk", "disk almost full".into());
        }
        if shed {
            return ("shed", "paused to protect the stream: render or master encoder under load".into());
        }
        if let Some(until) = self.busy_until.filter(|u| *u > now) {
            return ("shed", format!("dropped: encoder busy; retrying in {} s", until.duration_since(now).as_secs() + 1));
        }
        let wait = self.retry_at.map(|at| at.saturating_duration_since(now).as_secs() + 1).unwrap_or(0);
        ("retrying", format!("{}; retrying in {wait} s", self.error.as_deref().unwrap_or("stopped")))
    }
}

/// Free space (GB) of the filesystem holding `path` (or its nearest existing ancestor).
pub fn free_gb(path: &Path) -> Option<f64> {
    use std::os::unix::ffi::OsStrExt;
    let path = path.ancestors().find(|p| p.exists())?;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut st = std::mem::MaybeUninit::<libc::statvfs>::uninit();
    // SAFETY: statvfs writes the struct on success, path is NUL-terminated.
    if unsafe { libc::statvfs(path.as_ptr(), st.as_mut_ptr()) } != 0 {
        return None;
    }
    let st = unsafe { st.assume_init() };
    Some(st.f_bavail as f64 * st.f_frsize as f64 / 1_000_000_000.0)
}

/// Engine observations the supervisor needs; the hub in the engine, fixed values in tests.
pub trait Observe: Send {
    /// `(perf.dropped, perf.late, perf.fps)` of the renderer, if published.
    fn render(&self) -> Option<(u64, u64, f64)>;
    fn free_gb(&self, path: &Path) -> Option<f64> {
        free_gb(path)
    }
}

impl Observe for Arc<Hub> {
    fn render(&self) -> Option<(u64, u64, f64)> {
        let snap = self.snapshot.load();
        let int = |k: &str| snap.get(k).and_then(Value::as_i64).map(|v| v.max(0) as u64);
        Some((int("perf.dropped")?, int("perf.late").unwrap_or(0), snap.get("perf.fps").and_then(Value::as_f64).unwrap_or(0.0)))
    }
}

/// Run on a blocking worker until `stop`. Emits `Finished` only after every child exited,
/// every IO thread joined, and every usable file was probed.
pub fn run(plan: Plan, hub: Option<Arc<Hub>>, observe: Box<dyn Observe>, stop: Arc<AtomicBool>, events: tokio::sync::mpsc::UnboundedSender<Event>) {
    let cfg = &plan.config;
    let socket = if cfg.frames_socket.is_empty() { se_frames::default_socket_path() } else { crate::show::expand_home(&cfg.frames_socket) };
    let pref = cfg.encoder_pref();
    let mut feeds: Vec<Feed> = cfg
        .video
        .iter()
        .enumerate()
        .map(|(index, input)| {
            let role = cfg.role(index);
            Feed {
                index,
                input: input.clone(),
                role,
                encoder: if pref == EncoderPref::Software { Encoder::Software } else { Encoder::Nvenc },
                run: None,
                backoff: Backoff::default(),
                retry_at: None,
                disabled: (role == Role::Iso && plan.iso_dir.is_none()).then(|| "not recorded: the recording folder is unavailable, only the master records to the fallback folder".to_string()),
                error: None,
                note: None,
                path: String::new(),
                segments: 0,
                dropped: 0,
                drop_rate: Rate::default(),
                frame_rate: Rate::default(),
                busy: Busy::default(),
                busy_until: None,
            }
        })
        .collect();
    let mut disk = DiskGuard::new(cfg.min_free_gb);
    let mut holds = (false, false);
    let mut master_free = None;
    let mut shedder = Shedder::new();
    let (mut render_dropped, mut render_late, mut master_dropped) = (Rate::default(), Rate::default(), Rate::default());
    let mut next_disk = Instant::now();
    let mut next_load = Instant::now() + Duration::from_secs(1);
    let mut next_status = Instant::now();
    let mut last_status = Status::default();
    loop {
        let now = Instant::now();
        let stopping = stop.load(Ordering::Relaxed);
        // Reap finished children.
        for feed in &mut feeds {
            let Some(run) = feed.run.take_if(|r| r.handle.is_finished()) else { continue };
            let outcome = run.handle.join().unwrap_or_else(|_| Outcome { recording: None, path: None, error: Some("recording worker panicked".into()), started: false });
            if let Some(path) = outcome.path.clone() {
                let _ = events.send(Event::Ended { path, recording: outcome.recording.clone(), error: outcome.error.clone() });
            }
            let ran = run.started_at.map(|at| now.duration_since(at)).unwrap_or_default();
            match run.asked {
                Some(Hold::Busy) => {
                    let delay = feed.backoff.fail(ran).max(Duration::from_secs(30));
                    feed.busy_until = Some(now + delay);
                    feed.retry_at = feed.busy_until;
                    feed.error = Some("dropped: encoder busy".into());
                }
                Some(_) => {}
                None => {
                    let error = outcome.error.unwrap_or_else(|| "encoder exited".into());
                    if feed.role == Role::Master && pref == EncoderPref::Auto && feed.encoder == Encoder::Nvenc && !outcome.started && encode::encoder_unavailable(&error) {
                        // The hardware encoder cannot start at all: record with software rather
                        // than lose the show. ISOs never fall back (CPU belongs to the stream).
                        feed.encoder = Encoder::Software;
                        feed.note = Some("NVENC unavailable; recording with the software encoder".into());
                        feed.retry_at = Some(now);
                    } else {
                        let delay = feed.backoff.fail(ran);
                        feed.retry_at = Some(now + delay);
                    }
                    feed.error = Some(first_line(&error));
                }
            }
        }
        // Disk floors every 10 s.
        if now >= next_disk {
            next_disk = now + Duration::from_secs(10);
            master_free = observe.free_gb(&plan.master_dir);
            let iso_free = plan.iso_dir.as_deref().map(|d| observe.free_gb(d)).unwrap_or(master_free);
            holds = disk.update(master_free, iso_free);
        }
        // Load once a second: render and master pressure shed ISOs; busy ISOs drop out.
        if now >= next_load {
            next_load = now + Duration::from_secs(1);
            let mut load = Load::default();
            if let Some((dropped, late, fps)) = observe.render() {
                load.render_dropped_per_s = render_dropped.per_sec(dropped, now);
                load.render_late_per_s = render_late.per_sec(late, now);
                load.render_fps = fps;
            }
            // A new encoder needs a few seconds to settle (first picture, encoder init):
            // judge speed and drops only after `WARMUP`.
            let warm = |run: &Running| run.started_at.is_some_and(|at| now.duration_since(at) >= WARMUP);
            if let Some(feed) = feeds.iter_mut().find(|f| f.role == Role::Master) {
                match feed.run.as_ref().filter(|r| warm(r)) {
                    Some(run) => {
                        load.master_dropped_per_s = master_dropped.per_sec(run.live.dropped.load(Ordering::Relaxed), now);
                        load.master_speed = feed.frame_rate.per_sec(run.live.frames.load(Ordering::Relaxed), now) / cfg.fps.max(1) as f64;
                    }
                    None => feed.frame_rate = Rate::default(),
                }
            }
            let isos: Vec<(usize, Option<u32>)> = feeds.iter().filter(|f| f.role == Role::Iso && f.disabled.is_none()).map(|f| (f.index, f.input.height)).collect();
            shedder.update(now, load.pressure(cfg.fps), &policy::shed_order(&isos));
            for feed in feeds.iter_mut().filter(|f| f.role == Role::Iso) {
                let Some(run) = feed.run.as_mut().filter(|r| warm(r) && r.asked.is_none()) else {
                    feed.busy.reset();
                    feed.frame_rate = Rate::default();
                    continue;
                };
                let rate = feed.drop_rate.per_sec(run.live.dropped.load(Ordering::Relaxed), now);
                let speed = feed.frame_rate.per_sec(run.live.frames.load(Ordering::Relaxed), now) / cfg.fps.max(1) as f64;
                if feed.busy.update(now, rate, speed, cfg.fps) {
                    run.asked = Some(Hold::Busy);
                    run.stop.store(true, Ordering::Relaxed);
                }
            }
        }
        // Start, keep, or stop each child.
        for feed in &mut feeds {
            let held_disk = if feed.role == Role::Master { holds.0 } else { holds.1 };
            let hold = if stopping {
                Some(Hold::Stop)
            } else if held_disk {
                Some(Hold::LowDisk)
            } else if feed.role == Role::Iso && shedder.is_shed(feed.index) {
                Some(Hold::Shed)
            } else {
                None
            };
            if let Some(run) = &mut feed.run {
                if run.started_at.is_none() && run.live.started.load(Ordering::Relaxed) {
                    run.started_at = Some(now);
                    feed.segments += 1;
                    feed.error = None;
                    feed.busy_until = None;
                }
                feed.dropped = run.live.dropped.load(Ordering::Relaxed);
                if let Some(path) = run.live.path.lock().as_ref() {
                    feed.path = path.display().to_string();
                }
                if let Some(hold) = hold
                    && run.asked.is_none()
                {
                    run.asked = Some(hold);
                    run.stop.store(true, Ordering::Relaxed);
                }
                continue;
            }
            if hold.is_some() || feed.disabled.is_some() || feed.retry_at.is_some_and(|at| at > now) {
                continue;
            }
            feed.retry_at = None;
            let dir = if feed.role == Role::Master { plan.master_dir.clone() } else { plan.iso_dir.clone().expect("enabled ISO has a folder") };
            let job = Job {
                index: feed.index,
                input: feed.input.clone(),
                role: feed.role,
                audio: if feed.role == Role::Master { cfg.audio.clone() } else { Vec::new() },
                fps: cfg.fps,
                encoder: feed.encoder,
                dir,
                frames_socket: socket.clone(),
                hub: hub.clone(),
            };
            let (stop, live) = (Arc::new(AtomicBool::new(false)), Arc::new(Live::default()));
            let (child_stop, child_live, started) = (stop.clone(), live.clone(), events.clone());
            let handle = thread::spawn(move || {
                encode::run(&job, &child_stop, &child_live, &|record| {
                    let _ = started.send(Event::Started(record));
                })
            });
            feed.run = Some(Running { handle, stop, live, started_at: None, asked: None });
            feed.drop_rate = Rate::default();
            feed.busy.reset();
        }
        if stopping && feeds.iter().all(|f| f.run.is_none()) {
            break;
        }
        if now >= next_status {
            next_status = now + Duration::from_secs(1);
            let status = Status {
                feeds: feeds
                    .iter()
                    .map(|f| {
                        let shed = f.role == Role::Iso && shedder.is_shed(f.index);
                        let (state, detail) = f.state(now, stopping, holds, shed);
                        let detail = match state {
                            "low_disk" => low_disk_detail(f.role, master_free, cfg.min_free_gb),
                            _ => detail,
                        };
                        FeedStatus {
                            name: f.input.name.clone(),
                            role: f.role,
                            source: f.input.source.clone(),
                            state,
                            bytes: std::fs::metadata(&f.path).map(|m| m.len()).unwrap_or(0),
                            path: f.path.clone(),
                            dropped: f.dropped,
                            segments: f.segments,
                            detail,
                        }
                    })
                    .collect(),
                dir_free_gb: master_free,
            };
            if status != last_status {
                let _ = events.send(Event::Status(status.clone()));
                last_status = status;
            }
        }
        thread::sleep(Duration::from_millis(100));
    }
    let _ = events.send(Event::Finished);
}

fn low_disk_detail(role: Role, free: Option<f64>, min: f64) -> String {
    let free = free.map(|f| format!("{f:.0} GB free")).unwrap_or_else(|| "free space unknown".into());
    match role {
        Role::Master => format!("disk almost full ({free}, minimum {min:.0} GB): master recording stopped until space is freed"),
        Role::Iso => format!("disk space low ({free}): camera recordings stop under {:.0} GB to save room for the master", min * 1.5),
    }
}

fn first_line(error: &str) -> String {
    let line = error.lines().next().unwrap_or(error);
    if line.len() > 300 {
        let mut end = 300;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}…", &line[..end])
    } else {
        line.to_string()
    }
}

fn duration(path: &Path) -> Result<f64, String> {
    let mut child = Process(
        Command::new("ffprobe")
            .args(["-v", "error", "-show_entries", "format=duration", "-of", "default=noprint_wrappers=1:nokey=1"])
            .arg(path)
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("cannot run ffprobe: {e}"))?,
    );
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Some(status) = child.0.try_wait().map_err(|e| e.to_string())? {
            let mut text = String::new();
            if let Some(mut out) = child.0.stdout.take() {
                std::io::Read::read_to_string(&mut out, &mut text).map_err(|e| e.to_string())?;
            }
            let seconds = text.trim().parse::<f64>().ok().filter(|s| s.is_finite() && *s > 0.0);
            return if status.success() {
                seconds.ok_or_else(|| format!("recording {} contains no finalized media", path.display()))
            } else {
                Err(format!("cannot read finalized recording {}", path.display()))
            };
        }
        if Instant::now() >= deadline {
            return Err(format!("probing recording {} timed out", path.display()));
        }
        thread::sleep(Duration::from_millis(20));
    }
}

/// Selectable local devices, plus explicit source/format entry in Settings. Failure
/// to enumerate an optional subsystem never prevents users typing a device URL.
pub fn sources() -> Value {
    let mut video = vec![input_value("wide", "canvas:wide", ""), input_value("tall", "canvas:tall", "")];
    if let Ok(entries) = std::fs::read_dir("/sys/class/video4linux") {
        for entry in entries.flatten() {
            let device = format!("/dev/{}", entry.file_name().to_string_lossy());
            let label = std::fs::read_to_string(entry.path().join("name")).unwrap_or_else(|_| device.clone());
            video.push(input_value(label.trim(), &device, "v4l2"));
        }
    }
    let mut audio = vec![input_value("System default", "default", "pulse")];
    // pactl is optional; no shell, and a wedged sound server is bounded/reaped.
    if let Ok(mut child) = Command::new("pactl").args(["-f", "json", "list", "sources"]).stdout(Stdio::piped()).stderr(Stdio::null()).spawn() {
        let stdout = child.stdout.take().expect("piped stdout");
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            let _ = std::io::Read::read_to_end(&mut std::io::Read::take(stdout, 1_048_576), &mut bytes);
            bytes
        });
        let mut child = Process(child);
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(child.0.try_wait(), Ok(None)) && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(20));
        }
        let _ = child.0.kill();
        let _ = child.0.wait();
        if let Ok(bytes) = reader.join()
            && let Ok(items) = serde_json::from_slice::<Vec<serde_json::Value>>(&bytes)
        {
            for item in items {
                if let Some(name) = item.get("name").and_then(|v| v.as_str()) {
                    let label = item.get("description").and_then(|v| v.as_str()).unwrap_or(name);
                    audio.push(input_value(label, name, "pulse"));
                }
            }
        }
    }
    Value::map().with("video", Value::List(video)).with("audio", Value::List(audio))
}

pub fn input_value(name: &str, source: &str, format: &str) -> Value {
    Value::map().with("name", name).with("source", source).with("format", format)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

    struct Fixed(Option<f64>);
    impl Observe for Fixed {
        fn render(&self) -> Option<(u64, u64, f64)> {
            None
        }
        fn free_gb(&self, path: &Path) -> Option<f64> {
            self.0.or_else(|| free_gb(path))
        }
    }

    fn have_ffmpeg() -> bool {
        Command::new("ffmpeg").arg("-version").output().is_ok() && Command::new("ffprobe").arg("-version").output().is_ok()
    }

    fn config(dir: &Path, min_free_gb: f64) -> RecordingConfig {
        RecordingConfig {
            dir: dir.to_string_lossy().into_owned(),
            min_free_gb,
            encoder: "software".into(),
            video: vec![
                RecordingInput { codec: Some(crate::show::Codec::H264), ..RecordingInput::new("main", "testsrc2=size=160x90:rate=60", "lavfi") },
                // An ISO that can never open (no such device): it must not affect the master.
                RecordingInput { role: Some(Role::Iso), ..RecordingInput::new("kick", "/nonexistent/se-test-video", "v4l2") },
            ],
            audio: vec![RecordingInput::new("Mix", "sine=frequency=440:sample_rate=48000", "lavfi")],
            ..RecordingConfig::default()
        }
    }

    fn drain(rx: &mut mpsc::UnboundedReceiver<Event>) -> Vec<Event> {
        let mut out = Vec::new();
        while let Ok(event) = rx.try_recv() {
            out.push(event);
        }
        out
    }

    #[test]
    fn a_failing_iso_never_stops_the_master_and_keeps_retrying() {
        if !have_ffmpeg() {
            eprintln!("ffmpeg not available: skipped");
            return;
        }
        let d = tempfile::tempdir().unwrap();
        let plan = Plan { config: config(d.path(), 0.0), master_dir: d.path().to_path_buf(), iso_dir: Some(d.path().to_path_buf()) };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = stop.clone();
            thread::spawn(move || run(plan, None, Box::new(Fixed(None)), stop, tx))
        };
        let mut events = Vec::new();
        let deadline = Instant::now() + Duration::from_secs(20);
        // Record until the master runs and the ISO has failed at least twice (1 s backoff).
        loop {
            events.extend(drain(&mut rx));
            let master_started = events.iter().any(|e| matches!(e, Event::Started(r) if r.role == Role::Master));
            let iso_failures = events.iter().filter(|e| matches!(e, Event::Ended { path, recording: None, error: Some(_) } if path.to_string_lossy().contains("/kick-"))).count();
            if master_started && iso_failures >= 2 {
                break;
            }
            assert!(Instant::now() < deadline, "master never started or ISO never retried: {events:?}");
            thread::sleep(Duration::from_millis(100));
        }
        thread::sleep(Duration::from_millis(1500));
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        events.extend(drain(&mut rx));
        assert!(matches!(events.last(), Some(Event::Finished)));
        assert!(!events.iter().any(|e| matches!(e, Event::Started(r) if r.role == Role::Iso)));
        let master: Vec<&Recording> = events
            .iter()
            .filter_map(|e| match e {
                Event::Ended { recording: Some(r), .. } if r.role == Role::Master => Some(r),
                _ => None,
            })
            .collect();
        assert_eq!(master.len(), 1, "one uninterrupted master segment: {events:?}");
        let seconds = (master[0].end_ns.unwrap() - master[0].start_ns) as f64 / 1e9;
        assert!(seconds > 1.0, "master recorded {seconds} s");
        assert_eq!(master[0].tracks.as_list().map(<[Value]>::len), Some(1));
        assert_eq!(master[0].value().get_path("role").and_then(Value::as_str), Some("master"));
        let status = events.iter().rev().find_map(|e| match e {
            Event::Status(s) => Some(s),
            _ => None,
        });
        let kick = status.and_then(|s| s.feeds.iter().find(|f| f.name == "kick")).unwrap();
        assert_eq!(kick.role, Role::Iso);
        assert_eq!(kick.segments, 0);
    }

    #[test]
    fn low_disk_holds_the_master_and_reports_it_without_launching_encoders() {
        let d = tempfile::tempdir().unwrap();
        let plan = Plan { config: config(d.path(), 20.0), master_dir: d.path().to_path_buf(), iso_dir: Some(d.path().to_path_buf()) };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = stop.clone();
            thread::spawn(move || run(plan, None, Box::new(Fixed(Some(5.0))), stop, tx))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Ok(Event::Status(s)) = rx.try_recv() {
                break s;
            }
            assert!(Instant::now() < deadline, "no status");
            thread::sleep(Duration::from_millis(20));
        };
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert_eq!(status.dir_free_gb, Some(5.0));
        let main = &status.feeds[0];
        assert_eq!((main.role, main.state), (Role::Master, "low_disk"));
        assert!(main.detail.contains("disk almost full (5 GB free, minimum 20 GB)"), "{}", main.detail);
        assert_eq!(status.feeds[1].state, "low_disk");
        let rest = drain(&mut rx);
        assert!(!rest.iter().any(|e| matches!(e, Event::Started(_) | Event::Ended { .. })), "{rest:?}");
        assert!(std::fs::read_dir(d.path()).unwrap().next().is_none(), "no file was created");
    }

    #[test]
    fn fallback_mode_records_no_isos() {
        let d = tempfile::tempdir().unwrap();
        let plan = Plan { config: config(d.path(), 20.0), master_dir: d.path().to_path_buf(), iso_dir: None };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let stop = Arc::new(AtomicBool::new(false));
        let worker = {
            let stop = stop.clone();
            thread::spawn(move || run(plan, None, Box::new(Fixed(Some(5.0))), stop, tx))
        };
        let deadline = Instant::now() + Duration::from_secs(5);
        let status = loop {
            if let Ok(Event::Status(s)) = rx.try_recv() {
                break s;
            }
            assert!(Instant::now() < deadline, "no status");
            thread::sleep(Duration::from_millis(20));
        };
        stop.store(true, Ordering::Relaxed);
        worker.join().unwrap();
        assert_eq!(status.feeds[1].state, "stopped");
        assert!(status.feeds[1].detail.contains("only the master records to the fallback folder"));
    }
}
