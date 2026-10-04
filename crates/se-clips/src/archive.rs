//! After the show: the forever archive (`[recording.archive]`). Each recorded show moves through a
//! durable, restartable stage machine (table `archive_jobs`, one row per show):
//!
//! ```text
//! wait_offline → iso_cleanup → encode → verify → hold → package → commit → cleanup → done
//! ```
//!
//! * `wait_offline` — the show has been offline `min_offline_minutes` (operator `archive.run`
//!   skips the wait).
//! * `iso_cleanup` — camera ISO files are deleted once the show's clip job finished and no clip
//!   awaits review, or `iso_keep_days` after the show; never for a protected show (`.keep` or
//!   `keep` in the show folder). This never blocks archiving and is re-checked every minute.
//! * `encode` — every master segment → `<archive>/<show>/main.mkv` (`main-2.mkv`, …): AV1
//!   (libsvtav1, VBR, frame rate kept) at [`video_kbps`], every audio track → Opus
//!   `audio_kbps`. FFmpeg runs at nice 19, idle IO class, on half the CPU cores, writing
//!   `*.mkv.part` (removed and redone after a restart).
//! * `verify` — ffprobe duration within 1 s of the source, same audio tracks, and the first and
//!   last 2 s decode cleanly. A failure deletes the copy and encodes again.
//! * `hold` — the full-quality hot copy stays until the clips are settled (as for ISOs) or
//!   `iso_keep_days` passed; a protected show stays here. `archive.run` skips the hold.
//! * `package` — `data/` is copied with large files zstd-compressed (`*.zst`; the timeline
//!   files stay readable) and `clips/` is copied as-is.
//! * `commit` — `data/show.json` and the session's `meta.toml` point at the archive,
//!   `archive.json` is written, then clip rows and the stage change in one DB transaction.
//! * `cleanup` — only now are the hot master files, `data/` and `clips/` deleted.
//!
//! All of it waits while the show is LIVE ([`crate::live`]); a running encode is frozen within
//! 2 s and continues afterwards. A missing archive folder (unplugged drive) or a full disk waits
//! with backoff; failures retry with backoff forever. State: `archive.queue`,
//! `archive.dir_free_gb`, `archive.paused`, `health.archive`. Actions: `archive.run {show?}`,
//! `archive.pause`, `archive.retry {show}`. Query: `archive`. Events: `archive.done`,
//! `archive.failed`.

use crate::ffmpeg;
use crate::live::{self, Halt, LiveGuard, OnHold};
use crate::session;
use crate::show::{self, ArchiveConfig, RecordingConfig};
use crate::store::{self, ArchiveMaster, ArchiveRow};
use parking_lot::Mutex;
use se_hub::{EngineCtx, Hub};
use se_proto::{Event, Meta, Op, Origin, Value};
use se_store::Db;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

const TARGET: &str = "archive";
/// `archive.json` schema version.
pub const SCHEMA: u32 = 1;
/// Share of the size target kept for the container, `data/` and clips.
pub const OVERHEAD: f64 = 0.02;
/// The video bitrate never drops below this, whatever the size target.
pub const MIN_VIDEO_KBPS: u32 = 250;
/// SVT-AV1 speed/efficiency preset (0 slowest … 13 fastest).
pub const PRESET: u32 = 8;
/// Keyframe interval, seconds.
const GOP_S: f64 = 5.0;
/// Archive encoders' CPU niceness.
const NICE: i32 = 19;
/// Files under `data/` the show timeline reads; they stay uncompressed.
const PLAIN: &[&str] = &["show.json", "feedback.jsonl", "features.csv", "transcript.jsonl"];
/// Smaller files are copied as they are.
const COMPRESS_MIN: u64 = 64 * 1024;
/// Failures before `health.archive` fails.
const FAIL_AFTER: i64 = 3;
const GB: f64 = 1e9;

pub const STAGES: &[&str] = &["wait_offline", "iso_cleanup", "encode", "verify", "hold", "package", "commit", "cleanup", "done"];

fn next_stage(stage: &str) -> &'static str {
    let i = STAGES.iter().position(|s| *s == stage).unwrap_or(0);
    STAGES[(i + 1).min(STAGES.len() - 1)]
}

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Target video bitrate (kbps) for a size target of `gb_per_hour` (GB = 10⁹ bytes): the total
/// rate `gb_per_hour · 8000 / 3600` Mbps, less [`OVERHEAD`], less the Opus audio tracks.
pub fn video_kbps(gb_per_hour: f64, audio_tracks: usize, audio_kbps: u32) -> u32 {
    let total_kbps = gb_per_hour * 8_000_000.0 / 3600.0;
    let video = total_kbps * (1.0 - OVERHEAD) - audio_tracks as f64 * audio_kbps as f64;
    (video.round().max(0.0) as u32).max(MIN_VIDEO_KBPS)
}

/// Archive file name of master segment `i`.
pub fn master_name(i: usize) -> String {
    if i == 0 { "main.mkv".into() } else { format!("main-{}.mkv", i + 1) }
}

/// FFmpeg arguments for one master segment.
pub fn encode_args(src: &Path, out: &Path, video_kbps: u32, audio_kbps: u32, fps: f64) -> Vec<String> {
    let gop = if fps > 0.0 { (fps * GOP_S).round() as u32 } else { 300 };
    let s = |x: &str| x.to_string();
    vec![
        s("-nostdin"),
        s("-hide_banner"),
        s("-v"),
        s("error"),
        s("-nostats"),
        s("-progress"),
        s("pipe:1"),
        s("-y"),
        s("-i"),
        src.to_string_lossy().into_owned(),
        s("-map"),
        s("0:v:0"),
        s("-map"),
        s("0:a?"),
        s("-map_metadata"),
        s("0"),
        s("-fps_mode"),
        s("passthrough"),
        s("-c:v"),
        s("libsvtav1"),
        s("-preset"),
        PRESET.to_string(),
        s("-b:v"),
        format!("{video_kbps}k"),
        s("-g"),
        gop.to_string(),
        s("-svtav1-params"),
        s("rc=1:undershoot-pct=10:overshoot-pct=10"),
        s("-pix_fmt"),
        s("yuv420p10le"),
        s("-c:a"),
        s("libopus"),
        s("-b:a"),
        format!("{audio_kbps}k"),
        s("-f"),
        s("matroska"),
        out.to_string_lossy().into_owned(),
    ]
}

/// The upper half of the CPUs this process may use (archive encodes stay off the rest).
pub fn encode_cpus(allowed: &[usize]) -> Vec<usize> {
    if allowed.len() < 2 {
        return allowed.to_vec();
    }
    allowed[allowed.len() / 2..].to_vec()
}

