//! Runtime DB tables: `clips` (the review queue), `clip_jobs` (post-stream job state, so a
//! job interrupted by an engine restart is resumed), and `archive_jobs` (the per-show archive
//! stage machine, see `crate::archive`).

use crate::show;
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use se_proto::Value;
use se_store::Db;
use std::path::Path;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS clips (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session TEXT NOT NULL,
  key TEXT NOT NULL,
  rank INTEGER NOT NULL DEFAULT 0,
  score REAL NOT NULL,
  marker_score REAL NOT NULL,
  reasons TEXT NOT NULL DEFAULT '[]',
  labels TEXT NOT NULL DEFAULT '[]',
  title TEXT,
  start_ns INTEGER NOT NULL,
  peak_ns INTEGER NOT NULL,
  end_ns INTEGER NOT NULL,
  recording TEXT NOT NULL,
  rec_start_ns INTEGER NOT NULL,
  in_s REAL NOT NULL,
  out_s REAL NOT NULL,
  peak_s REAL NOT NULL,
  wide_path TEXT,
  tall_path TEXT,
  wide_thumb TEXT,
  tall_thumb TEXT,
  captions TEXT NOT NULL DEFAULT '',
  words TEXT NOT NULL DEFAULT '[]',
  transcript_from REAL NOT NULL DEFAULT 0,
  transcript_to REAL NOT NULL DEFAULT 0,
  audio_note TEXT NOT NULL DEFAULT '',
  music_dropped INTEGER NOT NULL DEFAULT 1,
  encoder TEXT NOT NULL DEFAULT '',
  version INTEGER NOT NULL DEFAULT 1,
  status TEXT NOT NULL DEFAULT 'ready',
  error TEXT,
  upload_url TEXT,
  created_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  UNIQUE (session, key)
);
CREATE INDEX IF NOT EXISTS clips_by_session ON clips (session, rank);
CREATE INDEX IF NOT EXISTS clips_by_status ON clips (status);
CREATE TABLE IF NOT EXISTS clip_jobs (
  session TEXT PRIMARY KEY,
  state TEXT NOT NULL,
  stage TEXT NOT NULL DEFAULT '',
  error TEXT,
  clips INTEGER NOT NULL DEFAULT 0,
  timings TEXT NOT NULL DEFAULT '{}',
  queued_at INTEGER NOT NULL,
  started_at INTEGER,
  finished_at INTEGER
);
"#;

const CONTEXT_SCHEMA: &str = r#"
ALTER TABLE clips ADD COLUMN kind TEXT NOT NULL DEFAULT 'talk';
ALTER TABLE clips ADD COLUMN song TEXT;
ALTER TABLE clips ADD COLUMN requester TEXT;
ALTER TABLE clips ADD COLUMN dmca_risk INTEGER NOT NULL DEFAULT 0;
ALTER TABLE clips ADD COLUMN context TEXT NOT NULL DEFAULT '{}';
CREATE TABLE clip_feedback (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  clip_id INTEGER NOT NULL,
  session TEXT NOT NULL,
  action TEXT NOT NULL,
  at INTEGER NOT NULL,
  context TEXT NOT NULL
);
CREATE INDEX clip_feedback_session ON clip_feedback(session, id);
"#;

