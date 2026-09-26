//! Effects engine (§9.2): waveform effects over heads with phase spread across the stage
//! layout; rate in Hz or beats (beat clock); size/rate/active are addressable
//! (`lights.effect.<name>.{active,rate,size}`). Evaluation runs on the DMX thread and never
//! allocates.

use crate::rig::{Rig, slot};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    DimmerSine,
    DimmerSaw,
    DimmerSquare,
    ColorChase,
    Rainbow,
    Circle,
    Sparkle,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "dimmer_sine" | "sine" => Kind::DimmerSine,
            "dimmer_saw" | "saw" => Kind::DimmerSaw,
            "dimmer_square" | "square" => Kind::DimmerSquare,
            "color_chase" | "chase" => Kind::ColorChase,
            "rainbow" => Kind::Rainbow,
            "circle" | "pan_tilt_circle" => Kind::Circle,
            "sparkle" => Kind::Sparkle,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::DimmerSine => "dimmer_sine",
            Kind::DimmerSaw => "dimmer_saw",
            Kind::DimmerSquare => "dimmer_square",
            Kind::ColorChase => "color_chase",
            Kind::Rainbow => "rainbow",
            Kind::Circle => "circle",
            Kind::Sparkle => "sparkle",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// `rate` = cycles per second.
    Hz,
    /// `rate` = beats per cycle (follows `beat.phase` / `beat.bpm`).
    Beats,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    X,
    Y,
    Index,
    Radial,
}

/// A parsed effect definition (`lights/effects/<name>.toml`).
#[derive(Clone, Debug, PartialEq)]
pub struct EffectDef {
    pub name: String,
    pub label: String,
    pub kind: Kind,
    pub targets: Vec<String>,
    pub unit: Unit,
    pub rate: f32,
    pub size: f32,
    pub spread: f32,
    pub order: Order,
    pub colors: Vec<[f32; 3]>,
    pub duty: f32,
    pub decay_s: f32,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    label: Option<String>,
    kind: String,
    #[serde(default)]
    targets: Vec<String>,
    unit: Option<String>,
    rate: Option<f32>,
    size: Option<f32>,
    spread: Option<f32>,
    order: Option<String>,
    #[serde(default)]
    colors: Vec<String>,
    duty: Option<f32>,
    decay: Option<toml::Value>,
    #[serde(default)]
    notes: Option<String>,
}

impl EffectDef {
    pub fn parse(name: &str, t: &toml::Table) -> Result<EffectDef, String> {
        let r: Raw = toml::Value::Table(t.clone()).try_into().map_err(|e: toml::de::Error| e.message().to_string())?;
        let _ = r.notes;
        let kind = Kind::parse(&r.kind).ok_or_else(|| format!("unknown effect kind `{}`", r.kind))?;
        let unit = match r.unit.as_deref() {
            None | Some("hz") => Unit::Hz,
            Some("beats") | Some("beat") => Unit::Beats,
            Some(o) => return Err(format!("unknown unit `{o}` (hz | beats)")),
        };
        let order = match r.order.as_deref() {
            None | Some("x") => Order::X,
            Some("y") => Order::Y,
            Some("index") => Order::Index,
            Some("radial") => Order::Radial,
            Some(o) => return Err(format!("unknown order `{o}` (x | y | index | radial)")),
        };
        let mut colors = Vec::new();
        for c in &r.colors {
            let [cr, cg, cb, _] = se_proto::value::parse_hex_color(c).ok_or_else(|| format!("bad colour `{c}`"))?;
            colors.push([cr, cg, cb]);
        }
        if kind == Kind::ColorChase && colors.len() < 2 {
            return Err("color_chase needs at least two `colors`".into());
        }
        let rate = r.rate.unwrap_or(if unit == Unit::Beats { 1.0 } else { 0.5 });
        if !(rate.is_finite() && rate > 0.0) {
            return Err("`rate` must be > 0".into());
        }
        let decay_s = match &r.decay {
            None => 0.15,
            Some(toml::Value::String(s)) => se_proto::parse_duration_ms(s).ok_or_else(|| format!("bad decay `{s}`"))? as f32 / 1000.0,
            Some(v) => v.as_float().or_else(|| v.as_integer().map(|i| i as f64)).ok_or("bad `decay`")? as f32 / 1000.0,
        };
        Ok(EffectDef {
            name: name.into(),
            label: r.label.unwrap_or_else(|| name.to_string()),
            kind,
            targets: if r.targets.is_empty() { vec!["all".into()] } else { r.targets },
            unit,
            rate,
            size: r.size.unwrap_or(1.0).clamp(0.0, 1.0),
            spread: r.spread.unwrap_or(1.0),
            order,
            colors,
            duty: r.duty.unwrap_or(0.5).clamp(0.01, 0.99),
            decay_s: decay_s.max(0.01),
        })
    }

