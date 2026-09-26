//! Backups and retention (§17.4).
//!
//! * **Runtime DB:** backed up daily (`VACUUM INTO` → integrity check → zstd) into
//!   `<data_dir>/backups/runtime-<utc>.db.zst`, keeping the newest `keep`. Deferred while on
//!   air unless a backup is two intervals overdue.
//! * **Session logs:** `<project>/sessions/<id>` older than `sessions_days` (by last write)
//!   are deleted, always keeping the newest `sessions_keep`, the running session, and any
//!   session containing a `.keep`/`keep` file (the clip pipeline protects sessions with
//!   unreviewed clips this way).
//! * **Recordings:** `[recording] dir` is held to a disk budget. Going over budget publishes
//!   a warning listing the oldest unprotected recordings. They are deleted only by the
//!   operator (`retention.prune_recordings`) or, with `auto_delete = true`, after the
//!   warning has stood for `grace_hours` — never while recording or on air, nor while
//!   their session has unreviewed/unuploaded clips or unfinished clip jobs.
//!
//! Config: `[retention]` (`sessions_days`, `sessions_keep`, `[retention.backups]`,
//! `[retention.recordings]`). Actions: `retention.backup_now`, `retention.prune_sessions`,
//! `retention.prune_recordings`, `retention.scan`. Query `retention`.

use crate::util::{self, Section};
use parking_lot::Mutex;
use se_hub::EngineCtx;
use se_proto::{Event, Meta, Op, Origin, Value};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

const TARGET: &str = "retention";
const KV_NS: &str = "retention";
const KV_PENDING: &str = "recordings.pending";
const GB: f64 = 1_000_000_000.0;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct BackupSettings {
    pub enabled: bool,
    /// Backup directory (default `<data_dir>/backups`).
    pub dir: String,
    pub keep: usize,
    pub interval: String,
}

impl Default for BackupSettings {
    fn default() -> Self {
        BackupSettings { enabled: true, dir: String::new(), keep: 14, interval: "24h".into() }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct RecordingSettings {
    /// Recording folder (default: `[recording] dir`).
    pub dir: String,
    pub budget_gb: f64,
    /// Deletion candidates bring usage back under this fraction of the budget; usage above
    /// it (but under budget) is a warning.
    pub warn_ratio: f64,
    /// Free space below this on the recordings disk is a preflight failure.
    pub min_free_gb: f64,
    pub auto_delete: bool,
    pub grace_hours: f64,
    pub extensions: Vec<String>,
}

impl Default for RecordingSettings {
    fn default() -> Self {
        RecordingSettings {
            dir: String::new(),
            budget_gb: 200.0,
            warn_ratio: 0.9,
            min_free_gb: 20.0,
            auto_delete: false,
            grace_hours: 24.0,
            extensions: ["mkv", "mp4", "mov", "flv", "ts", "m4v", "webm", "hybrid.mp4"].map(String::from).to_vec(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    pub sessions_days: u32,
    pub sessions_keep: usize,
    pub backups: BackupSettings,
    pub recordings: RecordingSettings,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { sessions_days: 90, sessions_keep: 10, backups: BackupSettings::default(), recordings: RecordingSettings::default() }
    }
}

fn mtime(p: &Path) -> i64 {
    std::fs::metadata(p).and_then(|m| m.modified()).ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0)
}

fn expand_home(s: &str) -> PathBuf {
    match (s.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(h)) => PathBuf::from(h).join(rest),
        _ if s == "~" => std::env::var_os("HOME").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(s)),
        _ => PathBuf::from(s),
    }
}

// ------------------------------------------------------------------------------------------
// backups

#[derive(Clone, Debug, PartialEq)]
pub struct BackupFile {
    pub path: PathBuf,
    pub bytes: u64,
    pub at: i64,
}

fn is_backup_name(n: &str) -> bool {
    n.starts_with("runtime-") && (n.ends_with(".db.zst") || n.ends_with(".db"))
}

/// Finished backups, newest first.
pub fn list_backups(dir: &Path) -> Vec<BackupFile> {
    let mut v: Vec<BackupFile> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_name().to_str().is_some_and(is_backup_name))
        .map(|e| {
            let p = e.path();
            BackupFile { bytes: e.metadata().map(|m| m.len()).unwrap_or(0), at: mtime(&p), path: p }
        })
        .collect();
    v.sort_by(|a, b| b.at.cmp(&a.at).then_with(|| b.path.cmp(&a.path)));
    v
}

fn integrity_ok(path: &Path) -> anyhow::Result<()> {
    let c = rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let r: String = c.query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
    anyhow::ensure!(r == "ok", "integrity check: {r}");
    Ok(())
}