const ARCHIVE_SCHEMA: &str = r#"
CREATE TABLE archive_jobs (
  session TEXT PRIMARY KEY,
  show TEXT NOT NULL,
  show_dir TEXT NOT NULL,
  dest TEXT NOT NULL DEFAULT '',
  stage TEXT NOT NULL,
  state TEXT NOT NULL,
  detail TEXT NOT NULL DEFAULT '',
  attempts INTEGER NOT NULL DEFAULT 0,
  next_try INTEGER NOT NULL DEFAULT 0,
  progress REAL NOT NULL DEFAULT 0,
  masters TEXT NOT NULL DEFAULT '[]',
  isos TEXT NOT NULL DEFAULT '[]',
  isos_left INTEGER NOT NULL DEFAULT 0,
  bytes_in INTEGER NOT NULL DEFAULT 0,
  bytes_out INTEGER NOT NULL DEFAULT 0,
  video_kbps INTEGER NOT NULL DEFAULT 0,
  ended_at INTEGER NOT NULL DEFAULT 0,
  queued_at INTEGER NOT NULL,
  updated_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE INDEX archive_jobs_open ON archive_jobs (state, queued_at);
"#;

pub fn migrate(db: &Db) -> Result<()> {
    db.migrate("clips/1", SCHEMA)?;
    db.migrate("clips/2", CONTEXT_SCHEMA)?;
    db.migrate("clips/3", ARCHIVE_SCHEMA)
}

fn now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// Review states.
pub const STATUSES: &[&str] = &["ready", "approved", "rejected", "uploaded", "failed"];

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ClipRow {
    pub id: i64,
    pub session: String,
    pub key: String,
    pub rank: i64,
    pub score: f64,
    pub marker_score: f64,
    pub reasons: Vec<String>,
    pub labels: Vec<String>,
    pub title: Option<String>,
    pub kind: String,
    pub song: Option<String>,
    pub requester: Option<String>,
    pub dmca_risk: bool,
    /// Additional song/video/channel context as JSON.
    pub context: String,
    pub start_ns: i64,
    pub peak_ns: i64,
    pub end_ns: i64,
    pub recording: String,
    pub rec_start_ns: i64,
    pub in_s: f64,
    pub out_s: f64,
    pub peak_s: f64,
    pub wide_path: Option<String>,
    pub tall_path: Option<String>,
    pub wide_thumb: Option<String>,
    pub tall_thumb: Option<String>,
    pub captions: String,
    /// JSON array of words (recording seconds) covering `transcript_from..transcript_to`.
    pub words: String,
    pub transcript_from: f64,
    pub transcript_to: f64,
    pub audio_note: String,
    pub music_dropped: bool,
    pub encoder: String,
    pub version: i64,
    pub status: String,
    pub error: Option<String>,
    pub upload_url: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

const COLS: &str = "id, session, key, rank, score, marker_score, reasons, labels, title, start_ns, peak_ns, end_ns, recording, rec_start_ns, \
    in_s, out_s, peak_s, wide_path, tall_path, wide_thumb, tall_thumb, captions, words, transcript_from, transcript_to, audio_note, music_dropped, \
    encoder, version, status, error, upload_url, created_at, updated_at, kind, song, requester, dmca_risk, context";

fn json_list(s: String) -> Vec<String> {
    serde_json::from_str(&s).unwrap_or_default()
}

fn row(r: &rusqlite::Row) -> rusqlite::Result<ClipRow> {
    Ok(ClipRow {
        id: r.get(0)?,
        session: r.get(1)?,
        key: r.get(2)?,
        rank: r.get(3)?,
        score: r.get(4)?,
        marker_score: r.get(5)?,
        reasons: json_list(r.get(6)?),
        labels: json_list(r.get(7)?),
        title: r.get(8)?,
        start_ns: r.get(9)?,
        peak_ns: r.get(10)?,
        end_ns: r.get(11)?,
        recording: r.get(12)?,
        rec_start_ns: r.get(13)?,
        in_s: r.get(14)?,
        out_s: r.get(15)?,
        peak_s: r.get(16)?,
        wide_path: r.get(17)?,
        tall_path: r.get(18)?,
        wide_thumb: r.get(19)?,
        tall_thumb: r.get(20)?,
        captions: r.get(21)?,
        words: r.get(22)?,
        transcript_from: r.get(23)?,
        transcript_to: r.get(24)?,
        audio_note: r.get(25)?,
        music_dropped: r.get::<_, i64>(26)? != 0,
        encoder: r.get(27)?,
        version: r.get(28)?,
        status: r.get(29)?,
        error: r.get(30)?,
        upload_url: r.get(31)?,
        created_at: r.get(32)?,
        kind: r.get(34)?,
        song: r.get(35)?,
        requester: r.get(36)?,
        dmca_risk: r.get::<_, i64>(37)? != 0,
        context: r.get(38)?,
        updated_at: r.get(33)?,
    })
}

impl ClipRow {
    /// Review-queue entry (no word list; the UI gets captions text).
    pub fn to_value(&self) -> Value {
        let opt = |s: &Option<String>| s.clone().map(Value::Str).unwrap_or_default();
        Value::map()
            .with("id", self.id)
            .with("session", self.session.clone())
            .with("key", self.key.clone())
            .with("rank", self.rank)
            .with("score", (self.score * 1000.0).round() / 1000.0)
            .with("marker_score", (self.marker_score * 1000.0).round() / 1000.0)
            .with("reasons", self.reasons.clone())
            .with("labels", self.labels.clone())
            .with("title", opt(&self.title))
            .with("kind", self.kind.clone())
            .with("song", opt(&self.song))
            .with("requester", opt(&self.requester))
            .with("dmca_risk", self.dmca_risk)
            .with("context", serde_json::from_str::<Value>(&self.context).unwrap_or_default())
            .with("start_ns", self.start_ns)
            .with("peak_ns", self.peak_ns)
            .with("end_ns", self.end_ns)
            .with("recording", self.recording.clone())
            .with("in", self.in_s)
            .with("out", self.out_s)
            .with("peak", self.peak_s)
            .with("duration", ((self.out_s - self.in_s) * 100.0).round() / 100.0)
            .with("transcript_from", self.transcript_from)
            .with("transcript_to", self.transcript_to)
            .with("wide", opt(&self.wide_path))
            .with("tall", opt(&self.tall_path))
            .with("wide_thumb", opt(&self.wide_thumb))
            .with("tall_thumb", opt(&self.tall_thumb))
            .with("captions", self.captions.clone())
            .with("audio", self.audio_note.clone())
            .with("music_dropped", self.music_dropped)
            .with("encoder", self.encoder.clone())
            .with("version", self.version)
            .with("status", self.status.clone())
            .with("error", opt(&self.error))
            .with("url", opt(&self.upload_url))
    }
}

/// Insert or refresh a clip by `(session, key)`. Review decisions survive a re-run: an
/// approved/rejected clip keeps its status; failed/ready ones take the new status.
pub fn upsert(db: &Db, c: &ClipRow) -> Result<i64> {
    let t = now();
    db.with(|conn| {
        conn.execute(
            "INSERT INTO clips (session, key, rank, score, marker_score, reasons, labels, title, start_ns, peak_ns, end_ns, recording, rec_start_ns,
               in_s, out_s, peak_s, wide_path, tall_path, wide_thumb, tall_thumb, captions, words, transcript_from, transcript_to, audio_note,
               music_dropped, encoder, version, status, error, created_at, updated_at, kind, song, requester, dmca_risk, context)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, 1, ?28, ?29, ?30, ?30, ?31, ?32, ?33, ?34, ?35)
             ON CONFLICT (session, key) DO UPDATE SET
               rank = excluded.rank, score = excluded.score, marker_score = excluded.marker_score, reasons = excluded.reasons,
               labels = excluded.labels, title = COALESCE(excluded.title, clips.title), start_ns = excluded.start_ns, peak_ns = excluded.peak_ns,
               end_ns = excluded.end_ns, recording = excluded.recording, rec_start_ns = excluded.rec_start_ns, in_s = excluded.in_s,
               out_s = excluded.out_s, peak_s = excluded.peak_s, wide_path = excluded.wide_path, tall_path = excluded.tall_path,
               wide_thumb = excluded.wide_thumb, tall_thumb = excluded.tall_thumb, captions = excluded.captions, words = excluded.words,
               transcript_from = excluded.transcript_from, transcript_to = excluded.transcript_to, audio_note = excluded.audio_note,
               music_dropped = excluded.music_dropped, encoder = excluded.encoder, version = clips.version + 1,
               kind = excluded.kind, song = excluded.song, requester = excluded.requester, dmca_risk = excluded.dmca_risk, context = excluded.context,
               status = CASE WHEN clips.status IN ('approved', 'rejected') AND excluded.status = 'ready' THEN clips.status ELSE excluded.status END,
               error = excluded.error, updated_at = excluded.updated_at",
            params![
                c.session,
                c.key,
                c.rank,
                c.score,
                c.marker_score,
                serde_json::to_string(&c.reasons).unwrap_or_default(),
                serde_json::to_string(&c.labels).unwrap_or_default(),
                c.title,
                c.start_ns,
                c.peak_ns,
                c.end_ns,
                c.recording,
                c.rec_start_ns,
                c.in_s,
                c.out_s,
                c.peak_s,
                c.wide_path,
                c.tall_path,
                c.wide_thumb,
                c.tall_thumb,
                c.captions,
                c.words,
                c.transcript_from,
                c.transcript_to,
                c.audio_note,
                c.music_dropped as i64,
                c.encoder,
                c.status,
                c.error,
                t,
                if c.kind.is_empty() { "talk" } else { &c.kind },
                c.song,
                c.requester,
                c.dmca_risk as i64,
                if c.context.is_empty() { "{}" } else { &c.context },
            ],
        )?;
        conn.query_row("SELECT id FROM clips WHERE session = ?1 AND key = ?2", params![c.session, c.key], |r| r.get(0))
    })
}