    /// Target leaf heads and their phase offsets (0..1) along `order`.
    pub fn layout(&self, rig: &Rig) -> Result<Vec<(usize, f32)>, String> {
        let mut heads: Vec<usize> = Vec::new();
        for t in &self.targets {
            for h in rig.leaves_for(t)? {
                if !heads.contains(&h) {
                    heads.push(h);
                }
            }
        }
        let key = |h: usize, i: usize| -> f32 {
            let p = rig.heads[h].position;
            match self.order {
                Order::X => p[0],
                Order::Y => p[1],
                Order::Index => i as f32,
                Order::Radial => ((p[0] - 0.5).powi(2) + (p[1] - 0.5).powi(2)).sqrt(),
            }
        };
        let keys: Vec<f32> = heads.iter().enumerate().map(|(i, h)| key(*h, i)).collect();
        let (lo, hi) = keys.iter().fold((f32::MAX, f32::MIN), |(a, b), k| (a.min(*k), b.max(*k)));
        let n = heads.len();
        Ok(heads
            .iter()
            .zip(keys)
            .map(|(h, k)| {
                // normalise to 0..(n-1)/n so the last head doesn't land on the first one's phase
                let span = hi - lo;
                let x = if span > 1e-6 { (k - lo) / span } else { 0.0 };
                (*h, if n > 1 { x * (n - 1) as f32 / n as f32 } else { 0.0 })
            })
            .collect())
    }
}

/// Continuous beat position from `beat.phase` (0..1 per beat) with wrap counting, falling back
/// to `beat.bpm` (or 120 BPM) when the phase signal is missing or frozen.
#[derive(Clone, Debug)]
pub struct BeatClock {
    beats: f64,
    last_phase: Option<f32>,
    last_change: f64,
    freewheel: bool,
    pos: f64,
    now: f64,
}

impl Default for BeatClock {
    fn default() -> Self {
        BeatClock { beats: 0.0, last_phase: None, last_change: f64::NEG_INFINITY, freewheel: false, pos: 0.0, now: 0.0 }
    }
}

impl BeatClock {
    /// Advance by `dt` seconds; returns the beat position.
    pub fn update(&mut self, dt: f64, phase: Option<f32>, bpm: Option<f32>) -> f64 {
        self.now += dt;
        let bpm = bpm.filter(|b| b.is_finite() && *b >= 20.0 && *b <= 400.0).unwrap_or(120.0) as f64;
        let Some(p) = phase.filter(|p| p.is_finite()).map(|p| p.rem_euclid(1.0)) else {
            self.pos += dt * bpm / 60.0;
            self.last_phase = None;
            self.freewheel = false;
            return self.pos;
        };
        let changed = self.last_phase != Some(p);
        if self.freewheel {
            if changed {
                // the phase moves again: re-align the counter to the freewheeled position
                self.beats = (self.pos - p as f64).round();
                self.freewheel = false;
                self.last_change = self.now;
                self.pos = self.beats + p as f64;
            } else {
                self.pos += dt * bpm / 60.0;
            }
        } else {
            match self.last_phase {
                Some(lp) if changed => {
                    if p < lp - 0.5 {
                        self.beats += 1.0;
                    } else if p > lp + 0.5 {
                        // stepped backwards across the wrap (jitter): undo a beat
                        self.beats -= 1.0;
                    }
                    self.last_change = self.now;
                }
                None => {
                    // align the counter so the position stays continuous
                    self.beats = (self.pos - p as f64).round();
                    self.last_change = self.now;
                }
                _ => {}
            }
            // frozen phase (analysis stopped): freewheel at bpm after one beat's time
            if self.now - self.last_change > (60.0 / bpm).max(0.25) {
                self.freewheel = true;
                self.pos += dt * bpm / 60.0;
            } else {
                self.pos = self.beats + p as f64;
            }
        }
        self.last_phase = Some(p);
        self.pos
    }
    pub fn position(&self) -> f64 {
        self.pos
    }
}

