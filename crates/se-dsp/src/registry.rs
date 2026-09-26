//! Effect registry: names → constructors, parameter metadata, descriptions, default mixes.
//! The names are a contract with the config files and `docs/audio-effects.md`.

use crate::fx::{delay, drive, dynamics, filter, modulation, performance, pitch, reverb, utility};
use crate::{Effect, ParamSpec};

struct Entry {
    kind: &'static str,
    describe: &'static str,
    params: &'static [ParamSpec],
    /// Default slot `(wet, dry)`.
    mix: (f32, f32),
    make: fn(f32) -> Box<dyn Effect>,
}

const INSERT: (f32, f32) = (1.0, 0.0);

macro_rules! entries {
    ($($kind:literal => $ty:path, $params:path, $mix:expr, $describe:literal;)*) => {
        static ENTRIES: &[Entry] = &[
            $(Entry { kind: $kind, describe: $describe, params: $params, mix: $mix, make: |sr| Box::new(<$ty>::new(sr)) },)*
        ];
        static KINDS: &[&str] = &[$($kind),*];
    };
}

entries! {
    "svf" => filter::Svf, filter::SVF_PARAMS, INSERT,
        "State-variable filter (TPT): low-pass, high-pass, band-pass, or notch with log-gliding cutoff and resonance.";
    "eq" => filter::Eq, filter::EQ_PARAMS, INSERT,
        "Parametric EQ: low shelf, three peaking bands, high shelf (RBJ biquads), output gain.";
    "djfilter" => filter::DjFilter, filter::DJ_PARAMS, INSERT,
        "One-knob DJ filter: left of center sweeps a low-pass down, right sweeps a high-pass up, flat in the middle.";
    "compressor" => dynamics::Compressor, dynamics::COMP_PARAMS, INSERT,
        "Stereo-linked soft-knee compressor with optional sidechain key.";
    "limiter" => dynamics::Limiter, dynamics::LIMITER_PARAMS, INSERT,
        "Lookahead brickwall limiter (inter-sample peak aware); output never exceeds the ceiling.";
    "gate" => dynamics::Gate, dynamics::GATE_PARAMS, INSERT,
        "Noise/ducking gate with hysteresis, hold, range, and optional sidechain key.";
    "transient" => dynamics::Transient, dynamics::TRANSIENT_PARAMS, INSERT,
        "Transient shaper: boost or cut attacks and sustain independently of level.";
    "saturation" => drive::Saturation, drive::SAT_PARAMS, INSERT,
        "2x-oversampled soft saturation (tanh, tape, tube) with tilt tone.";
    "distortion" => drive::Distortion, drive::DIST_PARAMS, INSERT,
        "2x-oversampled distortion (hard clip, wavefolder, fuzz) with tilt tone.";
    "bitcrush" => drive::Bitcrush, drive::CRUSH_PARAMS, INSERT,
        "Bit-depth and sample-rate reduction with optional anti-alias pre-filter.";
    "delay" => delay::Delay, delay::DELAY_PARAMS, (0.35, 1.0),
        "Tempo-synced stereo/ping-pong delay with filtered feedback; outputs echoes only.";
    "reverb" => reverb::Reverb, reverb::REVERB_PARAMS, (0.25, 1.0),
        "8-line feedback-delay-network reverb with predelay, damping, and width; outputs the reverb only.";
    "chorus" => modulation::Chorus, modulation::CHORUS_PARAMS, (0.5, 1.0),
        "Stereo two-voice chorus (free or tempo-synced LFO); outputs the modulated voices.";
    "flanger" => modulation::Flanger, modulation::FLANGER_PARAMS, (0.5, 0.5),
        "Stereo flanger with feedback (free or tempo-synced LFO); outputs the swept delay.";
    "phaser" => modulation::Phaser, modulation::PHASER_PARAMS, (0.5, 0.5),
        "Stereo allpass phaser, 2–12 stages with feedback (free or tempo-synced LFO); outputs the allpass chain.";
    "pitch" => pitch::PitchShift, pitch::PITCH_PARAMS, INSERT,
        "Granular pitch shifter (±24 semitones); latency = half the window.";
    "stutter" => performance::Stutter, performance::STUTTER_PARAMS, INSERT,
        "Beat repeat: on trigger waits for the grid, captures a slice, and loops it; click-free release.";
    "tapestop" => performance::TapeStop, performance::TAPESTOP_PARAMS, INSERT,
        "Tape stop: on trigger playback slows to a standstill; release spins back up or crossfades.";
    "vinylbrake" => performance::VinylBrake, performance::VINYL_PARAMS, INSERT,
        "Vinyl brake (exponential slow-down) or backspin on trigger.";
    "reverse" => performance::Reverse, performance::REVERSE_PARAMS, INSERT,
        "Reverse buffer: while triggered, every chunk plays the previous chunk backwards.";
    "chopper" => performance::Chopper, performance::CHOPPER_PARAMS, INSERT,
        "Tempo-synced gate chopper (straight, dotted, triplet, offbeat); always-on or triggered.";
    "gain" => utility::Gain, utility::GAIN_PARAMS, INSERT,
        "Utility: gain, balance, stereo width, polarity invert.";
}

fn entry(kind: &str) -> Option<&'static Entry> {
    ENTRIES.iter().find(|e| e.kind == kind)
}

/// Names of all built-in effects.
pub fn kinds() -> &'static [&'static str] {
    KINDS
}

/// Create effect `kind` at sample rate `sr`. Allocates every buffer up front (sized for
/// `MAX_BLOCK` and the longest delay/loop/capture at `sr`), so call it off the audio thread.
pub fn create(kind: &str, sr: f32) -> Option<Box<dyn Effect>> {
    entry(kind).map(|e| (e.make)(sr))
}

/// Parameter metadata of `kind`.
pub fn params_of(kind: &str) -> Option<&'static [ParamSpec]> {
    entry(kind).map(|e| e.params)
}

/// Default `(wet, dry)` slot mix for `kind`: `(1, 0)` for inserts and unknown kinds.
pub fn default_mix(kind: &str) -> (f32, f32) {
    entry(kind).map_or(INSERT, |e| e.mix)
}

/// One-line description of `kind` (empty for unknown kinds).
pub fn describe(kind: &str) -> &'static str {
    entry(kind).map_or("", |e| e.describe)
}