pub fn get(db: &Db, id: i64) -> Result<Option<ClipRow>> {
    db.with(|c| c.query_row(&format!("SELECT {COLS} FROM clips WHERE id = ?1"), [id], row).optional())
}

/// Clips ranked: by session (newest first), then rank. Filters are optional.
pub fn list(db: &Db, session: Option<&str>, status: Option<&str>, limit: usize) -> Result<Vec<ClipRow>> {
    db.with(|c| {
        let mut st = c.prepare(&format!(
            "SELECT {COLS} FROM clips WHERE (?1 IS NULL OR session = ?1) AND (?2 IS NULL OR status = ?2)
             ORDER BY session DESC, CASE status WHEN 'failed' THEN 1 ELSE 0 END, rank ASC, score DESC LIMIT ?3"
        ))?;
        let rows = st.query_map(params![session, status, limit as i64], row)?;
        rows.collect()
    })
}

pub fn set_status(db: &Db, id: i64, status: &str, url: Option<&str>) -> Result<bool> {
    let before = get(db, id)?;
    let n = db.with(|c| {
        c.execute("UPDATE clips SET status = ?2, upload_url = COALESCE(?3, upload_url), updated_at = ?4 WHERE id = ?1", params![id, status, url, now()])
    })?;
    if let Some(before) = before
        && before.status != status
        && matches!(status, "approved" | "rejected")
    {
        record_feedback(
            db,
            id,
            if status == "approved" { "keep" } else { "skip" },
            serde_json::json!({"previous_status": before.status, "previous_in": before.in_s, "previous_out": before.out_s}),
        )?;
    }
    Ok(n > 0)
}

