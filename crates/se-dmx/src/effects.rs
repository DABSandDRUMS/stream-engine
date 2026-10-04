//! Effects engine (§9.2): waveform effects over heads with phase spread across the stage
//! layout; rate in Hz or beats (beat clock); size/rate/active are addressable
//! (`lights.effect.<name>.{active,rate,size}`). Evaluation runs on the DMX thread and never
//! allocates.

use crate::palette::Spec;
use crate::rig::{Rig, slot};
use serde::Deserialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    DimmerSine,
    DimmerTriangle,
    DimmerSaw,
    DimmerSquare,
    ColorChase,
    ColorWave,
    Rainbow,
    Circle,
    Sparkle,
    Follow,
    FollowColor,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        Some(match s {
            "dimmer_sine" | "sine" => Kind::DimmerSine,
            "dimmer_triangle" | "triangle" => Kind::DimmerTriangle,
            "dimmer_saw" | "saw" => Kind::DimmerSaw,
            "dimmer_square" | "square" => Kind::DimmerSquare,
            "color_chase" | "chase" => Kind::ColorChase,
            "color_wave" | "wave" => Kind::ColorWave,
            "rainbow" => Kind::Rainbow,
            "circle" | "pan_tilt_circle" => Kind::Circle,
            "sparkle" => Kind::Sparkle,
            "follow" => Kind::Follow,
            "follow_color" => Kind::FollowColor,
            _ => return None,
        })
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::DimmerSine => "dimmer_sine",
            Kind::DimmerTriangle => "dimmer_triangle",
            Kind::DimmerSaw => "dimmer_saw",
            Kind::DimmerSquare => "dimmer_square",
            Kind::ColorChase => "color_chase",
            Kind::ColorWave => "color_wave",
            Kind::Rainbow => "rainbow",
            Kind::Circle => "circle",
            Kind::Sparkle => "sparkle",
            Kind::Follow => "follow",
            Kind::FollowColor => "follow_color",
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
    /// RGB literals or live addresses, using the shared palette reference syntax.
    pub colors: Vec<Spec>,
    pub duty: f32,
    pub decay_s: f32,
    pub signal: Option<String>,
    pub gain: f32,
    pub gate: f32,
    pub attack_s: f32,
    pub release_s: f32,
    pub invert: bool,
    /// Free lowercase tags (`tags = [...]`) for `lights.tags`.
    pub tags: Vec<String>,
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
    signal: Option<String>,
    gain: Option<f32>,
    gate: Option<f32>,
    attack: Option<toml::Value>,
    release: Option<toml::Value>,
    invert: Option<bool>,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
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
            let spec = Spec::parse(&toml::Value::String(c.clone()))?;
            match &spec {
                Spec::Literal(v) if v.as_color().is_some() => {}
                Spec::Address(_) => {}
                _ => return Err(format!("effect colour `{c}` must be a hex colour, stream:<slot> or @<address>")),
            }
            colors.push(spec);
        }
        if matches!(kind, Kind::ColorChase | Kind::ColorWave) && colors.len() < 2 {
            return Err(format!("{} needs at least two `colors`", kind.as_str()));
        }
        let follows = matches!(kind, Kind::Follow | Kind::FollowColor);
        if follows && r.signal.as_deref().is_none_or(|s| s.trim().is_empty()) {
            return Err(format!("{} requires a non-empty `signal` (for example band.kick)", kind.as_str()));
        }
        if follows && r.signal.as_deref().is_some_and(|s| !se_proto::address::is_valid(s, false)) {
            return Err("`signal` must be a valid signal address, for example band.kick".into());
        }
        if !follows && (r.signal.is_some() || r.gain.is_some() || r.gate.is_some() || r.attack.is_some() || r.release.is_some() || r.invert.is_some()) {
            return Err("signal/gain/gate/attack/release/invert are only valid for follow and follow_color".into());
        }
        if kind == Kind::FollowColor && colors.is_empty() {
            return Err("follow_color requires at least one `colors` entry".into());
        }
        let gain = r.gain.unwrap_or(1.0);
        if !gain.is_finite() || gain < 0.0 { return Err("`gain` must be finite and >= 0".into()); }
        let gate = r.gate.unwrap_or(0.0);
        if !gate.is_finite() || !(0.0..1.0).contains(&gate) { return Err("`gate` must be finite in 0..<1".into()); }
        let envelope_time = |value: &Option<toml::Value>, field: &str, default: f32| -> Result<f32, String> {
            let ms = match value {
                None => return Ok(default),
                Some(toml::Value::String(s)) => se_proto::parse_duration_ms(s).map(|ms| ms as f64).ok_or_else(|| format!("bad `{field}` duration `{s}`"))?,
                Some(v) => v.as_float().or_else(|| v.as_integer().map(|i| i as f64)).ok_or_else(|| format!("`{field}` must be a duration string or milliseconds"))?,
            };
            if !ms.is_finite() || ms < 0.0 || ms > f32::MAX as f64 { return Err(format!("`{field}` must be finite and >= 0")); }
            Ok((ms / 1000.0) as f32)
        };
        let attack_s = envelope_time(&r.attack, "attack", 0.0)?;
        let release_s = envelope_time(&r.release, "release", 0.15)?;
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
            signal: r.signal,
            gain,
            gate,
            attack_s,
            release_s,
            invert: r.invert.unwrap_or(false),
            tags: crate::tags::parse(r.tags)?,
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
        if self.order != Order::Index {
            if let Some(h) = heads.iter().find(|h| !rig.fixtures[rig.heads[**h].fixture].layout_verified) {
                return Err(format!("effect `{}` uses spatial order {:?}, but fixture `{}` has unverified layout; survey its position or use order = \"index\"", self.name, self.order, rig.fixtures[rig.heads[*h].fixture].id));
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

/// Follows the authoritative continuous `beat.position` publication when present.
/// Phase-only publishers and lost/stale signals use the same musical servo as audio.
#[derive(Clone, Debug)]
pub struct BeatClock {
    clock: se_clock::musical::MusicalClock,
    now: u64,
    last_phase: Option<f32>,
    last_position: Option<f32>,
    initialized: bool,
}

impl Default for BeatClock {
    fn default() -> Self {
        Self {
            clock: se_clock::musical::MusicalClock::new(0), now: 0,
            last_phase: None, last_position: None, initialized: false,
        }
    }
}

impl BeatClock {
    /// Advance by non-negative finite seconds; missing confidence permits legacy
    /// phase-only publishers. Repeated/frozen samples never refresh freshness.
    pub fn update(&mut self, dt: f64, phase: Option<f32>, bpm: Option<f32>, confidence: Option<f32>, position: Option<f32>) -> f64 {
        let first_sample = self.now == 0 && !self.initialized;
        if dt.is_finite() && dt > 0.0 {
            self.now = self.now.saturating_add((dt * 1e9) as u64);
        }
        let phase = phase.filter(|p| p.is_finite()).map(|p| p.rem_euclid(1.0));
        let position = position.filter(|p| p.is_finite() && *p >= 0.0);
        let bpm = bpm.filter(|b| se_clock::musical::MusicalClock::valid_bpm(*b));
        let conf = confidence.unwrap_or(1.0);
        let changed = position != self.last_position || phase != self.last_phase;
        if changed && conf.is_finite() {
            if let (Some(position), Some(bpm)) = (position, bpm) {
                // Recover precise fractional phase when f32 beat counts lose sub-beat
                // precision during long shows; choose the nearest matching beat.
                let raw = position as f64;
                let precise = phase.map_or(raw, |p| raw + (p as f64 - raw.rem_euclid(1.0) + 0.5).rem_euclid(1.0) - 0.5);
                // A lazy consumer (quantized control) may first sample minutes after
                // startup. That unsampled interval belongs to the publisher, not
                // to an independent 120 BPM fallback timeline.
                if first_sample {
                    self.clock = se_clock::musical::MusicalClock::new(self.now);
                }
                self.clock.follow(self.now, bpm, precise.max(0.0), conf);
                self.initialized = true;
            } else if let (Some(phase), Some(bpm)) = (phase, bpm) {
                if !self.initialized && conf >= 0.6 {
                    if first_sample {
                        self.clock = se_clock::musical::MusicalClock::new(self.now);
                    }
                    self.clock.follow(self.now, bpm, phase as f64, conf);
                    self.initialized = true;
                } else {
                    self.clock.observe(se_clock::musical::Observation { ts: self.now, bpm, phase, confidence: conf });
                }
            }
        } else if confidence.is_some_and(|c| !c.is_finite() || c < 0.35) && position.is_none() {
            self.clock.invalidate_source(self.now);
        }
        if phase.is_none() && position.is_none() && confidence.is_none() {
            if let Some(bpm) = bpm {
                let anchor = self.clock.snapshot();
                self.clock.follow(anchor.ts, bpm, anchor.position, 0.0);
            }
        }
        self.last_phase = phase;
        self.last_position = position;
        self.clock.advance(self.now).position
    }

    pub fn position(&self) -> f64 {
        self.clock.snapshot().position
    }
}

/// Per-effect runtime state (phase accumulator, sparkle envelopes).
#[derive(Clone, Debug, Default)]
pub struct EffectState {
    pub phase: f64,
    /// Last shared beat sample; beat-rate changes integrate without jumping phase.
    pub beat_position: Option<f64>,
    pub rng: u64,
    /// Sparkle envelope per target head.
    pub env: Vec<f32>,
    /// One envelope shared by every target; updated once per frame from a cached signal id.
    pub follow: f32,
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
    if matches!(def.kind, Kind::Follow | Kind::FollowColor) { return; }
    let rate = if rate.is_finite() && rate > 0.0 { rate } else { def.rate };
    st.phase = match def.unit {
        Unit::Hz => (st.phase + dt * rate as f64).rem_euclid(1.0e6),
        Unit::Beats => {
            let phase = match st.beat_position {
                Some(previous) => st.phase + (beat_pos - previous).max(0.0) / rate as f64,
                None => beat_pos / rate as f64,
            };
            st.beat_position = Some(beat_pos);
            phase
        }
    };
}

/// Gate/rescale, then invert, then smooth with separate attack and release time constants.
pub fn follow(def: &EffectDef, st: &mut EffectState, signal: f32, dt: f64) {
    let input = if signal.is_finite() { (signal * def.gain).clamp(0.0, 1.0) } else { 0.0 };
    let mut target = ((input - def.gate) / (1.0 - def.gate)).clamp(0.0, 1.0);
    if def.invert { target = 1.0 - target; }
    if target == st.follow { return; }
    let time = if target > st.follow { def.attack_s } else { def.release_s };
    let amount = if time <= 0.0 { 1.0 } else { 1.0 - (-(dt.max(0.0) as f32) / time).exp() };
    st.follow += (target - st.follow) * amount;
}

/// Apply one effect to its target heads' slot values (`out[head]`), `size` 0..1.
/// Coverage is layout-indexed; protected slot bits are indexed by global head.
/// Colors are pre-resolved once per frame; absent live endpoints relinquish RGB modulation.
pub fn apply(def: &EffectDef, st: &mut EffectState, colors: &[Option<[f32; 3]>], heads: &[(usize, f32)], coverage: &[bool], protected: &[u32], size: f32, dt: f64, rate: f32, out: &mut [[f32; crate::rig::SLOTS]]) {
    if size <= 0.0 {
        st.env.fill(0.0);
        return;
    }
    let size = size.min(1.0);
    let affected = match def.kind {
        Kind::DimmerSine | Kind::DimmerTriangle | Kind::DimmerSaw | Kind::DimmerSquare | Kind::Sparkle | Kind::Follow => 1 << slot::INTENSITY,
        Kind::ColorChase | Kind::ColorWave | Kind::Rainbow | Kind::FollowColor => (1 << slot::RED) | (1 << slot::GREEN) | (1 << slot::BLUE),
        Kind::Circle => (1 << slot::PAN) | (1 << slot::TILT),
    };
    for (k, (h, off)) in heads.iter().enumerate() {
        let protected = protected.get(*h).copied().unwrap_or(0);
        if !coverage.get(k).copied().unwrap_or(false) || protected & affected == affected {
            if let Some(env) = st.env.get_mut(k) { *env = 0.0; }
            continue;
        }
        let p = if matches!(def.kind, Kind::Follow | Kind::FollowColor) { 0.0 } else { frac(st.phase - (def.spread * off) as f64) };
        let o = &mut out[*h];
        match def.kind {
            Kind::Follow => {
                o[slot::INTENSITY] *= 1.0 - size + size * st.follow;
            }
            Kind::FollowColor => {
                let Some(Some(c)) = colors.first() else { continue };
                for (i, s) in [slot::RED, slot::GREEN, slot::BLUE].into_iter().enumerate() {
                    if protected & (1 << s) == 0 { o[s] += (c[i] - o[s]) * size * st.follow; }
                }
            }
            Kind::DimmerSine => {
                let m = 0.5 + 0.5 * (std::f32::consts::TAU * p).sin();
                o[slot::INTENSITY] *= 1.0 - size + size * m;
            }
            Kind::DimmerTriangle => {
                let m = 1.0 - (2.0 * p - 1.0).abs();
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
            Kind::ColorChase | Kind::ColorWave => {
                let n = colors.len();
                if n < 2 { continue; }
                let position = p * n as f32;
                let index = (position as usize).min(n - 1);
                let Some(mut c) = colors[index] else { continue };
                if def.kind == Kind::ColorWave {
                    let Some(next) = colors[(index + 1) % n] else { continue };
                    let t = position - index as f32;
                    let t = t * t * (3.0 - 2.0 * t);
                    for i in 0..3 {
                        c[i] += (next[i] - c[i]) * t;
                    }
                }
                for (i, s) in [slot::RED, slot::GREEN, slot::BLUE].into_iter().enumerate() {
                    if protected & (1 << s) == 0 {
                        o[s] += (c[i] - o[s]) * size;
                    }
                }
            }
            Kind::Rainbow => {
                let c = hsv(p, 1.0, 1.0);
                for (i, s) in [slot::RED, slot::GREEN, slot::BLUE].into_iter().enumerate() {
                    if protected & (1 << s) == 0 {
                        o[s] += (c[i] - o[s]) * size;
                    }
                }
            }
            Kind::Circle => {
                let a = std::f32::consts::TAU * p;
                if protected & (1 << slot::PAN) == 0 {
                    o[slot::PAN] = (o[slot::PAN] + 0.25 * size * a.cos()).clamp(0.0, 1.0);
                }
                if protected & (1 << slot::TILT) == 0 {
                    o[slot::TILT] = (o[slot::TILT] + 0.25 * size * a.sin()).clamp(0.0, 1.0);
                }
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

    fn literal_colors(d: &EffectDef) -> Vec<Option<[f32; 3]>> {
        d.colors.iter().map(|spec| match spec {
            Spec::Literal(v) => v.as_color().map(|[r, g, b, _]| [r, g, b]),
            _ => None,
        }).collect()
    }

    fn apply(d: &EffectDef, st: &mut EffectState, heads: &[(usize, f32)], coverage: &[bool], protected: &[u32], size: f32, dt: f64, rate: f32, out: &mut [[f32; SLOTS]]) {
        super::apply(d, st, &literal_colors(d), heads, coverage, protected, size, dt, rate, out);
    }

    #[test]
    fn follow_gate_gain_attack_release_and_invert_shape_one_shared_envelope() {
        let d = def("kind='follow'\nsignal='band.kick'\ngain=2\ngate=0.2\nattack='100ms'\nrelease='400ms'");
        let mut st = EffectState::default();
        follow(&d, &mut st, 0.1, 0.1);
        assert_eq!(st.follow, 0.0, "the gate removes quiet input");
        follow(&d, &mut st, 0.3, 0.1);
        let raised = 0.5 * (1.0 - (-1.0f32).exp());
        assert!((st.follow - raised).abs() < 1e-6);
        follow(&d, &mut st, 0.0, 0.4);
        assert!((st.follow - raised * (-1.0f32).exp()).abs() < 1e-6, "release uses its own time constant");
        let inverted = def("kind='follow'\nsignal='music.level'\ninvert=true\nattack=0\nrelease=0");
        follow(&inverted, &mut st, 0.25, 0.01);
        let mut out = [[0.0; SLOTS]; 2];
        for head in &mut out { head[slot::INTENSITY] = 0.8; }
        apply(&inverted, &mut st, &[(0, 0.0), (1, 0.5)], &[true, true], &[], 0.5, 0.01, 1.0, &mut out);
        for head in out { assert!((head[slot::INTENSITY] - 0.7).abs() < 1e-6, "spread never desynchronizes a signal envelope"); }
    }

    #[test]
    fn follow_color_blends_only_unprotected_rgb_and_keeps_intensity() {
        let d = def("kind='follow_color'\nsignal='band.snare'\ncolors=['#ff0000']\nattack=0\nrelease=0");
        let mut st = EffectState::default();
        follow(&d, &mut st, 0.5, 0.01);
        let mut out = [[0.0; SLOTS]];
        out[0][slot::GREEN] = 1.0;
        out[0][slot::BLUE] = 1.0;
        out[0][slot::INTENSITY] = 0.6;
        apply(&d, &mut st, &[(0, 0.0)], &[true], &[1 << slot::GREEN], 0.8, 0.01, 1.0, &mut out);
        assert!((out[0][slot::RED] - 0.4).abs() < 1e-6);
        assert_eq!(out[0][slot::GREEN], 1.0);
        assert!((out[0][slot::BLUE] - 0.6).abs() < 1e-6);
        assert_eq!(out[0][slot::INTENSITY], 0.6);
    }

    #[test]
    fn triangle_quarters_and_depth_only_attenuate_resolved_intensity() {
        let d = def("kind = \"dimmer_triangle\"\nspread = 0");
        for (phase, envelope) in [(0.0, 0.0), (0.25, 0.5), (0.5, 1.0), (0.75, 0.5), (1.0, 0.0)] {
            for depth in [0.0, 0.25, 1.0, 2.0] {
                let mut st = EffectState { phase, ..Default::default() };
                let mut out = [[0.0; SLOTS]];
                out[0][slot::INTENSITY] = 0.37;
                apply(&d, &mut st, &[(0, 0.0)], &[true], &[], depth, 0.0, 1.0, &mut out);
                let expected = 0.37 * (1.0 - depth.min(1.0) + depth.min(1.0) * envelope);
                assert!((out[0][slot::INTENSITY] - expected).abs() < 1e-6);
                assert!(out[0][slot::INTENSITY] <= 0.37);
            }
        }
    }

    #[test]
    fn color_wave_eases_between_authored_colors_and_wraps_without_a_step() {
        let d = def("kind = \"color_wave\"\ncolors = [\"#ff0000\", \"#0000ff\"]");
        for (phase, blue) in [(0.0, 0.0), (0.125, 0.15625), (0.25, 0.5), (0.5, 1.0), (0.75, 0.5), (1.0, 0.0)] {
            let mut st = EffectState { phase, ..Default::default() };
            let mut out = [[0.0; SLOTS]];
            out[0][slot::INTENSITY] = 0.2;
            apply(&d, &mut st, &[(0, 0.0)], &[true], &[], 1.0, 0.0, 1.0, &mut out);
            assert!((out[0][slot::RED] - (1.0 - blue)).abs() < 1e-6);
            assert!((out[0][slot::BLUE] - blue).abs() < 1e-6);
            assert_eq!(out[0][slot::INTENSITY], 0.2);
        }
    }

    #[test]
    fn smooth_color_cascade_blends_each_head_and_respects_channel_ownership() {
        let d = def("kind = \"color_wave\"\ncolors = [\"#ff0000\", \"#0000ff\"]\nspread = 1");
        let mut st = EffectState { phase: 0.25, ..Default::default() };
        let heads = [(3, 0.0), (1, 0.25), (0, 0.5), (2, 0.75)];
        let mut out = [[0.2; SLOTS]; 4];
        let protected = [0, 1 << slot::RED, 0, 0];
        apply(&d, &mut st, &heads, &[true, true, false, true], &protected, 0.5, 0.0, 1.0, &mut out);
        for (head, expected) in [(0, [0.2, 0.2, 0.2]), (1, [0.2, 0.1, 0.1]), (2, [0.1, 0.1, 0.6]), (3, [0.35, 0.1, 0.35])] {
            for (i, slot) in [slot::RED, slot::GREEN, slot::BLUE].into_iter().enumerate() {
                assert!((out[head][slot] - expected[i]).abs() < 1e-6, "head {head}, RGB component {i}");
            }
            assert_eq!(out[head][slot::INTENSITY], 0.2);
        }
    }

    #[test]
    fn independent_oscillators_and_rate_changes_keep_their_progress() {
        let hz = def("kind = \"dimmer_triangle\"\nunit = \"hz\"");
        let beats = def("kind = \"dimmer_triangle\"\nunit = \"beats\"");
        let mut fast = EffectState::default();
        let mut slow = EffectState::default();
        advance(&hz, &mut fast, 2.0, 0.125, 0.0);
        advance(&hz, &mut slow, 1.0, 0.125, 0.0);
        let mut out = [[0.2; SLOTS]; 2];
        apply(&hz, &mut fast, &[(0, 0.0)], &[true], &[], 1.0, 0.0, 2.0, &mut out);
        apply(&hz, &mut slow, &[(1, 0.0)], &[true], &[], 1.0, 0.0, 1.0, &mut out);
        assert!((out[0][slot::INTENSITY] - 0.1).abs() < 1e-6);
        assert!((out[1][slot::INTENSITY] - 0.05).abs() < 1e-6);
        advance(&hz, &mut slow, 2.0, 0.125, 0.0);
        assert!((slow.phase - 0.375).abs() < 1e-9, "Hz rate changes integrate from the current phase");

        let mut musical = EffectState::default();
        advance(&beats, &mut musical, 4.0, 0.0, 40.0);
        advance(&beats, &mut musical, 4.0, 0.0, 41.0);
        advance(&beats, &mut musical, 2.0, 0.0, 41.0);
        assert_eq!(musical.phase, 10.25, "changing beats/cycle cannot jump to beat_position/new_rate");
        advance(&beats, &mut musical, 2.0, 0.0, 41.5);
        out[0][slot::INTENSITY] = 0.2;
        apply(&beats, &mut musical, &[(0, 0.0)], &[true], &[], 1.0, 0.0, 2.0, &mut out);
        assert!((out[0][slot::INTENSITY] - 0.2).abs() < 1e-6, "new rate continues to the triangle peak");
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
            last = b.update(dt, Some(phase), Some(120.0), Some(1.0), None);
        }
        assert!((last - 4.0).abs() < 0.05, "beat position {last} after 2 s at 120 BPM");
        // phase freezes → freewheels at bpm after a beat
        let frozen = (t * 2.0).fract() as f32;
        for _ in 0..44 {
            last = b.update(dt, Some(frozen), Some(120.0), Some(1.0), None);
        }
        assert!(last > 5.0, "freewheeling continues ({last})");
        // no signal at all: internal clock
        let before = b.position();
        b.update(0.5, None, Some(60.0), None, None);
        assert!((b.position() - before - 0.5).abs() < 1e-9);
    }

    #[test]
    fn beat_publication_loss_invalid_values_and_reacquisition_never_reverse() {
        let mut b = BeatClock::default();
        let a = b.update(0.02, Some(0.2), Some(100.0), Some(0.9), Some(20.2));
        assert!((a - 20.2).abs() < 1e-5);
        let repeated = b.update(0.0, Some(0.2), Some(100.0), Some(0.9), Some(20.2));
        assert_eq!(a, repeated);
        let mut previous = a;
        for _ in 0..100 {
            let next = b.update(0.02, Some(0.2), Some(f32::NAN), Some(0.0), Some(20.2));
            assert!(next > previous);
            previous = next;
        }
        let resumed = b.update(0.02, Some(0.1), Some(100.0), Some(0.9), Some(23.1));
        assert!(resumed >= previous, "a late publication cannot undo freewheeled beats");
        let invalid = b.update(f64::NAN, Some(f32::NAN), Some(f32::NAN), Some(f32::NAN), Some(f32::NAN));
        assert_eq!(invalid, resumed);
    }

    #[test]
    fn first_late_sample_uses_the_shared_position_not_startup_fallback() {
        let mut clock = BeatClock::default();
        let position = clock.update(200.0, Some(0.25), Some(90.0), Some(1.0), Some(160.25));
        assert!((position - 160.25).abs() < 1e-6, "quantization must use the shared beat count");
        let next = clock.update(0.1, Some(0.4), Some(90.0), Some(1.0), Some(160.4));
        assert!((next - 160.4).abs() < 1e-5);
    }

    #[test]
    fn uncovered_heads_relinquish_effect_and_sparkle_envelopes() {
        let d = def("kind = \"sparkle\"");
        let mut st = EffectState { env: vec![1.0, 1.0], ..Default::default() };
        let heads = [(0, 0.0), (1, 0.5)];
        let mut out = [[0.0; SLOTS]; 2];
        out[0][slot::INTENSITY] = 0.7;
        out[1][slot::INTENSITY] = 0.8;
        apply(&d, &mut st, &heads, &[false, true], &[], 1.0, 0.0, 0.0, &mut out);
        assert_eq!(out[0][slot::INTENSITY], 0.7, "masked head is not owned by this effect");
        assert_eq!(st.env[0], 0.0);
        apply(&d, &mut st, &heads, &[true, false], &[], 1.0, 0.0, 0.0, &mut out);
        assert_eq!(out[0][slot::INTENSITY], 0.0, "reentry cannot reuse an old sparkle");
        assert_eq!(st.env[1], 0.0);
        st.env[0] = 1.0;
        apply(&d, &mut st, &heads, &[false, false], &[], 0.0, 0.0, 0.0, &mut out);
        assert_eq!(st.env[0], 0.0, "coverage releases even when effect size is zero");
    }

    #[test]
    fn protected_pan_does_not_block_tilt_and_masks_use_global_heads() {
        let d = def("kind = \"circle\"");
        let mut st = EffectState { phase: 0.125, ..Default::default() };
        let mut out = [[0.5; SLOTS]; 3];
        let protected = [1 << slot::TILT, 0, 1 << slot::PAN];
        apply(&d, &mut st, &[(2, 0.0), (0, 0.0)], &[true, false], &protected, 1.0, 0.0, 0.0, &mut out);
        assert_eq!(out[2][slot::PAN], 0.5);
        assert!((out[2][slot::TILT] - (0.5 + 0.25 / 2.0f32.sqrt())).abs() < 1e-6);
        assert_eq!(out[0], [0.5; SLOTS], "coverage remains layout-indexed");
        assert_eq!(out[1], [0.5; SLOTS], "untargeted head is unchanged");
    }

    #[test]
    fn protected_color_channels_and_intensity_remain_exact() {
        for src in [
            "kind = \"dimmer_sine\"",
            "kind = \"dimmer_triangle\"",
            "kind = \"dimmer_saw\"",
            "kind = \"dimmer_square\"",
            "kind = \"sparkle\"",
            "kind = \"rainbow\"",
            "kind = \"color_chase\"\ncolors = [\"#ff0000\", \"#00ff00\"]",
            "kind = \"color_wave\"\ncolors = [\"#ff0000\", \"#00ff00\"]",
        ] {
            let d = def(src);
            let mut st = EffectState { phase: 0.75, rng: 1, env: vec![1.0], ..Default::default() };
            let mut out = [[0.37; SLOTS]];
            let protected = [(1 << slot::INTENSITY) | (1 << slot::RED) | (1 << slot::GREEN) | (1 << slot::BLUE)];
            apply(&d, &mut st, &[(0, 0.0)], &[true], &protected, 1.0, 0.1, 100.0, &mut out);
            assert_eq!(out[0], [0.37; SLOTS], "{src}");
        }
        for src in ["kind = \"rainbow\"", "kind = \"color_chase\"\ncolors = [\"#ff0000\", \"#00ff00\"]", "kind = \"color_wave\"\ncolors = [\"#ff0000\", \"#00ff00\"]"] {
            let d = def(src);
            let mut st = EffectState { phase: 0.5, ..Default::default() };
            let mut out = [[0.37; SLOTS]];
            apply(&d, &mut st, &[(0, 0.0)], &[true], &[1 << slot::RED], 1.0, 0.0, 0.0, &mut out);
            assert_eq!(out[0][slot::RED], 0.37);
            assert_eq!(out[0][slot::GREEN], 1.0);
            assert_eq!(out[0][slot::BLUE], if d.kind == Kind::Rainbow { 1.0 } else { 0.0 });
            assert_eq!(out[0][slot::INTENSITY], 0.37);
        }
    }

    #[test]
    fn sparkle_protection_and_zero_size_clear_envelopes_before_reentry() {
        let d = def("kind = \"sparkle\"");
        for (coverage, protected, size) in [(false, 0, 1.0), (true, 1 << slot::INTENSITY, 1.0), (true, 0, 0.0)] {
            let mut st = EffectState { rng: 1, env: vec![1.0], ..Default::default() };
            let mut out = [[0.0; SLOTS]];
            out[0][slot::INTENSITY] = 0.8;
            apply(&d, &mut st, &[(0, 0.0)], &[coverage], &[protected], size, 0.0, 0.0, &mut out);
            assert_eq!(out[0][slot::INTENSITY], 0.8);
            assert_eq!(st.env[0], 0.0);
            apply(&d, &mut st, &[(0, 0.0)], &[true], &[0], 1.0, 0.0, 0.0, &mut out);
            assert_eq!(out[0][slot::INTENSITY], 0.0, "released protection cannot resurrect an old sparkle");
            out[0][slot::INTENSITY] = 0.8;
            apply(&d, &mut st, &[(0, 0.0)], &[true], &[0], 1.0, 0.1, 10.0, &mut out);
            assert_eq!(out[0][slot::INTENSITY], 0.8, "reentry can produce a new sparkle");
        }
    }

    #[test]
    fn spatial_order_requires_survey_but_logical_index_does_not() {
        let lib = crate::profile::library(&Default::default(), &mut Vec::new());
        let mut rig = Rig::compile(&toml::from_str("[fixtures.a]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\nlayout_verified = false").unwrap(), lib, Vec::new()).unwrap();
        for order in ["x", "y", "radial"] {
            let d = def(&format!("kind = \"rainbow\"\norder = \"{order}\""));
            assert!(d.layout(&rig).unwrap_err().contains("unverified layout"));
        }
        assert_eq!(def("kind = \"rainbow\"\norder = \"index\"").layout(&rig).unwrap(), vec![(0, 0.0)]);
        rig.fixtures[0].layout_verified = true;
        assert_eq!(def("kind = \"rainbow\"\norder = \"x\"").layout(&rig).unwrap(), vec![(0, 0.0)]);
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
            apply(&d, &mut st, &heads, &[true; 4], &[], 1.0, 0.0, 4.0, &mut out);
            // head 0 shows colour index (beat mod 4); head k lags by k steps
            let colors = literal_colors(&d);
            let expect = |i: usize| colors[i % 4].unwrap();
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
            apply(&d, &mut st, &[(0, 0.0)], &[true], &[], size, 0.0, 1.0, &mut out);
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
        for (src, field) in [
            ("kind='follow'", "signal"),
            ("kind='follow_color'\nsignal='band.kick'", "colors"),
            ("kind='follow'\nsignal='band.kick'\ngate=1", "gate"),
            ("kind='follow'\nsignal='band.kick'\ngain=-1", "gain"),
            ("kind='follow'\nsignal='band.kick'\nattack=-1", "attack"),
            ("kind='follow'\nsignal='band.kick'\nrelease='wrong'", "release"),
            ("kind='rainbow'\nsignal='band.kick'", "only valid"),
        ] { assert!(e(src).contains(field), "{src}: {}", e(src)); }
    }
}
