//! YouTube API quota ledger (§13.1): units spent per Pacific day, in the runtime DB. Google
//! resets the daily quota at midnight Pacific time, so the ledger keys days the same way.

use crate::youtube::{LIST_COST, SEARCH_COST};
use anyhow::Result;
use jiff::Timestamp;
use jiff::tz::TimeZone;
use rusqlite::{OptionalExtension, params};
use se_store::Db;

pub const PACIFIC: &str = "America/Los_Angeles";

pub const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS yt_quota (
  day TEXT PRIMARY KEY,
  used INTEGER NOT NULL DEFAULT 0,
  searches INTEGER NOT NULL DEFAULT 0,
  lists INTEGER NOT NULL DEFAULT 0,
  exhausted INTEGER NOT NULL DEFAULT 0
);
"#;

fn pacific() -> TimeZone {
    TimeZone::get(PACIFIC).expect("bundled tzdb has America/Los_Angeles")
}

/// The Pacific calendar day (`YYYY-MM-DD`) a timestamp falls in.
pub fn pacific_day(ts: Timestamp) -> String {
    ts.to_zoned(pacific()).date().to_string()
}

/// The next midnight Pacific after `ts` (DST-aware).
pub fn next_reset(ts: Timestamp) -> Timestamp {
    let z = ts.to_zoned(pacific());
    let tomorrow = z.date().tomorrow().expect("date in range");
    tomorrow.to_zoned(pacific()).map(|z| z.timestamp()).unwrap_or_else(|_| ts + jiff::SignedDuration::from_hours(24))
}

/// What lookups are possible right now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// Text search and links.
    Full,
    /// Links only (search would eat the units reserved for link lookups).
    Links,
    /// Nothing left today: cached library only.
    Library,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Full => "full",
            Mode::Links => "links",
            Mode::Library => "library",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Search,
    List,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Day {
    pub used: u32,
    pub searches: u32,
    pub lists: u32,
    pub exhausted: bool,
}

#[derive(Clone)]
pub struct Ledger {
    db: Db,
    /// Daily units (10,000 for a default Google Cloud project).
    pub limit: u32,
    /// Units kept back for link lookups; text search stops when it would dip into them.
    pub link_reserve: u32,
}

impl Ledger {
    pub fn new(db: Db, limit: u32, link_reserve: u32) -> Result<Ledger> {
        db.migrate("songs.quota.v1", SCHEMA)?;
        Ok(Ledger { db, limit, link_reserve })
    }

    pub fn day(&self, now: Timestamp) -> Day {
        let key = pacific_day(now);
        self.db
            .with(|c| {
                c.query_row("SELECT used, searches, lists, exhausted FROM yt_quota WHERE day = ?1", params![key], |r| {
                    Ok(Day { used: r.get(0)?, searches: r.get(1)?, lists: r.get(2)?, exhausted: r.get::<_, i64>(3)? != 0 })
                })
                .optional()
            })
            .ok()
            .flatten()
            .unwrap_or_default()
    }

    pub fn remaining(&self, now: Timestamp) -> u32 {
        let d = self.day(now);
        if d.exhausted { 0 } else { self.limit.saturating_sub(d.used) }
    }

    pub fn can_search(&self, now: Timestamp) -> bool {
        self.remaining(now) >= SEARCH_COST + self.link_reserve
    }

    pub fn can_list(&self, now: Timestamp) -> bool {
        self.remaining(now) >= LIST_COST
    }

    pub fn mode(&self, now: Timestamp) -> Mode {
        let r = self.remaining(now);
        if r >= SEARCH_COST + self.link_reserve {
            Mode::Full
        } else if r >= LIST_COST {
            Mode::Links
        } else {
            Mode::Library
        }
    }

    /// Record units about to be spent (charged before the call: a failed call still costs).
    pub fn charge(&self, now: Timestamp, kind: Kind) -> Result<()> {
        let key = pacific_day(now);
        let (units, s, l) = match kind {
            Kind::Search => (SEARCH_COST, 1, 0),
            Kind::List => (LIST_COST, 0, 1),
        };
        self.db.with(|c| {
            c.execute(
                "INSERT INTO yt_quota (day, used, searches, lists) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(day) DO UPDATE SET used = used + ?2, searches = searches + ?3, lists = lists + ?4",
                params![key, units, s, l],
            )
        })?;
        Ok(())
    }

