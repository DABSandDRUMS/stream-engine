//! Signal registry (§2.4): named continuous values with short history rings, plus LFOs.

use crate::config::LfoDef;
use se_proto::{Ts, Value};
use std::collections::HashMap;

pub const HISTORY: usize = 256;

#[derive(Clone)]
pub struct Ring {
    buf: Box<[f32; HISTORY]>,
    head: usize,
    len: usize,
}

impl Default for Ring {
    fn default() -> Self {
        Ring { buf: Box::new([0.0; HISTORY]), head: 0, len: 0 }
    }
}

impl Ring {
    pub fn push(&mut self, v: f32) {
        self.buf[self.head] = v;
        self.head = (self.head + 1) % HISTORY;
        self.len = (self.len + 1).min(HISTORY);
    }
    /// Oldest → newest.
    pub fn values(&self) -> Vec<f32> {
        let start = (self.head + HISTORY - self.len) % HISTORY;
        (0..self.len).map(|k| self.buf[(start + k) % HISTORY]).collect()
    }
}

#[derive(Default)]
pub struct Signals {
    names: Vec<String>,
    index: HashMap<String, usize>,
    values: Vec<f32>,
    updated: Vec<Ts>,
    hist: Vec<Ring>,
    pub generation: u64,
}

impl Signals {
    pub fn id(&self, name: &str) -> Option<usize> {
        self.index.get(name).copied()
    }
    pub fn ensure(&mut self, name: &str) -> usize {
        if let Some(i) = self.id(name) {
            return i;
        }
        let i = self.names.len();
        self.names.push(name.to_string());
        self.index.insert(name.to_string(), i);
        self.values.push(0.0);
        self.updated.push(0);
        self.hist.push(Ring::default());
        self.generation += 1;
        i
    }
    pub fn set(&mut self, name: &str, v: f32, ts: Ts) {
        let i = self.ensure(name);
        self.set_id(i, v, ts);
    }
    pub fn set_id(&mut self, i: usize, v: f32, ts: Ts) {
        self.values[i] = if v.is_finite() { v } else { 0.0 };
        self.updated[i] = ts;
    }
    pub fn get(&self, name: &str) -> Option<f32> {
        self.id(name).map(|i| self.values[i])
    }
    pub fn value(&self, i: usize) -> f32 {
        self.values[i]
    }
    pub fn names(&self) -> &[String] {
        &self.names
    }
    pub fn values(&self) -> &[f32] {
        &self.values
    }
    pub fn history(&self, name: &str) -> Option<Vec<f32>> {
        self.id(name).map(|i| self.hist[i].values())
    }
    /// Sample every signal into its history ring (called at the history rate).
    pub fn sample_history(&mut self) {
        for (i, h) in self.hist.iter_mut().enumerate() {
            h.push(self.values[i]);
        }
    }
    pub fn iter(&self) -> impl Iterator<Item = (&str, f32)> {
        self.names.iter().map(String::as_str).zip(self.values.iter().copied())
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    Sine,
    Tri,
    Saw,
    Square,
    Random,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Rate {
    Hz(f64),
    /// Cycles per `n` beats (follows `beat.phase`/`beat.bpm`).
    Beats(f64),
}

#[derive(Clone, Debug)]
pub struct Lfo {
    pub name: String,
    pub shape: Shape,
    pub rate: Rate,
    pub phase: f64,
    sig: usize,
    beats_acc: f64,
    last_beat_phase: f64,
}

impl Lfo {
    pub fn from_def(name: &str, d: &LfoDef, sigs: &mut Signals) -> Result<Lfo, String> {
        let shape = match d.shape.as_str() {
            "sine" => Shape::Sine,
            "tri" | "triangle" => Shape::Tri,
            "saw" => Shape::Saw,
            "square" => Shape::Square,
            "random" => Shape::Random,
            s => return Err(format!("lfo `{name}`: unknown shape `{s}`")),
        };
        let rate = parse_rate(&d.rate).ok_or_else(|| format!("lfo `{name}`: bad rate `{}`", d.rate))?;
        let full = if name.starts_with("lfo.") { name.to_string() } else { format!("lfo.{name}") };
        let sig = sigs.ensure(&full);
        Ok(Lfo { name: full, shape, rate, phase: d.phase, sig, beats_acc: 0.0, last_beat_phase: 0.0 })
    }

    /// Update from master time (seconds) and the beat clock.
    pub fn tick(&mut self, t_secs: f64, beat_phase: f32, sigs: &mut Signals, ts: Ts) {
        let cycles = match self.rate {
            Rate::Hz(hz) => t_secs * hz,
            Rate::Beats(n) => {
                let bp = beat_phase as f64;
                let mut d = bp - self.last_beat_phase;
                if d < -0.5 {
                    d += 1.0;
                }
                if d > 0.0 && d < 0.5 {
                    self.beats_acc += d;
                }
                self.last_beat_phase = bp;
                self.beats_acc / n.max(1e-6)
            }
        } + self.phase;
        let p = cycles.rem_euclid(1.0);
        let v = match self.shape {
            Shape::Sine => 0.5 - 0.5 * (p * std::f64::consts::TAU).cos(),
            Shape::Tri => 1.0 - (2.0 * p - 1.0).abs(),
            Shape::Saw => p,
            Shape::Square => {
                if p < 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
            Shape::Random => hash01(cycles.floor() as i64 ^ (self.sig as i64) << 20),
        };
        sigs.set_id(self.sig, v as f32, ts);
    }
}

fn hash01(x: i64) -> f64 {
    let mut z = (x as u64).wrapping_add(0x9E3779B97F4A7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

pub fn parse_rate(v: &Value) -> Option<Rate> {
    match v {
        Value::Int(_) | Value::Float(_) => Some(Rate::Hz(v.as_f64()?)),
        Value::Str(s) => {
            let s = s.trim().to_lowercase();
            if let Some(n) = s.strip_suffix("hz") {
                return n.trim().parse().ok().map(Rate::Hz);
            }
            for suf in ["beats", "beat", "b"] {
                if let Some(n) = s.strip_suffix(suf) {
                    let n = n.trim();
                    return if n.is_empty() { Some(Rate::Beats(1.0)) } else { n.parse().ok().map(Rate::Beats) };
                }
            }
            s.parse().ok().map(Rate::Hz)
        }
        _ => None,
    }
}

/// Built-in LFOs present in every project (project `[lfo.*]` may override them).
pub fn builtin_lfos() -> Vec<(&'static str, LfoDef)> {
    vec![
        ("slow", LfoDef { shape: "sine".into(), rate: Value::Float(0.1), phase: 0.0 }),
        ("mid", LfoDef { shape: "sine".into(), rate: Value::Float(0.5), phase: 0.0 }),
        ("fast", LfoDef { shape: "sine".into(), rate: Value::Float(2.0), phase: 0.0 }),
        ("beat", LfoDef { shape: "saw".into(), rate: Value::Str("1beat".into()), phase: 0.0 }),
        ("bar", LfoDef { shape: "saw".into(), rate: Value::Str("4beats".into()), phase: 0.0 }),
        ("random", LfoDef { shape: "random".into(), rate: Value::Float(1.0), phase: 0.0 }),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rates() {
        assert_eq!(parse_rate(&Value::from("0.1hz")), Some(Rate::Hz(0.1)));
        assert_eq!(parse_rate(&Value::from("4beats")), Some(Rate::Beats(4.0)));
        assert_eq!(parse_rate(&Value::from("beat")), Some(Rate::Beats(1.0)));
        assert_eq!(parse_rate(&Value::Float(2.0)), Some(Rate::Hz(2.0)));
    }

    #[test]
    fn lfo_shapes_in_range() {
        let mut s = Signals::default();
        let mut l = Lfo::from_def("x", &LfoDef { shape: "sine".into(), rate: Value::Float(1.0), phase: 0.0 }, &mut s).unwrap();
        l.tick(0.0, 0.0, &mut s, 0);
        assert!(s.get("lfo.x").unwrap().abs() < 1e-6);
        l.tick(0.5, 0.0, &mut s, 0);
        assert!((s.get("lfo.x").unwrap() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn beat_lfo_follows_phase() {
        let mut s = Signals::default();
        let mut l = Lfo::from_def("b", &LfoDef { shape: "saw".into(), rate: Value::from("2beats"), phase: 0.0 }, &mut s).unwrap();
        let mut ph = 0.0f32;
        for _ in 0..100 {
            ph = (ph + 0.01) % 1.0;
            l.tick(0.0, ph, &mut s, 0);
        }
        // one beat advanced over two-beat cycle → ~0.5
        assert!((s.get("lfo.b").unwrap() - 0.5).abs() < 0.02, "{}", s.get("lfo.b").unwrap());
    }

    #[test]
    fn history_ring() {
        let mut r = Ring::default();
        for i in 0..300 {
            r.push(i as f32);
        }
        let v = r.values();
        assert_eq!(v.len(), HISTORY);
        assert_eq!(*v.last().unwrap(), 299.0);
        assert_eq!(v[0], (300 - HISTORY) as f32);
    }
}
