//! Triggerable addresses and attack/hold/release envelopes (§6.2, §8.5).
//!
//! Triggering `X` sets `X.env` (0–1 envelope) and `X.active`, and emits the event
//! `X.trigger` with the payload. Patches, video effects, and audio effects read the envelope.

use crate::config::Conflict;
use se_proto::{Id, Ts, Value};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TriggerSpec {
    pub attack_ms: u64,
    /// `None` = hold until released.
    pub hold_ms: Option<u64>,
    pub release_ms: u64,
    pub retrigger: Conflict,
    /// Maximum simultaneous instances for `stack`.
    pub max_stack: usize,
}

impl Default for TriggerSpec {
    fn default() -> Self {
        TriggerSpec { attack_ms: 50, hold_ms: Some(2000), release_ms: 500, retrigger: Conflict::Stack, max_stack: 16 }
    }
}

impl TriggerSpec {
    /// Parse a manifest `trigger = { attack = "100ms", hold = "4s", release = "1s", retrigger = "stack" }`.
    pub fn from_value(v: &Value) -> Result<TriggerSpec, String> {
        let mut s = TriggerSpec::default();
        let Some(m) = v.as_map() else { return Ok(s) };
        let dur = |k: &str| -> Result<Option<u64>, String> {
            match m.get(k) {
                None => Ok(None),
                Some(Value::Str(x)) if x == "inf" || x == "latch" => Ok(Some(u64::MAX)),
                Some(Value::Str(x)) => se_proto::parse_duration_ms(x).map(Some).ok_or_else(|| format!("bad duration `{x}`")),
                Some(v) => v.as_f64().map(|f| Some(f.max(0.0) as u64)).ok_or_else(|| format!("bad duration for `{k}`")),
            }
        };
        if let Some(a) = dur("attack")? {
            s.attack_ms = a;
        }
        if let Some(h) = dur("hold")? {
            s.hold_ms = if h == u64::MAX { None } else { Some(h) };
        }
        if let Some(r) = dur("release")? {
            s.release_ms = r;
        }
        if let Some(Value::Str(r)) = m.get("retrigger") {
            s.retrigger = match r.as_str() {
                "stack" => Conflict::Stack,
                "replace" | "restart" => Conflict::Replace,
                "queue" => Conflict::Queue,
                "reject" | "ignore" => Conflict::Reject,
                x => return Err(format!("bad retrigger `{x}`")),
            };
        }
        if let Some(n) = m.get("max_stack").and_then(Value::as_i64) {
            s.max_stack = n.max(1) as usize;
        }
        Ok(s)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Instance {
    pub id: Id,
    pub start: Ts,
    pub attack: Ts,
    pub hold: Option<Ts>,
    pub release: Ts,
    pub released_at: Option<Ts>,
    /// Level when release began (release ramps from here).
    pub release_from: f64,
    /// Owner key (preset/actor) so `release` only ends its own instances.
    pub key: String,
    pub payload: Value,
}

impl Instance {
    pub fn level(&self, t: Ts) -> f64 {
        if let Some(r) = self.released_at {
            if self.release == 0 {
                return 0.0;
            }
            let p = (t.saturating_sub(r)) as f64 / self.release as f64;
            return (self.release_from * (1.0 - p)).max(0.0);
        }
        let el = t.saturating_sub(self.start);
        if el < self.attack { el as f64 / self.attack as f64 } else { 1.0 }
    }

    /// Enter release automatically once attack+hold elapses.
    pub fn auto_release(&mut self, t: Ts) {
        if self.released_at.is_none()
            && let Some(h) = self.hold
        {
            let end = self.start + self.attack + h;
            if t >= end {
                self.release_from = 1.0;
                self.released_at = Some(end);
            }
        }
    }

    pub fn finished(&self, t: Ts) -> bool {
        self.released_at.is_some_and(|r| t >= r + self.release)
    }
}

#[derive(Clone, Debug, Default)]
pub struct TriggerRt {
    pub spec: TriggerSpec,
    pub instances: Vec<Instance>,
    pub queued: Vec<Instance>,
    pub env_id: usize,
    pub active_id: usize,
    pub level: f64,
}

impl TriggerRt {
    /// Returns false when rejected by the retrigger policy.
    pub fn fire(&mut self, inst: Instance) -> bool {
        let busy = !self.instances.is_empty();
        match self.spec.retrigger {
            Conflict::Reject if busy => return false,
            Conflict::Queue if busy => {
                self.queued.push(inst);
                return true;
            }
            Conflict::Replace => self.instances.clear(),
            Conflict::Stack if self.instances.len() >= self.spec.max_stack => {
                self.instances.remove(0);
            }
            _ => {}
        }
        self.instances.push(inst);
        true
    }

    pub fn release(&mut self, key: Option<&str>, t: Ts) -> bool {
        let mut any = false;
        for i in &mut self.instances {
            if i.released_at.is_none() && key.is_none_or(|k| i.key == k) {
                i.release_from = i.level(t);
                i.released_at = Some(t);
                any = true;
            }
        }
        if key.is_none() {
            self.queued.clear();
        } else {
            self.queued.retain(|q| key.is_some_and(|k| q.key != k));
        }
        any
    }

    /// Advance; returns (level, active).
    pub fn tick(&mut self, t: Ts) -> (f64, bool) {
        for i in &mut self.instances {
            i.auto_release(t);
        }
        self.instances.retain(|i| !i.finished(t));
        if self.instances.is_empty() && !self.queued.is_empty() {
            let mut next = self.queued.remove(0);
            next.start = t;
            self.instances.push(next);
        }
        self.level = self.instances.iter().map(|i| i.level(t)).fold(0.0, f64::max);
        (self.level, !self.instances.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Ts = 1_000_000;

    fn inst(start: Ts) -> Instance {
        Instance {
            id: 1,
            start,
            attack: 100 * MS,
            hold: Some(1000 * MS),
            release: 500 * MS,
            released_at: None,
            release_from: 0.0,
            key: "k".into(),
            payload: Value::Null,
        }
    }

    #[test]
    fn envelope_shape() {
        let mut t = TriggerRt::default();
        t.fire(inst(0));
        assert!((t.tick(50 * MS).0 - 0.5).abs() < 1e-9);
        assert_eq!(t.tick(500 * MS), (1.0, true));
        assert!((t.tick(1350 * MS).0 - 0.5).abs() < 1e-9);
        assert_eq!(t.tick(1700 * MS), (0.0, false));
    }

    #[test]
    fn manual_release_and_policies() {
        let mut t = TriggerRt { spec: TriggerSpec { retrigger: Conflict::Reject, ..Default::default() }, ..Default::default() };
        assert!(t.fire(inst(0)));
        assert!(!t.fire(inst(0)));
        t.tick(200 * MS);
        t.release(None, 200 * MS);
        assert!((t.tick(450 * MS).0 - 0.5).abs() < 1e-9);
        let mut q = TriggerRt { spec: TriggerSpec { retrigger: Conflict::Queue, ..Default::default() }, ..Default::default() };
        q.fire(inst(0));
        q.fire(inst(0));
        assert_eq!(q.queued.len(), 1);
        q.tick(1700 * MS);
        assert_eq!(q.instances.len(), 1, "queued instance starts after the first ends");
    }

    #[test]
    fn spec_from_manifest() {
        let v = Value::map().with("attack", "100ms").with("hold", "4s").with("release", "1s").with("retrigger", "stack");
        let s = TriggerSpec::from_value(&v).unwrap();
        assert_eq!((s.attack_ms, s.hold_ms, s.release_ms), (100, Some(4000), 1000));
    }
}