/// Snapshot the runtime DB into `dir` (atomic: temp files, then rename). Returns the file.
pub fn make_backup(db: &se_store::Db, dir: &Path, now: i64) -> anyhow::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let stamp = util::stamp(now);
    let raw = dir.join(format!(".runtime-{stamp}.db.partial"));
    let packed = dir.join(format!(".runtime-{stamp}.db.zst.partial"));
    let out = dir.join(format!("runtime-{stamp}.db.zst"));
    let _ = std::fs::remove_file(&raw);
    let res = (|| -> anyhow::Result<()> {
        db.backup_to(&raw)?;
        integrity_ok(&raw)?;
        let mut src = std::fs::File::open(&raw)?;
        let dst = std::fs::File::create(&packed)?;
        let mut enc = zstd::Encoder::new(dst, 9)?;
        std::io::copy(&mut src, &mut enc)?;
        enc.finish()?.sync_all()?;
        std::fs::rename(&packed, &out)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&raw);
    let _ = std::fs::remove_file(&packed);
    res.map(|()| out)
}

/// Delete all but the newest `keep` backups (and stale partial files). Returns deleted paths.
pub fn prune_backups(dir: &Path, keep: usize) -> Vec<PathBuf> {
    let mut gone = Vec::new();
    for b in list_backups(dir).into_iter().skip(keep.max(1)) {
        if std::fs::remove_file(&b.path).is_ok() {
            gone.push(b.path);
        }
    }
    let now = util::unix_now();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        let n = e.file_name();
        let n = n.to_string_lossy();
        if n.starts_with(".runtime-") && n.ends_with(".partial") && now - mtime(&e.path()) > 3600 && std::fs::remove_file(e.path()).is_ok() {
            gone.push(e.path());
        }
    }
    gone
}

// ------------------------------------------------------------------------------------------
// sessions

#[derive(Clone, Debug, PartialEq)]
pub struct SessionDir {
    pub id: String,
    pub path: PathBuf,
    /// Newest write inside the session.
    pub last: i64,
    pub bytes: u64,
    pub protected: bool,
}

/// Newest file write (the directory's own mtime only when it holds no files) and total size.
fn dir_stats(p: &Path, depth: usize) -> (i64, u64) {
    let (last, bytes) = files_stats(p, depth);
    (if last == 0 { mtime(p) } else { last }, bytes)
}

fn files_stats(p: &Path, depth: usize) -> (i64, u64) {
    let mut last = 0;
    let mut bytes = 0;
    for e in std::fs::read_dir(p).into_iter().flatten().flatten() {
        let Ok(md) = e.metadata() else { continue };
        if md.is_dir() {
            if depth > 0 {
                let (l, b) = files_stats(&e.path(), depth - 1);
                last = last.max(l);
                bytes += b;
            }
        } else {
            bytes += md.len();
            last = last.max(md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs() as i64).unwrap_or(0));
        }
    }
    (last, bytes)
}

pub fn list_sessions(root: &Path) -> Vec<SessionDir> {
    let mut v: Vec<SessionDir> = std::fs::read_dir(root)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .filter_map(|e| {
            let id = e.file_name().to_str()?.to_string();
            if id.starts_with('.') {
                return None;
            }
            let p = e.path();
            let (last, bytes) = dir_stats(&p, 3);
            let protected = p.join(".keep").exists() || p.join("keep").exists();
            Some(SessionDir { id, path: p, last, bytes, protected })
        })
        .collect();
    v.sort_by(|a, b| b.last.cmp(&a.last).then_with(|| b.id.cmp(&a.id)));
    v
}

/// Sessions to delete: older than `days`, beyond the newest `keep_min`, not protected, not
/// currently in use.
pub fn prunable_sessions<'a>(all: &'a [SessionDir], now: i64, days: u32, keep_min: usize, in_use: &[String]) -> Vec<&'a SessionDir> {
    let cutoff = now - i64::from(days) * 86_400;
    all.iter().skip(keep_min).filter(|s| s.last < cutoff && !s.protected && !in_use.contains(&s.id)).collect()
}

// ------------------------------------------------------------------------------------------
// recordings

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Recording {
    pub path: PathBuf,
    pub bytes: u64,
    pub mtime: i64,
}

pub fn scan_recordings(dir: &Path, exts: &[String], depth: usize) -> Vec<Recording> {
    let mut out = Vec::new();
    fn walk(d: &Path, exts: &[String], depth: usize, out: &mut Vec<Recording>) {
        for e in std::fs::read_dir(d).into_iter().flatten().flatten() {
            let Ok(md) = e.metadata() else { continue };
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            if md.is_dir() {
                if depth > 0 && !name.starts_with('.') {
                    walk(&p, exts, depth - 1, out);
                }
            } else if md.is_file() && exts.iter().any(|x| name.ends_with(&format!(".{}", x.to_ascii_lowercase()))) {
                out.push(Recording { bytes: md.len(), mtime: mtime(&p), path: p });
            }
        }
    }
    walk(dir, exts, depth, &mut out);
    out.sort_by(|a, b| a.mtime.cmp(&b.mtime).then_with(|| a.path.cmp(&b.path)));
    out
}

/// Oldest recordings whose removal brings `total` back to at most `target` bytes. Empty
/// unless `total > budget`.
pub fn deletion_candidates(recs: &[Recording], budget: u64, target: u64) -> Vec<Recording> {
    let total: u64 = recs.iter().map(|r| r.bytes).sum();
    if total <= budget {
        return vec![];
    }
    let mut left = total;
    let mut out = Vec::new();
    for r in recs {
        if left <= target {
            break;
        }
        left -= r.bytes;
        out.push(r.clone());
    }
    out
}