fn allowed_cpus() -> Vec<usize> {
    // SAFETY: zeroed cpu_set_t is a valid empty set; sched_getaffinity fills it for this thread.
    let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    if unsafe { libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) } != 0 {
        return Vec::new();
    }
    (0..libc::CPU_SETSIZE as usize).filter(|i| unsafe { libc::CPU_ISSET(*i, &set) }).collect()
}

/// Free bytes on the file system holding `dir` (`None` when it doesn't exist).
pub fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    // SAFETY: `c` is a valid NUL-terminated path and `st` a properly sized out-parameter.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    (unsafe { libc::statvfs(c.as_ptr(), &mut st) } == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

fn dir_bytes(p: &Path) -> u64 {
    match std::fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| dir_bytes(&e.path())).sum(),
        Ok(m) => m.len(),
        Err(_) => 0,
    }
}

/// A show folder the operator protected with `.keep`/`keep`.
pub fn protected(show_dir: &Path) -> bool {
    show_dir.join(".keep").exists() || show_dir.join("keep").exists()
}

// ---- queueing -----------------------------------------------------------------------------

/// The session directory recorded in the sessions index, else under the project.
pub fn session_dir(db: &Db, project_root: &Path, session: &str) -> PathBuf {
    let from_db: Option<String> = db
        .with(|c| {
            use rusqlite::OptionalExtension;
            c.query_row("SELECT dir FROM sessions WHERE id = ?1", [session], |r| r.get(0)).optional()
        })
        .ok()
        .flatten();
    from_db.map(PathBuf::from).unwrap_or_else(|| project_root.join("sessions").join(session))
}

/// The archive row for a closed, recorded session (`None` when there is nothing to archive).
pub fn plan(db: &Db, project_root: &Path, session: &str) -> Option<ArchiveRow> {
    let dir = session_dir(db, project_root, session);
    let show_dir = show::show_dir_from_meta(&dir)?;
    let meta = session::parse_meta(&std::fs::read_to_string(dir.join("meta.toml")).ok()?).ok()?;
    let masters: Vec<ArchiveMaster> = meta
        .recordings
        .iter()
        .filter(|r| r.is_master())
        .enumerate()
        .map(|(i, r)| ArchiveMaster { src: r.path.to_string_lossy().into_owned(), name: master_name(i) })
        .collect();
    if masters.is_empty() {
        return None;
    }
    let isos: Vec<String> = meta.recordings.iter().filter(|r| r.is_iso()).map(|r| r.path.to_string_lossy().into_owned()).collect();
    let ended_at: i64 = db
        .with(|c| {
            use rusqlite::OptionalExtension;
            c.query_row("SELECT ended_at FROM sessions WHERE id = ?1", [session], |r| r.get::<_, Option<i64>>(0)).optional()
        })
        .ok()
        .flatten()
        .flatten()
        .unwrap_or_else(unix_now);
    Some(ArchiveRow {
        session: session.into(),
        show: show_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| session.into()),
        show_dir: show_dir.to_string_lossy().into_owned(),
        stage: STAGES[0].into(),
        state: "queued".into(),
        isos_left: !isos.is_empty(),
        masters,
        isos,
        ended_at,
        ..Default::default()
    })
}

/// Queue a session (no-op when queued before or nothing was recorded).
pub fn enqueue(db: &Db, project_root: &Path, session: &str) -> bool {
    plan(db, project_root, session).is_some_and(|row| store::archive_insert(db, &row).unwrap_or(false))
}

/// Queue every closed show that has no row yet (shows recorded before the archive existed).
pub fn backfill(db: &Db, project_root: &Path) -> usize {
    let sessions: Vec<String> = db
        .with(|c| {
            let mut st = c.prepare("SELECT id FROM sessions WHERE ended_at IS NOT NULL AND id NOT IN (SELECT session FROM archive_jobs) ORDER BY ended_at")?;
            st.query_map([], |r| r.get(0))?.collect()
        })
        .unwrap_or_default();
    sessions.iter().filter(|s| enqueue(db, project_root, s)).count()
}

// ---- stages -------------------------------------------------------------------------------

/// What a stage needs from the engine.
pub struct StageEnv {
    pub db: Db,
    pub project_root: PathBuf,
    pub cfg: RecordingConfig,
    /// Unix seconds now.
    pub now: i64,
    /// Seconds the show has been continuously offline.
    pub offline_s: i64,
    /// Operator `archive.run`: skip the offline wait and the hold.
    pub force: bool,
    /// Encode progress (0–1 of the show).
    pub progress: Arc<dyn Fn(f64) + Send + Sync>,
}

impl StageEnv {
    fn archive(&self) -> &ArchiveConfig {
        &self.cfg.archive
    }
    fn root(&self) -> PathBuf {
        self.archive().dir_path(&self.cfg.dir_path())
    }
}

/// The outcome of one stage.
#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// Done; the row is at its next stage.
    Next,
    /// Not yet (a condition); try again later.
    Wait(String),
    /// The archive folder or disk isn't usable; retry with backoff.
    Blocked(String),
}

/// Run the row's current stage once. On `Ok(Step::Next)` `row.stage` has advanced.
pub fn step(env: &StageEnv, row: &mut ArchiveRow) -> Result<Step, String> {
    let res = match row.stage.as_str() {
        "wait_offline" => wait_offline(env),
        "iso_cleanup" => {
            iso_sweep(env, row);
            Ok(Step::Next)
        }
        "encode" => encode(env, row),
        "verify" => verify(env, row),
        "hold" => hold(env, row),
        "package" => package(env, row),
        "commit" => return commit(env, row).map(|()| Step::Next),
        "cleanup" => cleanup(env, row),
        "done" => return Ok(Step::Wait("archived".into())),
        other => Err(format!("unknown archive stage `{other}`")),
    }?;
    if res == Step::Next {
        row.stage = next_stage(&row.stage).into();
        row.progress = 0.0;
    }
    Ok(res)
}

fn wait_offline(env: &StageEnv) -> Result<Step, String> {
    let need = env.archive().min_offline_minutes as i64 * 60;
    if env.force || env.offline_s >= need { Ok(Step::Next) } else { Ok(Step::Wait(format!("starts {} min after the show", env.archive().min_offline_minutes))) }
}

/// Clips settled, or the keep time is over.
fn hot_copy_done(env: &StageEnv, row: &ArchiveRow) -> bool {
    store::clips_settled(&env.db, &row.session).unwrap_or(false) || env.now - row.ended_at >= env.archive().iso_keep_days as i64 * 86_400
}

