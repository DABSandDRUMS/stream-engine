//! Turns decoded items into engine events, bounded so a spammy room can't flood the hub:
//!
//! * gifts: one event per finished streak (`repeat_end`) or per non-streakable gift; a streak
//!   whose end never arrives is flushed after [`STREAK_IDLE`] (or on disconnect) and a late end
//!   only adds the remainder;
//! * likes: aggregated into at most one `tiktok.like` per [`LIKE_WINDOW`];
//! * everything else: per-kind token buckets (excess is dropped and counted);
//! * duplicates (same message id, e.g. re-delivered after a reconnect) are dropped;
//! * backfill (`is_history` or the sign server's initial batch) updates state but fires no events.

use crate::normalize::{self, GiftItem, Item};
use crate::proto::{CONTROL_ENDED, CONTROL_PAUSED, CONTROL_SUSPENDED, CONTROL_UNPAUSED, ProtoMessageFetchResult};
use se_proto::{Actor, Event, Origin, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::{Duration, Instant};

pub const STREAK_IDLE: Duration = Duration::from_secs(10);
pub const LIKE_WINDOW: Duration = Duration::from_secs(1);
const FLUSHED_KEEP: Duration = Duration::from_secs(60);
const DEDUP_CAP: usize = 4096;
const LIKERS_CAP: usize = 256;
const DROP_REPORT_EVERY: Duration = Duration::from_secs(60);

/// Viewer events are viewer-originated: `Origin::Chat` gives them chat priority/trust (§2.1).
pub const ORIGIN: Origin = Origin::Chat;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Kind {
    Chat,
    Gift,
    Like,
    Follow,
    Share,
    Join,
    Sub,
}

impl Kind {
    pub const ALL: [Kind; 7] = [Kind::Chat, Kind::Gift, Kind::Like, Kind::Follow, Kind::Share, Kind::Join, Kind::Sub];

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Chat => "chat",
            Kind::Gift => "gift",
            Kind::Like => "like",
            Kind::Follow => "follow",
            Kind::Share => "share",
            Kind::Join => "join",
            Kind::Sub => "sub",
        }
    }

    /// (events per second, burst)
    fn limit(self) -> (f64, f64) {
        match self {
            Kind::Chat => (20.0, 40.0),
            Kind::Gift => (20.0, 40.0),
            Kind::Like => (2.0, 4.0),
            Kind::Follow | Kind::Share | Kind::Sub => (10.0, 20.0),
            Kind::Join => (5.0, 10.0),
        }
    }
}

#[derive(Debug, PartialEq)]
pub enum Output {
    Event(Kind, Event),
    Viewers(u64),
    StreamEnded,
    Log(&'static str, String),
    Dropped(Kind),
    DecodeError(String),
}

struct Bucket {
    rate: f64,
    burst: f64,
    tokens: f64,
    last: Option<Instant>,
}

impl Bucket {
    fn new((rate, burst): (f64, f64)) -> Self {
        Bucket { rate, burst, tokens: burst, last: None }
    }