/// Recording paths still needed by the clip pipeline. A `.keep` file also protects an
/// interrupted job before its database row is committed.
fn protected_recordings(ctx: &EngineCtx) -> anyhow::Result<HashSet<PathBuf>> {
    // Retention can start before the clips subsystem migrates its tables.
    let tables: HashSet<String> = ctx.db.with(|c| {
        let mut st = c.prepare("SELECT name FROM sqlite_master WHERE type='table' AND name IN ('clips', 'clip_jobs')")?;
        st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()
    })?;
    let mut pending = HashSet::new();
    let mut clip_paths = Vec::new();
    if tables.contains("clips") {
        let (sessions, recordings): (Vec<String>, Vec<String>) = ctx.db.with(|c| {
            let mut st = c.prepare("SELECT DISTINCT session FROM clips WHERE status IN ('ready', 'approved')")?;
            let sessions = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            let mut st = c.prepare("SELECT DISTINCT recording FROM clips WHERE status IN ('ready', 'approved')")?;
            let recordings = st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?;
            Ok((sessions, recordings))
        })?;
        pending.extend(sessions);
        clip_paths = recordings;
    }
    if tables.contains("clip_jobs") {
        let jobs: Vec<String> = ctx.db.with(|c| {
            let mut st = c.prepare("SELECT session FROM clip_jobs WHERE state IN ('queued', 'running')")?;
            st.query_map([], |r| r.get(0))?.collect::<rusqlite::Result<_>>()
        })?;
        pending.extend(jobs);
    }
    let mut paths: HashSet<PathBuf> = clip_paths.into_iter().map(|p| std::fs::canonicalize(&p).unwrap_or_else(|_| PathBuf::from(p))).collect();
    for session in list_sessions(&ctx.project_root.join("sessions")) {
        if !session.protected && !pending.contains(&session.id) {
            continue;
        }
        let Ok(meta) = std::fs::read_to_string(session.path.join("meta.toml")) else { continue };
        let Ok(meta) = toml::from_str::<toml::Table>(&meta) else { continue };
        if let Some(dir) = meta.get("show").and_then(|v| v.get("dir")).and_then(toml::Value::as_str) {
            paths.insert(std::fs::canonicalize(dir).unwrap_or_else(|_| PathBuf::from(dir)));
        }
        if let Some(recs) = meta.get("recordings").and_then(toml::Value::as_array) {
            for rec in recs {
                if let Some(path) = rec.get("path").and_then(toml::Value::as_str) {
                    paths.insert(std::fs::canonicalize(path).unwrap_or_else(|_| PathBuf::from(path)));
                }
            }
        }
    }
    Ok(paths)
}

fn recording_protected(path: &Path, protected: &HashSet<PathBuf>) -> bool {
    protected.iter().any(|p| path == p || (p.is_dir() && path.starts_with(p)))
}

pub fn free_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let existing = dir.ancestors().find(|p| p.exists())?;
    let c = std::ffi::CString::new(existing.as_os_str().as_bytes()).ok()?;
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `c` is a valid NUL-terminated path and `st` a properly sized out-parameter.
    let r = unsafe { libc::statvfs(c.as_ptr(), &mut st) };
    (r == 0).then(|| st.f_bavail as u64 * st.f_frsize as u64)
}

/// A recording announced for deletion.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Pending {
    pub path: PathBuf,
    pub bytes: u64,
    pub mtime: i64,
    pub warned_at: i64,
}

/// Merge the current candidate list into the pending map: new candidates start their grace
/// period now, candidates whose file changed restart it, others keep their warning time.
pub fn update_pending(prev: &[Pending], cands: &[Recording], now: i64) -> Vec<Pending> {
    cands
        .iter()
        .map(|c| {
            let warned_at = prev.iter().find(|p| p.path == c.path && p.mtime == c.mtime && p.bytes == c.bytes).map(|p| p.warned_at).unwrap_or(now);
            Pending { path: c.path.clone(), bytes: c.bytes, mtime: c.mtime, warned_at }
        })
        .collect()
}

/// Pending recordings whose grace period is over (and that are unchanged on disk).
pub fn due_for_deletion(pending: &[Pending], now: i64, grace_s: i64) -> Vec<&Pending> {
    pending.iter().filter(|p| now >= p.warned_at + grace_s && mtime(&p.path) == p.mtime).collect()
}

// ------------------------------------------------------------------------------------------
// subsystem

#[derive(Default)]
struct Status {
    backups: Vec<BackupFile>,
    backup_error: Option<String>,
    backup_dir: PathBuf,
    last_sessions_prune: i64,
    sessions: usize,
    sessions_bytes: u64,
    prunable_sessions: Vec<String>,
    rec_dir: PathBuf,
    rec_total: u64,
    rec_count: usize,
    rec_free: Option<u64>,
    pending: Vec<Pending>,
    notified: HashMap<&'static str, i64>,
}

