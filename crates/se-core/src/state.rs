//! The state tree (§2.1): every addressable value with metadata and layered resolution
//! `base → scene → bindings → overrides (priority, HTP/LTP) → clamps`, with provenance.

use crate::config::BindMode;
use se_proto::wire::{Layer, Provenance};
use se_proto::{Ease, Id, Merge, Meta, Origin, Ts, Value, ValueType, address};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anim {
    pub from: Value,
    pub to: Value,
    pub start: Ts,
    pub dur: Ts,
    pub ease: Ease,
}

impl Anim {
    pub fn value_at(&self, t: Ts) -> Value {
        let p = if self.dur == 0 { 1.0 } else { (t.saturating_sub(self.start)) as f64 / self.dur as f64 };
        lerp_value(&self.from, &self.to, self.ease.apply(p))
    }
    pub fn done(&self, t: Ts) -> bool {
        t >= self.start + self.dur
    }
}

/// Interpolate numbers and numeric lists; other values step at the end.
pub fn lerp_value(a: &Value, b: &Value, t: f64) -> Value {
    match (a, b) {
        (Value::List(x), Value::List(y)) if x.len() == y.len() => Value::List(x.iter().zip(y).map(|(p, q)| lerp_value(p, q, t)).collect()),
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) if !matches!(a, Value::Bool(_)) && !matches!(b, Value::Bool(_)) => {
                let v = x + (y - x) * t;
                if matches!(b, Value::Int(_)) && t >= 1.0 { b.clone() } else { Value::Float(v) }
            }
            _ => {
                if t >= 1.0 {
                    b.clone()
                } else {
                    a.clone()
                }
            }
        },
    }
}

/// One override layer entry.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Override {
    /// Owner key: `manual`, `preset:<name>`, `chat:<actor>`, `rule:<name>`, `timeline:<name>`, …
    pub key: String,
    pub priority: u16,
    pub value: Value,
    /// Insertion order (latest wins among equal priority).
    pub seq: u64,
    pub expires: Option<Ts>,
    pub anim: Option<Anim>,
    pub origin: Origin,
    pub causal: Option<Id>,
}