/// Delete the show's ISO files when allowed. Never for a protected show.
pub fn iso_sweep(env: &StageEnv, row: &mut ArchiveRow) {
    if row.isos.is_empty() {
        row.isos_left = false;
        return;
    }
    if protected(Path::new(&row.show_dir)) || !hot_copy_done(env, row) {
        row.isos_left = row.isos.iter().any(|p| Path::new(p).exists());
        return;
    }
    for p in &row.isos {
        match std::fs::remove_file(p) {
            Ok(()) => tracing::info!("archive: deleted camera recording {p}"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => tracing::warn!("archive: could not delete {p}: {e}"),
        }
    }
    row.isos_left = row.isos.iter().any(|p| Path::new(p).exists());
    if row.stage == "done" {
        // the last camera file was all that kept an archived show's hot folder
        let _ = std::fs::remove_dir(&row.show_dir);
    }
}

/// The archive root, created when it's the default under the recording folder. A configured
/// folder must already exist (an unplugged drive is waited for, never created on the root disk).
fn usable_root(env: &StageEnv) -> Result<PathBuf, Step> {
    let root = env.root();
    if root.is_dir() {
        return Ok(root);
    }
    if env.archive().dir.trim().is_empty() && env.cfg.dir_path().is_dir() && std::fs::create_dir_all(&root).is_ok() {
        return Ok(root);
    }
    Err(Step::Blocked(format!("archive folder missing: {}", root.display())))
}

fn dest_ok(row: &ArchiveRow) -> Result<PathBuf, Step> {
    let dest = PathBuf::from(&row.dest);
    if dest.is_dir() { Ok(dest) } else { Err(Step::Blocked(format!("archive folder missing: {}", dest.parent().unwrap_or(&dest).display()))) }
}

fn part_of(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".part");
    PathBuf::from(s)
}

fn encode(env: &StageEnv, row: &mut ArchiveRow) -> Result<Step, String> {
    if row.dest.is_empty() {
        let root = match usable_root(env) {
            Ok(r) => r,
            Err(s) => return Ok(s),
        };
        row.dest = root.join(&row.show).to_string_lossy().into_owned();
    } else if let Some(parent) = Path::new(&row.dest).parent()
        && !parent.is_dir()
    {
        return Ok(Step::Blocked(format!("archive folder missing: {}", parent.display())));
    }
    let dest = PathBuf::from(&row.dest);
    std::fs::create_dir_all(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    // the source files and their probes
    let mut todo = Vec::new();
    let mut total = 0.0;
    let mut bytes_in = 0u64;
    for m in &row.masters {
        let out = dest.join(&m.name);
        let _ = std::fs::remove_file(part_of(&out));
        let src = Path::new(&m.src);
        if out.exists() {
            if let Ok(p) = ffmpeg::probe(&out) {
                total += p.duration;
            }
            bytes_in += std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
            continue;
        }
        if !src.exists() {
            return Err(format!("master recording missing: {}", src.display()));
        }
        bytes_in += std::fs::metadata(src).map(|m| m.len()).unwrap_or(0);
        let probe = ffmpeg::probe(src)?;
        let duration = if probe.duration > 0.0 { probe.duration } else { copy_duration(src)? };
        total += duration;
        todo.push((src.to_path_buf(), out, probe, duration));
    }
    row.bytes_in = bytes_in as i64;
    let a = env.archive();
    let need = (total / 3600.0 * a.gb_per_hour * GB * 1.1) as u64 + 1_000_000_000;
    if !todo.is_empty() && free_bytes(&dest).is_some_and(|f| f < need) {
        return Ok(Step::Blocked(format!("archive disk needs {:.1} GB free for {}", need as f64 / GB, row.show)));
    }
    let cpus = encode_cpus(&allowed_cpus());
    let mut done_s = total - todo.iter().map(|t| t.3).sum::<f64>();
    for (src, out, probe, duration) in todo {
        let kbps = video_kbps(a.gb_per_hour, probe.audio_streams, a.audio_kbps);
        row.video_kbps = kbps as i64;
        let part = part_of(&out);
        let progress = env.progress.clone();
        let (base, all) = (done_s, total.max(1e-3));
        let res = run_encode(&encode_args(&src, &part, kbps, a.audio_kbps, probe.fps), &cpus, move |t| progress(((base + t) / all).clamp(0.0, 1.0)));
        if let Err(e) = res {
            let _ = std::fs::remove_file(&part);
            return Err(e);
        }
        std::fs::rename(&part, &out).map_err(|e| format!("{}: {e}", out.display()))?;
        done_s += duration;
    }
    Ok(Step::Next)
}

/// FFmpeg encode under the current halt (frozen while LIVE), reporting output seconds.
fn run_encode(args: &[String], cpus: &[usize], progress: impl Fn(f64) + Send + 'static) -> Result<(), String> {
    let mut cmd = ffmpeg::background_cmd("ffmpeg", NICE, cpus);
    cmd.args(args).env("SVT_LOG", "1").stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("archive ffmpeg launch: {e} (requires setpriv, nice, ionice, taskset and ffmpeg)"))?;
    let err = ffmpeg::drain(child.stderr.take().expect("piped"));
    let out = child.stdout.take().expect("piped");
    let reader = std::thread::spawn(move || {
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            if let Some(us) = line.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<f64>().ok()) {
                progress(us / 1e6);
            }
        }
    });
    let status = live::wait_child(&mut child, live::current().as_ref());
    let _ = reader.join();
    let err = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
    if status?.success() { Ok(()) } else { Err(format!("archive encode failed: {}", ffmpeg::tail(&err))) }
}

/// Duration of a file whose header has none (a recording cut short): demux it to the end.
fn copy_duration(path: &Path) -> Result<f64, String> {
    let args: Vec<String> = ["-nostdin", "-v", "error", "-nostats", "-i"]
        .iter()
        .map(|s| s.to_string())
        .chain([path.to_string_lossy().into_owned()])
        .chain(["-map", "0:v:0", "-c", "copy", "-f", "null", "-progress", "pipe:1", "-"].iter().map(|s| s.to_string()))
        .collect();
    let out = ffmpeg::run_capture(&args, NICE)?;
    String::from_utf8_lossy(&out)
        .lines()
        .rev()
        .find_map(|l| l.strip_prefix("out_time_us=").and_then(|v| v.trim().parse::<f64>().ok()))
        .map(|us| us / 1e6)
        .filter(|d| *d > 0.0)
        .ok_or_else(|| format!("{}: duration unknown", path.display()))
}

