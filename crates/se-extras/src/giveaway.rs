//! Chat-entry giveaways with weighted entries and a draw UI (§14.1, M11).
//!
//! * Viewers enter by typing the keyword (`!join`) in chat while a giveaway is open.
//! * Each entry's weight comes from the viewer's best role (`[giveaway.weights]`) plus a bonus
//!   per subscribed month, capped at `max_weight`. Bots and the broadcaster are excluded by
//!   default.
//! * `giveaway.draw` picks a winner with probability proportional to weight among entries
//!   that haven't won yet (drawing again = re-roll/next winner).
//! * Banned/timed-out users are removed (`twitch.user.purge`), so they can't win.
//! * The round survives engine restarts (runtime DB).
//!
//! Actions: `giveaway.open {title?, keyword?, min_role?, duration?}`, `giveaway.close`,
//! `giveaway.draw`, `giveaway.reset`, `giveaway.remove {user}`, `giveaway.enter {user, role?,
//! sub_months?}` (manual entry from the UI). State: `giveaway.{state,title,keyword,entries,
//! tickets,winner,winners,closes_at}`. Events: `giveaway.opened|closed|entered|drawn|reset`.
//! Query `giveaway`: the round with per-entry weight and win chance.

use crate::util::{self, Section};
use parking_lot::Mutex;
use rand::{Rng, SeedableRng};
use se_hub::{Bus, EngineCtx};
use se_proto::{Actor, Event, Meta, Op, Origin, Role, Value, ValueType};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

const TARGET: &str = "giveaway";
const KV_NS: &str = "giveaway";
const KV_ROUND: &str = "round";
const ROLES: [Role; 6] = [Role::Everyone, Role::Follower, Role::Sub, Role::Vip, Role::Mod, Role::Owner];

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Chat keyword that enters the giveaway (first word of the message, case-insensitive).
    pub keyword: String,
    /// Chat platforms whose messages count (`twitch`, `tiktok`).
    pub platforms: Vec<String>,
    /// Entry weight by role; a viewer gets the highest weight among their roles.
    pub weights: BTreeMap<String, f64>,
    /// Extra weight per subscribed month (from the chat badge).
    pub sub_month_bonus: f64,
    /// Months counted for the bonus at most.
    pub max_sub_months: i64,
    pub max_weight: f64,
    /// Lowest role allowed to enter by default (`giveaway.open min_role=sub` overrides).
    pub min_role: String,
    pub exclude_roles: Vec<String>,
    /// Lowercase logins never entered (chat bots).
    pub exclude_users: Vec<String>,
    /// Announce open/close/winner in chat.
    pub announce: bool,
    pub announce_open: String,
    pub announce_close: String,
    pub announce_winner: String,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            keyword: "!join".into(),
            platforms: vec!["twitch".into()],
            weights: [("everyone", 1.0), ("follower", 1.0), ("sub", 2.0), ("vip", 2.0), ("mod", 1.0), ("owner", 1.0)]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v))
                .collect(),
            sub_month_bonus: 0.05,
            max_sub_months: 60,
            max_weight: 5.0,
            min_role: "everyone".into(),
            exclude_roles: vec!["owner".into()],
            exclude_users: ["nightbot", "streamelements", "streamlabs", "moobot", "fossabot", "sery_bot", "wizebot"].map(String::from).to_vec(),
            announce: true,
            announce_open: "Giveaway open: {title}! Type {keyword} in chat to enter.".into(),
            announce_close: "Giveaway closed with {entries} entries. Good luck!".into(),
            announce_winner: "Congratulations @{user}, you won {title}! ({chance} chance)".into(),
        }
    }
}

/// Validated settings.
#[derive(Clone, Debug)]
pub struct Rules {
    pub keyword: String,
    pub platforms: Vec<String>,
    weights: [f64; 6],
    pub sub_month_bonus: f64,
    pub max_sub_months: i64,
    pub max_weight: f64,
    pub min_role: Role,
    pub exclude_roles: Vec<Role>,
    pub exclude_users: Vec<String>,
    pub settings: Settings,
}

fn role_index(r: Role) -> usize {
    ROLES.iter().position(|x| *x == r).unwrap_or(0)
}

fn parse_role(s: &str) -> Result<Role, String> {
    Role::parse(&s.to_ascii_lowercase()).ok_or_else(|| format!("unknown role `{s}` (everyone, follower, sub, vip, mod, owner)"))
}