/// After a retrim/re-cut.
pub fn update_cut(db: &Db, c: &ClipRow) -> Result<()> {
    let previous = get(db, c.id)?;
    db.with(|conn| {
        conn.execute(
            "UPDATE clips SET in_s = ?2, out_s = ?3, wide_path = ?4, tall_path = ?5, wide_thumb = ?6, tall_thumb = ?7, captions = ?8,
               words = ?9, transcript_from = ?10, transcript_to = ?11, encoder = ?12, version = version + 1, status = ?13, error = ?14,
               updated_at = ?15, kind = ?16, song = ?17, requester = ?18, dmca_risk = ?19, context = ?20,
               audio_note = ?21, music_dropped = ?22 WHERE id = ?1",
            params![
                c.id,
                c.in_s,
                c.out_s,
                c.wide_path,
                c.tall_path,
                c.wide_thumb,
                c.tall_thumb,
                c.captions,
                c.words,
                c.transcript_from,
                c.transcript_to,
                c.encoder,
                c.status,
                c.error,
                now(),
                c.kind,
                c.song,
                c.requester,
                c.dmca_risk as i64,
                c.context,
                c.audio_note,
                c.music_dropped as i64,
            ],
        )
    })?;
    if let Some(before) = previous {
        record_feedback(
            db,
            c.id,
            "retrim",
            serde_json::json!({
                "previous_in": before.in_s, "previous_out": before.out_s, "in": c.in_s, "out": c.out_s
            }),
        )?;
    }
    Ok(())
}

/// Immutable review history; `details` records the previous decision/window where applicable.
/// Called automatically for keep/skip/retrim; manual cuts call it after insertion.
pub fn record_feedback(db: &Db, id: i64, action: &str, details: serde_json::Value) -> Result<()> {
    let Some(clip) = get(db, id)? else { anyhow::bail!("no clip {id}") };
    let context = serde_json::json!({"clip": clip.to_value(), "details": details});
    db.with(|c| {
        c.execute(
            "INSERT INTO clip_feedback (clip_id, session, action, at, context) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, clip.session, action, now(), context.to_string()],
        )
    })?;
    let session_dir: Option<String> = db.with(|c| c.query_row("SELECT dir FROM sessions WHERE id = ?1", [&clip.session], |r| r.get(0)).optional())?;
    if let Some(dir) = session_dir
        && let Err(e) = export_feedback(db, &clip.session, Path::new(&dir))
    {
        tracing::warn!("could not export clip feedback for {}: {e:#}", clip.session);
    }
    Ok(())
}

/// Full ordered history for the review UI.
pub fn feedback(db: &Db, session: &str) -> Result<Vec<Value>> {
    db.with(|c| {
        let mut st = c.prepare("SELECT id, clip_id, action, at, context FROM clip_feedback WHERE session = ?1 ORDER BY id")?;
        st.query_map([session], |r| {
            let context: String = r.get(4)?;
            Ok(Value::map()
                .with("id", r.get::<_, i64>(0)?)
                .with("clip_id", r.get::<_, i64>(1)?)
                .with("action", r.get::<_, String>(2)?)
                .with("at", r.get::<_, i64>(3)?)
                .with("context", serde_json::from_str::<Value>(&context).unwrap_or_default()))
        })?
        .collect()
    })
}

