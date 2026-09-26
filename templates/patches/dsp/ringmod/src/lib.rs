//! Ring modulator: a stream-engine `dsp` patch (WebAssembly block ABI v1).
//!
//! ABI (host: `se_dsp::wasm`): no imports; exports `memory`, `se_dsp_abi`, `init`,
//! `input_buffer`, `output_buffer`, `params_buffer`, `process`, and the optional `reset`.
//! Buffers are planar f32: channel `c` starts at `ptr + c * max_frames * 4`. The params block
//! is `[env, bpm, beat_phase, bar_phase, trigger_count, 0, 0, 0, slot0, slot1, …]`, where the
//! slots are the `patch.toml` params in alphabetical order.
//!
//! Everything lives in statics sized for the host's largest block, so this module never
//! allocates; `init` only validates and records the stream format.
//!
//! DSP: `out = in · ((1 − depth) + depth · sin(2π·phase)) · gate`, where `gate` is
//! `1 − tremolo · (1 − g)` and `g` is a 16th-note square gate locked to the beat clock (open on
//! the first half of every 16th, 2 ms edges). With `sync` the carrier frequency is rounded to a
//! whole multiple `n` of the beat rate and its phase is set to `n · beat_phase` at each block,
//! so the carrier stays locked to the beat. `env` is ignored: the engine applies the trigger
//! envelope to this effect's wet signal. Each new trigger (the count in the header changes)
//! restarts the carrier at phase 0.
#![no_std]

#[cfg(not(target_arch = "wasm32"))]
compile_error!("build for wasm32-unknown-unknown (see build.sh)");

const ABI_VERSION: i32 = 1;
const CHANNELS: usize = 2;
/// Largest block the host sends (`se_dsp::MAX_BLOCK`).
const MAX_FRAMES: usize = 1024;
const HEADER: usize = 8;
const SLOTS: usize = 64;

const H_BPM: usize = 1;
const H_BEAT_PHASE: usize = 2;
const H_TRIGGERS: usize = 4;
const P_DEPTH: usize = HEADER;
const P_FREQ: usize = HEADER + 1;
const P_SYNC: usize = HEADER + 2;
const P_TREMOLO: usize = HEADER + 3;

/// Tremolo gate subdivision (16th notes).
const GATE_PER_BEAT: f64 = 4.0;
/// Tremolo gate edge length.
const GATE_EDGE_MS: f32 = 2.0;

struct State {
    ready: bool,
    sr: f32,
    max_frames: usize,
    /// Smoothed params are snapped to their targets on the first block after init/reset.
    primed: bool,
    /// Carrier phase in turns, [0, 1).
    phase: f64,
    depth: f32,
    tremolo: f32,
    gate: f32,
    triggers: f32,
}

static mut STATE: State =
    State { ready: false, sr: 48000.0, max_frames: MAX_FRAMES, primed: false, phase: 0.0, depth: 0.0, tremolo: 0.0, gate: 1.0, triggers: 0.0 };
static mut INPUT: [f32; CHANNELS * MAX_FRAMES] = [0.0; CHANNELS * MAX_FRAMES];
static mut OUTPUT: [f32; CHANNELS * MAX_FRAMES] = [0.0; CHANNELS * MAX_FRAMES];
static mut PARAMS: [f32; HEADER + SLOTS] = [0.0; HEADER + SLOTS];

fn state() -> &'static mut State {
    // SAFETY: wasm32 guests are single-threaded and every export takes this reference once.
    unsafe { &mut *(&raw mut STATE) }
}

#[unsafe(no_mangle)]
pub extern "C" fn se_dsp_abi() -> i32 {
    ABI_VERSION
}