/// Per-effect runtime state (phase accumulator, sparkle envelopes).
#[derive(Clone, Debug, Default)]
pub struct EffectState {
    pub phase: f64,
    pub rng: u64,
    /// Sparkle envelope per target head.
    pub env: Vec<f32>,
}

fn frac(x: f64) -> f32 {
    x.rem_euclid(1.0) as f32
}

fn xorshift(s: &mut u64) -> f32 {
    let mut x = *s;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    *s = x;
    (x >> 40) as f32 / (1u64 << 24) as f32
}

pub fn hsv(h: f32, s: f32, v: f32) -> [f32; 3] {
    let h6 = h.rem_euclid(1.0) * 6.0;
    let i = h6.floor() as i32;
    let f = h6 - i as f32;
    let p = v * (1.0 - s);
    let q = v * (1.0 - s * f);
    let t = v * (1.0 - s * (1.0 - f));
    match i {
        0 => [v, t, p],
        1 => [q, v, p],
        2 => [p, v, t],
        3 => [p, q, v],
        4 => [t, p, v],
        _ => [v, p, q],
    }
}

/// Advance an effect's phase: Hz effects integrate `rate` (smooth when rate is modulated);
/// beat effects lock to the beat position.
pub fn advance(def: &EffectDef, st: &mut EffectState, rate: f32, dt: f64, beat_pos: f64) {
    let rate = if rate.is_finite() && rate > 0.0 { rate } else { def.rate };
    st.phase = match def.unit {
        Unit::Hz => (st.phase + dt * rate as f64).rem_euclid(1.0e6),
        Unit::Beats => beat_pos / rate as f64,
    };
}

