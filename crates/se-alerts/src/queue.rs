//! The alert queue (§14.2): priorities, minimum spacing, maximum on-screen time,
//! interrupting, the veto window for alerts with viewer text (§12.1), pausing in
//! `ad_break`/`brb` with replay afterwards, and deletion sync.
//!
//! A deterministic state machine driven by `now_ms`; it returns [`Effect`]s for the driver.

use crate::config::{OnInterrupt, QueuePolicy};
use se_proto::{Actor, Id, Value};
use std::collections::VecDeque;

#[derive(Clone, Debug, PartialEq)]
pub struct Tts {
    pub text: String,
    pub voice: Option<String>,
}

/// One alert instance.
#[derive(Clone, Debug, PartialEq)]
pub struct Alert {
    pub id: u64,
    /// Alert definition name (`cheer`, `gift`).
    pub kind: String,
    pub variation: String,
    /// Event that produced it (`twitch.cheer`).
    pub event_type: String,
    pub title: String,
    /// Viewer text (already content-filtered); empty when none.
    pub message: String,
    pub user: String,
    pub user_id: String,
    pub message_id: Option<String>,
    pub amount: f64,
    pub currency: Option<String>,
    pub tier: Option<i64>,
    pub sound: Option<String>,
    pub image: Option<String>,
    pub duration_ms: u64,
    pub priority: i64,
    /// May interrupt smaller alerts.
    pub interrupt: bool,
    pub tts: Option<Tts>,
    /// Commands run when it shows (already templated, as tokens).
    pub cmds: Vec<Vec<String>>,
    pub actor: Option<Actor>,
    pub cause: Option<Id>,
    /// Gift bomb recipients.
    pub recipients: Vec<String>,
    /// Viewer text still needs a veto window.
    pub needs_veto: bool,
    pub sim: bool,
}

