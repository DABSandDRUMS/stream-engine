//! The state tree (§2.1): every addressable value with metadata and layered resolution
//! `base → live palette reference → scene → bindings → overrides (priority, HTP/LTP) → clamps`.

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

/// Interpolate colors, numbers and numeric lists; other values step at the end.
pub fn lerp_value(a: &Value, b: &Value, t: f64) -> Value {
    if (matches!(a, Value::Str(_)) || matches!(b, Value::Str(_)))
        && let (Some(a), Some(b)) = (a.as_color(), b.as_color())
    {
        let rgba: [f32; 4] = std::array::from_fn(|i| a[i] + (b[i] - a[i]) * t as f32);
        return Value::from(rgba);
    }
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

/// A finite, one-way reference from an authored video FX parameter to the shared palette.
#[derive(Clone, Debug)]
pub struct PaletteLink {
    reference: se_proto::palette::Reference,
    value: Option<Value>,
}

impl PaletteLink {
    fn parse(address: &str, value: &Value) -> Option<Self> {
        let video = address.starts_with("source.") || address.starts_with("scene.")
            || address.starts_with("render.canvas.") || address.starts_with("render.output.");
        if !video || !address.contains(".fx.") { return None; }
        Some(Self { reference: se_proto::palette::Reference::parse(value.as_str()?)?, value: None })
    }
}

#[derive(Clone, Debug)]
pub struct Param {
    pub addr: String,
    pub meta: Meta,
    pub base: Value,
    /// Authored live palette input; runtime overrides still take precedence.
    pub palette_link: Option<PaletteLink>,
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
    /// The base was set explicitly (project params, UI edit, adapter publish) rather than being
    /// a placeholder from implicit creation; a later declaration keeps explicit bases.
    pub base_explicit: bool,
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
    palette_targets: Vec<usize>,
    palette_sources: [Option<usize>; se_proto::palette::SLOTS.len()],
    palette_generation: u64,
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
                if !p.declared {
                    p.base = if p.base_explicit { meta.coerce(&p.base) } else { meta.default.clone() };
                } else if !p.base_explicit && p.base == p.meta.default {
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

    /// Revert an authored base to the service default while retaining runtime layers.
    pub fn reset_base(&mut self, id: usize) {
        self.params[id].base = self.params[id].meta.default.clone();
        self.params[id].base_explicit = false;
        self.params[id].palette_link = None;
        self.palette_targets.retain(|target| *target != id);
        self.mark(id);
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
        let palette_link = PaletteLink::parse(addr, &base);
        if palette_link.is_some() { self.palette_targets.push(i); }
        self.params.push(Param {
            addr: addr.to_string(),
            meta,
            base,
            palette_link,
            scene: None,
            mods: Vec::new(),
            overrides: Vec::new(),
            cap: None,
            published: resolved.clone(),
            resolved,
            declared,
            base_explicit: false,
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
        // Pending changes on surviving addresses still need resolving after ids move.
        let mut i = 0;
        self.is_dirty.retain(|_| {
            let k = keep[i];
            i += 1;
            k
        });
        self.dirty.clear();
        self.dirty.extend(self.is_dirty.iter().enumerate().filter_map(|(i, dirty)| dirty.then_some(i)));
        self.generation += 1;
        self.palette_targets.clear();
        self.palette_targets.extend(self.params.iter().enumerate().filter_map(|(i, p)| p.palette_link.is_some().then_some(i)));
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
        let next = PaletteLink::parse(&self.params[i].addr, &v);
        let old_reference = self.params[i].palette_link.as_ref().map(|link| link.reference);
        if old_reference != next.as_ref().map(|link| link.reference) {
            if old_reference.is_none() && next.is_some() { self.palette_targets.push(i); }
            if next.is_none() { self.palette_targets.retain(|target| *target != i); }
            self.params[i].palette_link = next;
        }
        self.params[i].base_explicit = true;
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

    fn sync_palette_links(&mut self, now: Ts) {
        if self.palette_targets.is_empty() { return; }
        let changed_generation = self.palette_generation != self.generation;
        if changed_generation {
            self.palette_sources = se_proto::palette::ADDRESSES.map(|address| self.id(address));
            self.palette_generation = self.generation;
        }
        // Compute each used source at most once, including same-tick overrides/clamps.
        let mut colors = [None; se_proto::palette::SLOTS.len()];
        for k in 0..self.palette_targets.len() {
            let target = self.palette_targets[k];
            let reference = self.params[target].palette_link.as_ref().expect("indexed palette target").reference;
            let slot = reference.slot();
            let source = self.palette_sources[slot];
            if !changed_generation && !self.is_dirty[target]
                && source.is_none_or(|source| !self.is_dirty[source] && !self.live[source])
            { continue; }
            let color = match colors[slot] {
                Some(color) => color,
                None => {
                    let color = source.and_then(|source| {
                        if self.is_dirty[source] || self.live[source] {
                            resolve_param(&self.params[source], now, None).as_color()
                        } else { self.params[source].resolved.as_color() }
                    });
                    colors[slot] = Some(color);
                    color
                }
            };
            let current = self.params[target].palette_link.as_ref().expect("indexed palette target").value.as_ref();
            let value = match (color, reference.component(), &self.params[target].meta.ty) {
                (Some(rgba), None, ValueType::Color) => {
                    if current.and_then(Value::as_color) == Some(rgba) { continue; }
                    Some(Value::from(rgba))
                }
                (Some(rgba), Some(component), ValueType::Float) => {
                    let component = rgba[component] as f64;
                    if current.and_then(Value::as_f64) == Some(component) { continue; }
                    Some(Value::Float(component))
                }
                _ => {
                    if current.is_none() { continue; }
                    None // Never reinterpret text, gates, or unrelated numeric settings.
                }
            };
            self.params[target].palette_link.as_mut().expect("indexed palette target").value = value;
            self.mark(target);
        }
    }

    /// Re-resolve dirty and live params. Calls `changed(id, &value)` for every changed value.
    pub fn resolve(&mut self, now: Ts, mut changed: impl FnMut(usize, &Value)) {
        self.sync_palette_links(now);
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
        let ids = match exact {
            Some(i) => i..i + 1,
            None if !address::is_pattern(pattern) => 0..0,
            None => 0..self.params.len(),
        };
        ids.filter(move |&i| exact.is_some() || address::matches(pattern, &self.params[i].addr))
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
    let explain = prov.is_some();
    let mut push = |kind: &str, source: &str, priority: Option<u16>, value: &Value, active: bool| {
        if let Some(l) = prov.as_deref_mut() {
            l.push(Layer { kind: kind.into(), source: source.into(), priority, value: value.clone(), active });
        }
    };
    let palette_value = p.palette_link.as_ref().and_then(|link| link.value.as_ref());
    let mut v = palette_value.unwrap_or(&p.base).clone();
    push("base", "project", None, &p.base, p.scene.is_none() && palette_value.is_none());
    if let Some(link) = &p.palette_link && let Some(value) = palette_value {
        push("palette", link.reference.address(), None, value, p.scene.is_none());
    }
    if let Some(s) = &p.scene {
        v = s.clone();
        push("scene", "scene", None, s, true);
    }
    // `replace` bindings sit under overrides: an override (cue, preset, manual) beats them.
    for m in p.mods.iter().filter(|m| m.mode == BindMode::Replace) {
        v = Value::Float(m.value);
        push("binding", &m.source, None, &v, true);
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
            if explain {
                for (k, o) in p.overrides.iter().enumerate() {
                    push(if o.anim.is_some() { "animation" } else { "override" }, &o.key, Some(o.priority), &o.current(now), winner == Some(k));
                }
            }
            if let Some(b) = best {
                v = Value::Float(b);
            }
        } else {
            let win = p.overrides.iter().enumerate().max_by(|(_, a), (_, b)| a.priority.cmp(&b.priority).then(a.seq.cmp(&b.seq))).map(|(k, _)| k);
            if explain {
                for (k, o) in p.overrides.iter().enumerate() {
                    push(if o.anim.is_some() { "animation" } else { "override" }, &o.key, Some(o.priority), &o.current(now), win == Some(k));
                }
            }
            if let Some(k) = win {
                v = p.overrides[k].current(now);
            }
        }
    }
    // `add` / `multiply` bindings modulate whatever won (base, scene, replace binding or
    // override), in declaration order; the result is clamped to the address range below.
    for m in p.mods.iter().filter(|m| m.mode != BindMode::Replace) {
        v = match m.mode {
            BindMode::Add => num_or_list(&v, &|x| x + m.value),
            _ => num_or_list(&v, &|x| x * m.value),
        };
        push("binding", &m.source, None, &Value::Float(m.value), true);
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
    fn resolution_without_provenance_matches_explain_through_animation_and_expiry() {
        for merge in [Merge::Ltp, Merge::Htp] {
            let mut t = StateTree::default();
            let mut meta = Meta::float(0.2, [0.0, 1.0]);
            meta.merge = merge;
            let id = t.declare("lights.par.intensity", meta);
            let mut animated = ov("cue", 100, 0.0);
            animated.anim = Some(Anim { from: Value::Float(0.0), to: Value::Float(1.0), start: 0, dur: 1000, ease: Ease::Linear });
            animated.expires = Some(2000);
            t.put_override(id, animated);
            t.put_override(id, ov("manual", 300, 0.4));
            t.set_mods(id, vec![Mod { source: "music".into(), mode: BindMode::Multiply, value: 0.8 }]);
            t.set_cap(id, Some([0.0, 0.6]));
            for now in [0, 500, 1000, 1500, 2000] {
                resolve(&mut t, now);
                let explained = t.explain("lights.par.intensity", now).unwrap();
                assert_eq!(t.value(id), &explained.value, "merge={merge:?}, now={now}");
                assert_eq!(explained.layers.iter().filter(|l| l.active && matches!(l.kind.as_str(), "animation" | "override")).count(), 1);
                if now == 500 {
                    let layer = explained.layers.iter().find(|l| l.source == "cue").unwrap();
                    assert_eq!(layer.kind, "animation");
                    assert_eq!(layer.value, Value::Float(0.5));
                    assert_eq!(layer.active, merge == Merge::Htp);
                }
                if now == 2000 {
                    assert!(explained.layers.iter().all(|l| l.source != "cue"));
                }
            }
        }
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
    fn replace_bindings_sit_under_overrides() {
        let mut t = StateTree::default();
        let a = t.declare("y", Meta::float(1.0, [0.0, 100.0]));
        t.set_mods(a, vec![Mod { source: "fader".into(), mode: BindMode::Replace, value: 40.0 }]);
        resolve(&mut t, 0);
        assert_eq!(t.value(a), &Value::Float(40.0));
        t.put_override(a, ov("manual", 300, 2.0));
        resolve(&mut t, 0);
        assert_eq!(t.value(a), &Value::Float(2.0), "manual beats a replace binding");
        t.remove_override(a, "manual");
        resolve(&mut t, 0);
        assert_eq!(t.value(a), &Value::Float(40.0), "binding is back once the override goes");
    }

    #[test]
    fn add_and_multiply_bindings_modulate_the_override_winner_then_clamp() {
        let mut t = StateTree::default();
        let size = t.declare("lights.effect.pulse.size", Meta::float(1.0, [0.0, 1.0]));
        t.put_override(size, ov("cue:pulse", 200, 0.8));
        t.set_mods(size, vec![Mod { source: "music.level".into(), mode: BindMode::Multiply, value: 0.5 }]);
        resolve(&mut t, 0);
        assert_eq!(t.value(size), &Value::Float(0.4), "multiply scales the cue-written size");
        t.put_override(size, ov("manual", 300, 0.6));
        resolve(&mut t, 0);
        assert_eq!(t.value(size), &Value::Float(0.3), "a manual override is modulated too");
        let y = t.declare("y", Meta::float(1.0, [0.0, 10.0]));
        t.put_override(y, ov("preset:p", 200, 2.0));
        t.set_mods(y, vec![Mod { source: "kick".into(), mode: BindMode::Add, value: 4.0 }]);
        resolve(&mut t, 0);
        assert_eq!(t.value(y), &Value::Float(6.0), "add offsets the overridden value");
        t.set_mods(y, vec![Mod { source: "kick".into(), mode: BindMode::Add, value: 9.0 }]);
        resolve(&mut t, 0);
        assert_eq!(t.value(y), &Value::Float(10.0), "clamped to the address range");
        t.set_mods(y, vec![
            Mod { source: "kick".into(), mode: BindMode::Add, value: 1.0 },
            Mod { source: "fader".into(), mode: BindMode::Replace, value: 5.0 },
        ]);
        t.remove_override(y, "preset:p");
        resolve(&mut t, 0);
        assert_eq!(t.value(y), &Value::Float(6.0), "replace resolves first, then add modulates it");
    }

    #[test]
    fn declare_keeps_explicit_base_but_replaces_placeholder() {
        let mut t = StateTree::default();
        let a = t.ensure("source.cam.ctrl.brightness", &Value::Int(0));
        t.set_base(a, Value::Int(40));
        let b = t.ensure("fx.rgb_split.amount", &Value::Float(0.0));
        t.declare("source.cam.ctrl.brightness", Meta::int(0, [0.0, 100.0]));
        t.declare("fx.rgb_split.amount", Meta::float(0.3, [0.0, 1.0]));
        assert_eq!(t.param(a).base, Value::Int(40), "explicit project value survives declaration");
        assert_eq!(t.param(b).base, Value::Float(0.3), "placeholder replaced by the declared default");
        t.set_base(b, Value::Float(0.3));
        t.declare("fx.rgb_split.amount", Meta::float(0.7, [0.0, 1.0]));
        assert_eq!(t.param(b).base, Value::Float(0.3), "explicit value equal to previous default must survive reload");
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

    #[test]
    fn matching_keeps_exact_and_wildcard_order_after_reindexing() {
        let mut t = StateTree::default();
        t.declare("patch.a", Meta::trigger());
        t.declare("patch.a.x", Meta::float(0.0, [0.0, 1.0]));
        t.declare("patch.b.x", Meta::float(0.0, [0.0, 1.0]));
        assert_eq!(t.matching("patch.a").collect::<Vec<_>>(), vec![0]);
        assert_eq!(t.matching("patch.*.x").collect::<Vec<_>>(), vec![1, 2]);
        assert_eq!(t.matching("patch.**").collect::<Vec<_>>(), vec![0, 1, 2]);
        assert!(t.matching("patch.missing").next().is_none());
        t.remove_prefix("patch.a");
        assert_eq!(t.matching("patch.b.x").collect::<Vec<_>>(), vec![0]);
        assert_eq!(t.matching("patch.*.x").collect::<Vec<_>>(), vec![0]);
    }

    #[test]
    fn live_video_palette_respects_manual_ownership_clamps_and_color_fades() {
        let mut t = StateTree::default();
        let accent = t.declare("palette.accent", Meta::color([1.0, 0.0, 0.0, 1.0]));
        let tint = t.declare("source.cam.fx.frame.tint", Meta::color([1.0; 4]));
        t.set_base(tint, Value::from("stream:accent"));
        let component = t.declare("render.output.wide.fx.fade.color_b", Meta::float(0.0, [0.0, 0.4]));
        t.set_base(component, Value::from("stream:accent.b"));
        let title = t.declare("scene.show.node.cam.fx.frame.title", Meta::string(""));
        t.set_base(title, Value::from("stream:accent"));
        let mut manual = ov("manual", 300, 0.0);
        manual.value = Value::from([0.0, 1.0, 0.0, 1.0]);
        t.put_override(tint, manual);
        let mut fade = ov("manual", 300, 0.0);
        fade.anim = Some(Anim { from: Value::from([1.0, 0.0, 0.0, 1.0]), to: Value::from("#0000ff"), start: 0, dur: 1000, ease: Ease::Linear });
        t.put_override(accent, fade);
        resolve(&mut t, 500);
        assert_eq!(t.value(tint), &Value::from([0.0, 1.0, 0.0, 1.0]), "local manual color wins during a shared fade");
        assert_eq!(t.value(component), &Value::Float(0.4), "component references remain clamped");
        assert_eq!(t.value(title), &Value::from("stream:accent"), "text parameters are not interpreted as colors");
        t.remove_override(tint, "manual");
        resolve(&mut t, 600);
        let color = t.value(tint).as_color().unwrap();
        assert!((color[0] - 0.4).abs() < 1e-6 && (color[2] - 0.6).abs() < 1e-6, "release reveals the current fade, not a captured color: {color:?}");
        assert!(t.explain("source.cam.fx.frame.tint", 600).unwrap().layers.iter().any(|layer| layer.kind == "palette" && layer.source == "palette.accent"));
    }

    #[test]
    fn palette_references_survive_late_declarations_and_reindexing_but_not_literal_replacement() {
        let mut t = StateTree::default();
        t.declare("source.old.fx.frame.tint", Meta::color([1.0; 4]));
        let tint = t.ensure("scene.show.node.cam.fx.frame.tint", &Value::from("stream:accent"));
        t.set_base(tint, Value::from("stream:accent"));
        resolve(&mut t, 0);
        t.declare("scene.show.node.cam.fx.frame.tint", Meta::color([1.0; 4]));
        let accent = t.declare("palette.accent", Meta::color([0.2, 0.4, 0.8, 1.0]));
        t.set_base(accent, Value::from([0.2, 0.4, 0.8, 1.0]));
        t.remove_prefix("source.old");
        resolve(&mut t, 0);
        let tint = t.id("scene.show.node.cam.fx.frame.tint").unwrap();
        assert_eq!(t.value(tint), &Value::from([0.2, 0.4, 0.8, 1.0]));
        t.set_base(tint, Value::from([0.9, 0.1, 0.0, 1.0]));
        let accent = t.id("palette.accent").unwrap();
        t.set_base(accent, Value::from([0.0, 0.0, 1.0, 1.0]));
        resolve(&mut t, 0);
        assert_eq!(t.value(tint), &Value::from([0.9, 0.1, 0.0, 1.0]), "changing to a fixed authored color removes the live binding");
    }
}