/// Decode 2 s of `path` from second `at`: no errors and at least one video frame.
fn decodes(path: &Path, at: f64) -> Result<(), String> {
    let mut args: Vec<String> = ["-nostdin", "-v", "error", "-xerror", "-nostats", "-threads", "2", "-ss"].iter().map(|s| s.to_string()).collect();
    args.push(format!("{:.3}", at.max(0.0)));
    args.extend(["-i".into(), path.to_string_lossy().into_owned()]);
    args.extend(["-t", "2", "-map", "0:v:0", "-map", "0:a?", "-f", "null", "-progress", "pipe:1", "-"].iter().map(|s| s.to_string()));
    let what = |e: String| format!("{} does not decode at {at:.0} s: {e}", path.display());
    let out = ffmpeg::run_capture(&args, NICE).map_err(what)?;
    let frames = String::from_utf8_lossy(&out).lines().rev().find_map(|l| l.strip_prefix("frame=").and_then(|v| v.trim().parse::<u64>().ok())).unwrap_or(0);
    if frames == 0 { Err(what("no video frames".into())) } else { Ok(()) }
}

/// Check one archived file against its source: the real (demuxed, not header) duration within
/// 1 s, the same audio tracks, and clean decoding of its first and last 2 s.
pub fn verify_file(src: &Path, out: &Path) -> Result<(f64, f64), String> {
    let a = ffmpeg::probe(out)?;
    let s = ffmpeg::probe(src)?;
    let src_d = if s.duration > 0.0 { s.duration } else { copy_duration(src)? };
    let out_d = copy_duration(out)?;
    for d in [a.duration, out_d] {
        if (d - src_d).abs() > 1.0 {
            return Err(format!("{}: {d:.1} s but the recording is {src_d:.1} s", out.display()));
        }
    }
    if a.audio_streams != s.audio_streams {
        return Err(format!("{}: {} audio tracks, the recording has {}", out.display(), a.audio_streams, s.audio_streams));
    }
    decodes(out, 0.0)?;
    decodes(out, out_d - 2.0)?;
    Ok((src_d, out_d))
}

fn verify(env: &StageEnv, row: &mut ArchiveRow) -> Result<Step, String> {
    let dest = match dest_ok(row) {
        Ok(d) => d,
        Err(s) => return Ok(s),
    };
    let mut files = Vec::new();
    let mut bytes = 0u64;
    for m in &row.masters {
        let out = dest.join(&m.name);
        let src = Path::new(&m.src);
        let checked = if src.exists() { verify_file(src, &out) } else { Err(format!("master recording missing: {}", src.display())) };
        match checked {
            Ok((src_d, out_d)) => {
                let len = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                bytes += len;
                files.push(serde_json::json!({"name": m.name, "source": m.src, "bytes": len, "source_duration": src_d, "duration": out_d}));
            }
            Err(e) => {
                // never trust a bad copy: encode it again
                let _ = std::fs::remove_file(&out);
                row.stage = "encode".into();
                return Err(e);
            }
        }
    }
    row.bytes_out = bytes as i64;
    let a = env.archive();
    let stamp =
        serde_json::json!({"verified_at": env.now, "files": files, "video_kbps": row.video_kbps, "audio_kbps": a.audio_kbps, "gb_per_hour": a.gb_per_hour});
    write_atomic(&dest.join(".verified.json"), serde_json::to_vec_pretty(&stamp).unwrap_or_default().as_slice())?;
    Ok(Step::Next)
}

fn hold(env: &StageEnv, row: &mut ArchiveRow) -> Result<Step, String> {
    if protected(Path::new(&row.show_dir)) {
        return Ok(Step::Wait("kept in the recording folder (.keep)".into()));
    }
    if env.force || hot_copy_done(env, row) { Ok(Step::Next) } else { Ok(Step::Wait("keeping the full-quality copy until its clips are reviewed".into())) }
}

