//! Allocation-free state access for the render thread.
//!
//! Every address the renderer reads is interned into an [`AddrTable`] when the plan is built
//! (IO side). On the render thread, [`Resolved`] maps those ids to indices of the current
//! [`Snapshot`] — re-resolved only when the snapshot's index changes — so per-frame reads are
//! plain vector lookups without string formatting or hashing.
//!
//! `when` expressions are evaluated through [`Whens`]: each expression is re-evaluated only
//! when a fingerprint of the values it reads changes, so steady-state frames never touch the
//! (allocating) expression evaluator.

use se_hub::Snapshot;
use se_proto::Value;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

/// Index into an [`AddrTable`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AddrId(pub u32);

/// Interned address strings (state addresses or signal names).
#[derive(Clone, Debug, Default)]
pub struct AddrTable {
    names: Vec<String>,
    index: HashMap<String, AddrId>,
}

impl AddrTable {
    pub fn intern(&mut self, name: &str) -> AddrId {
        if let Some(id) = self.index.get(name) {
            return *id;
        }
        let id = AddrId(self.names.len() as u32);
        self.names.push(name.to_string());
        self.index.insert(name.to_string(), id);
        id
    }

    pub fn name(&self, id: AddrId) -> &str {
        &self.names[id.0 as usize]
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn names(&self) -> &[String] {
        &self.names
    }
}

const MISSING: u32 = u32::MAX;

/// Snapshot indices for an [`AddrTable`] (state) and a signal table.
#[derive(Default)]
pub struct Resolved {
    state: Vec<u32>,
    signals: Vec<u32>,
    state_index: Option<Arc<HashMap<String, usize>>>,
    signal_index: Option<Arc<HashMap<String, usize>>>,
    state_len: usize,
    signal_len: usize,
}

impl Resolved {
    /// Re-resolve if the snapshot's index maps changed (or the tables grew). Allocates only
    /// when the tables themselves changed size (plan swap).
    pub fn update(&mut self, snap: &Snapshot, state: &AddrTable, signals: &AddrTable) {
        let state_changed = self.state_len != state.len() || !self.state_index.as_ref().is_some_and(|a| Arc::ptr_eq(a, &snap.index));
        if state_changed {
            self.state.resize(state.len(), MISSING);
            for (i, n) in state.names().iter().enumerate() {
                self.state[i] = snap.index.get(n.as_str()).map_or(MISSING, |v| *v as u32);
            }
            self.state_index = Some(snap.index.clone());
            self.state_len = state.len();
        }
        let sig_changed = self.signal_len != signals.len() || !self.signal_index.as_ref().is_some_and(|a| Arc::ptr_eq(a, &snap.signal_index));
        if sig_changed {
            self.signals.resize(signals.len(), MISSING);
            for (i, n) in signals.names().iter().enumerate() {
                self.signals[i] = snap.signal_index.get(n.as_str()).map_or(MISSING, |v| *v as u32);
            }
            self.signal_index = Some(snap.signal_index.clone());
            self.signal_len = signals.len();
        }
    }

    /// Force a full re-resolution on the next [`update`](Self::update).
    pub fn invalidate(&mut self) {
        self.state_index = None;
        self.signal_index = None;
    }

    #[inline]
    pub fn get<'s>(&self, snap: &'s Snapshot, id: AddrId) -> Option<&'s Value> {
        let i = *self.state.get(id.0 as usize)?;
        if i == MISSING { None } else { snap.values.get(i as usize) }
    }

    #[inline]
    pub fn f32(&self, snap: &Snapshot, id: AddrId, default: f32) -> f32 {
        self.get(snap, id).and_then(Value::as_f32).filter(|v| v.is_finite()).unwrap_or(default)
    }

    #[inline]
    pub fn bool(&self, snap: &Snapshot, id: AddrId, default: bool) -> bool {
        self.get(snap, id).map_or(default, Value::truthy)
    }

    #[inline]
    pub fn i64(&self, snap: &Snapshot, id: AddrId, default: i64) -> i64 {
        self.get(snap, id).and_then(Value::as_i64).unwrap_or(default)
    }

    #[inline]
    pub fn str<'s>(&self, snap: &'s Snapshot, id: AddrId) -> Option<&'s str> {
        self.get(snap, id).and_then(Value::as_str)
    }

    /// vec4/color: lists of 3–4 numbers, hex strings, or a single number broadcast.
    #[inline]
    pub fn vec4(&self, snap: &Snapshot, id: AddrId, default: [f32; 4]) -> [f32; 4] {
        match self.get(snap, id) {
            Some(Value::List(l)) if l.len() >= 3 => {
                let c = |i: usize, d: f32| l.get(i).and_then(Value::as_f32).filter(|v| v.is_finite()).unwrap_or(d);
                [c(0, default[0]), c(1, default[1]), c(2, default[2]), c(3, default[3])]
            }
            Some(v @ Value::Str(_)) => v.as_color().unwrap_or(default),
            _ => default,
        }
    }

    #[inline]
    pub fn signal(&self, snap: &Snapshot, id: AddrId) -> f32 {
        match self.signals.get(id.0 as usize) {
            Some(&i) if i != MISSING => snap.signals.get(i as usize).copied().filter(|v| v.is_finite()).unwrap_or(0.0),
            _ => 0.0,
        }
    }
}