impl Rules {
    pub fn new(s: &Settings) -> Result<Rules, String> {
        let mut weights = [f64::NAN; 6];
        for (k, v) in &s.weights {
            if !v.is_finite() || *v < 0.0 {
                return Err(format!("weights.{k} must be a number ≥ 0"));
            }
            weights[role_index(parse_role(k)?)] = *v;
        }
        // Roles without a weight inherit the next lower configured role's weight.
        let mut last = 1.0;
        for w in weights.iter_mut() {
            if w.is_nan() {
                *w = last;
            }
            last = *w;
        }
        if !(s.max_weight.is_finite() && s.max_weight > 0.0) {
            return Err("max_weight must be > 0".into());
        }
        if !(s.sub_month_bonus.is_finite() && s.sub_month_bonus >= 0.0) {
            return Err("sub_month_bonus must be ≥ 0".into());
        }
        let keyword = s.keyword.trim().to_string();
        if keyword.is_empty() || keyword.contains(char::is_whitespace) {
            return Err("keyword must be one word".into());
        }
        Ok(Rules {
            keyword,
            platforms: s.platforms.iter().map(|p| p.to_ascii_lowercase()).collect(),
            weights,
            sub_month_bonus: s.sub_month_bonus,
            max_sub_months: s.max_sub_months.max(0),
            max_weight: s.max_weight,
            min_role: parse_role(&s.min_role)?,
            exclude_roles: s.exclude_roles.iter().map(|r| parse_role(r)).collect::<Result<_, _>>()?,
            exclude_users: s.exclude_users.iter().map(|u| u.trim().trim_start_matches('@').to_ascii_lowercase()).collect(),
            settings: s.clone(),
        })
    }

    /// Entry weight for a viewer: best role weight + sub-month bonus, capped.
    pub fn weight(&self, roles: &[Role], sub_months: i64) -> f64 {
        let base = roles.iter().chain(std::iter::once(&Role::Everyone)).map(|r| self.weights[role_index(*r)]).fold(0.0, f64::max);
        let bonus = self.sub_month_bonus * sub_months.clamp(0, self.max_sub_months) as f64;
        (base + bonus).min(self.max_weight)
    }
}

impl Default for Rules {
    fn default() -> Self {
        Rules::new(&Settings::default()).expect("default giveaway settings are valid")
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    Idle,
    Open,
    Closed,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Open => "open",
            Phase::Closed => "closed",
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// `platform:user-id` (stable across renames).
    pub key: String,
    pub user: String,
    pub platform: String,
    pub roles: Vec<Role>,
    pub sub_months: i64,
    pub weight: f64,
    pub at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Winner {
    pub key: String,
    pub user: String,
    pub platform: String,
    pub weight: f64,
    /// Win probability at draw time.
    pub chance: f64,
    pub entries: usize,
    pub at: i64,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default, PartialEq)]
#[serde(default)]
pub struct Round {
    pub phase: Phase,
    pub title: String,
    pub keyword: String,
    pub min_role: Role,
    pub opened_at: i64,
    pub closes_at: Option<i64>,
    pub entries: Vec<Entry>,
    pub winners: Vec<Winner>,
}

#[derive(Debug, PartialEq)]
pub enum Entered {
    Yes(f64),
    Duplicate,
    NotOpen,
    Rejected(&'static str),
}

/// Index picked by a uniform `r01` in [0, 1) over positive `weights` (cumulative search).
pub fn pick(weights: &[f64], r01: f64) -> Option<usize> {
    let total: f64 = weights.iter().filter(|w| **w > 0.0).sum();
    if total <= 0.0 {
        return None;
    }
    let target = r01.clamp(0.0, 1.0) * total;
    let mut acc = 0.0;
    let mut last = None;
    for (i, w) in weights.iter().enumerate() {
        if *w <= 0.0 {
            continue;
        }
        acc += w;
        last = Some(i);
        if target < acc {
            return Some(i);
        }
    }
    // Only reachable through rounding at r01 → 1.0.
    last
}

impl Round {
    fn has_won(&self, key: &str) -> bool {
        self.winners.iter().any(|w| w.key == key)
    }

    /// Entries still in the draw.
    pub fn eligible(&self) -> impl Iterator<Item = &Entry> {
        self.entries.iter().filter(|e| e.weight > 0.0 && !self.has_won(&e.key))
    }

    pub fn tickets(&self) -> f64 {
        self.eligible().map(|e| e.weight).sum()
    }

    pub fn chance(&self, e: &Entry) -> f64 {
        let t = self.tickets();
        if t <= 0.0 || self.has_won(&e.key) { 0.0 } else { e.weight / t }
    }