/// Export the history in show time order, including reviews made after indexing closed.
pub fn export_feedback(db: &Db, session: &str, session_dir: &Path) -> Result<()> {
    let events = feedback(db, session)?;
    let path = show::paths_for(session_dir).feedback();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let temp = path.with_extension("jsonl.tmp");
    let mut body = String::new();
    for item in events {
        body.push_str(&serde_json::to_string(&item)?);
        body.push('\n');
    }
    std::fs::write(&temp, body)?;
    std::fs::rename(temp, path)?;
    Ok(())
}

/// Re-rank a session after a job (rank 1 = best).
pub fn rerank(db: &Db, session: &str) -> Result<()> {
    db.with(|c| {
        let ids: Vec<i64> = {
            let mut st = c.prepare("SELECT id FROM clips WHERE session = ?1 ORDER BY CASE status WHEN 'failed' THEN 1 ELSE 0 END, score DESC, peak_ns ASC")?;
            st.query_map([session], |r| r.get(0))?.collect::<rusqlite::Result<_>>()?
        };
        for (i, id) in ids.iter().enumerate() {
            c.execute("UPDATE clips SET rank = ?2 WHERE id = ?1", params![id, i as i64 + 1])?;
        }
        Ok(())
    })
}

/// Clips that still need a decision or an upload (all sessions).
pub fn pending_count(db: &Db) -> Result<i64> {
    db.with(|c| c.query_row("SELECT COUNT(*) FROM clips WHERE status = 'ready'", [], |r| r.get(0)))
}

/// Does the session still need its files (for retention's `.keep`)?
pub fn session_needs_files(db: &Db, session: &str) -> Result<bool> {
    db.with(|c| {
        let clips: i64 = c.query_row("SELECT COUNT(*) FROM clips WHERE session = ?1 AND status IN ('ready', 'approved')", [session], |r| r.get(0))?;
        let jobs: i64 = c.query_row("SELECT COUNT(*) FROM clip_jobs WHERE session = ?1 AND state IN ('queued', 'running')", [session], |r| r.get(0))?;
        Ok(clips + jobs > 0)
    })
}

// ---- jobs ---------------------------------------------------------------------------------

pub fn job_queue(db: &Db, session: &str) -> Result<()> {
    db.with(|c| {
        c.execute(
            "INSERT INTO clip_jobs (session, state, queued_at) VALUES (?1, 'queued', ?2)
             ON CONFLICT (session) DO UPDATE SET state = 'queued', stage = '', error = NULL, queued_at = excluded.queued_at, started_at = NULL, finished_at = NULL",
            params![session, now()],
        )
    })?;
    Ok(())
}

pub fn job_update(db: &Db, session: &str, state: &str, stage: &str, error: Option<&str>, clips: i64, timings: &Value) -> Result<()> {
    let t = now();
    db.with(|c| {
        c.execute(
            "UPDATE clip_jobs SET state = ?2, stage = ?3, error = ?4, clips = ?5, timings = ?6,
               started_at = CASE WHEN ?2 = 'running' AND started_at IS NULL THEN ?7 ELSE started_at END,
               finished_at = CASE WHEN ?2 IN ('done', 'failed') THEN ?7 ELSE NULL END
             WHERE session = ?1",
            params![session, state, stage, error, clips, serde_json::to_string(timings).unwrap_or_default(), t],
        )
    })?;
    Ok(())
}

/// Jobs to resume after a restart (queued or interrupted while running), oldest first.
pub fn jobs_unfinished(db: &Db) -> Result<Vec<String>> {
    db.with(|c| {
        let mut st = c.prepare("SELECT session FROM clip_jobs WHERE state IN ('queued', 'running') ORDER BY queued_at")?;
        st.query_map([], |r| r.get(0))?.collect()
    })
}

pub fn job(db: &Db, session: &str) -> Result<Option<Value>> {
    db.with(|c| {
        c.query_row("SELECT state, stage, error, clips, timings, queued_at, started_at, finished_at FROM clip_jobs WHERE session = ?1", [session], |r| {
            let timings: String = r.get(4)?;
            Ok(Value::map()
                .with("session", session)
                .with("state", r.get::<_, String>(0)?)
                .with("stage", r.get::<_, String>(1)?)
                .with("error", r.get::<_, Option<String>>(2)?.map(Value::Str).unwrap_or_default())
                .with("clips", r.get::<_, i64>(3)?)
                .with("timings", serde_json::from_str::<Value>(&timings).unwrap_or_default())
                .with("queued_at", r.get::<_, i64>(5)?)
                .with("started_at", r.get::<_, Option<i64>>(6)?.map(Value::Int).unwrap_or_default())
                .with("finished_at", r.get::<_, Option<i64>>(7)?.map(Value::Int).unwrap_or_default()))
        })
        .optional()
    })
}

