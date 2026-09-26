//! Stats (`stats.*`: latest follower/sub/cheer/tip/raid/gift, top cheerer/tipper/gifter for
//! the session and all time, session totals), goals (`goals.<name>.*`, persisted across
//! streams), top chatters, and the end-of-stream `credits` data.
//!
//! Every counted event is one row in `alerts_log` (tagged with the engine session and
//! whether it came from the simulator), so session and all-time views are plain queries and
//! simulated data can be purged after testing.

use crate::config::{Counts, GoalDef, StatsSettings};
use anyhow::Result;
use rusqlite::{OptionalExtension, params};
use se_proto::{Event, Role, Value, address};
use se_store::Db;
use std::collections::HashMap;

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS alerts_log (
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  session TEXT NOT NULL,
  ts INTEGER NOT NULL,
  kind TEXT NOT NULL,
  user TEXT NOT NULL,
  user_id TEXT NOT NULL DEFAULT '',
  amount REAL NOT NULL DEFAULT 0,
  tier INTEGER,
  months INTEGER,
  currency TEXT,
  message TEXT NOT NULL DEFAULT '',
  gift INTEGER NOT NULL DEFAULT 0,
  sim INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS alerts_log_session ON alerts_log(session, kind);
CREATE INDEX IF NOT EXISTS alerts_log_kind ON alerts_log(kind, user);
CREATE TABLE IF NOT EXISTS alerts_chatters (
  session TEXT NOT NULL, user TEXT NOT NULL, count INTEGER NOT NULL, sim INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (session, user)
);
CREATE TABLE IF NOT EXISTS alerts_goals (name TEXT PRIMARY KEY, current REAL NOT NULL, sim REAL NOT NULL DEFAULT 0, updated_at INTEGER NOT NULL);
"#;

fn unix_now() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// One counted contribution.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub kind: &'static str,
    pub user: String,
    pub user_id: String,
    pub amount: f64,
    pub tier: Option<i64>,
    pub months: Option<i64>,
    pub currency: Option<String>,
    pub message: String,
    pub gift: bool,
}

fn pstr(e: &Event, k: &str) -> Option<String> {
    e.payload.get_path(k).and_then(Value::as_str).map(String::from)
}

fn pnum(e: &Event, k: &str) -> Option<f64> {
    e.payload.get_path(k).and_then(Value::as_f64)
}

/// Display name for an event's viewer (tips respect `is_public`).
pub fn event_user(e: &Event) -> String {
    if e.ty == "tip" && e.payload.get_path("is_public").is_some_and(|v| !v.truthy()) {
        return "Anonymous".into();
    }
    pstr(e, "user").filter(|s| !s.is_empty()).or_else(|| e.actor.as_ref().map(|a| a.name.clone())).unwrap_or_else(|| "Anonymous".into())
}

/// Normalized tier 1–3 (accepts Twitch's `1000/2000/3000` and `prime`).
pub fn tier(e: &Event) -> Option<i64> {
    match e.payload.get_path("tier") {
        Some(Value::Str(s)) if s.eq_ignore_ascii_case("prime") => Some(1),
        Some(v) => v.as_i64().map(|t| if t >= 1000 { t / 1000 } else { t }).map(|t| t.clamp(1, 3)),
        None => None,
    }
}

/// Classify an event into a stats row.
pub fn classify(e: &Event) -> Option<Row> {
    let user = event_user(e);
    let user_id = e.actor.as_ref().map(|a| a.id.clone()).or_else(|| pstr(e, "user_id")).unwrap_or_default();
    let private = e.ty == "tip" && e.payload.get_path("is_public").is_some_and(|v| !v.truthy());
    let message = if private { String::new() } else { pstr(e, "message").unwrap_or_default() };
    let row = |kind, amount| Row {
        kind,
        user: user.clone(),
        user_id: user_id.clone(),
        amount,
        tier: None,
        months: None,
        currency: None,
        message: message.clone(),
        gift: false,
    };
    Some(match e.ty.as_str() {
        "twitch.follow" => row("follow", 1.0),
        "twitch.sub" | "twitch.resub" => Row {
            tier: tier(e),
            months: pnum(e, "months").map(|m| m as i64),
            gift: e.payload.get_path("is_gift").is_some_and(Value::truthy),
            ..row("sub", 1.0)
        },
        "twitch.gift" => Row { tier: tier(e), ..row("gift", pnum(e, "count").unwrap_or(1.0).max(1.0)) },
        "twitch.cheer" => row("cheer", pnum(e, "bits").unwrap_or(0.0)),
        "tip" => Row { currency: pstr(e, "currency"), ..row("tip", pnum(e, "amount").unwrap_or(0.0)) },
        "twitch.raid" => row("raid", pnum(e, "viewers").unwrap_or(0.0)),
        _ => return None,
    })
}