fn declare(ctx: &EngineCtx) {
    let own = |m: Meta| m.readonly().owner(TARGET);
    ctx.hub.declare("retention.backup.last", own(Meta::int(0, [0.0, 1e12]).unit("unix s").describe("Last runtime DB backup")));
    ctx.hub.declare("retention.recordings.used_gb", own(Meta::float(0.0, [0.0, 1e6]).unit("GB")));
    ctx.hub.declare("retention.recordings.budget_gb", own(Meta::float(0.0, [0.0, 1e6]).unit("GB")));
    ctx.hub.declare("retention.recordings.free_gb", own(Meta::float(0.0, [0.0, 1e6]).unit("GB")));
    ctx.hub.declare("retention.recordings.pending", own(Meta::int(0, [0.0, 1e9]).describe("Recordings announced for deletion")));
}

pub fn start(ctx: EngineCtx) {
    declare(&ctx);
    let status = Arc::new(Mutex::new(Status::default()));
    if let Ok(Some(v)) = ctx.db.kv_get(KV_NS, KV_PENDING)
        && let Ok(p) = serde_json::to_value(&v).and_then(serde_json::from_value::<Vec<Pending>>)
    {
        status.lock().pending = p;
    }
    {
        let status = status.clone();
        ctx.hub.register_query(
            "retention",
            Arc::new(move |_n, _a| {
                let v = query(&status.lock());
                Box::pin(async move { Ok(v) })
            }),
        );
    }
    let actions = ctx.hub.route_actions("retention");
    tokio::spawn(run(ctx, status, actions));
}

fn query(s: &Status) -> Value {
    Value::map()
        .with(
            "backups",
            Value::map().with("dir", s.backup_dir.display().to_string()).with("error", s.backup_error.clone().map(Value::Str).unwrap_or_default()).with(
                "files",
                s.backups
                    .iter()
                    .map(|b| Value::map().with("path", b.path.display().to_string()).with("bytes", b.bytes as i64).with("at", b.at))
                    .collect::<Vec<_>>(),
            ),
        )
        .with(
            "sessions",
            Value::map()
                .with("count", s.sessions as i64)
                .with("bytes", s.sessions_bytes as i64)
                .with("prunable", s.prunable_sessions.iter().cloned().map(Value::Str).collect::<Vec<_>>())
                .with("last_prune", s.last_sessions_prune),
        )
        .with(
            "recordings",
            Value::map()
                .with("dir", s.rec_dir.display().to_string())
                .with("bytes", s.rec_total as i64)
                .with("count", s.rec_count as i64)
                .with("free", s.rec_free.map(|f| Value::Int(f as i64)).unwrap_or_default())
                .with(
                    "pending",
                    s.pending
                        .iter()
                        .map(|p| {
                            Value::map()
                                .with("path", p.path.display().to_string())
                                .with("bytes", p.bytes as i64)
                                .with("mtime", p.mtime)
                                .with("warned_at", p.warned_at)
                        })
                        .collect::<Vec<_>>(),
                ),
        )
}

/// Ask for a desktop notification (at most once per `kind` every 6 h; the notifier also skips it
/// while the UI has focus).
fn notify(ctx: &EngineCtx, status: &Mutex<Status>, kind: &'static str, urgency: &str, title: &str, body: &str) {
    let now = util::unix_now();
    {
        let mut s = status.lock();
        if s.notified.get(kind).is_some_and(|t| now - t < 6 * 3600) {
            return;
        }
        s.notified.insert(kind, now);
    }
    let args = Value::map().with("key", kind).with("title", title).with("body", body).with("urgency", urgency).with("open", "maintenance");
    ctx.hub.command(util::action("notify.send", args));
}

struct Env {
    mode: String,
    recording: bool,
}

fn env(ctx: &EngineCtx) -> Env {
    let snap = ctx.hub.snapshot.load();
    Env { mode: snap.str("show.mode").unwrap_or("offline").to_string(), recording: snap.bool("obs.record.active") }
}

async fn run(mut ctx: EngineCtx, status: Arc<Mutex<Status>>, mut actions: tokio::sync::mpsc::UnboundedReceiver<se_proto::Command>) {
    let mut section: Section<Settings> = Section::new("retention", TARGET);
    section.reload(&ctx);
    let mut tick = tokio::time::interval(Duration::from_secs(60));
    let mut last_rec_scan = 0i64;
    loop {
        tokio::select! {
            r = ctx.config.changed() => {
                if r.is_err() { break; }
                section.reload(&ctx);
                last_rec_scan = 0; // [recording] dir can change without [retention] changing
            }
            _ = tick.tick() => {
                let cfg = section.value.clone();
                let now = util::unix_now();
                backups_step(&ctx, &status, &cfg, now, false).await;
                if now - status.lock().last_sessions_prune >= 86_400 && !util::on_air(&env(&ctx).mode) {
                    sessions_step(&ctx, &status, &cfg, now, true).await;
                }
                if now - last_rec_scan >= 600 {
                    last_rec_scan = now;
                    recordings_step(&ctx, &status, &cfg, now, false).await;
                }
            }
            a = actions.recv() => {
                let Some(cmd) = a else { break };
                let Op::Action { name, .. } = &cmd.op else { continue };
                let cfg = section.value.clone();
                let now = util::unix_now();
                match name.as_str() {
                    "retention.backup_now" => backups_step(&ctx, &status, &cfg, now, true).await,
                    "retention.prune_sessions" => sessions_step(&ctx, &status, &cfg, now, true).await,
                    "retention.prune_recordings" => recordings_step(&ctx, &status, &cfg, now, true).await,
                    "retention.scan" => {
                        sessions_step(&ctx, &status, &cfg, now, false).await;
                        recordings_step(&ctx, &status, &cfg, now, false).await;
                        last_rec_scan = now;
                    }
                    other => ctx.hub.log("warn", TARGET, format!("unknown action {other}")),
                }
            }
        }
    }
}