/// A parsed `when` clause plus the addresses it reads.
pub struct WhenPlan {
    pub expr: se_expr::Expr,
    /// State addresses read (with the core's aliases `mode`, `scene`, `preview` mapped).
    pub reads: Vec<AddrId>,
    /// Paths that were not state addresses: looked up as signals.
    pub signal_reads: Vec<AddrId>,
}

/// The core's expression aliases (§2.5).
pub fn alias(path: &str) -> &str {
    match path {
        "mode" => "show.mode",
        "scene" => "show.scene.program",
        "preview" => "show.scene.preview",
        p => p,
    }
}

impl WhenPlan {
    pub fn new(src: &str, state: &mut AddrTable, signals: &mut AddrTable) -> Result<WhenPlan, String> {
        let expr = se_expr::Expr::parse(src).map_err(|e| format!("`{src}`: {}", e.msg))?;
        let mut reads = Vec::new();
        let mut signal_reads = Vec::new();
        for p in expr.paths() {
            reads.push(state.intern(alias(&p)));
            signal_reads.push(signals.intern(&p));
        }
        Ok(WhenPlan { expr, reads, signal_reads })
    }
}

/// Per-frame evaluation cache for a plan's `when` clauses.
#[derive(Default)]
pub struct Whens {
    fingerprints: Vec<u64>,
    results: Vec<bool>,
}

struct SnapScope<'a> {
    snap: &'a Snapshot,
}

impl se_expr::Scope for SnapScope<'_> {
    fn lookup(&self, path: &str) -> Value {
        if let Some(v) = self.snap.get(alias(path)) {
            return v.clone();
        }
        if let Some(v) = self.snap.signal(path) {
            return Value::Float(v as f64);
        }
        Value::Null
    }
}

fn hash_value(v: Option<&Value>, h: &mut impl Hasher) {
    match v {
        None => 0u8.hash(h),
        Some(Value::Null) => 1u8.hash(h),
        Some(Value::Bool(b)) => (2u8, *b).hash(h),
        Some(Value::Int(i)) => (3u8, *i).hash(h),
        Some(Value::Float(f)) => (4u8, f.to_bits()).hash(h),
        Some(Value::Str(s)) => (5u8, s.as_str()).hash(h),
        Some(Value::List(l)) => {
            (6u8, l.len()).hash(h);
            for x in l {
                hash_value(Some(x), h);
            }
        }
        Some(Value::Map(m)) => {
            (7u8, m.len()).hash(h);
            for (k, x) in m {
                k.hash(h);
                hash_value(Some(x), h);
            }
        }
    }
}

/// FNV-1a: allocation-free, deterministic.
struct Fnv(u64);

impl Hasher for Fnv {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for b in bytes {
            self.0 ^= *b as u64;
            self.0 = self.0.wrapping_mul(0x100_0000_01b3);
        }
    }
}

impl Whens {
    /// Size the cache for a plan (IO side / plan swap).
    pub fn reset(&mut self, count: usize) {
        self.fingerprints.clear();
        self.fingerprints.resize(count, u64::MAX);
        self.results.clear();
        self.results.resize(count, true);
    }