fn sub_points(t: Option<i64>) -> f64 {
    match t.unwrap_or(1) {
        3 => 6.0,
        2 => 2.0,
        _ => 1.0,
    }
}

struct GoalRt {
    def: GoalDef,
    current: f64,
}

/// Changes to publish after an update.
#[derive(Debug, Default)]
pub struct Update {
    pub changes: Vec<(String, Value)>,
    /// Goals that crossed their target: `(name, current, target)`.
    pub reached: Vec<(String, f64, f64)>,
}

pub struct Stats {
    db: Db,
    pub settings: StatsSettings,
    session: String,
    goals: Vec<GoalRt>,
    chatters: HashMap<(String, bool), u64>,
}

/// Every `stats.*` address with its description (declared up front for the UI and overlays).
pub const STAT_ADDRS: &[(&str, &str)] = &[
    ("stats.latest.follow.user", "Latest follower"),
    ("stats.latest.sub.user", "Latest subscriber"),
    ("stats.latest.sub.tier", "Latest subscriber's tier"),
    ("stats.latest.sub.months", "Latest subscriber's months"),
    ("stats.latest.gift.user", "Latest gifter"),
    ("stats.latest.gift.count", "Latest gift count"),
    ("stats.latest.cheer.user", "Latest cheerer"),
    ("stats.latest.cheer.amount", "Latest cheer (bits)"),
    ("stats.latest.tip.user", "Latest tipper"),
    ("stats.latest.tip.amount", "Latest tip"),
    ("stats.latest.tip.currency", "Latest tip currency"),
    ("stats.latest.raid.user", "Latest raider"),
    ("stats.latest.raid.viewers", "Latest raid size"),
    ("stats.top.cheer.session.user", "Top cheerer this stream"),
    ("stats.top.cheer.session.amount", "Top cheerer's bits this stream"),
    ("stats.top.cheer.alltime.user", "Top cheerer of all time"),
    ("stats.top.cheer.alltime.amount", "Top cheerer's bits, all time"),
    ("stats.top.tip.session.user", "Top tipper this stream"),
    ("stats.top.tip.session.amount", "Top tip total this stream"),
    ("stats.top.tip.alltime.user", "Top tipper of all time"),
    ("stats.top.tip.alltime.amount", "Top tip total, all time"),
    ("stats.top.gift.session.user", "Top gifter this stream"),
    ("stats.top.gift.session.amount", "Top gifter's subs this stream"),
    ("stats.top.gift.alltime.user", "Top gifter of all time"),
    ("stats.top.gift.alltime.amount", "Top gifter's subs, all time"),
    ("stats.session.follows", "Follows this stream"),
    ("stats.session.subs", "Subs this stream (incl. gifted)"),
    ("stats.session.gifts", "Gifted subs this stream"),
    ("stats.session.bits", "Bits this stream"),
    ("stats.session.tips", "Tips this stream"),
    ("stats.session.raids", "Raids this stream"),
];

