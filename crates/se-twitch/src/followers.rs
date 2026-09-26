//! Follower cache (§12.1 follower role with minimum follow age): persisted in the runtime DB,
//! bulk-loaded when the channel connects, updated by `channel.follow`, and looked up on demand
//! for chatters we have not seen (negative answers are re-checked after a while).

use crate::auth::Account;
use crate::helix::{Helix, first};
use crate::time::{parse_rfc3339, unix_s};
use se_store::Db;
use serde_json::Value as J;
use std::collections::HashMap;

/// Re-check "not following" answers after this long.
const NEGATIVE_TTL_S: i64 = 600;
/// Bulk preload cap (100 per page).
pub const PRELOAD_MAX: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// Unix seconds; 0 = not following.
    pub followed_at: i64,
    pub checked_at: i64,
}

impl Entry {
    pub fn age_s(&self, now: i64) -> Option<i64> {
        (self.followed_at > 0).then(|| (now - self.followed_at).max(0))
    }
    /// Is this answer still good enough to skip a lookup?
    pub fn fresh(&self, now: i64) -> bool {
        self.followed_at > 0 || now - self.checked_at < NEGATIVE_TTL_S
    }
}

pub fn migrate(db: &Db) -> anyhow::Result<()> {
    db.migrate(
        "twitch_followers_v1",
        "CREATE TABLE IF NOT EXISTS twitch_followers (user_id TEXT PRIMARY KEY, followed_at INTEGER NOT NULL, checked_at INTEGER NOT NULL);",
    )
}

pub fn load(db: &Db) -> HashMap<String, Entry> {
    db.with(|c| {
        let mut st = c.prepare("SELECT user_id, followed_at, checked_at FROM twitch_followers")?;
        let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, Entry { followed_at: r.get(1)?, checked_at: r.get(2)? })))?;
        rows.collect::<Result<HashMap<_, _>, _>>()
    })
    .unwrap_or_default()
}

pub fn store(db: &Db, entries: &[(String, Entry)]) {
    let _ = db.with(|c| {
        let tx = c.unchecked_transaction()?;
        {
            let mut st = tx.prepare(
                "INSERT INTO twitch_followers (user_id, followed_at, checked_at) VALUES (?1, ?2, ?3)
                 ON CONFLICT(user_id) DO UPDATE SET followed_at = excluded.followed_at, checked_at = excluded.checked_at",
            )?;
            for (id, e) in entries {
                st.execute((id, e.followed_at, e.checked_at))?;
            }
        }
        tx.commit()
    });
}

fn followed_at(v: &J) -> i64 {
    v.get("followed_at").and_then(J::as_str).and_then(parse_rfc3339).map(|ms| ms / 1000).unwrap_or(0)
}

/// Is `user_id` following `broadcaster`? (Helix `Get Channel Followers` with `user_id`.)
pub async fn lookup(helix: &Helix, broadcaster: &str, user_id: &str) -> Result<Entry, String> {
    let q = [("broadcaster_id", broadcaster.to_string()), ("user_id", user_id.to_string())];
    let v = helix.get(Account::Broadcaster, "/channels/followers", &q).await.map_err(|e| e.to_string())?;
    Ok(Entry { followed_at: first(&v).map(followed_at).unwrap_or(0), checked_at: unix_s() })
}

/// Every follower (newest first) up to [`PRELOAD_MAX`].
pub async fn preload(helix: &Helix, broadcaster: &str) -> Result<Vec<(String, Entry)>, String> {
    let q = [("broadcaster_id", broadcaster.to_string())];
    let all = helix.get_all(Account::Broadcaster, "/channels/followers", &q, PRELOAD_MAX).await.map_err(|e| e.to_string())?;
    let now = unix_s();
    Ok(all
        .iter()
        .filter_map(|f| Some((f.get("user_id")?.as_str()?.to_string(), Entry { followed_at: followed_at(f), checked_at: now })))
        .filter(|(_, e)| e.followed_at > 0)
        .collect())
}