/// Apply one effect to its target heads' slot values (`out[head]`), `size` 0..1.
pub fn apply(def: &EffectDef, st: &mut EffectState, heads: &[(usize, f32)], size: f32, dt: f64, rate: f32, out: &mut [[f32; crate::rig::SLOTS]]) {
    if size <= 0.0 {
        return;
    }
    let size = size.min(1.0);
    for (k, (h, off)) in heads.iter().enumerate() {
        let p = frac(st.phase - (def.spread * off) as f64);
        let o = &mut out[*h];
        match def.kind {
            Kind::DimmerSine => {
                let m = 0.5 + 0.5 * (std::f32::consts::TAU * p).sin();
                o[slot::INTENSITY] *= 1.0 - size + size * m;
            }
            Kind::DimmerSaw => {
                let m = 1.0 - p;
                o[slot::INTENSITY] *= 1.0 - size + size * m;
            }
            Kind::DimmerSquare => {
                let m = if p < def.duty { 1.0 } else { 0.0 };
                o[slot::INTENSITY] *= 1.0 - size + size * m;
            }
            Kind::ColorChase => {
                let n = def.colors.len();
                let c = def.colors[((p * n as f32) as usize).min(n - 1)];
                for (i, s) in [slot::RED, slot::GREEN, slot::BLUE].into_iter().enumerate() {
                    o[s] += (c[i] - o[s]) * size;
                }
            }
            Kind::Rainbow => {
                let c = hsv(p, 1.0, 1.0);
                for (i, s) in [slot::RED, slot::GREEN, slot::BLUE].into_iter().enumerate() {
                    o[s] += (c[i] - o[s]) * size;
                }
            }
            Kind::Circle => {
                let a = std::f32::consts::TAU * p;
                o[slot::PAN] = (o[slot::PAN] + 0.25 * size * a.cos()).clamp(0.0, 1.0);
                o[slot::TILT] = (o[slot::TILT] + 0.25 * size * a.sin()).clamp(0.0, 1.0);
            }
            Kind::Sparkle => {
                // `rate` = sparkles per second per head; each sparkle decays over `decay`.
                let env = &mut st.env[k];
                *env = (*env - dt as f32 / def.decay_s).max(0.0);
                let chance = (rate.max(0.0) as f64 * dt) as f32;
                if xorshift(&mut st.rng) < chance {
                    *env = 1.0;
                }
                let base = o[slot::INTENSITY];
                o[slot::INTENSITY] = base * (1.0 - size) + size * base.max(0.0) * *env;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rig::SLOTS;

    fn def(src: &str) -> EffectDef {
        EffectDef::parse("t", &toml::from_str(src).unwrap()).unwrap()
    }

    #[test]
    fn beat_clock_counts_wraps_and_freewheels() {
        let mut b = BeatClock::default();
        // 120 BPM synthetic phase at 44 Hz for 4 beats
        let dt = 1.0 / 44.0;
        let mut t = 0.0f64;
        let mut last = 0.0;
        for _ in 0..(44 * 2) {
            t += dt;
            let phase = (t * 2.0).fract() as f32;
            last = b.update(dt, Some(phase), Some(120.0));
        }
        assert!((last - 4.0).abs() < 0.05, "beat position {last} after 2 s at 120 BPM");
        // phase freezes → freewheels at bpm after a beat
        let frozen = (t * 2.0).fract() as f32;
        for _ in 0..44 {
            last = b.update(dt, Some(frozen), Some(120.0));
        }
        assert!(last > 5.0, "freewheeling continues ({last})");
        // no signal at all: internal clock
        let before = b.position();
        b.update(0.5, None, Some(60.0));
        assert!((b.position() - before - 0.5).abs() < 1e-9);
    }

    #[test]
    fn beat_chase_steps_on_beats_across_the_layout() {
        let d = def("kind = \"color_chase\"\nunit = \"beats\"\nrate = 4\ncolors = [\"#ff0000\", \"#00ff00\", \"#0000ff\", \"#ffffff\"]\nspread = 1.0");
        let heads = [(0usize, 0.0f32), (1, 0.25), (2, 0.5), (3, 0.75)];
        let mut st = EffectState::default();
        let mut out = vec![[0.0f32; SLOTS]; 4];
        let color = |o: &[f32; SLOTS]| [o[slot::RED], o[slot::GREEN], o[slot::BLUE]];
        for beat in 0..8 {
            advance(&d, &mut st, 4.0, 0.0, beat as f64 + 0.1);
            for o in out.iter_mut() {
                *o = [0.0; SLOTS];
            }
            apply(&d, &mut st, &heads, 1.0, 0.0, 4.0, &mut out);
            // head 0 shows colour index (beat mod 4); head k lags by k steps
            let expect = |i: usize| d.colors[i % 4];
            for (k, o) in out.iter().enumerate() {
                assert_eq!(color(o), expect((beat + 4 - k) % 4), "beat {beat} head {k}");
            }
        }
    }

    #[test]
    fn dimmer_effects_scale_intensity_by_size() {
        let d = def("kind = \"dimmer_square\"\nrate = 1\nduty = 0.5");
        let mut st = EffectState::default();
        let mut out = vec![[0.0f32; SLOTS]; 1];
        for (phase, size, expect) in [(0.25, 1.0, 1.0), (0.75, 1.0, 0.0), (0.75, 0.5, 0.5)] {
            st.phase = phase;
            out[0][slot::INTENSITY] = 1.0;
            apply(&d, &mut st, &[(0, 0.0)], size, 0.0, 1.0, &mut out);
            assert!((out[0][slot::INTENSITY] - expect).abs() < 1e-6, "phase {phase} size {size}");
        }
    }

    #[test]
    fn hz_rate_integrates_smoothly() {
        let d = def("kind = \"rainbow\"\nrate = 1");
        let mut st = EffectState::default();
        advance(&d, &mut st, 1.0, 0.5, 0.0);
        advance(&d, &mut st, 2.0, 0.25, 0.0);
        assert!((st.phase - 1.0).abs() < 1e-9);
    }

    #[test]
    fn parse_errors() {
        let e = |s: &str| EffectDef::parse("t", &toml::from_str(s).unwrap()).unwrap_err();
        assert!(e("kind = \"wobble\"").contains("unknown effect kind"));
        assert!(e("kind = \"color_chase\"\ncolors = [\"#ff0000\"]").contains("two"));
        assert!(e("kind = \"rainbow\"\nrate = 0").contains("rate"));
        assert!(e("kind = \"rainbow\"\nunit = \"bars\"").contains("unit"));
    }
}
