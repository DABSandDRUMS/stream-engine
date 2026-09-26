//! Gift-bomb combining (§14.2): `twitch.gift {count, gift_id, user}` plus the gifted
//! `twitch.sub {is_gift, gift_id, gifter}` events that belong to it become ONE alert.
//!
//! EventSub doesn't order the gift event before its subs, so gifted subs that arrive first
//! are held for the gift window; if their gift event never comes they are flushed as a
//! combined alert (or a single gift-sub alert when there was only one).

use se_proto::{Event, Value};

fn s(e: &Event, k: &str) -> Option<String> {
    e.payload.get_path(k).and_then(Value::as_str).filter(|v| !v.is_empty()).map(String::from)
}

/// Is this a gifted sub (to be combined)?
pub fn is_gift_sub(e: &Event) -> bool {
    (e.ty == "twitch.sub" || e.ty == "twitch.resub") && e.payload.get_path("is_gift").is_some_and(Value::truthy)
}

fn recipient(e: &Event) -> String {
    s(e, "user").or_else(|| e.actor.as_ref().map(|a| a.name.clone())).unwrap_or_default()
}

#[derive(Debug)]
struct Bomb {
    gift_id: Option<String>,
    gifter: Option<String>,
    /// From the gift event; `None` while only subs have been seen.
    expected: Option<u64>,
    absorbed: u64,
    alert: Option<u64>,
    held: Vec<Event>,
    last_ms: u64,
}

impl Bomb {
    fn matches(&self, gift_id: Option<&str>, gifter: Option<&str>) -> bool {
        match (gift_id, self.gift_id.as_deref()) {
            (Some(a), Some(b)) => a == b,
            _ => matches!((gifter, self.gifter.as_deref()), (Some(a), Some(b)) if a.eq_ignore_ascii_case(b)),
        }
    }
}

/// What to do with an event.
#[derive(Debug, PartialEq)]
pub enum Gift {
    /// Not part of a gift bomb: route normally.
    Pass,
    /// A `twitch.gift`: create its combined alert now; `recipients` arrived before it.
    Announce { recipients: Vec<String> },
    /// A gifted sub absorbed into the bomb whose alert is `alert` (add the recipient).
    Absorb { alert: Option<u64>, recipient: String },
    /// Held until its gift event arrives (or the window closes).
    Held,
}

/// Held gifted subs whose gift event never arrived.
#[derive(Debug, PartialEq)]
pub struct Orphans {
    pub gifter: Option<String>,
    pub gift_id: Option<String>,
    pub subs: Vec<Event>,
}

pub struct Combiner {
    pub window_ms: u64,
    bombs: Vec<Bomb>,
}

impl Combiner {
    pub fn new(window_ms: u64) -> Combiner {
        Combiner { window_ms, bombs: Vec::new() }
    }

    /// Classify an event. Call [`set_alert`](Self::set_alert) after creating an announced alert.
    pub fn on_event(&mut self, e: &Event, now: u64) -> Gift {
        if e.ty == "twitch.gift" {
            let gift_id = s(e, "gift_id");
            let gifter = s(e, "user").or_else(|| e.actor.as_ref().map(|a| a.name.clone()));
            let count = e.payload.get_path("count").and_then(Value::as_i64).unwrap_or(1).max(1) as u64;
            // subs that raced ahead of their gift event
            let early = self.bombs.iter().position(|b| b.expected.is_none() && b.matches(gift_id.as_deref(), gifter.as_deref()));
            let recipients = match early {
                Some(i) => {
                    let b = &mut self.bombs[i];
                    b.expected = Some(count);
                    b.gift_id = gift_id.or(b.gift_id.take());
                    b.gifter = gifter.or(b.gifter.take());
                    b.last_ms = now;
                    b.absorbed = b.held.len() as u64;
                    b.held.drain(..).map(|h| recipient(&h)).collect()
                }
                None => {
                    self.bombs.push(Bomb { gift_id, gifter, expected: Some(count), absorbed: 0, alert: None, held: Vec::new(), last_ms: now });
                    Vec::new()
                }
            };
            return Gift::Announce { recipients };
        }
        if !is_gift_sub(e) {
            return Gift::Pass;
        }
        let gift_id = s(e, "gift_id");
        let gifter = s(e, "gifter");
        let found = self
            .bombs
            .iter()
            .position(|b| b.matches(gift_id.as_deref(), gifter.as_deref()))
            // no ids at all: the oldest announced bomb that still expects subs
            .or_else(|| {
                (gift_id.is_none() && gifter.is_none())
                    .then(|| self.bombs.iter().position(|b| b.expected.is_some_and(|x| b.absorbed < x) && b.gift_id.is_none()))
                    .flatten()
            });
        match found {
            Some(i) => {
                let b = &mut self.bombs[i];
                b.last_ms = now;
                if b.expected.is_some() {
                    b.absorbed += 1;
                    Gift::Absorb { alert: b.alert, recipient: recipient(e) }
                } else {
                    b.held.push(e.clone());
                    Gift::Held
                }
            }
            None => {
                self.bombs.push(Bomb { gift_id, gifter, expected: None, absorbed: 0, alert: None, held: vec![e.clone()], last_ms: now });
                Gift::Held
            }
        }
    }