    /// Open a giveaway. From idle this starts a fresh round; a closed round reopens with its
    /// entries kept (a new round needs `reset` first). Returns true for a fresh round.
    pub fn open(&mut self, title: Option<String>, keyword: String, min_role: Role, duration_s: Option<i64>, now: i64) -> Result<bool, String> {
        let fresh = match self.phase {
            Phase::Open => return Err("a giveaway is already open".into()),
            Phase::Idle => {
                *self = Round { opened_at: now, ..Default::default() };
                true
            }
            Phase::Closed => false,
        };
        self.phase = Phase::Open;
        if let Some(t) = title.filter(|t| !t.trim().is_empty()) {
            self.title = t.trim().to_string();
        } else if self.title.is_empty() {
            self.title = "the giveaway".into();
        }
        self.keyword = keyword;
        self.min_role = min_role;
        self.closes_at = duration_s.filter(|d| *d > 0).map(|d| now + d);
        Ok(fresh)
    }

    pub fn close(&mut self) -> Result<(), String> {
        if self.phase != Phase::Open {
            return Err("no giveaway is open".into());
        }
        self.phase = Phase::Closed;
        self.closes_at = None;
        Ok(())
    }

    pub fn enter(&mut self, rules: &Rules, actor: &Actor, sub_months: i64, now: i64) -> Entered {
        if self.phase != Phase::Open {
            return Entered::NotOpen;
        }
        if actor.id.is_empty() && actor.name.is_empty() {
            return Entered::Rejected("anonymous");
        }
        let key = format!("{}:{}", actor.platform, if actor.id.is_empty() { actor.name.to_ascii_lowercase() } else { actor.id.clone() });
        if self.entries.iter().any(|e| e.key == key) {
            return Entered::Duplicate;
        }
        if rules.exclude_users.iter().any(|u| u.eq_ignore_ascii_case(&actor.name)) {
            return Entered::Rejected("excluded user");
        }
        if actor.roles.iter().any(|r| rules.exclude_roles.contains(r)) {
            return Entered::Rejected("excluded role");
        }
        if actor.top_role() < self.min_role {
            return Entered::Rejected("role too low");
        }
        let weight = rules.weight(&actor.roles, sub_months);
        if weight <= 0.0 {
            return Entered::Rejected("zero weight");
        }
        self.entries.push(Entry {
            key,
            user: actor.name.clone(),
            platform: actor.platform.clone(),
            roles: actor.roles.clone(),
            sub_months: sub_months.max(0),
            weight,
            at: now,
        });
        Entered::Yes(weight)
    }

    /// Remove an entry by `platform:id` key or (case-insensitive) user name.
    pub fn remove(&mut self, who: &str) -> Option<Entry> {
        let who = who.trim().trim_start_matches('@');
        let i = self.entries.iter().position(|e| e.key == who || e.user.eq_ignore_ascii_case(who))?;
        Some(self.entries.remove(i))
    }

    /// Weighted draw among entries that haven't won. Closes an open round first.
    pub fn draw(&mut self, rng: &mut impl Rng, now: i64) -> Result<Winner, String> {
        if self.phase == Phase::Idle {
            return Err("no giveaway to draw from".into());
        }
        if self.phase == Phase::Open {
            self.close()?;
        }
        let pool: Vec<&Entry> = self.eligible().collect();
        let weights: Vec<f64> = pool.iter().map(|e| e.weight).collect();
        let total: f64 = weights.iter().sum();
        let i = pick(&weights, rng.random::<f64>()).ok_or_else(|| "no eligible entries left".to_string())?;
        let e = pool[i];
        let w = Winner {
            key: e.key.clone(),
            user: e.user.clone(),
            platform: e.platform.clone(),
            weight: e.weight,
            chance: e.weight / total,
            entries: pool.len(),
            at: now,
        };
        self.winners.push(w.clone());
        Ok(w)
    }

    pub fn reset(&mut self) {
        *self = Round::default();
    }