/// Exclusive lock shared with the show indexer (`<session>/.show-index.lock`).
struct IndexLock(#[allow(dead_code)] std::fs::File);

fn lock_index(env: &StageEnv, row: &ArchiveRow) -> Result<Option<IndexLock>, String> {
    use std::os::fd::AsRawFd;
    let dir = session_dir(&env.db, &env.project_root, &row.session);
    if !dir.is_dir() {
        return Ok(None);
    }
    let f = std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(dir.join(".show-index.lock")).map_err(|e| e.to_string())?;
    // SAFETY: flock on an owned descriptor; closing the file releases it.
    if unsafe { libc::flock(f.as_raw_fd(), libc::LOCK_EX) } != 0 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(Some(IndexLock(f)))
}

/// Keep `rel` (relative to `data/`) readable in the archive?
pub fn keep_plain(rel: &Path, bytes: u64) -> bool {
    let s = rel.to_string_lossy();
    bytes < COMPRESS_MIN || s.ends_with(".zst") || PLAIN.contains(&s.as_ref()) || (rel.starts_with("lanes") && s.ends_with(".jsonl"))
}

/// Block while held (frozen like an encode); `Err` when the work must stop.
fn checkpoint() -> Result<(), String> {
    let Some(h) = live::current() else { return Ok(()) };
    loop {
        if h.stop_now() {
            return Err(h.error());
        }
        if !h.held() {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn copy_tree(src: &Path, dst: &Path, rel: &Path, compress: bool) -> Result<(), String> {
    let here = src.join(rel);
    std::fs::create_dir_all(dst.join(rel)).map_err(|e| e.to_string())?;
    for e in std::fs::read_dir(&here).map_err(|e| format!("{}: {e}", here.display()))? {
        let e = e.map_err(|e| e.to_string())?;
        let r = rel.join(e.file_name());
        let md = e.metadata().map_err(|e| e.to_string())?;
        if md.is_dir() {
            copy_tree(src, dst, &r, compress)?;
            continue;
        }
        if !md.is_file() {
            continue;
        }
        checkpoint()?;
        let from = src.join(&r);
        if compress && !keep_plain(&r, md.len()) {
            let mut name = dst.join(&r).into_os_string();
            name.push(".zst");
            let input = std::fs::File::open(&from).map_err(|e| format!("{}: {e}", from.display()))?;
            let out = std::fs::File::create(&name).map_err(|e| e.to_string())?;
            let mut enc = zstd::stream::write::Encoder::new(out, 12).map_err(|e| e.to_string())?;
            copy_checked(input, &mut enc)?;
            enc.finish().and_then(|f| f.sync_all()).map_err(|e| e.to_string())?;
        } else {
            let to = dst.join(&r);
            let mut out = std::fs::File::create(&to).map_err(|e| e.to_string())?;
            copy_checked(std::fs::File::open(&from).map_err(|e| format!("{}: {e}", from.display()))?, &mut out)?;
            out.sync_all().map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Copy in 8 MiB chunks, honouring the live guard between chunks.
fn copy_checked(mut r: impl Read, w: &mut impl Write) -> Result<(), String> {
    let mut buf = vec![0u8; 8 << 20];
    loop {
        checkpoint()?;
        let n = r.read(&mut buf).map_err(|e| e.to_string())?;
        if n == 0 {
            return Ok(());
        }
        w.write_all(&buf[..n]).map_err(|e| e.to_string())?;
    }
}

fn package(env: &StageEnv, row: &mut ArchiveRow) -> Result<Step, String> {
    let dest = match dest_ok(row) {
        Ok(d) => d,
        Err(s) => return Ok(s),
    };
    let show_dir = PathBuf::from(&row.show_dir);
    let need = dir_bytes(&show_dir.join("data")) + dir_bytes(&show_dir.join("clips")) + 256_000_000;
    if free_bytes(&dest).is_some_and(|f| f < need) {
        return Ok(Step::Blocked(format!("archive disk needs {:.1} GB free for {}", need as f64 / GB, row.show)));
    }
    let _lock = lock_index(env, row)?;
    for (name, compress) in [("data", true), ("clips", false)] {
        let part = dest.join(format!("{name}.part"));
        let _ = std::fs::remove_dir_all(&part);
        let done = dest.join(name);
        let _ = std::fs::remove_dir_all(&done);
        let src = show_dir.join(name);
        if !src.is_dir() {
            continue;
        }
        copy_tree(&src, &part, Path::new(""), compress).inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&part);
        })?;
        std::fs::rename(&part, &done).map_err(|e| format!("{}: {e}", done.display()))?;
    }
    Ok(Step::Next)
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let tmp = part_of(path);
    let res = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    res.map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("{}: {e}", path.display())
    })
}

/// `data/show.json` in the archive: masters point at their archive copies, ISOs are gone.
fn rewrite_manifest(path: &Path, map: &HashMap<String, String>, isos: &HashSet<String>) -> Result<(), String> {
    let Ok(text) = std::fs::read_to_string(path) else { return Ok(()) };
    let mut v: serde_json::Value = serde_json::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    if let Some(recs) = v.get_mut("recordings").and_then(serde_json::Value::as_array_mut) {
        recs.retain(|r| {
            let p = r.get("path").and_then(serde_json::Value::as_str).unwrap_or("");
            r.get("role").and_then(serde_json::Value::as_str) != Some("iso") && !isos.contains(p)
        });
        for r in recs.iter_mut() {
            if let Some(new) = r.get("path").and_then(serde_json::Value::as_str).and_then(|p| map.get(p)) {
                r["path"] = serde_json::Value::String(new.clone());
            }
        }
    }
    write_atomic(path, &serde_json::to_vec_pretty(&v).map_err(|e| e.to_string())?)
}

/// The session's `meta.toml`: the show folder is the archive, recordings are its files.
fn rewrite_meta(path: &Path, dest: &Path, map: &HashMap<String, String>, isos: &HashSet<String>) -> Result<(), String> {
    let Ok(text) = std::fs::read_to_string(path) else { return Ok(()) };
    let mut doc: toml::Table = toml::from_str(&text).map_err(|e| format!("{}: {}", path.display(), e.message()))?;
    if let Some(show) = doc.get_mut("show").and_then(toml::Value::as_table_mut) {
        show.insert("dir".into(), toml::Value::String(dest.to_string_lossy().into_owned()));
    }
    if let Some(recs) = doc.get_mut("recordings").and_then(toml::Value::as_array_mut) {
        recs.retain(|r| {
            let p = r.get("path").and_then(toml::Value::as_str).unwrap_or("");
            r.get("role").and_then(toml::Value::as_str) != Some("iso") && !isos.contains(p)
        });
        for r in recs.iter_mut() {
            if let Some(t) = r.as_table_mut()
                && let Some(new) = t.get("path").and_then(toml::Value::as_str).and_then(|p| map.get(p)).cloned()
            {
                t.insert("path".into(), toml::Value::String(new));
            }
        }
    }
    write_atomic(path, toml::to_string(&doc).map_err(|e| e.to_string())?.as_bytes())
}

fn commit(env: &StageEnv, row: &mut ArchiveRow) -> Result<(), String> {
    let dest = PathBuf::from(&row.dest);
    if !dest.is_dir() {
        return Err(format!("archive folder missing: {}", dest.display()));
    }
    for m in &row.masters {
        if !dest.join(&m.name).is_file() {
            row.stage = "encode".into();
            return Err(format!("{} vanished before commit", dest.join(&m.name).display()));
        }
    }
    let _lock = lock_index(env, row)?;
    let map: HashMap<String, String> = row.masters.iter().map(|m| (m.src.clone(), dest.join(&m.name).to_string_lossy().into_owned())).collect();
    let isos: HashSet<String> = row.isos.iter().cloned().collect();
    rewrite_manifest(&dest.join("data").join("show.json"), &map, &isos)?;
    let session_dir = session_dir(&env.db, &env.project_root, &row.session);
    rewrite_meta(&session_dir.join("meta.toml"), &dest, &map, &isos)?;
    let verified: serde_json::Value = std::fs::read(dest.join(".verified.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let (data, clips) = (dir_bytes(&dest.join("data")), dir_bytes(&dest.join("clips")));
    let main: u64 = row.masters.iter().map(|m| std::fs::metadata(dest.join(&m.name)).map(|x| x.len()).unwrap_or(0)).sum();
    let a = env.archive();
    let doc = serde_json::json!({
        "schema": SCHEMA,
        "session": row.session,
        "show": row.show,
        "source_paths": row.masters.iter().map(|m| m.src.clone()).collect::<Vec<_>>(),
        "files": verified.get("files").cloned().unwrap_or_default(),
        "sizes": {"source_bytes": row.bytes_in, "video_bytes": main, "data_bytes": data, "clips_bytes": clips, "total_bytes": main + data + clips},
        "bitrate": {"video_kbps": row.video_kbps, "audio_kbps": a.audio_kbps, "gb_per_hour": a.gb_per_hour, "encoder": format!("libsvtav1 preset {PRESET}")},
        "verified_at": verified.get("verified_at").cloned().unwrap_or(serde_json::json!(env.now)),
        "archived_at": env.now,
    });
    write_atomic(&dest.join("archive.json"), &serde_json::to_vec_pretty(&doc).map_err(|e| e.to_string())?)?;
    let _ = std::fs::remove_file(dest.join(".verified.json"));
    let hot_clips = Path::new(&row.show_dir).join("clips").to_string_lossy().into_owned();
    let new_clips = dest.join("clips").to_string_lossy().into_owned();
    let pairs: Vec<(String, String)> = map.into_iter().collect();
    row.stage = next_stage("commit").into();
    row.state = "queued".into();
    row.detail = String::new();
    row.progress = 0.0;
    row.bytes_out = (main + data + clips) as i64;
    store::archive_commit(&env.db, row, &pairs, Some((&hot_clips, &new_clips))).map_err(|e| {
        row.stage = "commit".into();
        format!("db: {e:#}")
    })
}

fn cleanup(env: &StageEnv, row: &mut ArchiveRow) -> Result<Step, String> {
    let dest = PathBuf::from(&row.dest);
    let show_dir = PathBuf::from(&row.show_dir);
    // never delete a hot copy without a committed, complete archive beside it
    if !dest.join("archive.json").is_file() || row.masters.iter().any(|m| !dest.join(&m.name).is_file()) {
        return Ok(Step::Blocked(format!("archive of {} incomplete; keeping the recording", row.show)));
    }
    if dest == show_dir || protected(&show_dir) {
        return Ok(Step::Next);
    }
    let _lock = lock_index(env, row)?;
    for m in &row.masters {
        match std::fs::remove_file(&m.src) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(format!("{}: {e}", m.src)),
        }
    }
    for name in ["data", "clips"] {
        if dest.join(name).is_dir() {
            match std::fs::remove_dir_all(show_dir.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", show_dir.join(name).display())),
            }
        }
    }
    // the show folder goes when nothing else (pending ISOs, other files) is left in it
    let _ = std::fs::remove_dir(&show_dir);
    row.finished_at = Some(env.now);
    Ok(Step::Next)
}

/// Retry delay after `attempts` consecutive failures/blocks: 1 min doubling, at most 1 h.
pub fn backoff_s(attempts: i64) -> i64 {
    (60i64 << attempts.clamp(1, 7).saturating_sub(1)).min(3600)
}

// ---- subsystem ----------------------------------------------------------------------------

struct Archiver {
    ctx: EngineCtx,
    live: LiveGuard,
    paused: AtomicBool,
    /// Sessions the operator asked to run now (`*` = all).
    forced: Mutex<HashSet<String>>,
    current: Mutex<Option<Halt>>,
    wake: tokio::sync::Notify,
    /// Unix s since when the show has been offline (now while LIVE).
    offline_since: AtomicI64,
    /// Consecutive blocked attempts per show (missing folder, full disk) for backoff.
    blocked: Mutex<HashMap<String, i64>>,
    /// Why the last blocked show waits ("archive folder missing: …").
    blocked_reason: Mutex<Option<String>>,
    /// Progress (0–1) of the running stage, by session.
    progress: Mutex<(String, f64)>,
}

impl Archiver {
    fn hub(&self) -> &Arc<Hub> {
        &self.ctx.hub
    }
    fn cfg(&self) -> Result<RecordingConfig, String> {
        RecordingConfig::from_section(self.ctx.project_section("recording").as_ref())
    }
}

/// Start the archive worker, its actions and its state.
pub async fn start(ctx: EngineCtx, live: LiveGuard) -> anyhow::Result<()> {
    let paused = ctx.db.kv_get(TARGET, "paused").ok().flatten().is_some_and(|v| v.truthy());
    let a = Arc::new(Archiver {
        ctx: ctx.clone(),
        live,
        paused: AtomicBool::new(paused),
        forced: Mutex::new(HashSet::new()),
        current: Mutex::new(None),
        wake: tokio::sync::Notify::new(),
        offline_since: AtomicI64::new(unix_now()),
        blocked: Mutex::new(HashMap::new()),
        blocked_reason: Mutex::new(None),
        progress: Mutex::new((String::new(), 0.0)),
    });
    let own = |m: Meta| m.readonly().owner(TARGET);
    ctx.hub.declare("archive.paused", own(Meta::boolean(paused).describe("Archive work paused by the operator (archive.run resumes)")));
    ctx.hub.declare("archive.dir_free_gb", own(Meta::float(0.0, [0.0, 1e9]).unit("GB").describe("Free space where archives go")));
    ctx.hub.publish("archive.paused", Value::Bool(paused));
    // subscribe first so a show closing during the backfill isn't missed
    let bus = ctx.hub.subscribe();
    // the clip tables exist (lib.rs migrates before starting us); queue older shows
    {
        let (db, root) = (ctx.db.clone(), ctx.project_root.clone());
        let n = tokio::task::spawn_blocking(move || backfill(&db, &root)).await.unwrap_or(0);
        if n > 0 {
            tracing::info!("archive: queued {n} earlier shows");
        }
    }
    tokio::spawn(track_offline(a.clone()));
    tokio::spawn(events(a.clone(), bus));
    tokio::spawn(actions(a.clone(), ctx.hub.route_actions("archive")));
    tokio::spawn(config_watch(a.clone()));
    tokio::spawn(worker(a.clone()));
    let q = a.clone();
    ctx.hub.register_query(
        "archive",
        Arc::new(move |_, _| {
            let q = q.clone();
            Box::pin(async move {
                tokio::task::spawn_blocking(move || store::archive_list(&q.ctx.db).map(|rows| Value::List(rows.iter().map(ArchiveRow::to_value).collect())))
                    .await
                    .map_err(|e| e.to_string())?
                    .map_err(|e| format!("{e:#}"))
            })
        }),
    );
    Ok(())
}

async fn track_offline(a: Arc<Archiver>) {
    loop {
        if a.live.is_live() {
            a.offline_since.store(unix_now(), Ordering::Relaxed);
            a.live.wait_offline().await;
            a.offline_since.store(unix_now(), Ordering::Relaxed);
            a.wake.notify_one();
        }
        a.live.wait_live().await;
    }
}

async fn events(a: Arc<Archiver>, mut bus: tokio::sync::broadcast::Receiver<Arc<se_hub::Bus>>) {
    loop {
        let b = match bus.recv().await {
            Ok(b) => b,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        };
        let se_hub::Bus::Event(e) = &*b else { continue };
        if e.ty == "session.closed"
            && let Some(s) = e.payload.get_path("session").and_then(Value::as_str)
        {
            let (db, root, s) = (a.ctx.db.clone(), a.ctx.project_root.clone(), s.to_string());
            if tokio::task::spawn_blocking(move || enqueue(&db, &root, &s)).await.unwrap_or(false) {
                a.wake.notify_one();
            }
        }
    }
}

/// A disabled archive stops the running stage (it resumes from its stage when enabled again).
async fn config_watch(a: Arc<Archiver>) {
    let mut rx = a.ctx.config.clone();
    while rx.changed().await.is_ok() {
        if a.cfg().is_ok_and(|c| !c.archive.enabled)
            && let Some(h) = a.current.lock().as_ref()
        {
            h.abort();
        }
        a.wake.notify_one();
    }
}

fn find_row(db: &Db, show: &str) -> Option<ArchiveRow> {
    store::archive_list(db).ok()?.into_iter().find(|r| r.session == show || r.show == show)
}

async fn actions(a: Arc<Archiver>, mut rx: tokio::sync::mpsc::UnboundedReceiver<se_proto::Command>) {
    while let Some(c) = rx.recv().await {
        let Op::Action { name, args } = &c.op else { continue };
        if let Err(e) = action(&a, name, args) {
            a.hub().log("error", TARGET, format!("{name}: {e}"));
        }
    }
}

fn set_paused(a: &Archiver, on: bool) {
    a.paused.store(on, Ordering::Relaxed);
    let _ = a.ctx.db.kv_set(TARGET, "paused", &Value::Bool(on));
    a.hub().publish("archive.paused", Value::Bool(on));
    if let Some(h) = a.current.lock().as_ref() {
        h.set_hold(on);
    }
}

fn action(a: &Arc<Archiver>, name: &str, args: &Value) -> Result<(), String> {
    let show = args.get_path("show").or_else(|| args.get_path("session")).or_else(|| args.get_path("args.0")).and_then(Value::as_str).map(String::from);
    let db = &a.ctx.db;
    match name {
        // operator request: start now (skips the offline wait and the hold), still LIVE-gated
        "archive.run" => {
            set_paused(a, false);
            match show {
                Some(s) => {
                    let mut row = find_row(db, &s).or_else(|| {
                        enqueue(db, &a.ctx.project_root, &s);
                        find_row(db, &s)
                    });
                    let row = row.as_mut().ok_or_else(|| format!("no recorded show `{s}`"))?;
                    row.next_try = 0;
                    store::archive_save(db, row).map_err(|e| format!("{e:#}"))?;
                    a.forced.lock().insert(row.session.clone());
                }
                None => {
                    a.forced.lock().insert("*".into());
                }
            }
        }
        "archive.pause" => set_paused(a, true),
        // one show, or (no show) every failed or waiting one
        "archive.retry" => {
            let rows = match &show {
                Some(s) => vec![find_row(db, s).ok_or_else(|| format!("no archive entry for `{s}`"))?],
                None => store::archive_list(db).map_err(|e| format!("{e:#}"))?.into_iter().filter(|r| r.stage != "done" && r.state != "running").collect(),
            };
            for mut row in rows {
                row.attempts = 0;
                row.next_try = 0;
                if row.state == "failed" {
                    row.state = "queued".into();
                }
                store::archive_save(db, &row).map_err(|e| format!("{e:#}"))?;
                a.blocked.lock().remove(&row.session);
            }
        }
        other => return Err(format!("unknown action `{other}` (archive.run|pause|retry)")),
    }
    a.wake.notify_one();
    Ok(())
}

async fn worker(a: Arc<Archiver>) {
    let mut last_sweep = 0i64;
    loop {
        publish(&a).await;
        let cfg = a.cfg();
        let enabled = cfg.as_ref().is_ok_and(|c| c.archive.enabled);
        if !enabled || a.paused.load(Ordering::Relaxed) {
            idle(&a, Duration::from_secs(60)).await;
            continue;
        }
        if a.live.is_live() {
            tokio::select! {
                _ = a.live.wait_offline() => {}
                _ = a.wake.notified() => {}
            }
            continue;
        }
        let cfg = cfg.expect("checked above");
        let now = unix_now();
        if now - last_sweep >= 60 {
            last_sweep = now;
            sweep_isos(&a, &cfg).await;
        }
        let rows: Vec<ArchiveRow> = store::archive_list(&a.ctx.db).unwrap_or_default().into_iter().filter(|r| r.stage != "done" && r.next_try <= now).collect();
        let mut progressed = false;
        for row in rows {
            if a.live.is_live() || a.paused.load(Ordering::Relaxed) {
                break;
            }
            progressed |= run_row(&a, &cfg, row).await;
            publish(&a).await;
        }
        if !progressed {
            idle(&a, Duration::from_secs(30)).await;
        }
    }
}

async fn idle(a: &Archiver, d: Duration) {
    tokio::select! {
        _ = tokio::time::sleep(d) => {}
        _ = a.wake.notified() => {}
        _ = a.live.wait_live() => {}
    }
}

async fn sweep_isos(a: &Arc<Archiver>, cfg: &RecordingConfig) {
    let rows: Vec<ArchiveRow> = store::archive_list(&a.ctx.db).unwrap_or_default().into_iter().filter(|r| r.isos_left && r.stage != "wait_offline").collect();
    for mut row in rows {
        if a.live.is_live() {
            return;
        }
        let env = stage_env(a, cfg, false, &row.session);
        let db = a.ctx.db.clone();
        let _ = tokio::task::spawn_blocking(move || {
            iso_sweep(&env, &mut row);
            store::archive_save(&db, &row)
        })
        .await;
    }
}

fn stage_env(a: &Arc<Archiver>, cfg: &RecordingConfig, force: bool, session: &str) -> StageEnv {
    let (p, session) = (a.clone(), session.to_string());
    StageEnv {
        db: a.ctx.db.clone(),
        project_root: a.ctx.project_root.clone(),
        cfg: cfg.clone(),
        now: unix_now(),
        offline_s: unix_now() - a.offline_since.load(Ordering::Relaxed),
        force,
        progress: Arc::new(move |f| *p.progress.lock() = (session.clone(), f)),
    }
}

/// Advance one show as far as it goes now. Returns whether it made progress.
async fn run_row(a: &Arc<Archiver>, cfg: &RecordingConfig, mut row: ArchiveRow) -> bool {
    let mut progressed = false;
    loop {
        if a.live.is_live() || a.paused.load(Ordering::Relaxed) || row.stage == "done" {
            return progressed;
        }
        let force = {
            let f = a.forced.lock();
            f.contains("*") || f.contains(&row.session)
        };
        let halt = a.live.halt(OnHold::Pause);
        halt.set_hold(a.paused.load(Ordering::Relaxed));
        *a.current.lock() = Some(halt.clone());
        let env = stage_env(a, cfg, force, &row.session);
        *a.progress.lock() = (row.session.clone(), 0.0);
        row.state = "running".into();
        let _ = store::archive_save(&a.ctx.db, &row);
        let db = a.ctx.db.clone();
        let started = row.stage.clone();
        let task = tokio::task::spawn_blocking(move || {
            let res = live::with_halt(halt, || step(&env, &mut row));
            (res, row)
        });
        tokio::pin!(task);
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        let joined = loop {
            tokio::select! {
                r = &mut task => break r,
                _ = tick.tick() => publish(a).await,
            }
        };
        let (res, mut back) = joined.unwrap_or_else(|e| (Err(format!("archive stage panicked: {e}")), ArchiveRow::default()));
        *a.current.lock() = None;
        if back.session.is_empty() {
            return progressed;
        }
        if back.stage == started {
            back.progress = a.progress.lock().1;
        }
        row = back;
        let now = unix_now();
        match res {
            Ok(Step::Next) => {
                progressed = true;
                row.state = if row.stage == "done" { "done".into() } else { "queued".into() };
                row.detail.clear();
                row.attempts = 0;
                a.blocked.lock().remove(&row.session);
                *a.blocked_reason.lock() = None;
                let _ = store::archive_save(&db, &row);
                tracing::info!("archive {}: {started} done → {}", row.show, row.stage);
                if row.stage == "done" {
                    a.forced.lock().remove(&row.session);
                    a.hub().emit(Event::new(
                        "archive.done",
                        Origin::System,
                        Value::map()
                            .with("session", row.session.clone())
                            .with("show", row.show.clone())
                            .with("path", row.dest.clone())
                            .with("bytes", row.bytes_out),
                    ));
                    return true;
                }
            }
            Ok(Step::Wait(reason)) => {
                row.state = "waiting".into();
                row.detail = reason;
                row.next_try = now + 30;
                let _ = store::archive_save(&db, &row);
                return progressed;
            }
            Ok(Step::Blocked(reason)) => {
                let n = {
                    let mut b = a.blocked.lock();
                    let n = b.entry(row.session.clone()).or_insert(0);
                    *n += 1;
                    *n
                };
                row.state = "waiting".into();
                row.next_try = now + backoff_s(n).min(600);
                row.detail = reason.clone();
                *a.blocked_reason.lock() = Some(reason);
                let _ = store::archive_save(&db, &row);
                return progressed;
            }
            Err(e) if a.live.is_live() || e == live::ABORTED || a.paused.load(Ordering::Relaxed) => {
                // stopped by the guard, the operator, or a disabled archive: not a failure
                row.state = "queued".into();
                row.detail = "waiting: the show is live".into();
                let _ = store::archive_save(&db, &row);
                return progressed;
            }
            Err(e) => {
                row.attempts += 1;
                row.state = "failed".into();
                row.detail = e.clone();
                row.next_try = now + backoff_s(row.attempts);
                let _ = store::archive_save(&db, &row);
                a.hub().log("error", TARGET, format!("{} ({}): {e}; retrying", row.show, started));
                a.hub().emit(Event::new(
                    "archive.failed",
                    Origin::System,
                    Value::map()
                        .with("session", row.session.clone())
                        .with("show", row.show.clone())
                        .with("stage", started)
                        .with("error", e)
                        .with("attempts", row.attempts),
                ));
                return progressed;
            }
        }
    }
}

/// `health.archive` from the rows: (status, detail).
pub fn health(enabled: bool, paused: bool, rows: &[ArchiveRow], blocked: Option<&str>) -> (&'static str, String) {
    if !enabled {
        return ("pass", "archive off ([recording.archive] enabled = false)".into());
    }
    if let Some(r) = rows.iter().filter(|r| r.state == "failed" && r.attempts >= FAIL_AFTER).max_by_key(|r| r.attempts) {
        return ("fail", format!("archiving {} keeps failing ({}): {}; retrying automatically", r.show, r.stage, r.detail));
    }
    let open: Vec<&ArchiveRow> = rows.iter().filter(|r| r.stage != "done").collect();
    if let Some(reason) = blocked {
        return ("warn", format!("{reason}; {} waiting, recordings are kept", shows(open.len())));
    }
    if paused && !open.is_empty() {
        return ("warn", format!("archive paused: {} waiting (archive.run resumes)", shows(open.len())));
    }
    if !open.is_empty() {
        let now = open.iter().find(|r| r.state == "running").map(|r| format!(" ({} {}: {:.0}%)", r.stage, r.show, r.progress * 100.0)).unwrap_or_default();
        return ("warn", format!("{} waiting to archive{now}", shows(open.len())));
    }
    let done: Vec<&ArchiveRow> = rows.iter().filter(|r| r.stage == "done").collect();
    // fold from +0.0: an empty f64 `sum()` is -0.0, which printed as "-0.0 GB"
    let gb = done.iter().fold(0.0, |gb, r| gb + r.bytes_out as f64) / GB;
    ("pass", format!("archive up to date ({}, {gb:.1} GB)", shows(done.len())))
}

fn shows(n: usize) -> String {
    if n == 1 { "1 show".into() } else { format!("{n} shows") }
}

async fn publish(a: &Arc<Archiver>) {
    let a2 = a.clone();
    let Ok((rows, free)) = tokio::task::spawn_blocking(move || {
        let rows = store::archive_list(&a2.ctx.db).unwrap_or_default();
        // The default `<recording dir>/Archive` is only created on first use: until then
        // report the recording folder's drive, where it will be.
        let free = a2.cfg().ok().and_then(|c| {
            let rec = c.dir_path();
            free_bytes(&c.archive.dir_path(&rec)).or_else(|| if c.archive.dir.trim().is_empty() { free_bytes(&rec) } else { None })
        });
        (rows, free)
    })
    .await
    else {
        return;
    };
    let cfg = a.cfg();
    let enabled = cfg.as_ref().is_ok_and(|c| c.archive.enabled);
    let blocked = a.blocked_reason.lock().clone();
    let mut rows = rows;
    let (running, progress) = a.progress.lock().clone();
    for r in rows.iter_mut().filter(|r| r.state == "running" && r.session == running) {
        r.progress = progress;
    }
    let (status, detail) = match &cfg {
        Err(e) => ("fail", format!("{e}; archive paused")),
        Ok(_) => health(enabled, a.paused.load(Ordering::Relaxed), &rows, blocked.as_deref()),
    };
    let hub = a.hub();
    hub.publish("health.archive", Value::map().with("status", status).with("detail", detail));
    let queue: Vec<Value> = rows
        .iter()
        .filter(|r| r.stage != "done")
        .map(|r| {
            Value::map()
                .with("show", r.show.clone())
                .with("session", r.session.clone())
                .with("stage", r.stage.clone())
                .with("state", r.state.clone())
                .with("progress", (r.progress * 1000.0).round() / 1000.0)
                .with("detail", r.detail.clone())
        })
        .collect();
    hub.publish("archive.queue", Value::List(queue));
    hub.publish("archive.dir_free_gb", Value::Float(free.map(|f| (f as f64 / GB * 10.0).round() / 10.0).unwrap_or(0.0)));
}

#[cfg(test)]
mod tests;