    /// Google said the quota is gone (other consumers of the same project count too).
    pub fn mark_exhausted(&self, now: Timestamp) -> Result<()> {
        let key = pacific_day(now);
        self.db.with(|c| c.execute("INSERT INTO yt_quota (day, exhausted) VALUES (?1, 1) ON CONFLICT(day) DO UPDATE SET exhausted = 1", params![key]))?;
        Ok(())
    }

    /// Recent days, newest first (for the UI).
    pub fn history(&self, n: usize) -> Vec<(String, Day)> {
        self.db
            .with(|c| {
                let mut st = c.prepare("SELECT day, used, searches, lists, exhausted FROM yt_quota ORDER BY day DESC LIMIT ?1")?;
                let rows = st.query_map(params![n as i64], |r| {
                    Ok((r.get::<_, String>(0)?, Day { used: r.get(1)?, searches: r.get(2)?, lists: r.get(3)?, exhausted: r.get::<_, i64>(4)? != 0 }))
                })?;
                rows.collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ts(s: &str) -> Timestamp {
        s.parse().unwrap()
    }

    #[test]
    fn pacific_days_follow_dst() {
        // PDT (UTC-7): 06:59Z is still the previous Pacific day, 07:00Z is the next.
        assert_eq!(pacific_day(ts("2026-09-25T06:59:59Z")), "2026-09-24");
        assert_eq!(pacific_day(ts("2026-09-25T07:00:00Z")), "2026-09-25");
        // PST (UTC-8) in winter.
        assert_eq!(pacific_day(ts("2026-01-10T07:59:59Z")), "2026-01-09");
        assert_eq!(pacific_day(ts("2026-01-10T08:00:00Z")), "2026-01-10");
        assert_eq!(next_reset(ts("2026-09-25T12:00:00Z")), ts("2026-09-26T07:00:00Z"));
        assert_eq!(next_reset(ts("2026-01-10T12:00:00Z")), ts("2026-01-11T08:00:00Z"));
        // The day DST ends (Nov 1, 2026) is 25 hours long.
        assert_eq!(next_reset(ts("2026-11-01T12:00:00Z")), ts("2026-11-02T08:00:00Z"));
    }

    #[test]
    fn ledger_charges_degrades_and_resets_at_pacific_midnight() {
        let db = Db::memory().unwrap();
        let l = Ledger::new(db, 1000, 200).unwrap();
        let t = ts("2026-09-25T18:00:00Z"); // 11:00 PDT
        assert_eq!(l.mode(t), Mode::Full);
        for _ in 0..7 {
            l.charge(t, Kind::Search).unwrap();
        }
        // 300 left: one more search would leave 200 = the link reserve → still allowed
        assert_eq!(l.remaining(t), 300);
        assert!(l.can_search(t));
        l.charge(t, Kind::Search).unwrap();
        assert_eq!(l.remaining(t), 200);
        assert_eq!(l.mode(t), Mode::Links);
        assert!(!l.can_search(t) && l.can_list(t));
        for _ in 0..200 {
            l.charge(t, Kind::List).unwrap();
        }
        assert_eq!(l.mode(t), Mode::Library);
        let d = l.day(t);
        assert_eq!((d.used, d.searches, d.lists), (1000, 8, 200));
        // 23:59:59 PDT same day: still exhausted; 00:00 PDT: fresh day.
        assert_eq!(l.mode(ts("2026-09-26T06:59:59Z")), Mode::Library);
        assert_eq!(l.mode(ts("2026-09-26T07:00:00Z")), Mode::Full);
        assert_eq!(l.remaining(ts("2026-09-26T07:00:00Z")), 1000);
        assert_eq!(l.history(10).len(), 1);
    }

    #[test]
    fn google_exhaustion_overrides_our_count() {
        let l = Ledger::new(Db::memory().unwrap(), 10_000, 200).unwrap();
        let t = ts("2026-09-25T18:00:00Z");
        l.charge(t, Kind::List).unwrap();
        l.mark_exhausted(t).unwrap();
        assert_eq!(l.remaining(t), 0);
        assert_eq!(l.mode(t), Mode::Library);
        assert_eq!(l.mode(next_reset(t)), Mode::Full);
    }
}