    fn take(&mut self, now: Instant) -> bool {
        if let Some(last) = self.last {
            self.tokens = (self.tokens + now.saturating_duration_since(last).as_secs_f64() * self.rate).min(self.burst);
        }
        self.last = Some(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

struct Seen {
    order: VecDeque<i64>,
    set: HashSet<i64>,
}

impl Seen {
    /// True when `id` is new (0 = no id, always new).
    fn insert(&mut self, id: i64) -> bool {
        if id == 0 {
            return true;
        }
        if !self.set.insert(id) {
            return false;
        }
        self.order.push_back(id);
        if self.order.len() > DEDUP_CAP
            && let Some(old) = self.order.pop_front()
        {
            self.set.remove(&old);
        }
        true
    }
}

type StreakKey = (String, i64, i64);

struct Streak {
    gift: GiftItem,
    /// Count already emitted for this streak (by an idle flush).
    base: i64,
    last: Instant,
}

#[derive(Default)]
struct LikeWindow {
    started: Option<Instant>,
    sum: u64,
    total: u64,
    likers: Vec<(Actor, u64)>,
    more_likers: bool,
}

pub struct Processor {
    seen: Seen,
    streaks: HashMap<StreakKey, Streak>,
    flushed: HashMap<StreakKey, (i64, Instant)>,
    likes: LikeWindow,
    buckets: HashMap<Kind, Bucket>,
    dropped: u64,
    last_drop_report: Option<Instant>,
}

impl Default for Processor {
    fn default() -> Self {
        Self::new()
    }
}

fn event(kind: Kind, actor: Actor, payload: Value) -> Event {
    let payload = payload.with("user", actor.name.clone()).with("user_id", actor.id.clone());
    Event::new(format!("tiktok.{}", kind.as_str()), ORIGIN, payload).with_actor(actor)
}

fn gift_event(g: &GiftItem, count: i64) -> Event {
    let mut p = Value::map()
        .with("gift", g.name.clone())
        .with("gift_id", g.gift_id)
        .with("count", count)
        .with("diamond_count", g.diamond_count)
        .with("diamonds", g.diamond_count * count)
        .with("streak", g.streakable);
    if let Some(to) = &g.to_user {
        p = p.with("to_user", to.clone());
    }
    event(Kind::Gift, g.actor.clone(), p)
}

impl Processor {
    pub fn new() -> Self {
        Processor {
            seen: Seen { order: VecDeque::new(), set: HashSet::new() },
            streaks: HashMap::new(),
            flushed: HashMap::new(),
            likes: LikeWindow::default(),
            buckets: Kind::ALL.iter().map(|k| (*k, Bucket::new(k.limit()))).collect(),
            dropped: 0,
            last_drop_report: None,
        }
    }

    /// Process one batch. `live = false` for the sign server's initial batch (backfill).
    pub fn handle(&mut self, batch: &ProtoMessageFetchResult, live: bool, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        for m in &batch.messages {
            let decoded = match normalize::decode(m) {
                Ok(Some(d)) => d,
                Ok(None) => continue,
                Err(e) => {
                    out.push(Output::DecodeError(format!("{}: {e}", m.method)));
                    continue;
                }
            };
            let (id, item) = decoded;
            if !self.seen.insert(id) {
                continue;
            }
            let backfill = !live || m.is_history;
            self.item(item, backfill, now, &mut out);
        }
        out
    }

    fn item(&mut self, item: Item, backfill: bool, now: Instant, out: &mut Vec<Output>) {
        match item {
            Item::Viewers(n) => out.push(Output::Viewers(n)),
            Item::Control(a) => match a {
                CONTROL_ENDED | CONTROL_SUSPENDED => out.push(Output::StreamEnded),
                CONTROL_PAUSED => out.push(Output::Log("info", "stream paused by the host".into())),
                CONTROL_UNPAUSED => out.push(Output::Log("info", "stream resumed".into())),
                _ => {}
            },
            _ if backfill => {}
            Item::Chat { actor, text, message_id } => {
                let e = event(Kind::Chat, actor, Value::map().with("message", text).with("message_id", message_id));
                self.limited(Kind::Chat, e, now, out);
            }
            Item::Gift(g) => self.gift(g, now, out),
            Item::Like { actor, count, total } => self.like(actor, count, total, now),
            Item::Follow { actor } => self.limited(Kind::Follow, event(Kind::Follow, actor, Value::map()), now, out),
            Item::Share { actor } => self.limited(Kind::Share, event(Kind::Share, actor, Value::map()), now, out),
            Item::Join { actor } => self.limited(Kind::Join, event(Kind::Join, actor, Value::map()), now, out),
            Item::Sub { actor, months } => self.limited(Kind::Sub, event(Kind::Sub, actor, Value::map().with("months", months)), now, out),
        }
    }

    fn limited(&mut self, kind: Kind, e: Event, now: Instant, out: &mut Vec<Output>) {
        let ok = self.buckets.get_mut(&kind).is_some_and(|b| b.take(now));
        if ok {
            out.push(Output::Event(kind, e));
        } else {
            self.dropped += 1;
            out.push(Output::Dropped(kind));
        }
    }

    fn gift(&mut self, g: GiftItem, now: Instant, out: &mut Vec<Output>) {
        if !g.streakable {
            let count = g.repeat_count;
            self.limited(Kind::Gift, gift_event(&g, count), now, out);
            return;
        }
        let key = (g.actor.id.clone(), g.gift_id, g.group_id);
        let base = self.flushed.get(&key).map_or(0, |(c, _)| *c);
        if g.repeat_end {
            self.streaks.remove(&key);
            self.flushed.remove(&key);
            let count = g.repeat_count - base;
            if count > 0 {
                self.limited(Kind::Gift, gift_event(&g, count), now, out);
            }
        } else {
            let base = self.streaks.get(&key).map_or(base, |s| s.base);
            self.streaks.insert(key, Streak { gift: g, base, last: now });
        }
    }

    fn like(&mut self, actor: Actor, count: u64, total: u64, now: Instant) {
        let w = &mut self.likes;
        w.started.get_or_insert(now);
        w.sum += count;
        w.total = w.total.max(total);
        if let Some(e) = w.likers.iter_mut().find(|(a, _)| a.id == actor.id) {
            e.1 += count;
        } else if w.likers.len() < LIKERS_CAP {
            w.likers.push((actor, count));
        } else {
            w.more_likers = true;
        }
    }

    fn flush_likes(&mut self, now: Instant, out: &mut Vec<Output>) {
        let w = std::mem::take(&mut self.likes);
        if w.sum == 0 {
            return;
        }
        let mut top: Option<&(Actor, u64)> = None;
        for l in &w.likers {
            if top.is_none_or(|t| l.1 > t.1) {
                top = Some(l);
            }
        }
        let Some((actor, _)) = top.cloned() else { return };
        let likers = w.likers.len() as u64 + u64::from(w.more_likers);
        let p = Value::map().with("count", w.sum).with("total", w.total).with("likers", likers);
        self.limited(Kind::Like, event(Kind::Like, actor, p), now, out);
    }

    /// Periodic housekeeping: close like windows, flush idle streaks, report drops.
    pub fn tick(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        if self.likes.started.is_some_and(|s| now.saturating_duration_since(s) >= LIKE_WINDOW) {
            self.flush_likes(now, &mut out);
        }
        let idle: Vec<StreakKey> = self.streaks.iter().filter(|(_, s)| now.saturating_duration_since(s.last) >= STREAK_IDLE).map(|(k, _)| k.clone()).collect();
        for k in idle {
            self.flush_streak(k, now, &mut out);
        }
        self.flushed.retain(|_, (_, at)| now.saturating_duration_since(*at) < FLUSHED_KEEP);
        if self.dropped > 0 && self.last_drop_report.is_none_or(|t| now.saturating_duration_since(t) >= DROP_REPORT_EVERY) {
            out.push(Output::Log("warn", format!("rate limit: dropped {} TikTok events (spammy room)", self.dropped)));
            self.dropped = 0;
            self.last_drop_report = Some(now);
        }
        out
    }

    fn flush_streak(&mut self, key: StreakKey, now: Instant, out: &mut Vec<Output>) {
        let Some(s) = self.streaks.remove(&key) else { return };
        let count = s.gift.repeat_count - s.base;
        if count > 0 {
            self.limited(Kind::Gift, gift_event(&s.gift, count), now, out);
        }
        self.flushed.insert(key, (s.gift.repeat_count.max(s.base), now));
    }

    /// Emit everything pending (disconnect): open like window and unfinished streaks.
    pub fn flush_all(&mut self, now: Instant) -> Vec<Output> {
        let mut out = Vec::new();
        self.flush_likes(now, &mut out);
        let keys: Vec<StreakKey> = self.streaks.keys().cloned().collect();
        for k in keys {
            self.flush_streak(k, now, &mut out);
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::*;
    use prost::Message;

    fn user(id: i64, nick: &str) -> Option<User> {
        Some(User { id, nickname: nick.into(), ..Default::default() })
    }

    fn msg<M: Message>(method: &str, id: i64, m: &M) -> BaseProtoMessage {
        BaseProtoMessage { method: method.into(), payload: m.encode_to_vec(), msg_id: id, ..Default::default() }
    }

    fn gift(id: i64, uid: i64, repeat: i32, end: bool, streakable: bool) -> BaseProtoMessage {
        let g = WebcastGiftMessage {
            gift_id: 5655,
            repeat_count: repeat,
            repeat_end: i32::from(end),
            group_id: 1,
            user: user(uid, "gifter"),
            gift: Some(Gift { id: 5655, r#type: if streakable { 1 } else { 2 }, diamond_count: 1, name: "Rose".into(), ..Default::default() }),
            ..Default::default()
        };
        msg("WebcastGiftMessage", id, &g)
    }

    fn batch(messages: Vec<BaseProtoMessage>) -> ProtoMessageFetchResult {
        ProtoMessageFetchResult { messages, ..Default::default() }
    }

    fn gifts(out: &[Output]) -> Vec<(i64, i64)> {
        out.iter()
            .filter_map(|o| match o {
                Output::Event(Kind::Gift, e) => Some((e.payload.get_path("count")?.as_i64()?, e.payload.get_path("diamonds")?.as_i64()?)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn streak_emits_once_at_end_with_total_count() {
        let mut p = Processor::new();
        let t = Instant::now();
        let out = p.handle(&batch(vec![gift(1, 7, 1, false, true), gift(2, 7, 2, false, true), gift(3, 7, 3, false, true)]), true, t);
        assert!(gifts(&out).is_empty());
        let out = p.handle(&batch(vec![gift(4, 7, 5, true, true)]), true, t + Duration::from_secs(2));
        assert_eq!(gifts(&out), vec![(5, 5)]);
        assert!(gifts(&p.tick(t + Duration::from_secs(30))).is_empty());
    }

    #[test]
    fn non_streakable_gift_emits_immediately() {
        let mut p = Processor::new();
        let out = p.handle(&batch(vec![gift(1, 7, 1, false, false)]), true, Instant::now());
        assert_eq!(gifts(&out), vec![(1, 1)]);
    }

    #[test]
    fn idle_streak_flushes_and_late_end_adds_only_the_remainder() {
        let mut p = Processor::new();
        let t = Instant::now();
        p.handle(&batch(vec![gift(1, 7, 3, false, true)]), true, t);
        assert!(gifts(&p.tick(t + STREAK_IDLE - Duration::from_millis(1))).is_empty());
        assert_eq!(gifts(&p.tick(t + STREAK_IDLE)), vec![(3, 3)]);
        // the streak continues after the flush and then ends at 8 → 5 more, not 8
        p.handle(&batch(vec![gift(2, 7, 6, false, true)]), true, t + Duration::from_secs(11));
        let out = p.handle(&batch(vec![gift(3, 7, 8, true, true)]), true, t + Duration::from_secs(12));
        assert_eq!(gifts(&out), vec![(5, 5)]);
        // a late end with no new taps adds nothing
        p.handle(&batch(vec![gift(4, 8, 2, false, true)]), true, t);
        assert_eq!(gifts(&p.tick(t + STREAK_IDLE)), vec![(2, 2)]);
        assert!(gifts(&p.handle(&batch(vec![gift(5, 8, 2, true, true)]), true, t + STREAK_IDLE)).is_empty());
    }

    #[test]
    fn disconnect_flushes_unfinished_streaks() {
        let mut p = Processor::new();
        let t = Instant::now();
        p.handle(&batch(vec![gift(1, 7, 4, false, true), gift(2, 9, 2, false, true)]), true, t);
        let mut got = gifts(&p.flush_all(t));
        got.sort();
        assert_eq!(got, vec![(2, 2), (4, 4)]);
    }

    #[test]
    fn duplicate_message_ids_are_dropped() {
        let mut p = Processor::new();
        let t = Instant::now();
        let chat = msg("WebcastChatMessage", 77, &WebcastChatMessage { user: user(1, "a"), content: "hi".into(), ..Default::default() });
        assert_eq!(p.handle(&batch(vec![chat.clone(), chat.clone()]), true, t).len(), 1);
        assert!(p.handle(&batch(vec![chat]), true, t).is_empty());
    }

    #[test]
    fn likes_aggregate_into_one_event_per_window() {
        let mut p = Processor::new();
        let t = Instant::now();
        let like = |id: i64, uid: i64, n: i32, total: i64| {
            msg("WebcastLikeMessage", id, &WebcastLikeMessage { count: n, total, user: user(uid, &format!("u{uid}")), ..Default::default() })
        };
        let mut all = Vec::new();
        for i in 0..200 {
            all.extend(p.handle(&batch(vec![like(i + 1, i % 3, 5, 1000 + i * 5)]), true, t + Duration::from_millis(i as u64 * 4)));
        }
        assert!(all.iter().all(|o| !matches!(o, Output::Event(..))));
        let out = p.tick(t + LIKE_WINDOW);
        let likes: Vec<&Event> = out.iter().filter_map(|o| if let Output::Event(Kind::Like, e) = o { Some(e) } else { None }).collect();
        assert_eq!(likes.len(), 1);
        let e = likes[0];
        assert_eq!(e.payload.get_path("count").and_then(Value::as_i64), Some(1000));
        assert_eq!(e.payload.get_path("total").and_then(Value::as_i64), Some(1000 + 199 * 5));
        assert_eq!(e.payload.get_path("likers").and_then(Value::as_i64), Some(3));
        // user 0 liked 67 times (most), so the event is attributed to them
        assert_eq!(e.actor.as_ref().map(|a| a.id.as_str()), Some("0"));
        assert!(p.tick(t + LIKE_WINDOW * 3).is_empty());
    }

    #[test]
    fn chat_flood_is_capped_and_reported() {
        let mut p = Processor::new();
        let t = Instant::now();
        let msgs: Vec<_> =
            (1..=500).map(|i| msg("WebcastChatMessage", i, &WebcastChatMessage { user: user(i, "x"), content: "spam".into(), ..Default::default() })).collect();
        let out = p.handle(&batch(msgs), true, t);
        let events = out.iter().filter(|o| matches!(o, Output::Event(..))).count();
        let dropped = out.iter().filter(|o| matches!(o, Output::Dropped(Kind::Chat))).count();
        assert_eq!((events, dropped), (40, 460));
        let report = p.tick(t);
        assert!(report.iter().any(|o| matches!(o, Output::Log("warn", m) if m.contains("460"))));
        // one second later the bucket has refilled by the rate (20)
        let more: Vec<_> = (501..=600)
            .map(|i| msg("WebcastChatMessage", i, &WebcastChatMessage { user: user(i, "x"), content: "spam".into(), ..Default::default() }))
            .collect();
        let out = p.handle(&batch(more), true, t + Duration::from_secs(1));
        assert_eq!(out.iter().filter(|o| matches!(o, Output::Event(..))).count(), 20);
    }

    #[test]
    fn backfill_updates_state_but_fires_no_events() {
        let mut p = Processor::new();
        let t = Instant::now();
        let chat = msg("WebcastChatMessage", 1, &WebcastChatMessage { user: user(1, "a"), content: "old".into(), ..Default::default() });
        let seq = msg("WebcastRoomUserSeqMessage", 2, &WebcastRoomUserSeqMessage { total: 42, ..Default::default() });
        assert_eq!(p.handle(&batch(vec![chat.clone(), seq]), false, t), vec![Output::Viewers(42)]);
        // the same chat re-delivered over the socket is a duplicate, and history-flagged ones are skipped
        assert!(p.handle(&batch(vec![chat]), true, t).is_empty());
        let mut hist = msg("WebcastChatMessage", 3, &WebcastChatMessage { user: user(1, "a"), content: "older".into(), ..Default::default() });
        hist.is_history = true;
        assert!(p.handle(&batch(vec![hist]), true, t).is_empty());
    }

    #[test]
    fn stream_end_and_suspend_end_the_stream() {
        let mut p = Processor::new();
        for (i, action) in [(1, CONTROL_ENDED), (2, CONTROL_SUSPENDED)] {
            let m = msg("WebcastControlMessage", i, &WebcastControlMessage { action, ..Default::default() });
            assert_eq!(p.handle(&batch(vec![m]), true, Instant::now()), vec![Output::StreamEnded]);
        }
    }
}
