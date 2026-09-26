//! {{label}}: a stream-engine `dsp` patch (WebAssembly block ABI v2).
//!
//! ABI: no imports; export `memory`, `se_dsp_abi() -> 2`, `init(rate, channels, max_frames)`,
//! `input_buffer()`, `output_buffer()`, `params_buffer()` (byte offsets), `process(in, out,
//! frames, params)`, and optionally `reset()` / `latency()`. Buffers are planar f32: channel
//! `c` starts at `ptr + c * max_frames * 4`. The params block is `[env, bpm, beat_phase,
//! bar_phase, trigger_count, 0, 0, 0, slot0 … slot63, payload0 … payload15]`: the `patch.toml`
//! params in alphabetical order, then the payload of the last trigger (amount, bits, tier,
//! months, viewers, count, user hash 0–1, 0, user colour r g b a, 4 × 0). All memory is
//! static: never allocate after `init`.
//!
//! DSP: one-pole tone filter → `tanh`-style soft clip scaled by `drive` (a sub's tier pushes it
//! further) → output gain. The engine applies the trigger envelope to the wet signal, so `env`
//! is not used here.
#![no_std]

#[cfg(not(target_arch = "wasm32"))]
compile_error!("build for wasm32-unknown-unknown (see build.sh)");

const ABI_VERSION: i32 = 2;
const CHANNELS: usize = 2;
const MAX_FRAMES: usize = 1024;
const HEADER: usize = 8;
const SLOTS: usize = 64;
/// Trigger payload block (ABI v2).
const PAYLOAD: usize = HEADER + SLOTS;
const PAYLOAD_LEN: usize = 16;

const P_DRIVE: usize = HEADER;
const P_OUTPUT: usize = HEADER + 1;
const P_TONE: usize = HEADER + 2;
/// Sub tier of the last trigger (0 when the event had none).
const T_TIER: usize = PAYLOAD + 2;

struct State {
    ready: bool,
    sr: f32,
    max_frames: usize,
    /// One-pole low-pass state per channel.
    lp: [f32; CHANNELS],
    /// Smoothed parameters (no zipper noise).
    drive: f32,
    gain: f32,
    tone: f32,
    primed: bool,
}

static mut STATE: State = State { ready: false, sr: 48000.0, max_frames: MAX_FRAMES, lp: [0.0; CHANNELS], drive: 0.0, gain: 1.0, tone: 0.5, primed: false };
static mut INPUT: [f32; CHANNELS * MAX_FRAMES] = [0.0; CHANNELS * MAX_FRAMES];
static mut OUTPUT: [f32; CHANNELS * MAX_FRAMES] = [0.0; CHANNELS * MAX_FRAMES];
static mut PARAMS: [f32; HEADER + SLOTS + PAYLOAD_LEN] = [0.0; HEADER + SLOTS + PAYLOAD_LEN];

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
    s.lp = [0.0; CHANNELS];
    s.primed = false;
}

fn finite_or(x: f32, d: f32) -> f32 {
    if x.is_finite() { x } else { d }
}

/// Fast rational tanh approximation (exact limits ±1, smooth).
fn soft_clip(x: f32) -> f32 {
    let x = x.clamp(-3.0, 3.0);
    x * (27.0 + x * x) / (27.0 + 9.0 * x * x)
}

/// 10^(db/20) without libm: e^(db · ln10/20) via a short exp series on a range-reduced input.
fn db_to_gain(db: f32) -> f32 {
    let x = db * 0.115_129_25;
    // e^x = (e^(x/8))^8
    let y = x / 8.0;
    let e = 1.0 + y + y * y / 2.0 + y * y * y / 6.0 + y * y * y * y / 24.0;
    let e2 = e * e;
    let e4 = e2 * e2;
    e4 * e4
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

    let tier = finite_or(param(T_TIER), 0.0).clamp(0.0, 3.0);
    let drive = (finite_or(param(P_DRIVE), 0.4) + 0.15 * tier).clamp(0.0, 1.0);
    let gain = db_to_gain(finite_or(param(P_OUTPUT), 0.0).clamp(-24.0, 6.0));
    let tone = finite_or(param(P_TONE), 0.5).clamp(0.0, 1.0);
    if !s.primed {
        s.primed = true;
        (s.drive, s.gain, s.tone) = (drive, gain, tone);
    }
    // per-block smoothing toward the targets (~5 ms)
    let k = (n as f32 / (0.005 * s.sr)).min(1.0);
    let (d0, g0, t0) = (s.drive, s.gain, s.tone);
    s.drive += (drive - s.drive) * k;
    s.gain += (gain - s.gain) * k;
    s.tone += (tone - s.tone) * k;

    for c in 0..CHANNELS {
        let mut lp = s.lp[c];
        for i in 0..n {
            let t = i as f32 / n as f32;
            let d = d0 + (s.drive - d0) * t;
            let g = g0 + (s.gain - g0) * t;
            let tn = t0 + (s.tone - t0) * t;
            // SAFETY: i < n <= max_frames, c < CHANNELS: inside the planar buffers.
            let x = unsafe { input.add(c * stride + i).read() };
            let x = finite_or(x, 0.0);
            // tone: blend a one-pole low-pass (~1.5 kHz) with the dry signal
            lp += (x - lp) * 0.18;
            let shaped = lp + (x - lp) * tn;
            let pre = 1.0 + d * 11.0;
            let y = soft_clip(shaped * pre) / soft_clip(pre).max(1e-3) * g;
            unsafe { output.add(c * stride + i).write(y) };
        }
        s.lp[c] = if lp.is_finite() { lp } else { 0.0 };
    }
}

#[panic_handler]
fn on_panic(_: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}
