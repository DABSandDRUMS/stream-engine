//! SQLite tables of the song subsystem (runtime DB, §3.3): the video library/cache, the
//! text-query cache, queue rows (current queue + history), relay dedupe, and the song
//! metadata cache (MusicBrainz results by video id).

use crate::metadata::{self, SongInfo};
use crate::queue::{Entry, Paid, Status};
use crate::text;
use crate::youtube::Video;
use anyhow::Result;
use rusqlite::{OptionalExtension, Row, params};
use se_proto::{Role, Value};
use se_store::Db;

pub const KV_NS: &str = "songs";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS yt_videos (
  id TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  channel TEXT NOT NULL,
  duration_s INTEGER NOT NULL,
  info TEXT NOT NULL,
  fetched_at INTEGER NOT NULL,
  first_seen_at INTEGER NOT NULL,
  unplayable TEXT,
  play_count INTEGER NOT NULL DEFAULT 0,
  last_played_at INTEGER
);
CREATE TABLE IF NOT EXISTS yt_queries (
  key TEXT PRIMARY KEY,
  ids TEXT NOT NULL,
  created_at INTEGER NOT NULL,
  hits INTEGER NOT NULL DEFAULT 0
);
CREATE TABLE IF NOT EXISTS song_queue (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  video_id TEXT NOT NULL,
  title TEXT NOT NULL,
  channel TEXT NOT NULL,
  duration_s INTEGER NOT NULL,
  user TEXT NOT NULL,
  user_id TEXT,
  role TEXT NOT NULL,
  status TEXT NOT NULL,
  ord INTEGER NOT NULL DEFAULT 0,
  paid TEXT,
  requested_at INTEGER NOT NULL,
  started_at INTEGER,
  ended_at INTEGER,
  note TEXT
);
CREATE INDEX IF NOT EXISTS song_queue_status ON song_queue(status, ord);
CREATE INDEX IF NOT EXISTS song_queue_video ON song_queue(video_id, started_at);
CREATE INDEX IF NOT EXISTS song_queue_user ON song_queue(user, requested_at);
CREATE TABLE IF NOT EXISTS relay_seen (
  message_id TEXT PRIMARY KEY,
  received_at INTEGER NOT NULL
);
"#;

/// Song metadata by YouTube video id. `info` is the JSON [`SongInfo`] of a confident match,
/// NULL for "no confident match"; either way nothing is looked up again before `retry_at`.
const META_SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS song_meta (
  video_id TEXT PRIMARY KEY,
  info TEXT,
  fetched_at INTEGER NOT NULL,
  retry_at INTEGER NOT NULL,
  version INTEGER NOT NULL
);
"#;

/// A `song_meta` row.
#[derive(Clone, Debug, PartialEq)]
pub struct MetaRow {
    /// `None`: no confident match.
    pub info: Option<SongInfo>,
    /// Unix seconds.
    pub fetched_at: i64,
    pub retry_at: i64,
    pub version: i64,
}

impl MetaRow {
    /// Should this video be looked up (again)?
    pub fn due(&self, now_s: i64) -> bool {
        self.version != metadata::VERSION || now_s >= self.retry_at
    }
}

/// A library row: cached metadata plus what the player learned about it.
#[derive(Clone, Debug, PartialEq)]
pub struct Cached {
    pub video: Video,
    /// Unix seconds.
    pub fetched_at: i64,
    /// Player error that makes it unplayable here (`embedding disabled`, `removed`).
    pub unplayable: Option<String>,
    pub play_count: i64,
    pub last_played_at: Option<i64>,
}

#[derive(Clone)]
pub struct Store {
    db: Db,
}

fn now_s() -> i64 {
    text::now_ms() / 1000
}

fn role_parse(s: &str) -> Role {
    Role::parse(s).unwrap_or(Role::Everyone)
}

fn row_entry(r: &Row) -> rusqlite::Result<Entry> {
    let paid: Option<String> = r.get("paid")?;
    Ok(Entry {
        id: r.get("id")?,
        video: r.get("video_id")?,
        title: r.get("title")?,
        channel: r.get("channel")?,
        duration_s: r.get("duration_s")?,
        user: r.get("user")?,
        user_id: r.get("user_id")?,
        role: role_parse(&r.get::<_, String>("role")?),
        status: Status::parse(&r.get::<_, String>("status")?).unwrap_or(Status::Removed),
        paid: paid.and_then(|p| serde_json::from_str::<Paid>(&p).ok()),
        requested_at: r.get("requested_at")?,
        started_at: r.get("started_at")?,
        ended_at: r.get("ended_at")?,
        note: r.get("note")?,
    })
}