impl Override {
    pub fn current(&self, t: Ts) -> Value {
        match &self.anim {
            Some(a) => a.value_at(t),
            None => self.value.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct Mod {
    pub source: String,
    pub mode: BindMode,
    pub value: f64,
}

#[derive(Clone, Debug)]
pub struct Param {
    pub addr: String,
    pub meta: Meta,
    pub base: Value,
    pub scene: Option<Value>,
    pub mods: Vec<Mod>,
    pub overrides: Vec<Override>,
    /// Safety clamp (from `[safety.caps]`).
    pub cap: Option<[f64; 2]>,
    pub resolved: Value,
    /// Last value reported as a change (change detection is per tick).
    pub published: Value,
    /// Declared with metadata (vs implicitly created by a set).
    pub declared: bool,
}

/// Addressable state with layered resolution.
#[derive(Default)]
pub struct StateTree {
    params: Vec<Param>,
    index: HashMap<String, usize>,
    /// Bumped whenever the set of addresses changes (wildcard caches rebuild).
    pub generation: u64,
    seq: u64,
    /// Params whose value may change without a command (animations, mods, expiries).
    live: Vec<bool>,
    dirty: Vec<usize>,
    is_dirty: Vec<bool>,
}

impl StateTree {
    pub fn len(&self) -> usize {
        self.params.len()
    }
    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }
    pub fn id(&self, addr: &str) -> Option<usize> {
        self.index.get(addr).copied()
    }
    pub fn param(&self, id: usize) -> &Param {
        &self.params[id]
    }
    pub fn params(&self) -> &[Param] {
        &self.params
    }
    pub fn get(&self, addr: &str) -> Option<&Value> {
        self.id(addr).map(|i| &self.params[i].resolved)
    }
    pub fn value(&self, id: usize) -> &Value {
        &self.params[id].resolved
    }

    /// Declare (or re-declare) an address with metadata. Keeps runtime layers.
    pub fn declare(&mut self, addr: &str, meta: Meta) -> usize {
        if let Some(i) = self.id(addr) {
            let p = &mut self.params[i];
            if !p.declared || p.meta != meta {
                if !p.declared || p.base == p.meta.default {
                    p.base = meta.default.clone();
                }
                p.meta = meta;
                p.declared = true;
                self.mark(i);
            }
            return i;
        }
        let v = meta.default.clone();
        self.insert(addr, meta, v, true)
    }

    /// Get or implicitly create an address (untyped) holding `v` as its base.
    pub fn ensure(&mut self, addr: &str, v: &Value) -> usize {
        if let Some(i) = self.id(addr) {
            return i;
        }
        let meta = Meta { ty: infer_type(v), ..Default::default() };
        self.insert(addr, meta, v.clone(), false)
    }

    fn insert(&mut self, addr: &str, meta: Meta, base: Value, declared: bool) -> usize {
        let i = self.params.len();
        let resolved = meta.coerce(&base);
        self.params.push(Param {
            addr: addr.to_string(),
            meta,
            base,
            scene: None,
            mods: Vec::new(),
            overrides: Vec::new(),
            cap: None,
            published: resolved.clone(),
            resolved,
            declared,
        });
        self.index.insert(addr.to_string(), i);
        self.live.push(false);
        self.is_dirty.push(false);
        self.generation += 1;
        i
    }

    /// Remove every address under `prefix.` (or equal to it). Ids are compacted, so callers
    /// holding ids must rebuild on `generation` change.
    pub fn remove_prefix(&mut self, prefix: &str) -> usize {
        let dotted = format!("{prefix}.");
        let before = self.params.len();
        let keep: Vec<bool> = self.params.iter().map(|p| !(p.addr == prefix || p.addr.starts_with(&dotted))).collect();
        if keep.iter().all(|k| *k) {
            return 0;
        }
        let mut i = 0;
        self.params.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
        let mut i = 0;
        self.live.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
        self.index = self.params.iter().enumerate().map(|(i, p)| (p.addr.clone(), i)).collect();
        self.is_dirty = vec![false; self.params.len()];
        self.dirty.clear();
        self.generation += 1;
        before - self.params.len()
    }

    pub fn mark(&mut self, i: usize) {
        if !self.is_dirty[i] {
            self.is_dirty[i] = true;
            self.dirty.push(i);
        }
    }

    pub fn set_live(&mut self, i: usize, live: bool) {
        self.live[i] = live;
    }

    pub fn set_base(&mut self, i: usize, v: Value) -> Value {
        let old = std::mem::replace(&mut self.params[i].base, v);
        self.mark(i);
        old
    }

    /// Recompute one value immediately (reads later in the same tick see it); the change is
    /// still reported by the next [`StateTree::resolve`].
    pub fn refresh(&mut self, i: usize, now: Ts) {
        let p = &mut self.params[i];
        p.resolved = resolve_param(p, now, None);
        self.mark(i);
    }

    pub fn set_scene(&mut self, i: usize, v: Option<Value>) {
        if self.params[i].scene != v {
            self.params[i].scene = v;
            self.mark(i);
        }
    }

    pub fn set_cap(&mut self, i: usize, cap: Option<[f64; 2]>) {
        self.params[i].cap = cap;
        self.mark(i);
    }

    /// Insert or replace (same key) an override.
    pub fn put_override(&mut self, i: usize, mut o: Override) {
        self.seq += 1;
        o.seq = self.seq;
        let p = &mut self.params[i];
        p.overrides.retain(|x| x.key != o.key);
        let animated = o.anim.is_some() || o.expires.is_some();
        p.overrides.push(o);
        if animated {
            self.live[i] = true;
        }
        self.mark(i);
    }

    pub fn remove_override(&mut self, i: usize, key: &str) -> bool {
        let p = &mut self.params[i];
        let n = p.overrides.len();
        p.overrides.retain(|x| x.key != key);
        let removed = p.overrides.len() != n;
        if removed {
            self.mark(i);
        }
        removed
    }

    /// Remove overrides matching `pred` everywhere; returns affected addresses.
    pub fn remove_overrides_where(&mut self, mut pred: impl FnMut(&Override) -> bool) -> Vec<usize> {
        let mut hit = Vec::new();
        for (i, p) in self.params.iter_mut().enumerate() {
            let n = p.overrides.len();
            p.overrides.retain(|o| !pred(o));
            if p.overrides.len() != n {
                hit.push(i);
            }
        }
        for &i in &hit {
            self.mark(i);
        }
        hit
    }

    /// Replace the binding modulation list for a param.
    pub fn set_mods(&mut self, i: usize, mods: Vec<Mod>) {
        let p = &mut self.params[i];
        if p.mods.is_empty() && mods.is_empty() {
            return;
        }
        p.mods = mods;
        self.mark(i);
    }

    pub fn clear_all_mods(&mut self) {
        for i in 0..self.params.len() {
            if !self.params[i].mods.is_empty() {
                self.params[i].mods.clear();
                self.mark(i);
            }
        }
    }

    /// Re-resolve dirty and live params. Calls `changed(id, &value)` for every changed value.
    pub fn resolve(&mut self, now: Ts, mut changed: impl FnMut(usize, &Value)) {
        for i in 0..self.params.len() {
            if self.live[i] {
                self.mark(i);
            }
        }
        let dirty = std::mem::take(&mut self.dirty);
        for &i in &dirty {
            self.is_dirty[i] = false;
            // expire + settle animations
            let p = &mut self.params[i];
            let before = p.overrides.len();
            p.overrides.retain(|o| o.expires.is_none_or(|e| now < e));
            let mut still_live = !p.mods.is_empty();
            for o in &mut p.overrides {
                if let Some(a) = &o.anim {
                    if a.done(now) {
                        o.value = a.to.clone();
                        o.anim = None;
                    } else {
                        still_live = true;
                    }
                }
                if o.expires.is_some() {
                    still_live = true;
                }
            }
            let _ = before;
            self.live[i] = still_live;
            p.resolved = resolve_param(p, now, None);
            if p.resolved != p.published {
                p.published = p.resolved.clone();
                changed(i, &p.resolved);
            }
        }
        self.dirty = dirty;
        self.dirty.clear();
    }

    pub fn explain(&self, addr: &str, now: Ts) -> Option<Provenance> {
        let p = &self.params[self.id(addr)?];
        let mut layers = Vec::new();
        let v = resolve_param(p, now, Some(&mut layers));
        Some(Provenance { address: addr.to_string(), value: v, layers })
    }

    /// Addresses matching a pattern (or exact address).
    pub fn matching<'a>(&'a self, pattern: &'a str) -> impl Iterator<Item = usize> + 'a {
        let exact = self.id(pattern);
        self.params.iter().enumerate().filter_map(move |(i, p)| {
            if exact.is_some() {
                return (exact == Some(i)).then_some(i);
            }
            address::matches(pattern, &p.addr).then_some(i)
        })
    }
}

fn infer_type(v: &Value) -> ValueType {
    match v {
        Value::Bool(_) => ValueType::Bool,
        Value::Int(_) => ValueType::Int,
        Value::Float(_) => ValueType::Float,
        Value::Str(_) => ValueType::String,
        Value::List(_) => ValueType::List,
        Value::Map(_) => ValueType::Map,
        Value::Null => ValueType::Any,
    }
}

fn num_or_list(v: &Value, f: &dyn Fn(f64) -> f64) -> Value {
    match v {
        Value::List(l) => Value::List(l.iter().map(|x| num_or_list(x, f)).collect()),
        Value::Int(i) => Value::Float(f(*i as f64)),
        Value::Float(x) => Value::Float(f(*x)),
        other => other.clone(),
    }
}

/// Resolve one param, optionally recording provenance layers.
pub fn resolve_param(p: &Param, now: Ts, mut prov: Option<&mut Vec<Layer>>) -> Value {
    let mut push = |kind: &str, source: &str, priority: Option<u16>, value: &Value, active: bool| {
        if let Some(l) = prov.as_deref_mut() {
            l.push(Layer { kind: kind.into(), source: source.into(), priority, value: value.clone(), active });
        }
    };
    let mut v = p.base.clone();
    push("base", "project", None, &p.base, p.scene.is_none());
    if let Some(s) = &p.scene {
        v = s.clone();
        push("scene", "scene", None, s, true);
    }
    // bindings
    for m in &p.mods {
        let before = v.clone();
        v = match m.mode {
            BindMode::Add => num_or_list(&v, &|x| x + m.value),
            BindMode::Multiply => num_or_list(&v, &|x| x * m.value),
            BindMode::Replace => Value::Float(m.value),
        };
        let _ = before;
        push("binding", &m.source, None, &Value::Float(m.value), true);
    }
    // overrides
    if !p.overrides.is_empty() {
        if p.meta.merge == Merge::Htp {
            let mut best = v.as_f64();
            let mut winner: Option<usize> = None;
            for (k, o) in p.overrides.iter().enumerate() {
                if let Some(x) = o.current(now).as_f64()
                    && best.is_none_or(|b| x > b)
                {
                    best = Some(x);
                    winner = Some(k);
                }
            }
            for (k, o) in p.overrides.iter().enumerate() {
                push(if o.anim.is_some() { "animation" } else { "override" }, &o.key, Some(o.priority), &o.current(now), winner == Some(k));
            }
            if let Some(b) = best {
                v = Value::Float(b);
            }
        } else {
            let win = p.overrides.iter().enumerate().max_by(|(_, a), (_, b)| a.priority.cmp(&b.priority).then(a.seq.cmp(&b.seq))).map(|(k, _)| k);
            for (k, o) in p.overrides.iter().enumerate() {
                push(if o.anim.is_some() { "animation" } else { "override" }, &o.key, Some(o.priority), &o.current(now), win == Some(k));
            }
            if let Some(k) = win {
                v = p.overrides[k].current(now);
            }
        }
    }
    // clamps
    let mut out = p.meta.coerce(&v);
    if let Some([lo, hi]) = p.cap {
        out = num_or_list(&out, &|x| x.clamp(lo, hi));
        if matches!(p.meta.ty, se_proto::ValueType::Int) {
            out = Value::Int(out.as_i64().unwrap_or(0));
        }
    }
    if out != v || p.cap.is_some() {
        push("clamp", if p.cap.is_some() { "safety" } else { "meta" }, None, &out, out != v);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ov(key: &str, prio: u16, v: f64) -> Override {
        Override { key: key.into(), priority: prio, value: Value::Float(v), seq: 0, expires: None, anim: None, origin: Origin::Ui, causal: None }
    }

    fn resolve(t: &mut StateTree, now: Ts) -> Vec<(String, Value)> {
        let mut out = Vec::new();
        let names: Vec<String> = t.params().iter().map(|p| p.addr.clone()).collect();
        t.resolve(now, |i, v| out.push((names[i].clone(), v.clone())));
        out
    }

    #[test]
    fn priority_and_latest_wins() {
        let mut t = StateTree::default();
        let a = t.declare("fx.x.amount", Meta::float(0.2, [0.0, 1.0]));
        t.put_override(a, ov("chat:1", 100, 0.9));
        t.put_override(a, ov("preset:hype", 200, 0.5));
        resolve(&mut t, 0);
        assert_eq!(t.value(a), &Value::Float(0.5));
        t.put_override(a, ov("manual", 300, 0.1));
        resolve(&mut t, 0);
        assert_eq!(t.value(a), &Value::Float(0.1));
        t.remove_override(a, "manual");
        t.put_override(a, ov("preset:other", 200, 0.7));
        resolve(&mut t, 0);
        assert_eq!(t.value(a), &Value::Float(0.7), "latest among equal priority");
        let prov = t.explain("fx.x.amount", 0).unwrap();
        assert_eq!(prov.layers.iter().filter(|l| l.active && l.kind == "override").count(), 1);
    }

    #[test]
    fn htp_takes_max_and_clamps() {
        let mut t = StateTree::default();
        let a = t.declare("lights.par.intensity", Meta::float(0.0, [0.0, 1.0]).htp());
        t.put_override(a, ov("manual", 300, 0.3));
        t.put_override(a, ov("chat:x", 100, 0.8));
        resolve(&mut t, 0);
        assert_eq!(t.value(a), &Value::Float(0.8));
        t.set_cap(a, Some([0.0, 0.6]));
        resolve(&mut t, 0);
        assert_eq!(t.value(a), &Value::Float(0.6));
    }

    #[test]
    fn animation_and_expiry() {
        let mut t = StateTree::default();
        let a = t.declare("x", Meta::float(0.0, [0.0, 10.0]));
        let mut o = ov("preset:p", 200, 0.0);
        o.anim = Some(Anim { from: Value::Float(0.0), to: Value::Float(10.0), start: 0, dur: 1000, ease: Ease::Linear });
        o.expires = Some(2000);
        t.put_override(a, o);
        resolve(&mut t, 500);
        assert_eq!(t.value(a), &Value::Float(5.0));
        resolve(&mut t, 1500);
        assert_eq!(t.value(a), &Value::Float(10.0));
        resolve(&mut t, 2500);
        assert_eq!(t.value(a), &Value::Float(0.0));
    }

    #[test]
    fn bindings_modulate_under_overrides() {
        let mut t = StateTree::default();
        let a = t.declare("y", Meta::float(1.0, [0.0, 100.0]));
        t.set_mods(a, vec![Mod { source: "kick".into(), mode: BindMode::Add, value: 4.0 }]);
        resolve(&mut t, 0);
        assert_eq!(t.value(a), &Value::Float(5.0));
        t.put_override(a, ov("manual", 300, 2.0));
        resolve(&mut t, 0);
        assert_eq!(t.value(a), &Value::Float(2.0));
    }

    #[test]
    fn remove_prefix_reindexes() {
        let mut t = StateTree::default();
        t.declare("patch.a.x", Meta::float(0.0, [0.0, 1.0]));
        t.declare("patch.b.x", Meta::float(0.0, [0.0, 1.0]));
        t.declare("patch.a", Meta::trigger());
        assert_eq!(t.remove_prefix("patch.a"), 2);
        assert_eq!(t.id("patch.b.x"), Some(0));
        assert!(t.get("patch.a.x").is_none());
    }
}