async fn backups_step(ctx: &EngineCtx, status: &Mutex<Status>, cfg: &Settings, now: i64, force: bool) {
    let b = &cfg.backups;
    let dir = if b.dir.trim().is_empty() { ctx.data_dir.join("backups") } else { expand_home(b.dir.trim()) };
    let interval = se_proto::parse_duration_ms(&b.interval).map(|ms| (ms / 1000) as i64).filter(|s| *s >= 60).unwrap_or(86_400);
    let listed = {
        let d = dir.clone();
        tokio::task::spawn_blocking(move || list_backups(&d)).await.unwrap_or_default()
    };
    let last = listed.first().map(|f| f.at).unwrap_or(0);
    let age = now - last;
    let due = force || (b.enabled && age >= interval && (!util::on_air(&env(ctx).mode) || age >= 2 * interval));
    let mut error = status.lock().backup_error.clone();
    let mut files = listed;
    if due {
        let (db, d, keep) = (ctx.db.clone(), dir.clone(), b.keep);
        let r = tokio::task::spawn_blocking(move || {
            let out = make_backup(&db, &d, now);
            let pruned = prune_backups(&d, keep);
            (out, pruned, list_backups(&d))
        })
        .await;
        match r {
            Ok((Ok(path), pruned, list)) => {
                error = None;
                files = list;
                ctx.hub.log("info", TARGET, format!("runtime DB backed up to {} ({} old backups removed)", path.display(), pruned.len()));
                ctx.hub.emit(Event::new("retention.backup", Origin::System, Value::map().with("path", path.display().to_string())));
            }
            Ok((Err(e), _, list)) => {
                files = list;
                error = Some(format!("{e:#}"));
                ctx.hub.log("error", TARGET, format!("runtime DB backup failed: {e:#}"));
                notify(
                    ctx,
                    status,
                    "backup",
                    "normal",
                    "Backups aren't being made",
                    "Stream Engine couldn't save its backup. Open Settings → Backups to see why.",
                );
            }
            Err(e) => error = Some(format!("backup task: {e}")),
        }
    }
    let last = files.first().map(|f| f.at).unwrap_or(0);
    let health = match (&error, last) {
        (Some(e), _) => util::health("fail", format!("last backup attempt failed: {e}")),
        _ if !b.enabled => util::health("warn", "runtime DB backups are disabled ([retention.backups] enabled = false)"),
        (None, 0) => util::health("warn", "no runtime DB backup yet"),
        (None, t) if now - t > 2 * interval => util::health("warn", format!("last runtime DB backup is {} h old", (now - t) / 3600)),
        (None, t) => util::health("pass", format!("last backup {} h ago, {} kept in {}", (now - t) / 3600, files.len(), dir.display())),
    };
    ctx.hub.publish("health.backup", health);
    ctx.hub.publish("retention.backup.last", Value::Int(last));
    let mut s = status.lock();
    s.backups = files;
    s.backup_error = error;
    s.backup_dir = dir;
}

async fn sessions_step(ctx: &EngineCtx, status: &Mutex<Status>, cfg: &Settings, now: i64, delete: bool) {
    let root = ctx.project_root.join("sessions");
    let mut in_use = vec![ctx.hub.info.read().session.clone()];
    if let Ok(Some((id, _))) = ctx.db.open_session() {
        in_use.push(id);
    }
    let (days, keep) = (cfg.sessions_days.max(1), cfg.sessions_keep);
    let db = ctx.db.clone();
    let r = tokio::task::spawn_blocking(move || {
        let all = list_sessions(&root);
        let prune: Vec<SessionDir> = prunable_sessions(&all, now, days, keep, &in_use).into_iter().cloned().collect();
        let mut deleted = Vec::new();
        if delete {
            for s in &prune {
                match std::fs::remove_dir_all(&s.path) {
                    Ok(()) => {
                        let _ = db.with(|c| c.execute("DELETE FROM sessions WHERE id = ?1", [&s.id]));
                        deleted.push(s.clone());
                    }
                    Err(e) => tracing::warn!("delete session {}: {e}", s.path.display()),
                }
            }
        }
        (list_sessions(&root), prune, deleted)
    })
    .await;
    let Ok((all, prune, deleted)) = r else { return };
    if !deleted.is_empty() {
        let bytes: u64 = deleted.iter().map(|s| s.bytes).sum();
        ctx.hub.log("info", TARGET, format!("deleted {} session logs older than {days} days ({:.1} GB)", deleted.len(), bytes as f64 / GB));
    }
    let mut s = status.lock();
    if delete {
        s.last_sessions_prune = now;
    }
    s.sessions = all.len();
    s.sessions_bytes = all.iter().map(|x| x.bytes).sum();
    s.prunable_sessions = if delete { vec![] } else { prune.into_iter().map(|p| p.id).collect() };
}