fn row_cached(r: &Row) -> rusqlite::Result<Option<Cached>> {
    let info: String = r.get("info")?;
    let Ok(video) = serde_json::from_str::<Video>(&info) else { return Ok(None) };
    Ok(Some(Cached {
        video,
        fetched_at: r.get("fetched_at")?,
        unplayable: r.get("unplayable")?,
        play_count: r.get("play_count")?,
        last_played_at: r.get("last_played_at")?,
    }))
}

impl Store {
    pub fn open(db: Db) -> Result<Store> {
        db.migrate("songs.v1", SCHEMA)?;
        db.migrate("songs.meta.v1", META_SCHEMA)?;
        Ok(Store { db })
    }

    pub fn db(&self) -> &Db {
        &self.db
    }

    // ---- library ------------------------------------------------------------------------

    pub fn video(&self, id: &str) -> Option<Cached> {
        self.db.with(|c| c.query_row("SELECT * FROM yt_videos WHERE id = ?1", params![id], row_cached).optional()).ok().flatten().flatten()
    }

    /// Store fresh metadata (keeps play counts; clears a stale unplayable mark only if the API
    /// now says the video is embeddable again).
    pub fn put_videos(&self, videos: &[Video]) -> Result<()> {
        let now = now_s();
        self.db.with(|c| {
            let tx = c.unchecked_transaction()?;
            for v in videos {
                let info = serde_json::to_string(v).unwrap_or_default();
                tx.execute(
                    "INSERT INTO yt_videos (id, title, channel, duration_s, info, fetched_at, first_seen_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6)
                     ON CONFLICT(id) DO UPDATE SET title = ?2, channel = ?3, duration_s = ?4, info = ?5, fetched_at = ?6,
                       unplayable = CASE WHEN ?7 THEN unplayable ELSE NULL END",
                    params![v.id, v.title, v.channel, v.duration_s, info, now, !v.embeddable],
                )?;
            }
            tx.commit()
        })?;
        Ok(())
    }

    pub fn mark_unplayable(&self, id: &str, reason: &str) -> Result<()> {
        self.db.with(|c| c.execute("UPDATE yt_videos SET unplayable = ?2 WHERE id = ?1", params![id, reason]))?;
        Ok(())
    }

    /// Clear the unplayable mark of one video (or every video); returns how many changed.
    pub fn clear_unplayable(&self, id: Option<&str>) -> Result<usize> {
        let n = self.db.with(|c| match id {
            Some(id) => c.execute("UPDATE yt_videos SET unplayable = NULL WHERE id = ?1 AND unplayable IS NOT NULL", params![id]),
            None => c.execute("UPDATE yt_videos SET unplayable = NULL WHERE unplayable IS NOT NULL", []),
        })?;
        Ok(n)
    }

    pub fn record_play(&self, id: &str) -> Result<()> {
        self.db.with(|c| c.execute("UPDATE yt_videos SET play_count = play_count + 1, last_played_at = ?2 WHERE id = ?1", params![id, now_s()]))?;
        Ok(())
    }

    /// Cached result ids for a normalized text query and when they were cached (unix s);
    /// counts a hit.
    pub fn query_ids(&self, key: &str) -> Option<(Vec<String>, i64)> {
        let row: Option<(String, i64)> = self
            .db
            .with(|c| {
                let row = c.query_row("SELECT ids, created_at FROM yt_queries WHERE key = ?1", params![key], |r| Ok((r.get(0)?, r.get(1)?))).optional()?;
                if row.is_some() {
                    c.execute("UPDATE yt_queries SET hits = hits + 1 WHERE key = ?1", params![key])?;
                }
                Ok(row)
            })
            .ok()
            .flatten();
        row.and_then(|(s, at)| serde_json::from_str(&s).ok().map(|ids| (ids, at)))
    }

    pub fn put_query(&self, key: &str, ids: &[String]) -> Result<()> {
        let js = serde_json::to_string(ids)?;
        self.db.with(|c| {
            c.execute(
                "INSERT INTO yt_queries (key, ids, created_at) VALUES (?1, ?2, ?3) ON CONFLICT(key) DO UPDATE SET ids = ?2, created_at = ?3",
                params![key, js, now_s()],
            )
        })?;
        Ok(())
    }