#[unsafe(no_mangle)]
pub extern "C" fn init(sample_rate: f32, channels: i32, max_frames: i32) -> i32 {
    if channels != CHANNELS as i32 {
        return -1;
    }
    if max_frames < 1 || max_frames > MAX_FRAMES as i32 {
        return -2;
    }
    if !(sample_rate.is_finite() && sample_rate > 0.0) {
        return -3;
    }
    let s = state();
    s.sr = sample_rate;
    s.max_frames = max_frames as usize;
    s.ready = true;
    reset();
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn input_buffer() -> i32 {
    (&raw const INPUT) as usize as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn output_buffer() -> i32 {
    (&raw const OUTPUT) as usize as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn params_buffer() -> i32 {
    (&raw const PARAMS) as usize as i32
}

#[unsafe(no_mangle)]
pub extern "C" fn reset() {
    let s = state();
    s.primed = false;
    s.phase = 0.0;
    s.gate = 1.0;
}

#[unsafe(no_mangle)]
pub extern "C" fn process(input: i32, output: i32, frames: i32, params: i32) {
    let s = state();
    if !s.ready || frames <= 0 {
        return;
    }
    let n = (frames as usize).min(s.max_frames);
    let stride = s.max_frames;
    let input = input as usize as *const f32;
    let output = output as usize as *mut f32;
    // SAFETY: the host passes the offsets returned by `*_buffer()`; they lie inside memory.
    let param = |k: usize| unsafe { (params as usize as *const f32).add(k).read() };

    let sr = s.sr as f64;
    let bpm = finite_or(param(H_BPM), 120.0).clamp(1.0, 999.0) as f64;
    let beat_hz = bpm / 60.0;
    let beat0 = fract(finite_or(param(H_BEAT_PHASE), 0.0).max(0.0) as f64);
    let beat_inc = beat_hz / sr;
    let freq = finite_or(param(P_FREQ), 440.0).clamp(20.0, 2000.0).min(0.45 * s.sr) as f64;
    let depth = finite_or(param(P_DEPTH), 0.0).clamp(0.0, 1.0);
    let tremolo = finite_or(param(P_TREMOLO), 0.0).clamp(0.0, 1.0);
    let triggers = param(H_TRIGGERS);

    if !s.primed {
        s.primed = true;
        s.depth = depth;
        s.tremolo = tremolo;
        s.gate = gate_target(beat0);
        s.triggers = triggers;
    } else if triggers != s.triggers {
        s.triggers = triggers;
        s.phase = 0.0;
    }
    let inc = if param(P_SYNC) >= 0.5 {
        let harmonic = round_pos(freq / beat_hz).max(1.0);
        s.phase = fract(beat0 * harmonic);
        harmonic * beat_hz / sr
    } else {
        freq / sr
    };

    // Linear per-block ramps: no zipper noise, and no exponential decay into denormals.
    let (d0, t0) = (s.depth, s.tremolo);
    let (dd, dt) = ((depth - d0) / n as f32, (tremolo - t0) / n as f32);
    let gate_step = 1.0 / (GATE_EDGE_MS * 0.001 * s.sr).max(1.0);
    for i in 0..n {
        let k = (i + 1) as f32;
        let d = d0 + dd * k;
        let t = t0 + dt * k;
        let g = gate_target(beat0 + i as f64 * beat_inc);
        s.gate += (g - s.gate).clamp(-gate_step, gate_step);
        let carrier = sin_turns(s.phase as f32);
        s.phase += inc;
        if s.phase >= 1.0 {
            s.phase -= 1.0;
        }
        let gain = ((1.0 - d) + d * carrier) * (1.0 - t * (1.0 - s.gate));
        for c in 0..CHANNELS {
            let at = c * stride + i;
            // SAFETY: `at < CHANNELS * max_frames`, the size of both host-validated buffers.
            unsafe { output.add(at).write(input.add(at).read() * gain) };
        }
    }
    s.depth = depth;
    s.tremolo = tremolo;
}

/// Tremolo gate level at beat position `beat` (open for the first half of each 16th).
fn gate_target(beat: f64) -> f32 {
    if fract(beat * GATE_PER_BEAT) < 0.5 { 1.0 } else { 0.0 }
}

fn finite_or(v: f32, fallback: f32) -> f32 {
    if v.is_finite() { v } else { fallback }
}

/// Fractional part of a non-negative value.
fn fract(x: f64) -> f64 {
    x - (x as u64) as f64
}

/// Round a non-negative value to the nearest integer.
fn round_pos(x: f64) -> f64 {
    ((x + 0.5) as u64) as f64
}

/// `sin(2π·p)` for `p` in [0, 1): quarter-wave fold plus an odd Taylor series to z¹¹
/// (|error| < 1e-6 in f32; `core` has no `sin`).
fn sin_turns(p: f32) -> f32 {
    let mut x = p - 0.5;
    if x > 0.25 {
        x = 0.5 - x;
    } else if x < -0.25 {
        x = -0.5 - x;
    }
    let z = x * core::f32::consts::TAU;
    let z2 = z * z;
    let s = z * (1.0 + z2 * (-1.0 / 6.0 + z2 * (1.0 / 120.0 + z2 * (-1.0 / 5040.0 + z2 * (1.0 / 362_880.0 + z2 * (-1.0 / 39_916_800.0))))));
    // sin(2π(x + ½)) = −sin(2πx)
    -s
}

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