    /// Remember the alert created for the most recent announcement of this gift.
    pub fn set_alert(&mut self, e: &Event, alert: u64) {
        let gift_id = s(e, "gift_id");
        let gifter = s(e, "user").or_else(|| e.actor.as_ref().map(|a| a.name.clone()));
        if let Some(b) = self
            .bombs
            .iter_mut()
            .rev()
            .find(|b| b.expected.is_some() && b.matches(gift_id.as_deref(), gifter.as_deref()) || (gift_id.is_none() && gifter.is_none()))
        {
            b.alert = Some(alert);
        }
    }

    /// Expire quiet bombs; returns held subs whose gift event never came.
    pub fn tick(&mut self, now: u64) -> Vec<Orphans> {
        let mut out = Vec::new();
        let window = self.window_ms;
        self.bombs.retain_mut(|b| {
            if now.saturating_sub(b.last_ms) < window {
                return true;
            }
            if b.expected.is_none() && !b.held.is_empty() {
                out.push(Orphans { gifter: b.gifter.clone(), gift_id: b.gift_id.clone(), subs: std::mem::take(&mut b.held) });
            }
            false
        });
        out
    }

    pub fn open(&self) -> usize {
        self.bombs.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_proto::Origin;

    fn gift(id: Option<&str>, gifter: &str, count: i64) -> Event {
        let mut p = Value::map().with("count", count).with("tier", 1).with("user", gifter);
        if let Some(id) = id {
            p = p.with("gift_id", id);
        }
        Event::new("twitch.gift", Origin::Twitch, p)
    }

    fn sub(id: Option<&str>, gifter: Option<&str>, user: &str) -> Event {
        let mut p = Value::map().with("tier", 1).with("months", 1).with("is_gift", true).with("user", user);
        if let Some(id) = id {
            p = p.with("gift_id", id);
        }
        if let Some(g) = gifter {
            p = p.with("gifter", g);
        }
        Event::new("twitch.sub", Origin::Twitch, p)
    }

    #[test]
    fn gift_then_subs_combine() {
        let mut c = Combiner::new(3000);
        let g = gift(Some("g1"), "santa", 50);
        assert_eq!(c.on_event(&g, 0), Gift::Announce { recipients: vec![] });
        c.set_alert(&g, 7);
        for i in 0..50 {
            assert_eq!(c.on_event(&sub(Some("g1"), Some("santa"), &format!("r{i}")), 10), Gift::Absorb { alert: Some(7), recipient: format!("r{i}") });
        }
        assert!(c.tick(2000).is_empty());
        assert_eq!(c.open(), 1);
        assert!(c.tick(3010).is_empty(), "announced bombs expire quietly");
        assert_eq!(c.open(), 0);
        // an ordinary sub passes through
        let mut plain = sub(None, None, "x");
        plain.payload = plain.payload.with("is_gift", false);
        assert_eq!(c.on_event(&plain, 0), Gift::Pass);
    }

    #[test]
    fn subs_before_their_gift_event_are_held_then_merged() {
        let mut c = Combiner::new(3000);
        assert_eq!(c.on_event(&sub(Some("g2"), None, "a"), 0), Gift::Held);
        assert_eq!(c.on_event(&sub(Some("g2"), None, "b"), 5), Gift::Held);
        let g = gift(Some("g2"), "elf", 3);
        assert_eq!(c.on_event(&g, 10), Gift::Announce { recipients: vec!["a".into(), "b".into()] });
        c.set_alert(&g, 9);
        assert_eq!(c.on_event(&sub(Some("g2"), None, "c"), 11), Gift::Absorb { alert: Some(9), recipient: "c".into() });
        assert!(c.tick(10_000).is_empty());
    }

    #[test]
    fn orphans_flush_after_the_window() {
        let mut c = Combiner::new(3000);
        c.on_event(&sub(None, Some("ghost"), "a"), 0);
        c.on_event(&sub(None, Some("GHOST"), "b"), 1000);
        c.on_event(&sub(None, Some("other"), "z"), 1000);
        assert!(c.tick(3999).is_empty(), "window slides with activity");
        let o = c.tick(4000);
        assert_eq!(o.len(), 2);
        let ghost = o.iter().find(|x| x.gifter.as_deref() == Some("ghost")).unwrap();
        assert_eq!(ghost.subs.len(), 2);
    }

    #[test]
    fn subs_without_ids_join_the_open_bomb() {
        let mut c = Combiner::new(3000);
        let g = gift(None, "anon", 2);
        c.on_event(&g, 0);
        c.set_alert(&g, 1);
        assert_eq!(c.on_event(&sub(None, None, "a"), 1), Gift::Absorb { alert: Some(1), recipient: "a".into() });
        assert_eq!(c.on_event(&sub(None, None, "b"), 2), Gift::Absorb { alert: Some(1), recipient: "b".into() });
        // bomb full: a third anonymous gifted sub is its own thing
        assert_eq!(c.on_event(&sub(None, None, "c"), 3), Gift::Held);
    }
}