pub fn jobs(db: &Db, limit: usize) -> Result<Vec<Value>> {
    let sessions: Vec<String> = db.with(|c| {
        let mut st = c.prepare("SELECT session FROM clip_jobs ORDER BY queued_at DESC LIMIT ?1")?;
        st.query_map([limit as i64], |r| r.get(0))?.collect()
    })?;
    let mut out = Vec::new();
    for s in sessions {
        if let Some(j) = job(db, &s)? {
            out.push(j);
        }
    }
    Ok(out)
}

// ---- archive ------------------------------------------------------------------------------

/// One master file of a show and its name in the archive folder.
#[derive(Clone, Debug, Default, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ArchiveMaster {
    pub src: String,
    pub name: String,
}

/// A show's row in the archive stage machine (`crate::archive`).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ArchiveRow {
    pub session: String,
    /// Show folder name (also the archive folder name).
    pub show: String,
    pub show_dir: String,
    /// `<archive dir>/<show>`, fixed when encoding starts.
    pub dest: String,
    pub stage: String,
    /// `queued | running | waiting | failed | done`.
    pub state: String,
    pub detail: String,
    /// Consecutive failures (backoff and health).
    pub attempts: i64,
    /// Unix s before which the row is not retried.
    pub next_try: i64,
    pub progress: f64,
    pub masters: Vec<ArchiveMaster>,
    /// Hot ISO files still to delete (independently of archiving).
    pub isos: Vec<String>,
    pub isos_left: bool,
    pub bytes_in: i64,
    pub bytes_out: i64,
    pub video_kbps: i64,
    /// Unix s the show (session) ended.
    pub ended_at: i64,
    pub queued_at: i64,
    pub updated_at: i64,
    pub finished_at: Option<i64>,
}

const ARCHIVE_COLS: &str = "session, show, show_dir, dest, stage, state, detail, attempts, next_try, progress, masters, isos, isos_left, \
    bytes_in, bytes_out, video_kbps, ended_at, queued_at, updated_at, finished_at";

fn archive_row(r: &rusqlite::Row) -> rusqlite::Result<ArchiveRow> {
    Ok(ArchiveRow {
        session: r.get(0)?,
        show: r.get(1)?,
        show_dir: r.get(2)?,
        dest: r.get(3)?,
        stage: r.get(4)?,
        state: r.get(5)?,
        detail: r.get(6)?,
        attempts: r.get(7)?,
        next_try: r.get(8)?,
        progress: r.get(9)?,
        masters: serde_json::from_str(&r.get::<_, String>(10)?).unwrap_or_default(),
        isos: json_list(r.get(11)?),
        isos_left: r.get::<_, i64>(12)? != 0,
        bytes_in: r.get(13)?,
        bytes_out: r.get(14)?,
        video_kbps: r.get(15)?,
        ended_at: r.get(16)?,
        queued_at: r.get(17)?,
        updated_at: r.get(18)?,
        finished_at: r.get(19)?,
    })
}

impl ArchiveRow {
    pub fn to_value(&self) -> Value {
        Value::map()
            .with("session", self.session.clone())
            .with("show", self.show.clone())
            .with("show_dir", self.show_dir.clone())
            .with("dest", self.dest.clone())
            .with("stage", self.stage.clone())
            .with("state", self.state.clone())
            .with("detail", self.detail.clone())
            .with("attempts", self.attempts)
            .with("next_try", self.next_try)
            .with("progress", (self.progress * 1000.0).round() / 1000.0)
            .with("masters", self.masters.iter().map(|m| m.src.clone()).collect::<Vec<_>>())
            .with("isos", self.isos.clone())
            .with("isos_left", self.isos_left)
            .with("bytes_in", self.bytes_in)
            .with("bytes_out", self.bytes_out)
            .with("video_kbps", self.video_kbps)
            .with("ended_at", self.ended_at)
            .with("queued_at", self.queued_at)
            .with("finished_at", self.finished_at.map(Value::Int).unwrap_or_default())
    }
}

/// Queue a show; an existing row is left alone. Returns whether it was added.
pub fn archive_insert(db: &Db, row: &ArchiveRow) -> Result<bool> {
    let t = now();
    let n = db.with(|c| {
        c.execute(
            "INSERT OR IGNORE INTO archive_jobs (session, show, show_dir, stage, state, masters, isos, isos_left, ended_at, queued_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)",
            params![
                row.session,
                row.show,
                row.show_dir,
                row.stage,
                row.state,
                serde_json::to_string(&row.masters).unwrap_or_default(),
                serde_json::to_string(&row.isos).unwrap_or_default(),
                row.isos_left as i64,
                row.ended_at,
                t
            ],
        )
    })?;
    Ok(n > 0)
}