    /// Evaluate `plans[i]`, re-running the expression only when an input changed.
    pub fn eval(&mut self, i: usize, plan: &WhenPlan, snap: &Snapshot, res: &Resolved) -> bool {
        let mut h = Fnv(0xcbf2_9ce4_8422_2325);
        for (a, s) in plan.reads.iter().zip(&plan.signal_reads) {
            let v = res.get(snap, *a);
            hash_value(v, &mut h);
            if v.is_none() {
                res.signal(snap, *s).to_bits().hash(&mut h);
            }
        }
        let fp = h.finish();
        if self.fingerprints[i] != fp {
            self.fingerprints[i] = fp;
            self.results[i] = plan.expr.eval_bool(&SnapScope { snap });
        }
        self.results[i]
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn snapshot(values: &[(&str, Value)], signals: &[(&str, f32)]) -> Snapshot {
        Snapshot {
            index: Arc::new(values.iter().enumerate().map(|(i, (k, _))| (k.to_string(), i)).collect()),
            values: values.iter().map(|(_, v)| v.clone()).collect(),
            signal_names: Arc::new(signals.iter().map(|(k, _)| k.to_string()).collect()),
            signal_index: Arc::new(signals.iter().enumerate().map(|(i, (k, _))| (k.to_string(), i)).collect()),
            signals: signals.iter().map(|(_, v)| *v).collect(),
            ..Default::default()
        }
    }

    #[test]
    fn resolves_and_reads_typed_values() {
        let mut t = AddrTable::default();
        let sig = AddrTable::default();
        let a = t.intern("fx.vhs.amount");
        let b = t.intern("scene.duo.node.x.rect.wide");
        let c = t.intern("missing.addr");
        assert_eq!(t.intern("fx.vhs.amount"), a);
        let snap = snapshot(&[("scene.duo.node.x.rect.wide", Value::from([0.1f32, 0.2, 0.3, 0.4])), ("fx.vhs.amount", Value::Float(0.5))], &[]);
        let mut r = Resolved::default();
        r.update(&snap, &t, &sig);
        assert_eq!(r.f32(&snap, a, 0.0), 0.5);
        assert_eq!(r.vec4(&snap, b, [0.0; 4]), [0.1, 0.2, 0.3, 0.4]);
        assert_eq!(r.f32(&snap, c, 7.0), 7.0);
        // a new snapshot with a different index re-resolves
        let snap2 = snapshot(&[("fx.vhs.amount", Value::Int(1)), ("missing.addr", Value::Bool(true))], &[]);
        r.update(&snap2, &t, &sig);
        assert_eq!(r.f32(&snap2, a, 0.0), 1.0);
        assert!(r.bool(&snap2, c, false));
        assert_eq!(r.vec4(&snap2, b, [9.0; 4]), [9.0; 4]);
    }

    #[test]
    fn when_uses_core_aliases_signals_and_caches() {
        let mut t = AddrTable::default();
        let mut s = AddrTable::default();
        let w = WhenPlan::new("mode == 'chill' && band.level > 0.5", &mut t, &mut s).unwrap();
        let mut whens = Whens::default();
        whens.reset(1);
        let snap = snapshot(&[("show.mode", Value::Str("chill".into()))], &[("band.level", 0.8)]);
        let mut r = Resolved::default();
        r.update(&snap, &t, &s);
        assert!(whens.eval(0, &w, &snap, &r));
        let snap2 = snapshot(&[("show.mode", Value::Str("live".into()))], &[("band.level", 0.8)]);
        r.update(&snap2, &t, &s);
        assert!(!whens.eval(0, &w, &snap2, &r));
        let snap3 = snapshot(&[("show.mode", Value::Str("chill".into()))], &[("band.level", 0.1)]);
        r.update(&snap3, &t, &s);
        assert!(!whens.eval(0, &w, &snap3, &r));
        assert!(WhenPlan::new("mode ==", &mut t, &mut s).is_err());
    }

    #[test]
    fn steady_state_eval_does_not_allocate() {
        let mut t = AddrTable::default();
        let mut s = AddrTable::default();
        let w = WhenPlan::new("mode == 'chill'", &mut t, &mut s).unwrap();
        let snap = snapshot(&[("show.mode", Value::Str("chill".into()))], &[]);
        let mut r = Resolved::default();
        r.update(&snap, &t, &s);
        let mut whens = Whens::default();
        whens.reset(1);
        assert!(whens.eval(0, &w, &snap, &r));
        let scope = se_alloc::Scope::begin();
        for _ in 0..100 {
            r.update(&snap, &t, &s);
            assert!(whens.eval(0, &w, &snap, &r));
        }
        assert_eq!(scope.allocs(), 0);
    }
}