impl Alert {
    /// Payload of `alert.show` / `alert.update` and the `alerts.current` state.
    pub fn to_value(&self) -> Value {
        Value::map()
            .with("id", self.id)
            .with("kind", self.kind.clone())
            .with("variation", self.variation.clone())
            .with("event", self.event_type.clone())
            .with("title", self.title.clone())
            .with("message", self.message.clone())
            .with("user", self.user.clone())
            .with("amount", self.amount)
            .with("currency", self.currency.clone().map(Value::Str).unwrap_or_default())
            .with("tier", self.tier.map(Value::Int).unwrap_or_default())
            .with("sound", self.sound.clone().map(Value::Str).unwrap_or_default())
            .with("image", self.image.clone().map(Value::Str).unwrap_or_default())
            .with("duration", self.duration_ms)
            .with("priority", self.priority)
            .with("recipients", self.recipients.clone())
            .with("count", if self.recipients.is_empty() { Value::Null } else { Value::Int(self.recipients.len() as i64) })
            .with("sim", self.sim)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HideReason {
    Done,
    Skipped,
    Vetoed,
    Interrupted,
    Paused,
    Purged,
}

impl HideReason {
    pub fn as_str(self) -> &'static str {
        match self {
            HideReason::Done => "done",
            HideReason::Skipped => "skipped",
            HideReason::Vetoed => "vetoed",
            HideReason::Interrupted => "interrupted",
            HideReason::Paused => "paused",
            HideReason::Purged => "purged",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    Show(Alert),
    Hide {
        alert: Alert,
        reason: HideReason,
    },
    /// The on-screen alert changed (text removed, more gift recipients).
    Update(Alert),
    /// Removed from the queue without showing.
    Dropped {
        alert: Alert,
        reason: &'static str,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PauseReason {
    Mode,
    Manual,
    Panic,
}

impl PauseReason {
    pub fn as_str(self) -> &'static str {
        match self {
            PauseReason::Mode => "mode",
            PauseReason::Manual => "manual",
            PauseReason::Panic => "panic",
        }
    }
}

#[derive(Clone, Debug)]
struct Pending {
    alert: Alert,
    seq: u64,
    veto_until: Option<u64>,
}

#[derive(Clone, Debug)]
struct Showing {
    alert: Alert,
    seq: u64,
    hide_at: u64,
}

#[derive(Clone, Debug)]
pub struct HistoryItem {
    pub alert: Alert,
    pub shown_ms: u64,
}

const HISTORY: usize = 50;

pub struct AlertQueue {
    pub policy: QueuePolicy,
    pending: Vec<Pending>,
    current: Option<Showing>,
    pauses: [bool; 3],
    next_ok: u64,
    seq: u64,
    next_id: u64,
    history: VecDeque<HistoryItem>,
}

impl AlertQueue {
    pub fn new(policy: QueuePolicy) -> AlertQueue {
        AlertQueue { policy, pending: Vec::new(), current: None, pauses: [false; 3], next_ok: 0, seq: 0, next_id: 1, history: VecDeque::new() }
    }

    pub fn alloc_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    pub fn paused(&self) -> Option<PauseReason> {
        [PauseReason::Panic, PauseReason::Manual, PauseReason::Mode].into_iter().find(|r| self.pauses[*r as usize])
    }

    pub fn current(&self) -> Option<&Alert> {
        self.current.as_ref().map(|s| &s.alert)
    }

    pub fn current_remaining(&self, now: u64) -> Option<u64> {
        self.current.as_ref().map(|s| s.hide_at.saturating_sub(now))
    }

    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    /// Queued alerts in show order: `(alert, veto remaining ms)`.
    pub fn queued(&self, now: u64) -> Vec<(&Alert, u64)> {
        let mut v: Vec<&Pending> = self.pending.iter().collect();
        v.sort_by_key(|p| (std::cmp::Reverse(p.alert.priority), p.seq));
        v.into_iter().map(|p| (&p.alert, p.veto_until.map(|t| t.saturating_sub(now)).unwrap_or(0))).collect()
    }

    /// Alerts inside their veto window: `(id, remaining ms)`.
    pub fn veto_pending(&self, now: u64) -> Vec<(u64, u64)> {
        self.pending.iter().filter_map(|p| p.veto_until.filter(|t| *t > now).map(|t| (p.alert.id, t - now))).collect()
    }

    pub fn history(&self) -> impl Iterator<Item = &HistoryItem> {
        self.history.iter().rev()
    }

    fn enqueue(&mut self, alert: Alert, seq: u64, veto_until: Option<u64>) {
        self.pending.push(Pending { alert, seq, veto_until });
    }

    /// Add an alert (its `id` must come from [`alloc_id`](Self::alloc_id)).
    pub fn push(&mut self, alert: Alert, now: u64) -> Vec<Effect> {
        let veto_until = (alert.needs_veto && self.policy.veto_window.ms() > 0).then(|| now + self.policy.veto_window.ms());
        self.seq += 1;
        let seq = self.seq;
        self.enqueue(alert, seq, veto_until);
        let mut fx = Vec::new();
        while self.pending.len() > self.policy.max_queue.max(1) {
            // lowest priority, newest first
            let i = self.pending.iter().enumerate().min_by_key(|(_, p)| (p.alert.priority, std::cmp::Reverse(p.seq))).map(|(i, _)| i).unwrap();
            let p = self.pending.remove(i);
            fx.push(Effect::Dropped { alert: p.alert, reason: "queue full" });
        }
        fx.extend(self.tick(now));
        fx
    }

    fn best_ready(&self, now: u64) -> Option<usize> {
        self.pending
            .iter()
            .enumerate()
            .filter(|(_, p)| p.veto_until.is_none_or(|t| now >= t))
            .max_by_key(|(_, p)| (p.alert.priority, std::cmp::Reverse(p.seq)))
            .map(|(i, _)| i)
    }

    fn show(&mut self, i: usize, now: u64, fx: &mut Vec<Effect>) {
        let p = self.pending.remove(i);
        let dur = p.alert.duration_ms.clamp(500, self.policy.max_on_screen.ms().max(500));
        self.history.push_back(HistoryItem { alert: p.alert.clone(), shown_ms: now });
        if self.history.len() > HISTORY {
            self.history.pop_front();
        }
        fx.push(Effect::Show(p.alert.clone()));
        self.current = Some(Showing { alert: p.alert, seq: p.seq, hide_at: now + dur });
    }

    fn hide(&mut self, reason: HideReason, now: u64, fx: &mut Vec<Effect>) -> Option<Showing> {
        let s = self.current.take()?;
        fx.push(Effect::Hide { alert: s.alert.clone(), reason });
        self.next_ok = now + self.policy.min_spacing.ms();
        Some(s)
    }

    /// Advance time: end the current alert, interrupt, start the next.
    pub fn tick(&mut self, now: u64) -> Vec<Effect> {
        let mut fx = Vec::new();
        if self.current.as_ref().is_some_and(|s| now >= s.hide_at) {
            self.hide(HideReason::Done, now, &mut fx);
        }
        if self.paused().is_some() {
            return fx;
        }
        if self.policy.interrupt
            && let Some(cur) = &self.current
            && let Some(i) = self.best_ready(now)
            && self.pending[i].alert.interrupt
            && self.pending[i].alert.priority >= cur.alert.priority + self.policy.interrupt_margin
        {
            if let Some(s) = self.hide(HideReason::Interrupted, now, &mut fx)
                && self.policy.on_interrupt == OnInterrupt::Requeue
            {
                self.enqueue(s.alert, s.seq, None);
            }
            // the interrupting alert goes up at once; spacing applies after it
            let i = self.best_ready(now).unwrap_or(i);
            self.show(i, now, &mut fx);
            return fx;
        }
        if self.current.is_none()
            && now >= self.next_ok
            && let Some(i) = self.best_ready(now)
        {
            self.show(i, now, &mut fx);
        }
        fx
    }

    /// Hold or release the queue. Pausing takes the current alert off screen and puts it back
    /// at the front, so it replays in full afterwards.
    pub fn set_pause(&mut self, reason: PauseReason, on: bool, now: u64) -> Vec<Effect> {
        let was = self.paused().is_some();
        self.pauses[reason as usize] = on;
        let mut fx = Vec::new();
        if !was && self.paused().is_some() {
            if let Some(s) = self.hide(HideReason::Paused, now, &mut fx) {
                self.enqueue(s.alert, s.seq, None);
            }
        } else if was && self.paused().is_none() {
            self.next_ok = now;
            fx.extend(self.tick(now));
        }
        fx
    }

    /// Kill an alert: queued ones are dropped, the on-screen one is hidden.
    pub fn veto(&mut self, id: u64, now: u64) -> Vec<Effect> {
        let mut fx = Vec::new();
        if let Some(i) = self.pending.iter().position(|p| p.alert.id == id) {
            let p = self.pending.remove(i);
            fx.push(Effect::Dropped { alert: p.alert, reason: "vetoed" });
        } else if self.current.as_ref().is_some_and(|s| s.alert.id == id) {
            self.hide(HideReason::Vetoed, now, &mut fx);
            fx.extend(self.tick(now));
        }
        fx
    }

    /// End an alert's veto window now. Returns false when it isn't queued.
    pub fn approve(&mut self, id: u64, now: u64) -> (bool, Vec<Effect>) {
        match self.pending.iter_mut().find(|p| p.alert.id == id) {
            Some(p) => {
                p.veto_until = None;
                (true, self.tick(now))
            }
            None => (false, Vec::new()),
        }
    }

    /// Take the current alert off screen.
    pub fn skip(&mut self, now: u64) -> Vec<Effect> {
        let mut fx = Vec::new();
        self.hide(HideReason::Skipped, now, &mut fx);
        fx.extend(self.tick(now));
        fx
    }

    /// Drop everything queued (the on-screen alert finishes).
    pub fn clear(&mut self) -> Vec<Effect> {
        self.pending.drain(..).map(|p| Effect::Dropped { alert: p.alert, reason: "cleared" }).collect()
    }

    /// Show a past alert again (new id).
    pub fn replay(&mut self, id: u64, now: u64) -> Option<Vec<Effect>> {
        let mut a = self.history.iter().find(|h| h.alert.id == id)?.alert.clone();
        a.id = self.alloc_id();
        a.needs_veto = false;
        Some(self.push(a, now))
    }

    /// Deletion sync: a banned/timed-out user's alerts disappear.
    pub fn purge_user(&mut self, user_id: &str, user: &str, now: u64) -> Vec<Effect> {
        let hit = |a: &Alert| (!user_id.is_empty() && a.user_id == user_id) || (!user.is_empty() && a.user.eq_ignore_ascii_case(user));
        let mut fx = Vec::new();
        let (gone, keep): (Vec<Pending>, Vec<Pending>) = std::mem::take(&mut self.pending).into_iter().partition(|p| hit(&p.alert));
        self.pending = keep;
        fx.extend(gone.into_iter().map(|p| Effect::Dropped { alert: p.alert, reason: "purged" }));
        if self.current.as_ref().is_some_and(|s| hit(&s.alert)) {
            self.hide(HideReason::Purged, now, &mut fx);
            fx.extend(self.tick(now));
        }
        fx
    }

    /// Deletion sync: a deleted chat message's text is removed from its alert.
    pub fn strip_message(&mut self, message_id: &str) -> Vec<Effect> {
        let mut fx = Vec::new();
        for p in &mut self.pending {
            if p.alert.message_id.as_deref() == Some(message_id) {
                p.alert.message.clear();
                p.alert.tts = None;
                p.alert.needs_veto = false;
                p.veto_until = None;
            }
        }
        if let Some(s) = &mut self.current
            && s.alert.message_id.as_deref() == Some(message_id)
        {
            s.alert.message.clear();
            s.alert.tts = None;
            fx.push(Effect::Update(s.alert.clone()));
        }
        fx
    }

    /// Change a queued or on-screen alert in place (gift recipients).
    pub fn update(&mut self, id: u64, f: impl FnOnce(&mut Alert)) -> Vec<Effect> {
        if let Some(p) = self.pending.iter_mut().find(|p| p.alert.id == id) {
            f(&mut p.alert);
            return Vec::new();
        }
        if let Some(s) = &mut self.current
            && s.alert.id == id
        {
            f(&mut s.alert);
            return vec![Effect::Update(s.alert.clone())];
        }
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_core::config::Dur;

    fn policy() -> QueuePolicy {
        QueuePolicy { min_spacing: Dur(1000), max_on_screen: Dur(10_000), veto_window: Dur(3000), interrupt_margin: 30, ..Default::default() }
    }

    fn alert(q: &mut AlertQueue, kind: &str, prio: i64, dur: u64) -> Alert {
        Alert {
            id: q.alloc_id(),
            kind: kind.into(),
            variation: String::new(),
            event_type: format!("twitch.{kind}"),
            title: kind.into(),
            message: String::new(),
            user: format!("{kind}_user"),
            user_id: format!("id_{kind}"),
            message_id: None,
            amount: 0.0,
            currency: None,
            tier: None,
            sound: None,
            image: None,
            duration_ms: dur,
            priority: prio,
            interrupt: true,
            tts: None,
            cmds: vec![],
            actor: None,
            cause: None,
            recipients: vec![],
            needs_veto: false,
            sim: false,
        }
    }

    fn shown(fx: &[Effect]) -> Vec<String> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Show(a) => Some(a.kind.clone()),
                _ => None,
            })
            .collect()
    }

    fn hidden(fx: &[Effect]) -> Vec<(String, HideReason)> {
        fx.iter()
            .filter_map(|e| match e {
                Effect::Hide { alert, reason } => Some((alert.kind.clone(), *reason)),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn one_at_a_time_with_spacing_priority_and_cap() {
        let mut q = AlertQueue::new(policy());
        let a = alert(&mut q, "follow", 10, 4000);
        assert_eq!(shown(&q.push(a, 0)), vec!["follow"]);
        let b = alert(&mut q, "follow2", 10, 4000);
        let c = alert(&mut q, "sub", 39, 60_000);
        assert!(shown(&q.push(b, 100)).is_empty());
        assert!(shown(&q.push(c, 200)).is_empty(), "priority 39 < 10 + margin 30 → no interrupt");
        assert!(q.tick(3999).is_empty());
        let fx = q.tick(4000);
        assert_eq!(hidden(&fx), vec![("follow".into(), HideReason::Done)]);
        assert!(shown(&fx).is_empty(), "min spacing");
        // higher priority first, even though it came later
        assert_eq!(shown(&q.tick(5000)), vec!["sub"]);
        // max on-screen: 60 s requested, capped at 10 s
        assert!(q.tick(14_999).is_empty());
        assert_eq!(hidden(&q.tick(15_000)), vec![("sub".into(), HideReason::Done)]);
        assert_eq!(shown(&q.tick(16_000)), vec!["follow2"]);
    }

    #[test]
    fn big_alerts_interrupt_small_ones_and_requeue_them() {
        let mut q = AlertQueue::new(policy());
        let small = alert(&mut q, "follow", 10, 5000);
        q.push(small, 0);
        let big = alert(&mut q, "raid", 90, 8000);
        let fx = q.push(big, 1000);
        assert_eq!(hidden(&fx), vec![("follow".into(), HideReason::Interrupted)]);
        assert_eq!(shown(&fx), vec!["raid"]);
        assert_eq!(q.len(), 1, "interrupted alert requeued");
        assert!(q.tick(8999).is_empty());
        q.tick(9000);
        assert_eq!(shown(&q.tick(10_000)), vec!["follow"]);
        // drop policy
        let mut q = AlertQueue::new(QueuePolicy { on_interrupt: OnInterrupt::Drop, ..policy() });
        let small = alert(&mut q, "follow", 10, 5000);
        q.push(small, 0);
        let big = alert(&mut q, "raid", 90, 8000);
        q.push(big, 1000);
        assert!(q.is_empty());
        // non-interrupting alerts wait
        let mut q = AlertQueue::new(policy());
        let small = alert(&mut q, "follow", 10, 5000);
        q.push(small, 0);
        let mut big = alert(&mut q, "raid", 90, 8000);
        big.interrupt = false;
        assert!(q.push(big, 1000).is_empty());
    }

    #[test]
    fn veto_window_holds_viewer_text_and_mods_can_kill_or_approve() {
        let mut q = AlertQueue::new(policy());
        let mut a = alert(&mut q, "cheer", 50, 5000);
        a.message = "hello chat".into();
        a.needs_veto = true;
        let id = a.id;
        assert!(q.push(a, 0).is_empty(), "held for the veto window");
        assert_eq!(q.veto_pending(1000), vec![(id, 2000)]);
        // other alerts without text don't wait behind it
        let f = alert(&mut q, "follow", 10, 1000);
        assert_eq!(shown(&q.push(f, 1000)), vec!["follow"]);
        // mod vetoes → never shows
        let fx = q.veto(id, 1500);
        assert!(matches!(&fx[0], Effect::Dropped { reason: "vetoed", .. }));
        assert!(shown(&q.tick(10_000)).is_empty());
        assert!(q.is_empty());
        // window elapses → shows by itself
        let mut b = alert(&mut q, "cheer", 50, 5000);
        b.message = "hi".into();
        b.needs_veto = true;
        q.push(b, 20_000);
        assert!(shown(&q.tick(22_999)).is_empty());
        assert_eq!(shown(&q.tick(23_000)), vec!["cheer"]);
        // approve ends the window early
        let mut c = alert(&mut q, "tip", 50, 5000);
        c.message = "yo".into();
        c.needs_veto = true;
        let cid = c.id;
        q.push(c, 40_000);
        let (ok, fx) = q.approve(cid, 41_500);
        assert!(ok);
        assert_eq!(shown(&fx), vec!["tip"]);
        // vetoing the on-screen alert takes it down
        let fx = q.veto(cid, 42_000);
        assert_eq!(hidden(&fx), vec![("tip".into(), HideReason::Vetoed)]);
    }

    #[test]
    fn pause_takes_alert_down_and_replays_it_after() {
        let mut q = AlertQueue::new(policy());
        let a = alert(&mut q, "sub", 40, 6000);
        q.push(a, 0);
        let b = alert(&mut q, "follow", 10, 3000);
        q.push(b, 100);
        let fx = q.set_pause(PauseReason::Mode, true, 2000);
        assert_eq!(hidden(&fx), vec![("sub".into(), HideReason::Paused)]);
        // nothing shows while paused, new alerts wait
        let c = alert(&mut q, "raid", 90, 3000);
        assert!(shown(&q.push(c, 3000)).is_empty());
        assert!(shown(&q.tick(60_000)).is_empty());
        assert_eq!(q.len(), 3);
        // a second pause reason keeps it held
        q.set_pause(PauseReason::Manual, true, 61_000);
        assert!(q.set_pause(PauseReason::Mode, false, 62_000).is_empty());
        let fx = q.set_pause(PauseReason::Manual, false, 63_000);
        assert_eq!(shown(&fx), vec!["raid"], "priority order on resume");
        q.tick(66_000);
        assert_eq!(shown(&q.tick(67_000)), vec!["sub"], "the interrupted sub replays in full");
        assert!(q.tick(72_999).iter().all(|e| !matches!(e, Effect::Hide { .. })));
        assert_eq!(hidden(&q.tick(73_000)), vec![("sub".into(), HideReason::Done)]);
        assert_eq!(shown(&q.tick(74_000)), vec!["follow"]);
    }

    #[test]
    fn deletion_sync_purges_users_and_strips_messages() {
        let mut q = AlertQueue::new(policy());
        let mut a = alert(&mut q, "cheer", 50, 5000);
        a.user_id = "troll".into();
        a.message = "bad".into();
        q.push(a, 0);
        let mut b = alert(&mut q, "tip", 40, 5000);
        b.user_id = "troll".into();
        q.push(b, 10);
        let mut c = alert(&mut q, "sub", 40, 5000);
        c.message = "resub text".into();
        c.message_id = Some("m1".into());
        c.tts = Some(Tts { text: "resub text".into(), voice: None });
        q.push(c, 20);
        let fx = q.purge_user("troll", "", 1000);
        assert_eq!(hidden(&fx), vec![("cheer".into(), HideReason::Purged)]);
        assert!(fx.iter().any(|e| matches!(e, Effect::Dropped { alert, reason: "purged" } if alert.kind == "tip")));
        assert_eq!(q.len(), 1);
        assert!(q.strip_message("m1").is_empty(), "queued: changed silently");
        let fx = q.tick(2000);
        match &fx[0] {
            Effect::Show(a) => {
                assert_eq!(a.kind, "sub");
                assert!(a.message.is_empty() && a.tts.is_none());
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn queue_full_drops_lowest_priority_newest() {
        let mut q = AlertQueue::new(QueuePolicy { max_queue: 2, ..policy() });
        let a = alert(&mut q, "raid", 90, 5000);
        q.push(a, 0); // showing
        let b = alert(&mut q, "f1", 10, 1000);
        let c = alert(&mut q, "sub", 40, 1000);
        let d = alert(&mut q, "f2", 10, 1000);
        q.push(b, 1);
        q.push(c, 2);
        let fx = q.push(d, 3);
        assert!(matches!(&fx[0], Effect::Dropped { alert, reason: "queue full" } if alert.kind == "f2"));
        let kinds: Vec<String> = q.queued(3).iter().map(|(a, _)| a.kind.clone()).collect();
        assert_eq!(kinds, vec!["sub", "f1"]);
    }

    #[test]
    fn replay_and_update() {
        let mut q = AlertQueue::new(policy());
        let a = alert(&mut q, "gift", 60, 3000);
        let id = a.id;
        q.push(a, 0);
        let fx = q.update(id, |a| a.recipients.push("x".into()));
        assert!(matches!(&fx[0], Effect::Update(a) if a.recipients == vec!["x"]));
        q.tick(3000);
        let fx = q.replay(id, 5000).unwrap();
        match &fx[0] {
            Effect::Show(a) => assert!(a.id != id && a.kind == "gift"),
            other => panic!("{other:?}"),
        }
        assert!(q.replay(999, 0).is_none());
    }
}