pub fn archive_get(db: &Db, session: &str) -> Result<Option<ArchiveRow>> {
    db.with(|c| c.query_row(&format!("SELECT {ARCHIVE_COLS} FROM archive_jobs WHERE session = ?1"), [session], archive_row).optional())
}

/// All rows, oldest show first.
pub fn archive_list(db: &Db) -> Result<Vec<ArchiveRow>> {
    db.with(|c| {
        let mut st = c.prepare(&format!("SELECT {ARCHIVE_COLS} FROM archive_jobs ORDER BY ended_at, queued_at"))?;
        st.query_map([], archive_row)?.collect()
    })
}

/// Store the worker's view of a row (everything but its identity and queue time).
pub fn archive_save(db: &Db, row: &ArchiveRow) -> Result<()> {
    db.with(|c| {
        c.execute(
            "UPDATE archive_jobs SET dest = ?2, stage = ?3, state = ?4, detail = ?5, attempts = ?6, next_try = ?7, progress = ?8,
               masters = ?9, isos = ?10, isos_left = ?11, bytes_in = ?12, bytes_out = ?13, video_kbps = ?14, updated_at = ?15,
               finished_at = ?16
             WHERE session = ?1",
            params![
                row.session,
                row.dest,
                row.stage,
                row.state,
                row.detail,
                row.attempts,
                row.next_try,
                row.progress,
                serde_json::to_string(&row.masters).unwrap_or_default(),
                serde_json::to_string(&row.isos).unwrap_or_default(),
                row.isos_left as i64,
                row.bytes_in,
                row.bytes_out,
                row.video_kbps,
                now(),
                row.finished_at
            ],
        )
    })?;
    Ok(())
}

/// Commit an archived show in one transaction: clip rows follow their recording
/// (`recordings` old → new) and media (`clips_dir` old prefix → new prefix), and the row moves to
/// its next stage — so a crash leaves either the old paths and stage, or both new.
pub fn archive_commit(db: &Db, row: &ArchiveRow, recordings: &[(String, String)], clips_dir: Option<(&str, &str)>) -> Result<()> {
    db.with(|c| {
        let tx = c.unchecked_transaction()?;
        for (old, new) in recordings {
            tx.execute("UPDATE clips SET recording = ?3 WHERE session = ?1 AND recording = ?2", params![row.session, old, new])?;
        }
        if let Some((old, new)) = clips_dir {
            let old = format!("{}/", old.trim_end_matches('/'));
            let new = format!("{}/", new.trim_end_matches('/'));
            for col in ["wide_path", "tall_path", "wide_thumb", "tall_thumb"] {
                tx.execute(
                    &format!("UPDATE clips SET {col} = ?3 || substr({col}, length(?2) + 1) WHERE session = ?1 AND substr({col}, 1, length(?2)) = ?2"),
                    params![row.session, old, new],
                )?;
            }
        }
        tx.execute(
            "UPDATE archive_jobs SET stage = ?2, state = ?3, detail = ?4, attempts = 0, next_try = 0, progress = ?5, bytes_out = ?6, updated_at = ?7 WHERE session = ?1",
            params![row.session, row.stage, row.state, row.detail, row.progress, row.bytes_out, now()],
        )?;
        tx.commit()
    })?;
    Ok(())
}