    /// Best playable library match for folded text (score ≥ `min_score`), ties broken by plays.
    pub fn search_library(&self, query: &str, min_score: f32) -> Vec<Cached> {
        let all: Vec<Cached> = self
            .db
            .with(|c| {
                let mut st = c.prepare("SELECT * FROM yt_videos WHERE unplayable IS NULL")?;
                let rows = st.query_map([], row_cached)?;
                Ok(rows.filter_map(|r| r.ok().flatten()).collect())
            })
            .unwrap_or_default();
        let mut scored: Vec<(f32, Cached)> = all
            .into_iter()
            .map(|c| {
                let hay = text::fold(&format!("{} {}", c.video.title, c.video.channel));
                (text::match_score(query, &hay), c)
            })
            .filter(|(s, _)| *s >= min_score)
            .collect();
        scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(b.1.play_count.cmp(&a.1.play_count)));
        scored.into_iter().map(|(_, c)| c).collect()
    }

    /// Library listing for the UI: most played first, optionally filtered by folded text.
    pub fn library(&self, filter: Option<&str>, limit: usize) -> Vec<Value> {
        let f = filter.map(text::fold).filter(|f| !f.is_empty());
        let rows: Vec<Cached> = self
            .db
            .with(|c| {
                let mut st = c.prepare("SELECT * FROM yt_videos ORDER BY play_count DESC, last_played_at DESC, first_seen_at DESC")?;
                let rows = st.query_map([], row_cached)?;
                Ok(rows.filter_map(|r| r.ok().flatten()).collect())
            })
            .unwrap_or_default();
        rows.into_iter()
            .filter(|c| f.as_deref().is_none_or(|f| text::match_score(f, &text::fold(&format!("{} {}", c.video.title, c.video.channel))) >= 0.99))
            .take(limit)
            .map(|c| {
                Value::map()
                    .with("video", c.video.id.clone())
                    .with("title", c.video.title.clone())
                    .with("channel", c.video.channel.clone())
                    .with("duration", c.video.duration_s as f64)
                    .with("plays", c.play_count)
                    .with("last_played_at", c.last_played_at.map(Value::from).unwrap_or(Value::Null))
                    .with("unplayable", c.unplayable.clone().map(Value::from).unwrap_or(Value::Null))
                    .with("url", crate::youtube::watch_url(&c.video.id))
            })
            .collect()
    }

    // ---- queue rows ---------------------------------------------------------------------

    /// Insert a new entry; returns its id.
    pub fn insert_entry(&self, e: &Entry) -> Result<i64> {
        let paid = e.paid.as_ref().map(|p| serde_json::to_string(p).unwrap_or_default());
        let id = self.db.with(|c| {
            c.execute(
                "INSERT INTO song_queue (video_id, title, channel, duration_s, user, user_id, role, status, paid, requested_at, note)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
                params![
                    e.video,
                    e.title,
                    e.channel,
                    e.duration_s,
                    e.user,
                    e.user_id,
                    crate::queue::role_str(e.role),
                    e.status.as_str(),
                    paid,
                    e.requested_at,
                    e.note
                ],
            )?;
            Ok(c.last_insert_rowid())
        })?;
        Ok(id)
    }

    pub fn update_entry(&self, e: &Entry) -> Result<()> {
        let paid = e.paid.as_ref().map(|p| serde_json::to_string(p).unwrap_or_default());
        self.db.with(|c| {
            c.execute(
                "UPDATE song_queue SET status = ?2, paid = ?3, started_at = ?4, ended_at = ?5, note = ?6 WHERE id = ?1",
                params![e.id, e.status.as_str(), paid, e.started_at, e.ended_at, e.note],
            )
        })?;
        Ok(())
    }

    /// Persist the play order of `upcoming` (1..n).
    pub fn save_order(&self, ids: &[i64]) -> Result<()> {
        self.db.with(|c| {
            let tx = c.unchecked_transaction()?;
            for (i, id) in ids.iter().enumerate() {
                tx.execute("UPDATE song_queue SET ord = ?2 WHERE id = ?1", params![id, i as i64 + 1])?;
            }
            tx.commit()
        })?;
        Ok(())
    }

    /// Rows still in play: `(playing, queued in order, pending oldest first)`.
    pub fn load_active(&self) -> Result<(Option<Entry>, Vec<Entry>, Vec<Entry>)> {
        let all: Vec<Entry> = self.db.with(|c| {
            let mut st = c.prepare("SELECT * FROM song_queue WHERE status IN ('playing', 'queued', 'pending') ORDER BY ord, requested_at, id")?;
            let rows = st.query_map([], row_entry)?;
            rows.collect()
        })?;
        let mut playing = None;
        let (mut queued, mut pending) = (Vec::new(), Vec::new());
        for e in all {
            match e.status {
                Status::Playing if playing.is_none() => playing = Some(e),
                // a second "playing" row can only come from a crash mid-transition: requeue it first
                Status::Playing | Status::Queued => queued.push(e),
                _ => pending.push(e),
            }
        }
        pending.sort_by_key(|e| e.requested_at);
        Ok((playing, queued, pending))
    }

    /// Finished entries, newest first.
    pub fn history(&self, n: usize) -> Vec<Entry> {
        self.db
            .with(|c| {
                let mut st = c.prepare(
                    "SELECT * FROM song_queue WHERE status IN ('played', 'skipped', 'error') ORDER BY COALESCE(ended_at, started_at, requested_at) DESC LIMIT ?1",
                )?;
                let rows = st.query_map(params![n as i64], row_entry)?;
                rows.collect()
            })
            .unwrap_or_default()
    }

    /// When this video last started playing (unix ms). Entries that failed to play don't count.
    pub fn last_played(&self, video: &str) -> Option<i64> {
        self.db
            .with(|c| {
                c.query_row("SELECT MAX(started_at) FROM song_queue WHERE video_id = ?1 AND status IN ('playing', 'played', 'skipped')", params![video], |r| {
                    r.get(0)
                })
            })
            .ok()
            .flatten()
    }

    /// When this user last had a request accepted (unix ms).
    pub fn last_request(&self, login: &str) -> Option<i64> {
        self.db
            .with(|c| {
                c.query_row("SELECT MAX(requested_at) FROM song_queue WHERE user = ?1 COLLATE NOCASE AND status NOT IN ('rejected')", params![login], |r| {
                    r.get(0)
                })
            })
            .ok()
            .flatten()
    }

    // ---- relay --------------------------------------------------------------------------

    /// Record a Ko-fi `message_id`; false if it was seen before (duplicate delivery).
    pub fn first_delivery(&self, message_id: &str) -> Result<bool> {
        let n = self.db.with(|c| c.execute("INSERT OR IGNORE INTO relay_seen (message_id, received_at) VALUES (?1, ?2)", params![message_id, now_s()]))?;
        Ok(n == 1)
    }

    /// Forget dedupe records older than `days`.
    pub fn prune_seen(&self, days: i64) -> Result<()> {
        self.db.with(|c| c.execute("DELETE FROM relay_seen WHERE received_at < ?1", params![now_s() - days * 86_400]))?;
        Ok(())
    }

    // ---- song metadata ------------------------------------------------------------------

    pub fn song_meta(&self, video: &str) -> Option<MetaRow> {
        self.db
            .with(|c| {
                c.query_row("SELECT info, fetched_at, retry_at, version FROM song_meta WHERE video_id = ?1", params![video], |r| {
                    let info: Option<String> = r.get(0)?;
                    Ok(MetaRow { info: info.and_then(|s| serde_json::from_str(&s).ok()), fetched_at: r.get(1)?, retry_at: r.get(2)?, version: r.get(3)? })
                })
                .optional()
            })
            .ok()
            .flatten()
    }

    pub fn put_song_meta(&self, video: &str, info: Option<&SongInfo>, fetched_at: i64, retry_at: i64) -> Result<()> {
        let js = info.map(serde_json::to_string).transpose()?;
        self.db.with(|c| {
            c.execute(
                "INSERT INTO song_meta (video_id, info, fetched_at, retry_at, version) VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(video_id) DO UPDATE SET info = ?2, fetched_at = ?3, retry_at = ?4, version = ?5",
                params![video, js, fetched_at, retry_at, metadata::VERSION],
            )
        })?;
        Ok(())
    }

    // ---- small settings ----------------------------------------------------------------

    pub fn kv_get(&self, key: &str) -> Option<Value> {
        self.db.kv_get(KV_NS, key).ok().flatten()
    }

    pub fn kv_set(&self, key: &str, v: &Value) -> Result<()> {
        self.db.kv_set(KV_NS, key, v)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::youtube::parse_videos;

    fn videos(name: &str) -> Vec<Video> {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/youtube").join(name);
        parse_videos(&serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()).unwrap()
    }

    #[test]
    fn library_and_query_cache_round_trip() {
        let s = Store::open(Db::memory().unwrap()).unwrap();
        s.put_videos(&videos("videos_search.json")).unwrap();
        s.put_query("bohemian rhapsody", &["fJ9rUzIMcZQ".into(), "NoEmbed0001".into()]).unwrap();
        assert_eq!(s.query_ids("bohemian rhapsody").unwrap().0.len(), 2);
        assert!(s.query_ids("never heard of it").is_none());
        let c = s.video("fJ9rUzIMcZQ").unwrap();
        assert_eq!(c.video.duration_s, 359);
        s.record_play("fJ9rUzIMcZQ").unwrap();
        s.mark_unplayable("yk3prd8GER4", "embedding disabled (150)").unwrap();
        assert_eq!(s.video("yk3prd8GER4").unwrap().unplayable.as_deref(), Some("embedding disabled (150)"));
        // library search skips unplayable rows and ranks by score, then plays
        let hits = s.search_library(&text::fold("bohemian rhapsody"), 0.6);
        assert_eq!(hits.first().map(|c| c.video.id.as_str()), Some("fJ9rUzIMcZQ"));
        assert!(hits.iter().all(|c| c.video.id != "yk3prd8GER4"));
        // refreshed metadata that says embeddable clears the unplayable mark
        let mut v = s.video("yk3prd8GER4").unwrap().video;
        v.embeddable = true;
        s.put_videos(&[v]).unwrap();
        assert!(s.video("yk3prd8GER4").unwrap().unplayable.is_none());
        s.mark_unplayable("fJ9rUzIMcZQ", "embedding disabled (150)").unwrap();
        s.mark_unplayable("NoEmbed0001", "embedding disabled (150)").unwrap();
        assert_eq!(s.clear_unplayable(Some("fJ9rUzIMcZQ")).unwrap(), 1);
        assert_eq!(s.clear_unplayable(None).unwrap(), 1);
        assert!(s.video("NoEmbed0001").unwrap().unplayable.is_none());
        assert_eq!(s.video("fJ9rUzIMcZQ").unwrap().play_count, 1);
        assert_eq!(s.library(Some("queen"), 10).len(), 2);
    }

    #[test]
    fn queue_rows_restore_in_order() {
        let s = Store::open(Db::memory().unwrap()).unwrap();
        let mk = |user: &str, status: Status, at: i64| Entry {
            id: 0,
            video: "dQw4w9WgXcQ".into(),
            title: "t".into(),
            channel: "c".into(),
            duration_s: 10,
            user: user.into(),
            user_id: None,
            role: Role::Sub,
            status,
            paid: Some(Paid { points: 5, redemption_id: Some("r1".into()), ..Default::default() }),
            requested_at: at,
            started_at: None,
            ended_at: None,
            note: None,
        };
        let a = s.insert_entry(&mk("a", Status::Queued, 1)).unwrap();
        let b = s.insert_entry(&mk("b", Status::Queued, 2)).unwrap();
        let p = s.insert_entry(&mk("p", Status::Pending, 3)).unwrap();
        let mut cur = mk("c", Status::Playing, 0);
        cur.id = s.insert_entry(&cur).unwrap();
        cur.started_at = Some(5);
        s.update_entry(&cur).unwrap();
        s.save_order(&[b, a]).unwrap();
        let (playing, queued, pending) = s.load_active().unwrap();
        assert_eq!(playing.unwrap().id, cur.id);
        assert_eq!(queued.iter().map(|e| e.id).collect::<Vec<_>>(), vec![b, a]);
        assert_eq!(pending[0].id, p);
        assert_eq!(pending[0].paid.as_ref().unwrap().redemption_id.as_deref(), Some("r1"));
        assert_eq!(s.last_played("dQw4w9WgXcQ"), Some(5));
        // a failed play (player error) doesn't start the no-repeat window
        let mut failed = mk("f", Status::Queued, 9);
        failed.video = "fJ9rUzIMcZQ".into();
        failed.id = s.insert_entry(&failed).unwrap();
        failed.status = Status::Error;
        failed.started_at = Some(10);
        s.update_entry(&failed).unwrap();
        assert_eq!(s.last_played("fJ9rUzIMcZQ"), None);
        assert_eq!(s.last_request("A"), Some(1));
        assert!(s.first_delivery("m1").unwrap());
        assert!(!s.first_delivery("m1").unwrap());
    }
}