    pub fn to_query(&self) -> Value {
        let tickets = self.tickets();
        let entries: Vec<Value> = self
            .entries
            .iter()
            .map(|e| {
                let won = self.has_won(&e.key);
                Value::map()
                    .with("key", e.key.clone())
                    .with("user", e.user.clone())
                    .with("platform", e.platform.clone())
                    .with("role", role_name(e.roles.iter().copied().max().unwrap_or_default()))
                    .with("sub_months", e.sub_months)
                    .with("weight", e.weight)
                    .with("chance", if won || tickets <= 0.0 { 0.0 } else { e.weight / tickets })
                    .with("won", won)
                    .with("at", e.at)
            })
            .collect();
        Value::map()
            .with("state", self.phase.as_str())
            .with("title", self.title.clone())
            .with("keyword", self.keyword.clone())
            .with("min_role", role_name(self.min_role))
            .with("opened_at", self.opened_at)
            .with("closes_at", self.closes_at.map(Value::Int).unwrap_or_default())
            .with("tickets", tickets)
            .with("entries", entries)
            .with("winners", self.winners.iter().map(util::to_value).collect::<Vec<_>>())
    }
}

pub fn role_name(r: Role) -> &'static str {
    match r {
        Role::Everyone => "everyone",
        Role::Follower => "follower",
        Role::Sub => "sub",
        Role::Vip => "vip",
        Role::Mod => "mod",
        Role::Owner => "owner",
    }
}

/// `true` when `message`'s first word is `keyword`.
pub fn is_entry(message: &str, keyword: &str) -> bool {
    message.split_whitespace().next().is_some_and(|w| w.eq_ignore_ascii_case(keyword))
}

// ------------------------------------------------------------------------------------------
// subsystem

struct Shared {
    round: Round,
    rules: Rules,
}

fn declare(ctx: &EngineCtx) {
    let h = &ctx.hub;
    let own = |m: Meta| m.readonly().owner(TARGET);
    h.declare("giveaway.state", own(Meta::enumeration("idle", &["idle", "open", "closed"]).describe("Giveaway phase")));
    h.declare("giveaway.title", own(Meta::string("")));
    h.declare("giveaway.keyword", own(Meta::string("")));
    h.declare("giveaway.entries", own(Meta::int(0, [0.0, 1e9]).describe("Number of entrants")));
    h.declare("giveaway.tickets", own(Meta::float(0.0, [0.0, 1e12]).describe("Total weight still in the draw")));
    h.declare("giveaway.winner", own(Meta::string("").describe("Latest winner")));
    h.declare("giveaway.winners", own(Meta { ty: ValueType::List, default: Value::List(vec![]), ..Default::default() }));
    h.declare("giveaway.closes_at", own(Meta::int(0, [0.0, 1e12]).unit("unix s").describe("Auto-close time (0 = manual)")));
}

fn publish(ctx: &EngineCtx, r: &Round) {
    let h = &ctx.hub;
    h.publish("giveaway.state", Value::Str(r.phase.as_str().into()));
    h.publish("giveaway.title", Value::Str(r.title.clone()));
    h.publish("giveaway.keyword", Value::Str(r.keyword.clone()));
    h.publish("giveaway.entries", Value::Int(r.entries.len() as i64));
    h.publish("giveaway.tickets", Value::Float(r.tickets()));
    h.publish("giveaway.winner", Value::Str(r.winners.last().map(|w| w.user.clone()).unwrap_or_default()));
    h.publish("giveaway.winners", Value::List(r.winners.iter().map(|w| Value::Str(w.user.clone())).collect()));
    h.publish("giveaway.closes_at", Value::Int(r.closes_at.unwrap_or(0)));
}

fn emit(ctx: &EngineCtx, ty: &str, payload: Value, causal: Option<u64>) {
    ctx.hub.emit(Event::new(ty, Origin::System, payload).with_causal(causal));
}

fn pct(x: f64) -> String {
    let p = x * 100.0;
    if p >= 10.0 { format!("{p:.0}%") } else { format!("{p:.1}%") }
}

pub fn start(ctx: EngineCtx) {
    declare(&ctx);
    let round: Round = match ctx.db.kv_get(KV_NS, KV_ROUND) {
        Ok(Some(v)) => serde_json::to_value(&v).ok().and_then(|j| serde_json::from_value(j).ok()).unwrap_or_else(|| {
            ctx.hub.log("warn", TARGET, "stored giveaway could not be read; starting fresh");
            Round::default()
        }),
        Ok(None) => Round::default(),
        Err(e) => {
            ctx.hub.log("error", TARGET, format!("load giveaway: {e:#}"));
            Round::default()
        }
    };
    let shared = Arc::new(Mutex::new(Shared { round, rules: Rules::default() }));
    {
        let shared = shared.clone();
        ctx.hub.register_query(
            "giveaway",
            Arc::new(move |_name, _args| {
                let v = {
                    let s = shared.lock();
                    s.round.to_query().with("rules", util::to_value(&s.rules.settings))
                };
                Box::pin(async move { Ok(v) })
            }),
        );
    }
    let actions = ctx.hub.route_actions("giveaway");
    tokio::spawn(run(ctx, shared, actions));
}

async fn run(mut ctx: EngineCtx, shared: Arc<Mutex<Shared>>, mut actions: tokio::sync::mpsc::UnboundedReceiver<se_proto::Command>) {
    let mut settings: Section<Settings> = Section::new("giveaway", TARGET);
    let apply_settings = |ctx: &EngineCtx, settings: &mut Section<Settings>, shared: &Mutex<Shared>| {
        if settings.reload(ctx) {
            match Rules::new(&settings.value) {
                Ok(r) => shared.lock().rules = r,
                Err(e) => ctx.hub.log("error", TARGET, format!("[giveaway] in project.toml: {e}; keeping the previous settings")),
            }
        }
    };
    apply_settings(&ctx, &mut settings, &shared);
    publish(&ctx, &shared.lock().round);
    let mut bus = ctx.hub.subscribe();
    let mut rng = rand::rngs::StdRng::from_os_rng();
    let mut dirty = false;
    let mut persist = tokio::time::interval(Duration::from_secs(2));
    let mut clock = tokio::time::interval(Duration::from_millis(500));
    let mut entries_published = usize::MAX;
    loop {
        tokio::select! {
            r = ctx.config.changed() => {
                if r.is_err() { break; }
                apply_settings(&ctx, &mut settings, &shared);
            }
            b = bus.recv() => match b {
                Ok(b) => {
                    if let Bus::Event(e) = &*b && on_event(&ctx, &shared, e) {
                        dirty = true;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => ctx.hub.log("warn", TARGET, format!("missed {n} bus messages")),
                Err(_) => break,
            },
            a = actions.recv() => {
                let Some(cmd) = a else { break };
                if let Op::Action { name, args } = &cmd.op {
                    match on_action(&ctx, &shared, &mut rng, name, args, Some(cmd.id)) {
                        Ok(()) => dirty = true,
                        Err(e) => ctx.hub.log("warn", TARGET, format!("{name}: {e}")),
                    }
                }
            }
            _ = clock.tick() => {
                let due = {
                    let s = shared.lock();
                    s.round.phase == Phase::Open && s.round.closes_at.is_some_and(|t| util::unix_now() >= t)
                };
                if due && on_action(&ctx, &shared, &mut rng, "giveaway.close", &Value::Null, None).is_ok() {
                    dirty = true;
                }
                // entry counts publish at most twice a second during a flood of `!join`s
                let (n, round) = { let s = shared.lock(); (s.round.entries.len(), s.round.clone()) };
                if n != entries_published {
                    entries_published = n;
                    publish(&ctx, &round);
                }
            }
            _ = persist.tick() => {
                if dirty {
                    dirty = false;
                    let v = util::to_value(&shared.lock().round);
                    if let Err(e) = ctx.db.kv_set(KV_NS, KV_ROUND, &v) {
                        ctx.hub.log("error", TARGET, format!("save giveaway: {e:#}"));
                    }
                }
            }
        }
    }
}

/// Chat entries and deletion sync. Returns true when the round changed.
fn on_event(ctx: &EngineCtx, shared: &Mutex<Shared>, e: &Event) -> bool {
    match e.ty.as_str() {
        "twitch.chat" | "tiktok.chat" => {
            let Some(actor) = &e.actor else { return false };
            let msg = e.payload.get_path("message").and_then(Value::as_str).unwrap_or("");
            let mut s = shared.lock();
            let Shared { round, rules } = &mut *s;
            if round.phase != Phase::Open || !rules.platforms.contains(&actor.platform) || !is_entry(msg, &round.keyword) {
                return false;
            }
            let months = e.payload.get_path("sub_months").and_then(Value::as_i64).unwrap_or(0);
            match round.enter(rules, actor, months, util::unix_now()) {
                Entered::Yes(w) => {
                    let n = round.entries.len();
                    drop(s);
                    emit(ctx, "giveaway.entered", Value::map().with("user", actor.name.clone()).with("weight", w).with("entries", n as i64), Some(e.id));
                    true
                }
                _ => false,
            }
        }
        "twitch.user.purge" => {
            let id = e.payload.get_path("user_id").and_then(Value::as_str).unwrap_or("");
            let user = e.payload.get_path("user").and_then(Value::as_str).unwrap_or("");
            let mut s = shared.lock();
            let key = format!("twitch:{id}");
            let removed = if !id.is_empty() && s.round.remove(&key).is_some() {
                true
            } else {
                !user.is_empty()
                    && s.round.entries.iter().any(|x| x.platform == "twitch" && x.user.eq_ignore_ascii_case(user))
                    && s.round.remove(user).is_some()
            };
            if removed {
                ctx.hub.log("info", TARGET, format!("removed {user} from the giveaway (ban/timeout)"));
            }
            removed
        }
        _ => false,
    }
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get_path(key).and_then(Value::as_str).or_else(|| args.get_path("args.0").and_then(Value::as_str).filter(|_| key == "title" || key == "user"))
}

/// What an action did: events to emit, a chat announcement, and an operator log line.
#[derive(Default)]
pub struct Outcome {
    pub events: Vec<(&'static str, Value)>,
    pub say: Option<String>,
    pub log: Option<String>,
}

fn on_action(ctx: &EngineCtx, shared: &Mutex<Shared>, rng: &mut impl Rng, name: &str, args: &Value, causal: Option<u64>) -> Result<(), String> {
    let (out, round) = {
        let mut s = shared.lock();
        let Shared { round, rules } = &mut *s;
        let out = apply(round, rules, rng, name, args, util::unix_now())?;
        (out, round.clone())
    };
    for (ty, p) in out.events {
        emit(ctx, ty, p, causal);
    }
    publish(ctx, &round);
    if let Some(l) = out.log {
        ctx.hub.log("info", TARGET, l);
    }
    if let Some(text) = out.say {
        util::bot_say(ctx, &text);
    }
    Ok(())
}

/// Apply one `giveaway.*` action to the round.
pub fn apply(round: &mut Round, rules: &Rules, rng: &mut impl Rng, name: &str, args: &Value, now: i64) -> Result<Outcome, String> {
    let announce = rules.settings.announce;
    let mut out = Outcome::default();
    match name {
        "giveaway.open" => {
            let keyword = str_arg(args, "keyword")
                .map(|k| k.trim().to_string())
                .filter(|k| !k.is_empty() && !k.contains(char::is_whitespace))
                .unwrap_or_else(|| rules.keyword.clone());
            let min_role = match str_arg(args, "min_role") {
                Some(r) => parse_role(r)?,
                None => rules.min_role,
            };
            let duration = match args.get_path("duration") {
                Some(Value::Str(d)) => Some(se_proto::parse_duration_ms(d).ok_or_else(|| format!("bad duration `{d}`"))? as i64 / 1000),
                Some(v) => v.as_i64(),
                None => None,
            };
            let fresh = round.open(str_arg(args, "title").map(String::from), keyword, min_role, duration, now)?;
            if announce {
                out.say = Some(util::template(&rules.settings.announce_open, &[("title", &round.title), ("keyword", &round.keyword)]));
            }
            out.events.push((
                "giveaway.opened",
                Value::map()
                    .with("title", round.title.clone())
                    .with("keyword", round.keyword.clone())
                    .with("min_role", role_name(round.min_role))
                    .with("fresh", fresh)
                    .with("entries", round.entries.len() as i64),
            ));
        }
        "giveaway.close" => {
            round.close()?;
            if announce {
                out.say = Some(util::template(&rules.settings.announce_close, &[("title", &round.title), ("entries", &round.entries.len().to_string())]));
            }
            out.events.push(("giveaway.closed", Value::map().with("title", round.title.clone()).with("entries", round.entries.len() as i64)));
        }
        "giveaway.draw" => {
            let was_open = round.phase == Phase::Open;
            let w = round.draw(rng, now)?;
            if announce {
                out.say = Some(util::template(&rules.settings.announce_winner, &[("title", &round.title), ("user", &w.user), ("chance", &pct(w.chance))]));
            }
            if was_open {
                out.events.push(("giveaway.closed", Value::map().with("title", round.title.clone()).with("entries", round.entries.len() as i64)));
            }
            out.events.push((
                "giveaway.drawn",
                Value::map()
                    .with("user", w.user.clone())
                    .with("platform", w.platform.clone())
                    .with("weight", w.weight)
                    .with("chance", w.chance)
                    .with("entries", w.entries as i64)
                    .with("title", round.title.clone())
                    .with("n", round.winners.len() as i64),
            ));
        }
        "giveaway.reset" => {
            round.reset();
            out.events.push(("giveaway.reset", Value::Null));
        }
        "giveaway.remove" => {
            let who = str_arg(args, "user").ok_or("giveaway.remove needs user")?;
            let e = round.remove(who).ok_or_else(|| format!("{who} has not entered"))?;
            out.log = Some(format!("removed {} from the giveaway", e.user));
        }
        "giveaway.enter" => {
            let user = str_arg(args, "user").unwrap_or("").trim().trim_start_matches('@').to_string();
            if user.is_empty() {
                return Err("giveaway.enter needs user".into());
            }
            let role = match args.get_path("role").and_then(Value::as_str) {
                Some(r) => parse_role(r)?,
                None => Role::Everyone,
            };
            let platform = args.get_path("platform").and_then(Value::as_str).unwrap_or("manual").to_string();
            let actor = Actor { platform, id: user.to_ascii_lowercase(), name: user.clone(), roles: vec![role] };
            let months = args.get_path("sub_months").and_then(Value::as_i64).unwrap_or(0);
            match round.enter(rules, &actor, months, now) {
                Entered::Yes(w) => {
                    out.events.push(("giveaway.entered", Value::map().with("user", user).with("weight", w).with("entries", round.entries.len() as i64)))
                }
                Entered::Duplicate => return Err(format!("{user} already entered")),
                Entered::NotOpen => return Err("no giveaway is open".into()),
                Entered::Rejected(why) => return Err(format!("{user} can't enter: {why}")),
            }
        }
        other => return Err(format!("unknown action {other}")),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actor(name: &str, roles: &[Role]) -> Actor {
        Actor { platform: "twitch".into(), id: format!("id-{name}"), name: name.into(), roles: roles.to_vec() }
    }

    fn open_round() -> Round {
        let mut r = Round::default();
        r.open(Some("a drum stick".into()), "!join".into(), Role::Everyone, None, 100).unwrap();
        r
    }

    #[test]
    fn weights_by_role_and_months() {
        let rules = Rules::default();
        assert_eq!(rules.weight(&[], 0), 1.0);
        assert_eq!(rules.weight(&[Role::Follower], 0), 1.0);
        assert_eq!(rules.weight(&[Role::Sub], 0), 2.0);
        // best role wins: a subscribed mod gets the sub weight
        assert_eq!(rules.weight(&[Role::Mod, Role::Sub], 0), 2.0);
        assert!((rules.weight(&[Role::Sub], 12) - 2.6).abs() < 1e-9);
        // capped by max_sub_months (60) then by max_weight (5)
        assert!((rules.weight(&[Role::Sub], 1000) - 5.0).abs() < 1e-9);
        let s = Settings { max_weight: 3.0, ..Default::default() };
        assert_eq!(Rules::new(&s).unwrap().weight(&[Role::Sub], 1000), 3.0);
    }

    #[test]
    fn missing_role_weights_inherit_lower_role() {
        let s = Settings { weights: [("everyone".to_string(), 1.0), ("sub".to_string(), 3.0)].into_iter().collect(), ..Default::default() };
        let r = Rules::new(&s).unwrap();
        assert_eq!(r.weight(&[Role::Follower], 0), 1.0);
        assert_eq!(r.weight(&[Role::Vip], 0), 3.0);
        assert_eq!(r.weight(&[Role::Mod], 0), 3.0);
    }

    #[test]
    fn invalid_settings_are_rejected() {
        assert!(Rules::new(&Settings { min_role: "king".into(), ..Default::default() }).is_err());
        assert!(Rules::new(&Settings { weights: [("sub".to_string(), -1.0)].into_iter().collect(), ..Default::default() }).is_err());
        assert!(Rules::new(&Settings { keyword: "two words".into(), ..Default::default() }).is_err());
        assert!(Rules::new(&Settings { max_weight: 0.0, ..Default::default() }).is_err());
    }

    #[test]
    fn entry_rules() {
        let rules = Rules::default();
        let mut r = Round::default();
        assert_eq!(r.enter(&rules, &actor("a", &[]), 0, 1), Entered::NotOpen);
        r = open_round();
        assert_eq!(r.enter(&rules, &actor("a", &[]), 0, 1), Entered::Yes(1.0));
        assert_eq!(r.enter(&rules, &actor("a", &[Role::Sub]), 5, 2), Entered::Duplicate);
        assert_eq!(r.enter(&rules, &actor("Nightbot", &[Role::Mod]), 0, 3), Entered::Rejected("excluded user"));
        assert_eq!(r.enter(&rules, &actor("me", &[Role::Owner]), 0, 3), Entered::Rejected("excluded role"));
        r.close().unwrap();
        assert_eq!(r.enter(&rules, &actor("late", &[]), 0, 4), Entered::NotOpen);
        // reopening keeps entries; a sub-only round rejects plain viewers
        r.open(None, "!join".into(), Role::Sub, None, 5).unwrap();
        assert_eq!(r.entries.len(), 1);
        assert_eq!(r.title, "a drum stick");
        assert_eq!(r.enter(&rules, &actor("viewer", &[Role::Follower]), 0, 6), Entered::Rejected("role too low"));
        assert!(matches!(r.enter(&rules, &actor("subby", &[Role::Sub]), 3, 6), Entered::Yes(_)));
        assert!(r.open(None, "!join".into(), Role::Everyone, None, 7).is_err(), "already open");
    }

    #[test]
    fn keyword_matching() {
        assert!(is_entry("!join", "!join"));
        assert!(is_entry("  !JOIN please ", "!join"));
        assert!(!is_entry("I want to !join", "!join"));
        assert!(!is_entry("!joined", "!join"));
        assert!(!is_entry("", "!join"));
    }

    #[test]
    fn pick_bounds() {
        assert_eq!(pick(&[], 0.5), None);
        assert_eq!(pick(&[0.0, 0.0], 0.5), None);
        assert_eq!(pick(&[1.0, 3.0], 0.0), Some(0));
        assert_eq!(pick(&[1.0, 3.0], 0.2499), Some(0));
        assert_eq!(pick(&[1.0, 3.0], 0.25), Some(1));
        assert_eq!(pick(&[1.0, 0.0, 3.0], 0.9999999), Some(2));
        assert_eq!(pick(&[1.0, 3.0, 0.0], 1.0), Some(1), "r01 = 1 clamps to the last positive weight");
    }

    #[test]
    fn draws_exclude_previous_winners_and_run_out() {
        let rules = Rules::default();
        let mut r = open_round();
        for n in ["a", "b", "c"] {
            r.enter(&rules, &actor(n, &[]), 0, 1);
        }
        let mut rng = rand::rngs::StdRng::seed_from_u64(7);
        let mut seen = std::collections::HashSet::new();
        for _ in 0..3 {
            let w = r.draw(&mut rng, 2).unwrap();
            assert!(seen.insert(w.user.clone()), "{} won twice", w.user);
        }
        assert_eq!(r.phase, Phase::Closed, "drawing closes the round");
        assert_eq!(r.draw(&mut rng, 3).unwrap_err(), "no eligible entries left");
        assert_eq!(r.tickets(), 0.0);
        r.reset();
        assert!(r.draw(&mut rng, 4).is_err());
    }

    #[test]
    fn removal_by_name_or_key() {
        let rules = Rules::default();
        let mut r = open_round();
        r.enter(&rules, &actor("Alice", &[]), 0, 1);
        r.enter(&rules, &actor("bob", &[]), 0, 1);
        assert!(r.remove("@alice").is_some());
        assert!(r.remove("twitch:id-bob").is_some());
        assert!(r.remove("carol").is_none());
        assert!(r.entries.is_empty());
    }

    /// Chi-square goodness of fit: winners are drawn proportionally to weight.
    #[test]
    fn draws_are_weighted_correctly() {
        let rules = Rules::default();
        let mut base = open_round();
        // weights: 1 (viewer), 1 (follower), 2 (sub), 2.6 (sub 12 mo), 5 (sub, capped)
        let people = [("v", vec![], 0), ("f", vec![Role::Follower], 0), ("s", vec![Role::Sub], 0), ("s12", vec![Role::Sub], 12), ("s99", vec![Role::Sub], 99)];
        for (n, roles, months) in &people {
            assert!(matches!(base.enter(&rules, &actor(n, roles), *months, 1), Entered::Yes(_)));
        }
        let weights: Vec<f64> = base.entries.iter().map(|e| e.weight).collect();
        let total: f64 = weights.iter().sum();
        let trials = 200_000usize;
        let mut rng = rand::rngs::StdRng::seed_from_u64(0x5eed);
        let mut counts = vec![0usize; people.len()];
        for _ in 0..trials {
            let mut r = base.clone();
            let w = r.draw(&mut rng, 2).unwrap();
            counts[r.entries.iter().position(|e| e.key == w.key).unwrap()] += 1;
        }
        let chi2: f64 = counts
            .iter()
            .zip(&weights)
            .map(|(&o, w)| {
                let e = trials as f64 * w / total;
                (o as f64 - e).powi(2) / e
            })
            .sum();
        // df = 4; critical value at p = 0.001 is 18.47
        assert!(chi2 < 18.47, "chi² = {chi2:.2}, counts {counts:?}, weights {weights:?}");
        // and each observed share is within 1% absolute of its weight share
        for (&o, w) in counts.iter().zip(&weights) {
            assert!((o as f64 / trials as f64 - w / total).abs() < 0.01);
        }
    }

    /// Second draws are proportional to weight among the remaining entries.
    #[test]
    fn redraw_is_weighted_over_remaining() {
        let rules = Rules::default();
        let mut base = open_round();
        base.enter(&rules, &actor("heavy", &[Role::Sub]), 99, 1); // 5
        base.enter(&rules, &actor("light", &[]), 0, 1); // 1
        base.enter(&rules, &actor("mid", &[Role::Sub]), 0, 1); // 2
        let mut rng = rand::rngs::StdRng::seed_from_u64(42);
        let (mut light, mut mid, mut n) = (0usize, 0usize, 0usize);
        for _ in 0..60_000 {
            let mut r = base.clone();
            if r.draw(&mut rng, 2).unwrap().user != "heavy" {
                continue;
            }
            n += 1;
            match r.draw(&mut rng, 3).unwrap().user.as_str() {
                "light" => light += 1,
                "mid" => mid += 1,
                other => panic!("{other} drawn twice"),
            }
        }
        let share = light as f64 / n as f64;
        assert!((share - 1.0 / 3.0).abs() < 0.015, "light share {share:.4} (n = {n}, mid {mid})");
    }

    #[test]
    fn round_survives_serialization() {
        let rules = Rules::default();
        let mut r = open_round();
        r.enter(&rules, &actor("a", &[Role::Sub]), 3, 1);
        let v = util::to_value(&r);
        let back: Round = serde_json::from_value(serde_json::to_value(&v).unwrap()).unwrap();
        assert_eq!(back, r);
    }
}