/// Clips of a show are settled once its clip job finished (or none was ever queued) and no clip
/// still waits for review — the full-quality hot files are no longer needed for clipping.
pub fn clips_settled(db: &Db, session: &str) -> Result<bool> {
    db.with(|c| {
        let ready: i64 = c.query_row("SELECT COUNT(*) FROM clips WHERE session = ?1 AND status = 'ready'", [session], |r| r.get(0))?;
        let job: Option<String> = c.query_row("SELECT state FROM clip_jobs WHERE session = ?1", [session], |r| r.get(0)).optional()?;
        Ok(ready == 0 && job.is_none_or(|s| s == "done"))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn clip(session: &str, key: &str, score: f64) -> ClipRow {
        ClipRow {
            session: session.into(),
            key: key.into(),
            score,
            marker_score: score,
            reasons: vec!["chat".into()],
            recording: "/r.mkv".into(),
            in_s: 1.0,
            out_s: 21.0,
            peak_s: 11.0,
            status: "ready".into(),
            words: "[]".into(),
            ..Default::default()
        }
    }

    #[test]
    fn upsert_keeps_review_decisions_and_ranks_by_score() {
        let db = Db::memory().unwrap();
        migrate(&db).unwrap();
        let a = upsert(&db, &clip("s1", "p1", 1.0)).unwrap();
        let b = upsert(&db, &clip("s1", "p2", 3.0)).unwrap();
        upsert(&db, &ClipRow { status: "failed".into(), ..clip("s1", "p3", 9.0) }).unwrap();
        rerank(&db, "s1").unwrap();
        let l = list(&db, Some("s1"), None, 10).unwrap();
        assert_eq!(l.iter().map(|c| (c.key.as_str(), c.rank)).collect::<Vec<_>>(), vec![("p2", 1), ("p1", 2), ("p3", 3)]);
        assert!(set_status(&db, a, "approved", None).unwrap());
        // re-running the job refreshes the clip but keeps the decision
        let id = upsert(&db, &ClipRow { in_s: 2.0, ..clip("s1", "p1", 1.5) }).unwrap();
        assert_eq!(id, a);
        let c = get(&db, a).unwrap().unwrap();
        assert_eq!((c.status.as_str(), c.in_s, c.version), ("approved", 2.0, 2));
        assert_eq!(pending_count(&db).unwrap(), 1);
        assert!(session_needs_files(&db, "s1").unwrap());
        set_status(&db, a, "uploaded", Some("https://x/1")).unwrap();
        set_status(&db, b, "rejected", None).unwrap();
        assert!(!session_needs_files(&db, "s1").unwrap());
        assert_eq!(get(&db, a).unwrap().unwrap().upload_url.as_deref(), Some("https://x/1"));
        assert!(!set_status(&db, 999, "approved", None).unwrap());
        assert_eq!(list(&db, None, Some("rejected"), 10).unwrap().len(), 1);
    }

    #[test]
    fn review_history_keeps_actions_and_song_context_after_retrim() {
        let db = Db::memory().unwrap();
        migrate(&db).unwrap();
        let root = tempfile::tempdir().unwrap();
        let session_dir = root.path().join("session");
        let show_dir = root.path().join("show");
        std::fs::create_dir_all(&session_dir).unwrap();
        std::fs::write(session_dir.join("meta.toml"), format!("show = {{ dir = {:?}, name = \"show\" }}\n", show_dir.to_string_lossy())).unwrap();
        db.with(|c| c.execute("INSERT INTO sessions (id, started_at, dir) VALUES (?1, 0, ?2)", params!["s1", session_dir.to_string_lossy()])).unwrap();
        let song = ClipRow {
            kind: "song".into(),
            song: Some("Drum Solo".into()),
            requester: Some("dj".into()),
            dmca_risk: true,
            context: r#"{"video":"clip-1","channel":"artist"}"#.into(),
            ..clip("s1", "song-1", 2.0)
        };
        let id = upsert(&db, &song).unwrap();
        assert!(set_status(&db, id, "approved", None).unwrap());
        let mut cut = get(&db, id).unwrap().unwrap();
        cut.in_s = 3.0;
        cut.out_s = 23.0;
        update_cut(&db, &cut).unwrap();
        set_status(&db, id, "rejected", None).unwrap();
        let events = feedback(&db, "s1").unwrap();
        assert_eq!(events.iter().map(|v| v.get_path("action").and_then(Value::as_str)).collect::<Vec<_>>(), vec![Some("keep"), Some("retrim"), Some("skip")]);
        assert_eq!(events[1].get_path("context.details.previous_in").and_then(Value::as_f64), Some(1.0));
        assert_eq!(events[2].get_path("context.clip.song").and_then(Value::as_str), Some("Drum Solo"));
        assert_eq!(events[2].get_path("context.clip.context.video").and_then(Value::as_str), Some("clip-1"));
        assert_eq!(std::fs::read_to_string(show_dir.join("data/feedback.jsonl")).unwrap().lines().count(), 3);
    }

    #[test]
    fn jobs_resume_after_restart() {
        let db = Db::memory().unwrap();
        migrate(&db).unwrap();
        job_queue(&db, "s1").unwrap();
        job_queue(&db, "s2").unwrap();
        job_update(&db, "s1", "running", "transcribe", None, 0, &Value::Null).unwrap();
        job_update(&db, "s2", "done", "done", None, 4, &Value::map().with("total_s", 12.5)).unwrap();
        assert_eq!(jobs_unfinished(&db).unwrap(), vec!["s1".to_string()]);
        let j = job(&db, "s2").unwrap().unwrap();
        assert_eq!(j.get_path("timings.total_s").and_then(Value::as_f64), Some(12.5));
        assert!(j.get_path("finished_at").and_then(Value::as_i64).is_some());
        assert_eq!(jobs(&db, 10).unwrap().len(), 2);
    }
}
