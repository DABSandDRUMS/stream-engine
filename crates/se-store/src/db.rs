//! Runtime database (SQLite, WAL): live runtime state for crash recovery, audit log,
//! sessions index, and a namespaced key/value store. Subsystems add their own tables via
//! [`Db::migrate`].

use anyhow::Result;
use parking_lot::Mutex;
use rusqlite::{Connection, OptionalExtension, params};
use se_proto::Value;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS migrations (name TEXT PRIMARY KEY, applied_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS runtime (id INTEGER PRIMARY KEY CHECK (id = 1), state TEXT NOT NULL, saved_at INTEGER NOT NULL, session TEXT);
CREATE TABLE IF NOT EXISTS kv (ns TEXT NOT NULL, key TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY (ns, key));
CREATE TABLE IF NOT EXISTS audit (
  id INTEGER PRIMARY KEY AUTOINCREMENT, ts INTEGER NOT NULL, origin TEXT NOT NULL, actor TEXT,
  op TEXT NOT NULL, ok INTEGER NOT NULL, error TEXT
);
CREATE INDEX IF NOT EXISTS audit_ts ON audit(ts);
CREATE TABLE IF NOT EXISTS sessions (id TEXT PRIMARY KEY, started_at INTEGER NOT NULL, ended_at INTEGER, dir TEXT NOT NULL, meta TEXT);
"#;

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db { conn: Arc::new(Mutex::new(conn)) })
    }

    pub fn memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db { conn: Arc::new(Mutex::new(conn)) })
    }

    /// Apply a named migration once.
    pub fn migrate(&self, name: &str, sql: &str) -> Result<()> {
        let c = self.conn.lock();
        let done: Option<String> = c.query_row("SELECT name FROM migrations WHERE name = ?1", [name], |r| r.get(0)).optional()?;
        if done.is_none() {
            c.execute_batch(sql)?;
            c.execute("INSERT INTO migrations (name, applied_at) VALUES (?1, ?2)", params![name, unix_now()])?;
        }
        Ok(())
    }

    /// Run arbitrary work with the connection (subsystems' own tables).
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> Result<T> {
        Ok(f(&self.conn.lock())?)
    }

    pub fn save_runtime(&self, state_json: &str, session: Option<&str>) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO runtime (id, state, saved_at, session) VALUES (1, ?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET state = excluded.state, saved_at = excluded.saved_at, session = excluded.session",
            params![state_json, unix_now(), session],
        )?;
        Ok(())
    }

    /// `(state json, session id)`.
    pub fn load_runtime(&self) -> Result<Option<(String, Option<String>)>> {
        Ok(self.conn.lock().query_row("SELECT state, session FROM runtime WHERE id = 1", [], |r| Ok((r.get(0)?, r.get(1)?))).optional()?)
    }

    pub fn kv_set(&self, ns: &str, key: &str, v: &Value) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO kv (ns, key, value) VALUES (?1, ?2, ?3) ON CONFLICT(ns, key) DO UPDATE SET value = excluded.value",
            params![ns, key, serde_json::to_string(v)?],
        )?;
        Ok(())
    }

    pub fn kv_get(&self, ns: &str, key: &str) -> Result<Option<Value>> {
        let s: Option<String> = self.conn.lock().query_row("SELECT value FROM kv WHERE ns = ?1 AND key = ?2", params![ns, key], |r| r.get(0)).optional()?;
        Ok(s.map(|s| serde_json::from_str(&s)).transpose()?)
    }

    pub fn kv_del(&self, ns: &str, key: &str) -> Result<()> {
        self.conn.lock().execute("DELETE FROM kv WHERE ns = ?1 AND key = ?2", params![ns, key])?;
        Ok(())
    }

    pub fn kv_list(&self, ns: &str) -> Result<Vec<(String, Value)>> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT key, value FROM kv WHERE ns = ?1 ORDER BY key")?;
        let rows = st.query_map([ns], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        let mut out = Vec::new();
        for r in rows {
            let (k, v) = r?;
            out.push((k, serde_json::from_str(&v)?));
        }
        Ok(out)
    }

    pub fn audit(&self, origin: &str, actor: Option<&str>, op: &str, ok: bool, error: Option<&str>) -> Result<()> {
        self.conn.lock().execute(
            "INSERT INTO audit (ts, origin, actor, op, ok, error) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![unix_now(), origin, actor, op, ok as i64, error],
        )?;
        Ok(())
    }

    pub fn audit_recent(&self, n: usize) -> Result<Vec<Value>> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT ts, origin, actor, op, ok, error FROM audit ORDER BY id DESC LIMIT ?1")?;
        let rows = st.query_map([n as i64], |r| {
            Ok(Value::map()
                .with("ts", r.get::<_, i64>(0)?)
                .with("origin", r.get::<_, String>(1)?)
                .with("actor", r.get::<_, Option<String>>(2)?.map(Value::Str).unwrap_or_default())
                .with("op", r.get::<_, String>(3)?)
                .with("ok", r.get::<_, i64>(4)? != 0)
                .with("error", r.get::<_, Option<String>>(5)?.map(Value::Str).unwrap_or_default()))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn session_open(&self, id: &str, dir: &str) -> Result<()> {
        self.conn.lock().execute("INSERT OR IGNORE INTO sessions (id, started_at, dir) VALUES (?1, ?2, ?3)", params![id, unix_now(), dir])?;
        Ok(())
    }

    pub fn session_close(&self, id: &str) -> Result<()> {
        self.conn.lock().execute("UPDATE sessions SET ended_at = ?2 WHERE id = ?1", params![id, unix_now()])?;
        Ok(())
    }

    /// The most recent session that was never closed (crash recovery continues it).
    pub fn open_session(&self) -> Result<Option<(String, String)>> {
        Ok(self
            .conn
            .lock()
            .query_row("SELECT id, dir FROM sessions WHERE ended_at IS NULL ORDER BY started_at DESC LIMIT 1", [], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    pub fn sessions(&self, n: usize) -> Result<Vec<Value>> {
        let c = self.conn.lock();
        let mut st = c.prepare("SELECT id, started_at, ended_at, dir FROM sessions ORDER BY started_at DESC LIMIT ?1")?;
        let rows = st.query_map([n as i64], |r| {
            Ok(Value::map()
                .with("id", r.get::<_, String>(0)?)
                .with("started_at", r.get::<_, i64>(1)?)
                .with("ended_at", r.get::<_, Option<i64>>(2)?.map(Value::Int).unwrap_or_default())
                .with("dir", r.get::<_, String>(3)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// Online backup to a file (daily backups, §17.4).
    pub fn backup_to(&self, dst: &Path) -> Result<()> {
        if let Some(d) = dst.parent() {
            std::fs::create_dir_all(d)?;
        }
        let c = self.conn.lock();
        c.execute("VACUUM INTO ?1", [dst.to_string_lossy()])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn runtime_kv_audit_sessions() {
        let d = tempfile::tempdir().unwrap();
        let db = Db::open(&d.path().join("rt.db")).unwrap();
        db.save_runtime("{\"mode\":\"live\"}", Some("s1")).unwrap();
        db.save_runtime("{\"mode\":\"brb\"}", Some("s1")).unwrap();
        assert_eq!(db.load_runtime().unwrap().unwrap().0, "{\"mode\":\"brb\"}");
        db.kv_set("bot.counters", "deaths", &Value::Int(3)).unwrap();
        assert_eq!(db.kv_get("bot.counters", "deaths").unwrap(), Some(Value::Int(3)));
        db.audit("cli", None, "preset.fire hype", true, None).unwrap();
        assert_eq!(db.audit_recent(5).unwrap().len(), 1);
        db.session_open("s1", "/x").unwrap();
        assert_eq!(db.open_session().unwrap().unwrap().0, "s1");
        db.session_close("s1").unwrap();
        assert!(db.open_session().unwrap().is_none());
        db.migrate("t1", "CREATE TABLE t (x INTEGER);").unwrap();
        db.migrate("t1", "CREATE TABLE t (x INTEGER);").unwrap();
        db.backup_to(&d.path().join("bk/rt.db")).unwrap();
        assert!(d.path().join("bk/rt.db").exists());
    }
}
