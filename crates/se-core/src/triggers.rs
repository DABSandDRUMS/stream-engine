//! Triggerable addresses and attack/hold/release envelopes (§6.2, §8.5).
//!
//! Triggering `X` sets `X.env` (0–1 envelope) and `X.active`, and emits the event
//! `X.trigger` with the payload. Patches, video effects, and audio effects read the envelope.
//! Patch triggers also publish the payload's numbers ([`TriggerPayload`]) as `X.payload.*` in
//! the same tick, so shader and dsp patches see the payload of the trigger that just fired.

use crate::config::Conflict;
use se_proto::{Actor, Id, Ts, Value};
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
    /// Plateau amplitude, fixed at creation (musical patches use conservative strength).
    #[serde(default = "full_strength")]
    pub strength: f64,
    /// Owner key (preset/actor) so `release` only ends its own instances.
    pub key: String,
    pub payload: Value,
}

fn full_strength() -> f64 { 1.0 }

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
        self.strength * if el < self.attack { el as f64 / self.attack as f64 } else { 1.0 }
    }

    /// Enter release automatically once attack+hold elapses.
    pub fn auto_release(&mut self, t: Ts) {
        if self.released_at.is_none()
            && let Some(h) = self.hold
        {
            let end = self.start + self.attack + h;
            if t >= end {
                self.release_from = self.strength;
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
    /// `X.payload.*` state ids ([`PAYLOAD_FIELDS`], then `user_color`); patch triggers only.
    pub payload_ids: Vec<usize>,
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

/// Scalar fields of [`TriggerPayload`] in float order (`X.payload.<field>`, `se.trigger.<field>`
/// in shaders, the dsp params block); `user_color` follows as 4 floats at [`PAYLOAD_COLOR`].
pub const PAYLOAD_FIELDS: [&str; 7] = ["amount", "bits", "tier", "months", "viewers", "count", "user_hash"];
/// Float index of `user_color` (r, g, b, a) in [`TriggerPayload::floats`].
pub const PAYLOAD_COLOR: usize = 8;
/// Length of [`TriggerPayload::floats`].
pub const PAYLOAD_FLOATS: usize = 12;

/// The numbers of a trigger's payload and actor that every patch receives (§6.3).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct TriggerPayload {
    /// How big the event was: payload `amount`, else `bits`, `count`, `viewers`.
    pub amount: f32,
    pub bits: f32,
    pub tier: f32,
    pub months: f32,
    pub viewers: f32,
    pub count: f32,
    /// Stable 0–1 hash of the user (actor id, else payload `user_id`, else `user`); 0 without one.
    pub user_hash: f32,
    /// The user's chat colour (payload `color`, `#rrggbb`), else a hue picked by `user_hash`;
    /// transparent without a user. sRGB-encoded like the palette.
    pub user_color: [f32; 4],
}

fn number(v: &Value) -> Option<f32> {
    match v {
        Value::Str(s) => s.trim().parse::<f64>().ok(),
        v => v.as_f64(),
    }
    .filter(|f| f.is_finite())
    .map(|f| f as f32)
}

/// 24-bit FNV-1a of `s` as 0..1 (exact in f32).
fn hash01(s: &str) -> f32 {
    let mut h: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        h = (h ^ b as u32).wrapping_mul(0x0100_0193);
    }
    (h >> 8) as f32 / (1u32 << 24) as f32
}

/// Fully saturated, bright colour of hue `h` (0..1).
fn hue(h: f32) -> [f32; 4] {
    let c = |n: f32| {
        let k = (n + h * 6.0) % 6.0;
        1.0 - 0.75 * k.min(4.0 - k).clamp(0.0, 1.0)
    };
    [c(5.0), c(3.0), c(1.0), 1.0]
}

impl TriggerPayload {
    /// From the `X.trigger` event's payload and actor.
    pub fn from_event(payload: &Value, actor: Option<&Actor>) -> TriggerPayload {
        let get = |k: &str| payload.get_path(k).and_then(number);
        let user = match actor.filter(|a| !a.id.is_empty()) {
            Some(a) => Some(format!("{}:{}", a.platform, a.id)),
            None => payload.get_path("user_id").or_else(|| payload.get_path("user")).and_then(Value::as_str).filter(|s| !s.is_empty()).map(str::to_string),
        };
        let user_hash = user.as_deref().map_or(0.0, hash01);
        let chat_color = payload.get_path("color").and_then(Value::as_str).and_then(se_proto::value::parse_hex_color);
        let user_color = match (chat_color, &user) {
            (Some(c), _) => c,
            (None, Some(_)) => hue(user_hash),
            (None, None) => [0.0; 4],
        };
        let bits = get("bits").unwrap_or(0.0);
        let count = get("count").unwrap_or(0.0);
        let viewers = get("viewers").unwrap_or(0.0);
        TriggerPayload {
            amount: get("amount").or(get("bits")).or(get("count")).or(get("viewers")).unwrap_or(0.0),
            bits,
            tier: get("tier").unwrap_or(0.0),
            months: get("months").unwrap_or(0.0),
            viewers,
            count,
            user_hash,
            user_color,
        }
    }

    /// Flat layout: [`PAYLOAD_FIELDS`] in order, one reserved 0, then `user_color`.
    pub fn floats(&self) -> [f32; PAYLOAD_FLOATS] {
        let [r, g, b, a] = self.user_color;
        [self.amount, self.bits, self.tier, self.months, self.viewers, self.count, self.user_hash, 0.0, r, g, b, a]
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
            strength: 1.0,
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
    fn restrained_amplitude_fades_from_peak_or_early_release_without_jumping() {
        let mut natural = TriggerRt::default();
        natural.fire(Instance { strength: 0.3, ..inst(0) });
        assert!((natural.tick(50 * MS).0 - 0.15).abs() < 1e-9);
        assert!((natural.tick(500 * MS).0 - 0.3).abs() < 1e-9);
        assert!((natural.tick(1350 * MS).0 - 0.15).abs() < 1e-9);
        assert_eq!(natural.tick(1700 * MS), (0.0, false));

        let mut early = TriggerRt::default();
        early.fire(Instance { strength: 0.3, ..inst(0) });
        early.release(Some("k"), 50 * MS);
        assert!((early.tick(50 * MS).0 - 0.15).abs() < 1e-9);
        assert!((early.tick(300 * MS).0 - 0.075).abs() < 1e-9);
        assert_eq!(early.tick(550 * MS), (0.0, false));
    }

    #[test]
    fn older_serialized_envelopes_keep_full_strength() {
        let serialized = serde_json::json!({
            "id": 1, "start": 0, "attack": 100_000_000, "hold": 1_000_000_000,
            "release": 500_000_000, "released_at": null, "release_from": 0.0,
            "key": "manual", "payload": null
        });
        let restored: Instance = serde_json::from_value(serialized).unwrap();
        assert_eq!(restored.level(500 * MS), 1.0);
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

    #[test]
    fn payload_numbers_user_hash_and_colour() {
        let cheer = Value::map().with("bits", 5000).with("message", "hi").with("user", "drumfan42");
        let actor = Actor { platform: "twitch".into(), id: "123".into(), name: "drumfan42".into(), roles: Vec::new() };
        let p = TriggerPayload::from_event(&cheer, Some(&actor));
        assert_eq!((p.amount, p.bits, p.tier), (5000.0, 5000.0, 0.0), "amount falls back to bits");
        assert!(p.user_hash > 0.0 && p.user_hash < 1.0);
        assert_eq!(p.user_color[3], 1.0, "a user always gets an opaque colour");
        // the same user hashes the same whatever the event; another user differs
        let sub = TriggerPayload::from_event(&Value::map().with("tier", "2").with("months", 7), Some(&actor));
        assert_eq!((sub.tier, sub.months, sub.amount), (2.0, 7.0, 0.0), "numeric strings count");
        assert_eq!(sub.user_hash, p.user_hash);
        assert_eq!(sub.user_color, p.user_color);
        let other = Actor { id: "124".into(), ..actor.clone() };
        assert_ne!(TriggerPayload::from_event(&cheer, Some(&other)).user_hash, p.user_hash);
        // explicit amount wins over bits; a chat colour wins over the hashed hue
        let tip = Value::map().with("amount", 3.5).with("bits", 100).with("color", "#FF8000");
        let t = TriggerPayload::from_event(&tip, Some(&actor));
        assert_eq!(t.amount, 3.5);
        assert_eq!(t.user_color, [1.0, 128.0 / 255.0, 0.0, 1.0]);
        // no user at all: hash 0, transparent colour; raids size by viewers
        let raid = TriggerPayload::from_event(&Value::map().with("viewers", 300), None);
        assert_eq!((raid.amount, raid.viewers, raid.user_hash, raid.user_color), (300.0, 300.0, 0.0, [0.0; 4]));
        assert_eq!(TriggerPayload::from_event(&Value::Null, None), TriggerPayload::default());
        let f = t.floats();
        assert_eq!(f[PAYLOAD_FIELDS.iter().position(|n| *n == "amount").unwrap()], 3.5);
        assert_eq!(&f[PAYLOAD_COLOR..PAYLOAD_COLOR + 4], &t.user_color);
    }
}