impl Stats {
    pub fn new(db: Db, session: &str, settings: StatsSettings) -> Result<Stats> {
        db.migrate("se-alerts/1", SCHEMA)?;
        Ok(Stats { db, settings, session: session.to_string(), goals: Vec::new(), chatters: HashMap::new() })
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    /// A new stream started (session rotated): session views reset.
    pub fn set_session(&mut self, s: &str) -> Result<Update> {
        self.flush_chatters()?;
        self.session = s.to_string();
        Ok(Update { changes: self.all()?, reached: Vec::new() })
    }

    fn counts_sim(&self, g: &GoalDef) -> bool {
        g.count_simulated.unwrap_or(self.settings.count_simulated)
    }

    /// Count an event. `sim` marks simulator events.
    pub fn record(&mut self, e: &Event, sim: bool) -> Result<Update> {
        let mut up = Update::default();
        let row = classify(e);
        if let Some(r) = &row
            && (!sim || self.settings.count_simulated)
        {
            self.db.with(|c| {
                c.execute(
                    "INSERT INTO alerts_log (session, ts, kind, user, user_id, amount, tier, months, currency, message, gift, sim)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                    params![self.session, unix_now(), r.kind, r.user, r.user_id, r.amount, r.tier, r.months, r.currency, r.message, r.gift as i64, sim as i64],
                )
            })?;
            up.changes = self.all_stats()?;
        }
        for i in 0..self.goals.len() {
            if sim && !self.counts_sim(&self.goals[i].def) {
                continue;
            }
            let delta = goal_delta(&self.goals[i].def.counts, e, row.as_ref());
            if delta != 0.0 {
                let (ch, reached) = self.goal_add_at(i, delta, sim)?;
                up.changes.extend(ch);
                up.reached.extend(reached);
            }
        }
        Ok(up)
    }

    /// Count a chat line for top chatters (flushed to the DB periodically).
    pub fn chat(&mut self, e: &Event, sim: bool) {
        if sim && !self.settings.count_simulated {
            return;
        }
        if e.payload.get_path("bot").is_some_and(Value::truthy) {
            return;
        }
        let Some(a) = &e.actor else { return };
        if self.settings.exclude_broadcaster && a.top_role() >= Role::Owner {
            return;
        }
        if self.settings.ignore_chatters.iter().any(|n| n.eq_ignore_ascii_case(&a.name)) {
            return;
        }
        *self.chatters.entry((a.name.clone(), sim)).or_default() += 1;
    }

    pub fn flush_chatters(&mut self) -> Result<()> {
        if self.chatters.is_empty() {
            return Ok(());
        }
        let batch: Vec<((String, bool), u64)> = self.chatters.drain().collect();
        let session = self.session.clone();
        self.db.with(|c| {
            for ((user, sim), n) in &batch {
                c.execute(
                    "INSERT INTO alerts_chatters (session, user, count, sim) VALUES (?1, ?2, ?3, ?4)
                     ON CONFLICT(session, user) DO UPDATE SET count = count + excluded.count, sim = MAX(sim, excluded.sim)",
                    params![session, user, *n as i64, *sim as i64],
                )?;
            }
            Ok(())
        })
    }

    fn latest(&self, kind: &str) -> Result<Option<(String, f64, Option<i64>, Option<i64>, Option<String>)>> {
        self.db.with(|c| {
            c.query_row("SELECT user, amount, tier, months, currency FROM alerts_log WHERE kind = ?1 ORDER BY id DESC LIMIT 1", [kind], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .optional()
        })
    }

    fn top(&self, kind: &str, session: Option<&str>) -> Result<Option<(String, f64)>> {
        self.db.with(|c| {
            let sql = "SELECT user, SUM(amount) AS s FROM alerts_log WHERE kind = ?1 AND (?2 IS NULL OR session = ?2)
                       GROUP BY user ORDER BY s DESC, MIN(id) ASC LIMIT 1";
            c.query_row(sql, params![kind, session], |r| Ok((r.get(0)?, r.get(1)?))).optional()
        })
    }

    fn all_stats(&self) -> Result<Vec<(String, Value)>> {
        let mut out: Vec<(String, Value)> = Vec::new();
        let mut put = |a: &str, v: Value| out.push((a.to_string(), v));
        let s = |x: Option<String>| Value::Str(x.unwrap_or_default());
        let l = self.latest("follow")?;
        put("stats.latest.follow.user", s(l.map(|x| x.0)));
        let l = self.latest("sub")?;
        put("stats.latest.sub.user", s(l.as_ref().map(|x| x.0.clone())));
        put("stats.latest.sub.tier", Value::Int(l.as_ref().and_then(|x| x.2).unwrap_or(0)));
        put("stats.latest.sub.months", Value::Int(l.as_ref().and_then(|x| x.3).unwrap_or(0)));
        let l = self.latest("gift")?;
        put("stats.latest.gift.user", s(l.as_ref().map(|x| x.0.clone())));
        put("stats.latest.gift.count", Value::Int(l.as_ref().map(|x| x.1 as i64).unwrap_or(0)));
        let l = self.latest("cheer")?;
        put("stats.latest.cheer.user", s(l.as_ref().map(|x| x.0.clone())));
        put("stats.latest.cheer.amount", Value::Int(l.as_ref().map(|x| x.1 as i64).unwrap_or(0)));
        let l = self.latest("tip")?;
        put("stats.latest.tip.user", s(l.as_ref().map(|x| x.0.clone())));
        put("stats.latest.tip.amount", Value::Float(l.as_ref().map(|x| x.1).unwrap_or(0.0)));
        put("stats.latest.tip.currency", s(l.as_ref().and_then(|x| x.4.clone())));
        let l = self.latest("raid")?;
        put("stats.latest.raid.user", s(l.as_ref().map(|x| x.0.clone())));
        put("stats.latest.raid.viewers", Value::Int(l.as_ref().map(|x| x.1 as i64).unwrap_or(0)));
        for kind in ["cheer", "tip", "gift"] {
            for (scope, sess) in [("session", Some(self.session.as_str())), ("alltime", None)] {
                let t = self.top(kind, sess)?;
                put(&format!("stats.top.{kind}.{scope}.user"), s(t.as_ref().map(|x| x.0.clone())));
                let amt = t.map(|x| x.1).unwrap_or(0.0);
                put(&format!("stats.top.{kind}.{scope}.amount"), if kind == "tip" { Value::Float(amt) } else { Value::Int(amt as i64) });
            }
        }
        let totals: HashMap<String, (i64, f64)> = self.db.with(|c| {
            let mut st = c.prepare("SELECT kind, COUNT(*), SUM(amount) FROM alerts_log WHERE session = ?1 GROUP BY kind")?;
            st.query_map([&self.session], |r| Ok((r.get::<_, String>(0)?, (r.get(1)?, r.get(2)?))))?.collect()
        })?;
        let n = |k: &str| totals.get(k).map(|x| x.0).unwrap_or(0);
        let sum = |k: &str| totals.get(k).map(|x| x.1).unwrap_or(0.0);
        put("stats.session.follows", Value::Int(n("follow")));
        put("stats.session.subs", Value::Int(n("sub")));
        put("stats.session.gifts", Value::Int(sum("gift") as i64));
        put("stats.session.bits", Value::Int(sum("cheer") as i64));
        put("stats.session.tips", Value::Float((sum("tip") * 100.0).round() / 100.0));
        put("stats.session.raids", Value::Int(n("raid")));
        Ok(out)
    }

    /// Every `stats.*` and `goals.*` value (for the initial publish).
    pub fn all(&self) -> Result<Vec<(String, Value)>> {
        let mut v = self.all_stats()?;
        for g in &self.goals {
            v.extend(goal_values(g));
        }
        Ok(v)
    }

    // ---- goals --------------------------------------------------------------------------

    /// Apply `[goals.*]`; returns the addresses of goals that no longer exist.
    pub fn set_goals(&mut self, defs: Vec<GoalDef>) -> Result<Vec<String>> {
        let removed: Vec<String> = self.goals.iter().filter(|g| !defs.iter().any(|d| d.name == g.def.name)).map(|g| format!("goals.{}", g.def.name)).collect();
        let mut goals = Vec::new();
        for def in defs {
            let current: f64 =
                self.db.with(|c| c.query_row("SELECT current FROM alerts_goals WHERE name = ?1", [&def.name], |r| r.get(0)).optional())?.unwrap_or(0.0);
            goals.push(GoalRt { def, current });
        }
        self.goals = goals;
        Ok(removed)
    }

    pub fn goal_defs(&self) -> impl Iterator<Item = (&GoalDef, f64)> {
        self.goals.iter().map(|g| (&g.def, g.current))
    }

    fn goal_index(&self, name: &str) -> Result<usize> {
        self.goals.iter().position(|g| g.def.name == name).ok_or_else(|| anyhow::anyhow!("unknown goal `{name}`"))
    }

    fn goal_store(&mut self, i: usize, current: f64, sim_delta: f64) -> Result<()> {
        let name = self.goals[i].def.name.clone();
        self.db.with(|c| {
            c.execute(
                "INSERT INTO alerts_goals (name, current, sim, updated_at) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(name) DO UPDATE SET current = excluded.current, sim = sim + ?3, updated_at = excluded.updated_at",
                params![name, current, sim_delta, unix_now()],
            )
        })?;
        self.goals[i].current = current;
        Ok(())
    }

    fn goal_add_at(&mut self, i: usize, delta: f64, sim: bool) -> Result<(Vec<(String, Value)>, Vec<(String, f64, f64)>)> {
        let before = self.goals[i].current;
        let after = ((before + delta) * 100.0).round() / 100.0;
        self.goal_store(i, after, if sim { delta } else { 0.0 })?;
        let g = &self.goals[i];
        let reached = if before < g.def.target && after >= g.def.target { vec![(g.def.name.clone(), after, g.def.target)] } else { Vec::new() };
        Ok((goal_values(g), reached))
    }

    pub fn goal_add(&mut self, name: &str, delta: f64) -> Result<Update> {
        let i = self.goal_index(name)?;
        let (changes, reached) = self.goal_add_at(i, delta, false)?;
        Ok(Update { changes, reached })
    }

    pub fn goal_set(&mut self, name: &str, value: f64) -> Result<Update> {
        let i = self.goal_index(name)?;
        let delta = value - self.goals[i].current;
        self.goal_add(name, delta)
    }

    /// Reset to zero (also forgets simulated contributions).
    pub fn goal_reset(&mut self, name: &str) -> Result<Update> {
        let i = self.goal_index(name)?;
        self.db.with(|c| c.execute("UPDATE alerts_goals SET current = 0, sim = 0, updated_at = ?2 WHERE name = ?1", params![name, unix_now()]))?;
        self.goals[i].current = 0.0;
        Ok(Update { changes: goal_values(&self.goals[i]), reached: Vec::new() })
    }

    /// Remove everything the simulator contributed (log rows, chatters, goal amounts).
    pub fn purge_simulated(&mut self) -> Result<Update> {
        self.flush_chatters()?;
        let (rows, goals) = self.db.with(|c| {
            let rows = c.execute("DELETE FROM alerts_log WHERE sim = 1", [])?;
            c.execute("DELETE FROM alerts_chatters WHERE sim = 1", [])?;
            let goals = c.execute("UPDATE alerts_goals SET current = MAX(0, current - sim), sim = 0 WHERE sim != 0", [])?;
            Ok((rows, goals))
        })?;
        for g in &mut self.goals {
            g.current =
                self.db.with(|c| c.query_row("SELECT current FROM alerts_goals WHERE name = ?1", [&g.def.name], |r| r.get(0)).optional())?.unwrap_or(0.0);
        }
        tracing::info!("purged {rows} simulated stats rows, {goals} goals adjusted");
        Ok(Update { changes: self.all()?, reached: Vec::new() })
    }

    // ---- credits ------------------------------------------------------------------------

    /// Session summary for the credits roll and the eventlist.
    pub fn credits(&mut self, top_chatters: usize) -> Result<Value> {
        self.flush_chatters()?;
        let session = self.session.clone();
        self.db.with(|c| {
            let list = |sql: &str, f: &dyn Fn(&rusqlite::Row<'_>) -> rusqlite::Result<Value>| -> rusqlite::Result<Vec<Value>> {
                let mut st = c.prepare(sql)?;
                st.query_map([&session], |r| f(r))?.collect()
            };
            let follows = list("SELECT user FROM alerts_log WHERE session = ?1 AND kind = 'follow' GROUP BY user ORDER BY MIN(id)", &|r| Ok(Value::Str(r.get(0)?)))?;
            let subs = list(
                "SELECT user, MAX(tier), MAX(months), MAX(gift) FROM alerts_log WHERE session = ?1 AND kind = 'sub' GROUP BY user ORDER BY MIN(id)",
                &|r| {
                    Ok(Value::map()
                        .with("user", r.get::<_, String>(0)?)
                        .with("tier", r.get::<_, Option<i64>>(1)?.unwrap_or(1))
                        .with("months", r.get::<_, Option<i64>>(2)?.unwrap_or(1))
                        .with("gift", r.get::<_, i64>(3)? != 0))
                },
            )?;
            let sums = |kind: &str| -> rusqlite::Result<Vec<Value>> {
                list(
                    &format!("SELECT user, SUM(amount), MAX(currency) FROM alerts_log WHERE session = ?1 AND kind = '{kind}' GROUP BY user, currency ORDER BY SUM(amount) DESC"),
                    &|r| {
                        let amt: f64 = r.get(1)?;
                        Ok(Value::map().with("user", r.get::<_, String>(0)?).with("amount", amt).with("currency", r.get::<_, Option<String>>(2)?.map(Value::Str).unwrap_or_default()))
                    },
                )
            };
            let cheers = sums("cheer")?;
            let tips = sums("tip")?;
            let gifts = sums("gift")?;
            let raids = list("SELECT user, amount FROM alerts_log WHERE session = ?1 AND kind = 'raid' ORDER BY id", &|r| {
                Ok(Value::map().with("user", r.get::<_, String>(0)?).with("viewers", r.get::<_, f64>(1)? as i64))
            })?;
            let mut st = c.prepare("SELECT user, count FROM alerts_chatters WHERE session = ?1 ORDER BY count DESC, user LIMIT ?2")?;
            let chatters: Vec<Value> = st
                .query_map(params![session, top_chatters as i64], |r| Ok(Value::map().with("user", r.get::<_, String>(0)?).with("messages", r.get::<_, i64>(1)?)))?
                .collect::<rusqlite::Result<_>>()?;
            let total = |v: &[Value], k: &str| v.iter().filter_map(|x| x.get_path(k).and_then(Value::as_f64)).sum::<f64>();
            let totals = Value::map()
                .with("follows", follows.len())
                .with("subs", subs.len())
                .with("gifts", total(&gifts, "amount") as i64)
                .with("bits", total(&cheers, "amount") as i64)
                .with("tips", (total(&tips, "amount") * 100.0).round() / 100.0)
                .with("raids", raids.len());
            Ok(Value::map()
                .with("session", session.clone())
                .with("follows", follows)
                .with("subs", subs)
                .with("gifts", gifts)
                .with("cheers", cheers)
                .with("tips", tips)
                .with("raids", raids)
                .with("chatters", chatters)
                .with("totals", totals))
        })
    }

    /// Recent counted events (newest first) for the eventlist overlay's first paint.
    pub fn recent(&self, n: usize) -> Result<Value> {
        self.db.with(|c| {
            let mut st =
                c.prepare("SELECT kind, user, amount, tier, months, currency, message, gift, ts, user_id FROM alerts_log ORDER BY id DESC LIMIT ?1")?;
            let rows = st.query_map([n as i64], |r| {
                Ok(Value::map()
                    .with("kind", r.get::<_, String>(0)?)
                    .with("user", r.get::<_, String>(1)?)
                    .with("amount", r.get::<_, f64>(2)?)
                    .with("tier", r.get::<_, Option<i64>>(3)?.map(Value::Int).unwrap_or_default())
                    .with("months", r.get::<_, Option<i64>>(4)?.map(Value::Int).unwrap_or_default())
                    .with("currency", r.get::<_, Option<String>>(5)?.map(Value::Str).unwrap_or_default())
                    .with("message", r.get::<_, String>(6)?)
                    .with("gift", r.get::<_, i64>(7)? != 0)
                    .with("ts", r.get::<_, i64>(8)?)
                    .with("user_id", r.get::<_, String>(9)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>().map(Value::List)
        })
    }
}

fn goal_values(g: &GoalRt) -> Vec<(String, Value)> {
    let p = format!("goals.{}", g.def.name);
    vec![
        (format!("{p}.current"), Value::Float(g.current)),
        (format!("{p}.target"), Value::Float(g.def.target)),
        (format!("{p}.label"), Value::Str(g.def.label.clone())),
        (format!("{p}.progress"), Value::Float((g.current / g.def.target).clamp(0.0, 1.0))),
    ]
}

/// How much an event adds to a goal.
fn goal_delta(counts: &Counts, e: &Event, row: Option<&Row>) -> f64 {
    match counts {
        Counts::Builtin(k) => match (k.as_str(), row) {
            ("follows", Some(r)) if r.kind == "follow" => 1.0,
            ("subs", Some(r)) if r.kind == "sub" => 1.0,
            ("sub_points", Some(r)) if r.kind == "sub" => sub_points(r.tier),
            ("bits", Some(r)) if r.kind == "cheer" => r.amount,
            ("tips", Some(r)) if r.kind == "tip" => r.amount,
            ("gifts", Some(r)) if r.kind == "gift" => r.amount,
            ("raids", Some(r)) if r.kind == "raid" => 1.0,
            _ => 0.0,
        },
        Counts::Custom { when, cond, add } => {
            if !address::matches(when, &e.ty) {
                return 0.0;
            }
            let scope = |p: &str| -> Value {
                if p == "event" {
                    return e.payload.clone();
                }
                match p.strip_prefix("event.") {
                    Some(k) => e.payload.get_path(k).cloned().unwrap_or_default(),
                    None => Value::Null,
                }
            };
            if cond.as_ref().is_some_and(|c| !c.eval_bool(&scope)) {
                return 0.0;
            }
            add.eval(&scope).as_f64().filter(|x| x.is_finite()).unwrap_or(0.0)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_goals;
    use se_proto::{Actor, Origin};

    fn ev(ty: &str, user: &str, payload: Value) -> Event {
        Event::new(ty, Origin::Twitch, payload.with("user", user)).with_actor(Actor {
            platform: "twitch".into(),
            id: format!("id-{user}"),
            name: user.into(),
            roles: vec![Role::Everyone],
        })
    }

    fn get<'a>(ch: &'a [(String, Value)], a: &str) -> &'a Value {
        &ch.iter().rev().find(|(k, _)| k == a).unwrap_or_else(|| panic!("{a} not published")).1
    }

    fn goals() -> Vec<GoalDef> {
        let t: toml::Table = "[subs]\ntarget = 10\n[bits]\ncounts = \"bits\"\ntarget = 1000\n[points]\ncounts = \"sub_points\"\ntarget = 100\n[hype]\nwhen = \"twitch.redeem\"\nif = \"event.reward == 'HYPE'\"\nadd = \"event.cost / 1000\"\ntarget = 10"
            .parse()
            .unwrap();
        parse_goals(Some(&toml::Value::Table(t))).unwrap()
    }

    #[test]
    fn latest_top_and_session_totals() {
        let mut s = Stats::new(Db::memory().unwrap(), "s1", StatsSettings::default()).unwrap();
        s.record(&ev("twitch.cheer", "ana", Value::map().with("bits", 100)), false).unwrap();
        s.record(&ev("twitch.cheer", "bo", Value::map().with("bits", 300)), false).unwrap();
        let up = s.record(&ev("twitch.cheer", "ana", Value::map().with("bits", 250)), false).unwrap();
        assert_eq!(get(&up.changes, "stats.latest.cheer.user"), &Value::Str("ana".into()));
        assert_eq!(get(&up.changes, "stats.latest.cheer.amount"), &Value::Int(250));
        assert_eq!(get(&up.changes, "stats.top.cheer.session.user"), &Value::Str("ana".into()), "350 total beats 300");
        assert_eq!(get(&up.changes, "stats.top.cheer.session.amount"), &Value::Int(350));
        assert_eq!(get(&up.changes, "stats.session.bits"), &Value::Int(650));
        // new stream: session resets, all-time stays
        let up = s.set_session("s2").unwrap();
        assert_eq!(get(&up.changes, "stats.top.cheer.session.user"), &Value::Str(String::new()));
        assert_eq!(get(&up.changes, "stats.top.cheer.alltime.amount"), &Value::Int(350));
        assert_eq!(get(&up.changes, "stats.latest.cheer.user"), &Value::Str("ana".into()), "latest survives streams");
        // private tips are anonymous
        let up = s
            .record(&ev("tip", "secret", Value::map().with("amount", 5.5).with("currency", "USD").with("is_public", false).with("message", "hidden")), false)
            .unwrap();
        assert_eq!(get(&up.changes, "stats.latest.tip.user"), &Value::Str("Anonymous".into()));
        assert_eq!(get(&up.changes, "stats.session.tips"), &Value::Float(5.5));
    }

    #[test]
    fn goals_count_persist_and_fire_once() {
        let db = Db::memory().unwrap();
        let mut s = Stats::new(db.clone(), "s1", StatsSettings::default()).unwrap();
        s.set_goals(goals()).unwrap();
        for i in 0..9 {
            let up = s.record(&ev("twitch.sub", &format!("u{i}"), Value::map().with("tier", 3)), false).unwrap();
            assert!(up.reached.is_empty());
        }
        let up = s.record(&ev("twitch.sub", "u9", Value::map().with("tier", 1).with("is_gift", true)), false).unwrap();
        assert_eq!(up.reached.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(), vec!["subs"]);
        assert_eq!(get(&up.changes, "goals.subs.current"), &Value::Float(10.0));
        assert_eq!(get(&up.changes, "goals.subs.progress"), &Value::Float(1.0));
        assert_eq!(get(&up.changes, "goals.points.current"), &Value::Float(55.0), "9×tier3 (6) + 1×tier1");
        assert!(s.record(&ev("twitch.sub", "u10", Value::map()), false).unwrap().reached.is_empty(), "fires only when crossing");
        // gift events don't double count subs
        let up = s.record(&ev("twitch.gift", "santa", Value::map().with("count", 5)), false).unwrap();
        assert!(!up.changes.iter().any(|(k, _)| k == "goals.subs.current"));
        // custom goal with expression
        let up = s.record(&ev("twitch.redeem", "x", Value::map().with("reward", "HYPE").with("cost", 2000)), false).unwrap();
        assert_eq!(get(&up.changes, "goals.hype.current"), &Value::Float(2.0));
        s.record(&ev("twitch.redeem", "x", Value::map().with("reward", "OTHER").with("cost", 2000)), false).unwrap();
        // restart: a new Stats on the same DB sees the same goal values
        let mut s2 = Stats::new(db, "s2", StatsSettings::default()).unwrap();
        s2.set_goals(goals()).unwrap();
        let all = s2.all().unwrap();
        assert_eq!(get(&all, "goals.subs.current"), &Value::Float(11.0));
        assert_eq!(get(&all, "goals.hype.current"), &Value::Float(2.0));
        // manual edits
        assert_eq!(get(&s2.goal_set("subs", 3.0).unwrap().changes, "goals.subs.current"), &Value::Float(3.0));
        assert_eq!(get(&s2.goal_add("subs", 2.0).unwrap().changes, "goals.subs.current"), &Value::Float(5.0));
        assert_eq!(get(&s2.goal_reset("subs").unwrap().changes, "goals.subs.current"), &Value::Float(0.0));
        assert!(s2.goal_add("nope", 1.0).is_err());
    }

    #[test]
    fn simulated_data_can_be_purged_or_ignored() {
        let mut s = Stats::new(Db::memory().unwrap(), "s1", StatsSettings::default()).unwrap();
        s.set_goals(goals()).unwrap();
        s.record(&ev("twitch.cheer", "real", Value::map().with("bits", 100)), false).unwrap();
        s.record(&ev("twitch.cheer", "fake", Value::map().with("bits", 900)), true).unwrap();
        let all = s.all().unwrap();
        assert_eq!(get(&all, "stats.top.cheer.alltime.user"), &Value::Str("fake".into()));
        assert_eq!(get(&all, "goals.bits.current"), &Value::Float(1000.0));
        let up = s.purge_simulated().unwrap();
        assert_eq!(get(&up.changes, "stats.top.cheer.alltime.user"), &Value::Str("real".into()));
        assert_eq!(get(&up.changes, "goals.bits.current"), &Value::Float(100.0));
        // count_simulated = false: sim events leave no trace
        let mut s = Stats::new(Db::memory().unwrap(), "s1", StatsSettings { count_simulated: false, ..Default::default() }).unwrap();
        s.set_goals(goals()).unwrap();
        assert!(s.record(&ev("twitch.cheer", "fake", Value::map().with("bits", 900)), true).unwrap().changes.is_empty());
    }

    #[test]
    fn credits_summarize_the_session() {
        let mut s = Stats::new(Db::memory().unwrap(), "s1", StatsSettings::default()).unwrap();
        s.record(&ev("twitch.follow", "f1", Value::map()), false).unwrap();
        s.record(&ev("twitch.follow", "f1", Value::map()), false).unwrap();
        s.record(&ev("twitch.sub", "s1", Value::map().with("tier", 2).with("months", 5)), false).unwrap();
        s.record(&ev("twitch.cheer", "c1", Value::map().with("bits", 100)), false).unwrap();
        s.record(&ev("twitch.cheer", "c1", Value::map().with("bits", 50)), false).unwrap();
        s.record(&ev("twitch.cheer", "c2", Value::map().with("bits", 500)), false).unwrap();
        s.record(&ev("twitch.raid", "r1", Value::map().with("viewers", 42)), false).unwrap();
        s.record(&ev("tip", "t1", Value::map().with("amount", 3.0).with("currency", "EUR")), false).unwrap();
        for (who, n) in [("chatty", 5), ("quiet", 1)] {
            for _ in 0..n {
                s.chat(&ev("twitch.chat", who, Value::map()), false);
            }
        }
        let mut owner = ev("twitch.chat", "me", Value::map());
        owner.actor.as_mut().unwrap().roles = vec![Role::Owner];
        s.chat(&owner, false);
        s.chat(&ev("twitch.chat", "Nightbot", Value::map()), false);
        let c = s.credits(10).unwrap();
        assert_eq!(c.get_path("follows").unwrap().as_list().unwrap().len(), 1, "one per user");
        assert_eq!(c.get_path("subs.0.tier"), Some(&Value::Int(2)));
        assert_eq!(c.get_path("cheers.0.user"), Some(&Value::Str("c2".into())));
        assert_eq!(c.get_path("cheers.1.amount"), Some(&Value::Float(150.0)));
        assert_eq!(c.get_path("raids.0.viewers"), Some(&Value::Int(42)));
        assert_eq!(c.get_path("tips.0.currency"), Some(&Value::Str("EUR".into())));
        let chatters: Vec<&str> =
            c.get_path("chatters").unwrap().as_list().unwrap().iter().filter_map(|v| v.get_path("user").and_then(Value::as_str)).collect();
        assert_eq!(chatters, vec!["chatty", "quiet"], "broadcaster and bots excluded");
        assert_eq!(c.get_path("totals.bits"), Some(&Value::Int(650)));
    }
}
