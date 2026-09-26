//! Counters and quotes in the runtime DB (§3.3, §14.3).

use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use se_core::rng::Rng;
use se_proto::Value;
use se_store::Db;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS bot_counters (name TEXT PRIMARY KEY, value INTEGER NOT NULL, updated_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS bot_quotes (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  text TEXT NOT NULL,
  added_by TEXT NOT NULL DEFAULT '',
  created_at INTEGER NOT NULL
);
"#;

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Quote {
    pub id: i64,
    pub text: String,
    pub added_by: String,
    pub created_at: i64,
}

impl Quote {
    pub fn to_value(&self) -> Value {
        Value::map().with("id", self.id).with("text", self.text.clone()).with("added_by", self.added_by.clone()).with("created_at", self.created_at)
    }

    /// `2026-09-25` (UTC) for replies.
    pub fn date(&self) -> String {
        let days = self.created_at.div_euclid(86_400);
        // civil-from-days (Howard Hinnant)
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        format!("{y:04}-{m:02}-{d:02}")
    }
}

#[derive(Clone)]
pub struct Store {
    db: Db,
}

fn quote_row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Quote> {
    Ok(Quote { id: r.get(0)?, text: r.get(1)?, added_by: r.get(2)?, created_at: r.get(3)? })
}

impl Store {
    pub fn new(db: Db) -> Result<Store> {
        db.migrate("se-bot/1", SCHEMA)?;
        Ok(Store { db })
    }

    pub fn counter_get(&self, name: &str) -> Result<i64> {
        self.db.with(|c| c.query_row("SELECT value FROM bot_counters WHERE name = ?1", [name], |r| r.get(0)).optional().map(|v| v.unwrap_or(0)))
    }

    /// Add `delta` and return the new value.
    pub fn counter_add(&self, name: &str, delta: i64) -> Result<i64> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO bot_counters (name, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET value = value + excluded.value, updated_at = excluded.updated_at",
                params![name, delta, unix_now()],
            )?;
            c.query_row("SELECT value FROM bot_counters WHERE name = ?1", [name], |r| r.get(0))
        })
    }

    pub fn counter_set(&self, name: &str, value: i64) -> Result<()> {
        self.db.with(|c| {
            c.execute(
                "INSERT INTO bot_counters (name, value, updated_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(name) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
                params![name, value, unix_now()],
            )
            .map(|_| ())
        })
    }

    pub fn counter_delete(&self, name: &str) -> Result<bool> {
        self.db.with(|c| c.execute("DELETE FROM bot_counters WHERE name = ?1", [name]).map(|n| n > 0))
    }

    pub fn counters(&self) -> Result<Vec<(String, i64)>> {
        self.db.with(|c| {
            let mut st = c.prepare("SELECT name, value FROM bot_counters ORDER BY name")?;
            st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect()
        })
    }

    pub fn quote_add(&self, text: &str, by: &str) -> Result<i64> {
        self.db.with(|c| {
            c.execute("INSERT INTO bot_quotes (text, added_by, created_at) VALUES (?1, ?2, ?3)", params![text, by, unix_now()])?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn quote_get(&self, id: i64) -> Result<Option<Quote>> {
        self.db.with(|c| c.query_row("SELECT id, text, added_by, created_at FROM bot_quotes WHERE id = ?1", [id], quote_row).optional())
    }

    /// A random quote, optionally containing all `words` (case-insensitive).
    pub fn quote_pick(&self, words: &[String], rng: &mut Rng) -> Result<Option<Quote>> {
        let all = self.quotes()?;
        let needles: Vec<String> = words.iter().map(|w| w.to_lowercase()).collect();
        let hits: Vec<&Quote> = all.iter().filter(|q| needles.iter().all(|n| q.text.to_lowercase().contains(n))).collect();
        if hits.is_empty() {
            return Ok(None);
        }
        Ok(Some(hits[rng.below(hits.len() as u64) as usize].clone()))
    }

    pub fn quote_edit(&self, id: i64, text: &str) -> Result<bool> {
        self.db.with(|c| c.execute("UPDATE bot_quotes SET text = ?2 WHERE id = ?1", params![id, text]).map(|n| n > 0))
    }

    pub fn quote_delete(&self, id: i64) -> Result<bool> {
        self.db.with(|c| c.execute("DELETE FROM bot_quotes WHERE id = ?1", [id]).map(|n| n > 0))
    }

    pub fn quotes(&self) -> Result<Vec<Quote>> {
        self.db.with(|c| {
            let mut st = c.prepare("SELECT id, text, added_by, created_at FROM bot_quotes ORDER BY id")?;
            st.query_map([], quote_row)?.collect()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_add_set_delete() {
        let s = Store::new(Db::memory().unwrap()).unwrap();
        assert_eq!(s.counter_get("deaths").unwrap(), 0);
        assert_eq!(s.counter_add("deaths", 1).unwrap(), 1);
        assert_eq!(s.counter_add("deaths", 1).unwrap(), 2);
        s.counter_set("deaths", 40).unwrap();
        assert_eq!(s.counter_add("deaths", -1).unwrap(), 39);
        assert_eq!(s.counters().unwrap(), vec![("deaths".to_string(), 39)]);
        assert!(s.counter_delete("deaths").unwrap());
        assert!(!s.counter_delete("deaths").unwrap());
    }

    #[test]
    fn quotes_ids_are_stable_after_delete() {
        let s = Store::new(Db::memory().unwrap()).unwrap();
        let a = s.quote_add("the snare is the heart", "mod1").unwrap();
        let b = s.quote_add("more cowbell", "mod2").unwrap();
        assert!(s.quote_delete(a).unwrap());
        let c = s.quote_add("never again", "mod1").unwrap();
        assert!(c > b, "ids are never reused");
        let mut rng = Rng::new(1);
        assert_eq!(s.quote_pick(&["COWBELL".into()], &mut rng).unwrap().unwrap().id, b);
        assert!(s.quote_pick(&["nothing".into()], &mut rng).unwrap().is_none());
        assert!(s.quote_edit(b, "less cowbell").unwrap());
        assert_eq!(s.quote_get(b).unwrap().unwrap().text, "less cowbell");
    }

    #[test]
    fn quote_dates() {
        let q = Quote { id: 1, text: String::new(), added_by: String::new(), created_at: 1_790_294_400 };
        assert_eq!(q.date(), "2026-09-25");
    }
}
