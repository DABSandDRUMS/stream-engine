//! Real-time audio DSP for stream-engine (§8.5, §8.6).
//!
//! * [`Effect`]: one stereo insert effect (filters, dynamics, drive, time, pitch, performance).
//!   Every effect is created by name through [`registry`] with parameter metadata
//!   ([`ParamSpec`]) so the engine can declare `audio.bus.<b>.fx.<e>.<param>` addresses.
//! * [`slot::FxSlot`]: wraps an effect with wet/dry, bypass crossfade, trigger envelope, and
//!   dry-path latency compensation; [`slot::FxChain`] is an ordered list of slots.
//! * [`sampler`]: one-shot / multi-sample player (sound effects, drum layering).
//! * [`wasm`] (feature `wasm`): `dsp` WebAssembly patches as effects (fixed block ABI).
//!
//! Real-time contract: after construction nothing here allocates, locks, or does I/O in
//! `process`/`set_param`/`trigger`. Buffers are sized for [`MAX_BLOCK`] frames up front.

// Delay networks and filter banks index several parallel arrays by the same position; index
// loops are the clearest form there.
#![allow(clippy::needless_range_loop)]

pub mod fx;
pub mod registry;
pub mod sampler;
pub mod slot;
pub mod smooth;
pub mod util;
#[cfg(feature = "wasm")]
pub mod wasm;

pub use registry::{create, kinds, params_of};
pub use slot::{FxChain, FxSlot};
pub use smooth::Smoother;

/// Largest block (frames) any `process` call receives. Callers split bigger periods.
pub const MAX_BLOCK: usize = 1024;

/// Musical time for tempo-synced effects. The engine derives it from `beat.bpm`/`beat.phase`
/// (or tap tempo) and advances it sample-accurately per block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Transport {
    /// Tempo (> 0).
    pub bpm: f32,
    /// Running beat position at the first frame of the block (fractional part = beat phase).
    pub beat: f64,
    /// Beats per bar (downbeat every `beats_per_bar` beats).
    pub beats_per_bar: u32,
}

impl Default for Transport {
    fn default() -> Self {
        Transport { bpm: 120.0, beat: 0.0, beats_per_bar: 4 }
    }
}

impl Transport {
    /// Samples per beat at sample rate `sr`.
    pub fn samples_per_beat(&self, sr: f32) -> f64 {
        60.0 * sr as f64 / self.bpm.max(1.0) as f64
    }

    /// Beat position at `frame` frames into the block.
    pub fn beat_at(&self, frame: usize, sr: f32) -> f64 {
        self.beat + frame as f64 / self.samples_per_beat(sr)
    }

    /// Beat phase 0–1 at the block start.
    pub fn phase(&self) -> f32 {
        self.beat.rem_euclid(1.0) as f32
    }

    /// Bar phase 0–1 at the block start.
    pub fn bar_phase(&self) -> f32 {
        let bpb = self.beats_per_bar.max(1) as f64;
        (self.beat.rem_euclid(bpb) / bpb) as f32
    }
}

/// Per-block processing context.
pub struct Ctx<'a> {
    /// Sample rate (Hz).
    pub sr: f32,
    pub transport: Transport,
    /// Sidechain key (mono, same length as the block) for effects with a `key` input
    /// (sidechain compressor, ducking gate). Empty when no key is routed.
    pub key: &'a [f32],
}

/// How a parameter is presented and interpreted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ParamKind {
    Float,
    /// 0 = off, 1 = on.
    Bool,
    /// Value is the option index (0-based).
    Choice(&'static [&'static str]),
}

/// Metadata for one effect parameter (becomes `…fx.<e>.<name>` in the state tree).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamSpec {
    pub name: &'static str,
    pub min: f32,
    pub max: f32,
    pub default: f32,
    pub unit: &'static str,
    pub kind: ParamKind,
    pub description: &'static str,
}

impl ParamSpec {
    pub const fn float(name: &'static str, min: f32, max: f32, default: f32, unit: &'static str, description: &'static str) -> ParamSpec {
        ParamSpec { name, min, max, default, unit, kind: ParamKind::Float, description }
    }

    pub const fn boolean(name: &'static str, default: bool, description: &'static str) -> ParamSpec {
        ParamSpec { name, min: 0.0, max: 1.0, default: if default { 1.0 } else { 0.0 }, unit: "", kind: ParamKind::Bool, description }
    }

    pub const fn choice(name: &'static str, options: &'static [&'static str], default: usize, description: &'static str) -> ParamSpec {
        ParamSpec { name, min: 0.0, max: (options.len() - 1) as f32, default: default as f32, unit: "", kind: ParamKind::Choice(options), description }
    }

    /// Clamp (and round choices/bools) a raw value into this parameter's domain.
    pub fn clamp(&self, v: f32) -> f32 {
        let v = if v.is_finite() { v } else { self.default };
        match self.kind {
            ParamKind::Float => v.clamp(self.min, self.max),
            ParamKind::Bool => {
                if v >= 0.5 {
                    1.0
                } else {
                    0.0
                }
            }
            ParamKind::Choice(o) => v.round().clamp(0.0, (o.len() - 1) as f32),
        }
    }
}

/// One stereo insert effect.
pub trait Effect: Send {
    /// Registry name (`"svf"`, `"stutter"`, …).
    fn kind(&self) -> &'static str;

    /// Parameter metadata; `set_param` indexes into this.
    fn params(&self) -> &'static [ParamSpec];

    /// Set parameter `idx` to `value` (already inside the spec's domain; choices/bools as
    /// index/0-1). Called at block boundaries on the audio thread. Continuous parameters are
    /// smoothed internally (no zipper noise).
    fn set_param(&mut self, idx: usize, value: f32);

    /// Trigger edge from the core's trigger envelope (`X.active`): `true` on fire, `false` on
    /// release. Performance effects (stutter, tape stop, vinyl brake, reverse, …) start and
    /// stop here; the default ignores it.
    fn trigger(&mut self, _on: bool) {}

    /// Process one stereo block in place. `l.len() == r.len() <= MAX_BLOCK`.
    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]);

    /// Processing latency in samples (lookahead, pitch windows). Constant between
    /// `set_param` calls.
    fn latency(&self) -> usize {
        0
    }

    /// Tail length in samples after the input goes silent (reverb, delay feedback).
    fn tail(&self) -> usize {
        0
    }

    /// Clear internal state (delay lines, envelopes, captured buffers).
    fn reset(&mut self);
}

/// Decibels to linear gain.
#[inline]
pub fn db_to_gain(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// Linear gain to decibels (floored at -120 dB).
#[inline]
pub fn gain_to_db(g: f32) -> f32 {
    20.0 * g.abs().max(1e-6).log10()
}