async fn recordings_step(ctx: &EngineCtx, status: &Mutex<Status>, cfg: &Settings, now: i64, prune_now: bool) {
    let rc = cfg.recordings.clone();
    let e = env(ctx);
    let dir = if !rc.dir.trim().is_empty() {
        expand_home(rc.dir.trim())
    } else {
        let recording_dir = ctx.project_section("recording").and_then(|v| v.get("dir").and_then(toml::Value::as_str).map(str::to_owned));
        if recording_dir.as_ref().is_some_and(|d| d.trim().is_empty()) {
            ctx.hub.publish("health.recordings", util::health("fail", "[recording] dir is empty; recordings retention paused"));
            return;
        }
        recording_dir.map(|d| expand_home(&d)).unwrap_or_else(|| expand_home("~/Videos/Stream Engine"))
    };
    if !dir.is_absolute() {
        ctx.hub.publish("health.recordings", util::health("fail", format!("invalid recordings directory: {}", dir.display())));
        return;
    }
    let dir = dir.canonicalize().unwrap_or(dir);
    let budget = (rc.budget_gb.max(0.0) * GB) as u64;
    let target = (rc.budget_gb.max(0.0) * rc.warn_ratio.clamp(0.1, 1.0) * GB) as u64;
    let exts = rc.extensions.clone();
    let d = dir.clone();
    let Ok((recs, free)) = tokio::task::spawn_blocking(move || (scan_recordings(&d, &exts, 3), free_bytes(&d))).await else { return };
    let total: u64 = recs.iter().map(|r| r.bytes).sum();
    let protected = match protected_recordings(ctx) {
        Ok(paths) => paths,
        Err(err) => {
            ctx.hub.log("error", TARGET, format!("cannot verify pending clips; not pruning recordings: {err:#}"));
            ctx.hub.publish("health.recordings", util::health("fail", "cannot verify pending clips; recordings retention paused"));
            return;
        }
    };
    let mut left = total;
    let mut cands = Vec::new();
    if total > budget {
        for rec in &recs {
            if left <= target {
                break;
            }
            if !recording_protected(&rec.path, &protected) {
                cands.push(rec.clone());
                left -= rec.bytes;
            }
        }
    }
    let prev = status.lock().pending.clone();
    let mut pending = update_pending(&prev, &cands, now);
    let busy = e.recording || util::on_air(&e.mode);
    // deletion: operator request, or auto after the grace period
    let to_delete: Vec<Pending> = if busy {
        if prune_now {
            ctx.hub.log("warn", TARGET, "not deleting recordings while recording or on air");
        }
        vec![]
    } else if prune_now {
        pending.clone()
    } else if rc.auto_delete {
        due_for_deletion(&pending, now, (rc.grace_hours.max(0.0) * 3600.0) as i64).into_iter().cloned().collect()
    } else {
        vec![]
    };
    let mut freed = 0u64;
    for p in &to_delete {
        if recording_protected(&p.path, &protected) {
            continue;
        }
        match std::fs::remove_file(&p.path) {
            Ok(()) => {
                freed += p.bytes;
                ctx.hub.log(
                    "warn",
                    TARGET,
                    format!("deleted recording {} ({:.1} GB) to stay within the {:.0} GB budget", p.path.display(), p.bytes as f64 / GB, rc.budget_gb),
                );
                ctx.hub.emit(Event::new(
                    "retention.recording_deleted",
                    Origin::System,
                    Value::map().with("path", p.path.display().to_string()).with("bytes", p.bytes as i64),
                ));
                pending.retain(|x| x.path != p.path);
            }
            Err(err) => ctx.hub.log("error", TARGET, format!("could not delete {}: {err}", p.path.display())),
        }
    }
    let total = total - freed.min(total);
    let free = free.map(|f| f + freed);
    let pending_bytes: u64 = pending.iter().map(|p| p.bytes).sum();
    let health = if free.is_some_and(|f| (f as f64) < rc.min_free_gb * GB) {
        notify(
            ctx,
            status,
            "disk",
            "critical",
            "The recordings disk is almost full",
            &format!("Only {:.0} GB left where your recordings go. Delete old recordings in Settings → Backups.", free.unwrap_or(0) as f64 / GB),
        );
        util::health("fail", format!("only {:.1} GB free on the recordings disk ({})", free.unwrap_or(0) as f64 / GB, dir.display()))
    } else if !pending.is_empty() {
        let when = if rc.auto_delete {
            let first = pending.iter().map(|p| p.warned_at).min().unwrap_or(now) + (rc.grace_hours * 3600.0) as i64;
            format!("will be deleted after {} UTC unless moved", util::stamp(first))
        } else {
            "delete them from the UI or raise [retention.recordings] budget_gb".to_string()
        };
        let body = format!(
            "{:.0} of {:.0} GB used: {} oldest recordings ({:.1} GB) {when}",
            total as f64 / GB,
            rc.budget_gb,
            pending.len(),
            pending_bytes as f64 / GB
        );
        notify(
            ctx,
            status,
            "budget",
            "normal",
            "Recordings use more space than you allowed",
            "Delete old recordings in Settings → Backups, or raise the limit.",
        );
        util::health("warn", body)
    } else if total > target {
        util::health("warn", format!("recordings use {:.0} of {:.0} GB", total as f64 / GB, rc.budget_gb))
    } else {
        util::health("pass", format!("recordings {:.0}/{:.0} GB, {:.0} GB free", total as f64 / GB, rc.budget_gb, free.unwrap_or(0) as f64 / GB))
    };
    ctx.hub.publish("health.recordings", health);
    ctx.hub.publish("retention.recordings.used_gb", Value::Float(total as f64 / GB));
    ctx.hub.publish("retention.recordings.budget_gb", Value::Float(rc.budget_gb));
    ctx.hub.publish("retention.recordings.free_gb", Value::Float(free.unwrap_or(0) as f64 / GB));
    ctx.hub.publish("retention.recordings.pending", Value::Int(pending.len() as i64));
    if pending != prev {
        let _ = ctx.db.kv_set(KV_NS, KV_PENDING, &util::to_value(&pending));
    }
    let mut s = status.lock();
    s.rec_dir = dir;
    s.rec_total = total;
    s.rec_count = recs.len() - to_delete.len().min(recs.len());
    s.rec_free = free;
    s.pending = pending;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::SystemTime;

    fn touch(p: &Path, bytes: usize, age_s: i64) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, vec![0u8; bytes]).unwrap();
        let t = SystemTime::now() - Duration::from_secs(age_s as u64);
        std::fs::File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    #[test]
    fn backup_roundtrip_and_prune() {
        let d = tempfile::tempdir().unwrap();
        let db = se_store::Db::open(&d.path().join("runtime.db")).unwrap();
        db.kv_set("t", "k", &Value::Str("v".into())).unwrap();
        let bdir = d.path().join("backups");
        let now = util::unix_now();
        let mut made = vec![];
        for i in 0..4 {
            let p = make_backup(&db, &bdir, now + i).unwrap();
            // distinct mtimes for ordering
            std::fs::File::options().write(true).open(&p).unwrap().set_modified(SystemTime::now() - Duration::from_secs(100 - i as u64)).unwrap();
            made.push(p);
        }
        assert_eq!(list_backups(&bdir).len(), 4);
        // restore the newest: decompress and read the kv back
        let newest = &list_backups(&bdir)[0].path;
        assert_eq!(newest, made.last().unwrap());
        let raw = zstd::decode_all(std::fs::File::open(newest).unwrap()).unwrap();
        let restored = d.path().join("restored.db");
        std::fs::write(&restored, raw).unwrap();
        integrity_ok(&restored).unwrap();
        let rdb = se_store::Db::open(&restored).unwrap();
        assert_eq!(rdb.kv_get("t", "k").unwrap(), Some(Value::Str("v".into())));
        // prune keeps the newest two; no partial files left behind
        let gone = prune_backups(&bdir, 2);
        assert_eq!(gone.len(), 2);
        let left: Vec<PathBuf> = list_backups(&bdir).into_iter().map(|b| b.path).collect();
        assert_eq!(left, vec![made[3].clone(), made[2].clone()]);
        assert!(std::fs::read_dir(&bdir).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().ends_with(".partial")));
    }

    #[test]
    fn session_retention_rules() {
        let d = tempfile::tempdir().unwrap();
        let root = d.path();
        let day = 86_400;
        for (id, age_days) in [("s1", 200), ("s2", 150), ("s3", 120), ("s4", 100), ("s5", 5), ("s6", 1)] {
            touch(&root.join(id).join("events.log.zst"), 10, age_days * day);
        }
        touch(&root.join("s2").join(".keep"), 0, 150 * day);
        let all = list_sessions(root);
        assert_eq!(all.first().unwrap().id, "s6", "newest first");
        let now = util::unix_now();
        let ids = |v: Vec<&SessionDir>| v.into_iter().map(|s| s.id.clone()).collect::<Vec<_>>();
        // 90 days, keep the newest 2: s1, s3, s4 go; s2 is protected
        assert_eq!(ids(prunable_sessions(&all, now, 90, 2, &[])), vec!["s4", "s3", "s1"]);
        // keep_min protects the newest n regardless of age
        assert_eq!(ids(prunable_sessions(&all, now, 90, 5, &[])), vec!["s1"]);
        // the running session is never deleted
        assert_eq!(ids(prunable_sessions(&all, now, 90, 2, &["s1".into()])), vec!["s4", "s3"]);
        // a recently written file inside keeps an old session
        touch(&root.join("s1").join("clips").join("new.mp4"), 1, 10);
        let all = list_sessions(root);
        assert_eq!(ids(prunable_sessions(&all, now, 90, 2, &[])), vec!["s4", "s3"]);
    }

    #[test]
    fn recordings_budget_candidates() {
        let d = tempfile::tempdir().unwrap();
        let exts = RecordingSettings::default().extensions;
        touch(&d.path().join("2026-01-01 20-00-00.mkv"), 400, 5000);
        touch(&d.path().join("2026-01-02 20-00-00.mp4"), 300, 4000);
        touch(&d.path().join("vertical/2026-01-02 20-00-00.mp4"), 200, 3000);
        touch(&d.path().join("2026-01-03 20-00-00.mkv"), 100, 2000);
        touch(&d.path().join("notes.txt"), 999, 1000);
        let recs = scan_recordings(d.path(), &exts, 3);
        assert_eq!(recs.len(), 4, "only video files, subfolders included");
        assert_eq!(recs.iter().map(|r| r.bytes).sum::<u64>(), 1000);
        assert!(deletion_candidates(&recs, 1000, 900).is_empty(), "at budget: nothing");
        let c = deletion_candidates(&recs, 900, 500);
        assert_eq!(c.iter().map(|r| r.bytes).collect::<Vec<_>>(), vec![400, 300], "oldest first until under target");
        let c = deletion_candidates(&recs, 999, 950);
        assert_eq!(c.len(), 1);
        assert!(free_bytes(d.path()).is_some_and(|f| f > 0));
    }

    #[test]
    fn grace_period_before_deletion() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("a.mkv");
        touch(&p, 10, 1000);
        let rec = Recording { path: p.clone(), bytes: 10, mtime: mtime(&p) };
        let pend = update_pending(&[], std::slice::from_ref(&rec), 100);
        assert_eq!(pend[0].warned_at, 100);
        // still a candidate later: keeps its original warning time
        let pend2 = update_pending(&pend, std::slice::from_ref(&rec), 5000);
        assert_eq!(pend2[0].warned_at, 100);
        assert!(due_for_deletion(&pend2, 100 + 3599, 3600).is_empty());
        assert_eq!(due_for_deletion(&pend2, 100 + 3600, 3600).len(), 1);
        // the file changed since the warning → not deleted, warning restarts
        touch(&p, 20, 0);
        assert!(due_for_deletion(&pend2, 100 + 3600, 3600).is_empty());
        let rec2 = Recording { path: p.clone(), bytes: 20, mtime: mtime(&p) };
        assert_eq!(update_pending(&pend2, &[rec2], 9000)[0].warned_at, 9000);
        // no longer a candidate → dropped
        assert!(update_pending(&pend2, &[], 9000).is_empty());
    }

    #[test]
    fn pending_clip_protects_show_recording_even_from_manual_prune() {
        let d = tempfile::tempdir().unwrap();
        let db = se_store::Db::memory().unwrap();
        db.with(|c| {
            c.execute_batch(
                "CREATE TABLE clips (session TEXT, status TEXT, recording TEXT);
             CREATE TABLE clip_jobs (session TEXT, state TEXT);
             INSERT INTO clips VALUES ('s1', 'ready', '/missing/example.mkv');
             INSERT INTO clips VALUES ('s2', 'uploaded', '/missing/uploaded.mkv');",
            )
        })
        .unwrap();
        let (hub, _rx) = se_hub::Hub::new(Arc::new(se_clock::Clock::new()));
        let (_tx, config) = tokio::sync::watch::channel(Arc::new(se_core::Config::default()));
        let ctx = EngineCtx {
            hub,
            db,
            project_root: d.path().to_path_buf(),
            data_dir: d.path().to_path_buf(),
            share_dir: d.path().to_path_buf(),
            config,
            http: "127.0.0.1:0".parse().unwrap(),
            dev: true,
        };
        let show = d.path().join("shows").join("show");
        touch(&show.join("one.mkv"), 100, 1000);
        touch(&show.join("two.mp4"), 100, 900);
        ctx.db.with(|c| c.execute("UPDATE clips SET recording = ?1 WHERE session = 's1'", [show.join("one.mkv").to_str().unwrap()])).unwrap();
        std::fs::create_dir_all(d.path().join("sessions/s1")).unwrap();
        std::fs::create_dir_all(d.path().join("sessions/s2")).unwrap();
        std::fs::write(d.path().join("sessions/s1/meta.toml"), format!("show = {{ dir = {:?} }}\n", show.to_str().unwrap())).unwrap();
        std::fs::write(d.path().join("sessions/s2/meta.toml"), "recordings = []\n").unwrap();
        let protected = protected_recordings(&ctx).unwrap();
        assert!(protected.contains(&show.join("one.mkv")));
        assert!(recording_protected(&show.join("one.mkv"), &protected));
        assert!(recording_protected(&show.join("two.mp4"), &protected));
        assert!(!recording_protected(&d.path().join("shows/another.mkv"), &protected));
        std::fs::remove_file(d.path().join("sessions/s1/meta.toml")).unwrap();
        let direct_paths = protected_recordings(&ctx).unwrap();
        assert!(recording_protected(&show.join("one.mkv"), &direct_paths));
        assert!(!recording_protected(&show.join("two.mp4"), &direct_paths));
        ctx.db.with(|c| c.execute("UPDATE clips SET status = 'uploaded' WHERE session = 's1'", [])).unwrap();
        assert!(!recording_protected(&show.join("one.mkv"), &protected_recordings(&ctx).unwrap()));
        ctx.db.with(|c| c.execute_batch("DROP TABLE clips; DROP TABLE clip_jobs")).unwrap();
        assert!(protected_recordings(&ctx).unwrap().is_empty(), "fresh runtime database has no pending clips");
    }
}
