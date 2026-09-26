//! `dsp` WebAssembly patch host (§6.1, §8.6): modules are compiled ahead of time with
//! wasmtime/cranelift at load and run on the audio thread as an [`Effect`] with a fixed block
//! ABI, a per-block time budget, and click-free auto-bypass on overrun, trap, or memory growth.
//!
//! # ABI v1 and v2
//! The module has **no imports** and exports `memory` plus:
//! * `se_dsp_abi() -> i32`: 1 or 2 (v2 adds the trigger payload block, below).
//! * `init(sample_rate: f32, channels: i32, max_frames: i32) -> i32`: 0 = ok, < 0 = error.
//!   All guest allocation happens here; linear memory may not grow afterwards.
//! * `input_buffer() -> i32`, `output_buffer() -> i32`, `params_buffer() -> i32`: byte offsets
//!   into `memory`, queried once after `init` and bounds-checked by the host.
//! * `process(in: i32, out: i32, frames: i32, params: i32)`: planar f32 buffers, channel `c` at
//!   `ptr + c * max_frames * 4`, `frames <= max_frames`; the guest overwrites all `frames`
//!   output samples of every channel.
//! * optional `reset()` and `latency() -> i32` (samples, queried once after `init`).
//!
//! The host always passes `channels = 2` and `max_frames = MAX_BLOCK`. The params block is
//! [`PARAMS_LEN`] f32: `[0]` trigger envelope 0–1, `[1]` bpm, `[2]` beat phase 0–1, `[3]` bar
//! phase 0–1, `[4]` trigger count (+1 on every trigger-on edge), `[5..8]` reserved (0), then
//! `[8 + k]` = patch param slot `k` (the manifest's params in alphabetical order).
//!
//! ABI v2 appends [`PAYLOAD_LEN`] floats at [`PAYLOAD_BASE`] (so the block is
//! [`PARAMS_LEN_V2`] long): the last trigger's payload — `[72]` amount, `[73]` bits, `[74]` tier,
//! `[75]` months, `[76]` viewers, `[77]` count, `[78]` user hash 0–1, `[79]` reserved, `[80..84]`
//! user colour r, g, b, a, `[84..88]` reserved (the engine's `TriggerPayload::floats` order).
//! The engine updates it in the same block as the trigger edge, never after it.
//!
//! # Real-time behaviour
//! [`WasmFx::process`], [`Effect::set_param`], and [`Effect::trigger`] never allocate, lock, or
//! do I/O on the normal path (proved by `tests/wasm_no_alloc.rs`). What makes the wasmtime call
//! path allocation-free: the `process` export is resolved once into a [`TypedFunc`] (no
//! dynamic `Val` marshalling), wasm backtraces are off (`wasm_backtrace_max_frames(None)`), no
//! fuel, no GC/component/async features are compiled in, and the epoch deadline is a plain
//! store field write. The first guest call on a new thread lazily installs wasmtime's signal
//! stack (`mmap` + `sigaltstack`, no heap allocation); call [`prepare_thread`] from the audio
//! thread outside the callback to do it ahead of time. A trap allocates inside wasmtime (it
//! boxes the trap record into an error: 2 allocations, 1 free, measured); the error is parked
//! in the instance and freed when the instance is dropped, and the instance never calls the
//! guest again.
//!
//! * **Budget**: every call is timed. [`OVERRUN_BLOCKS`] consecutive blocks over the budget →
//!   [`STATE_OVER_BUDGET`]: a [`BYPASS_FADE_MS`] crossfade from the module's output to the
//!   dry input (the guest still runs during the fade), then plain pass-through.
//! * **Hard deadline**: an engine-wide epoch ticks every [`EPOCH_TICK`]; a call still running
//!   ~4× the budget (at least [`MIN_DEADLINE_MS`]) after it started gets one grace window
//!   and is interrupted → [`STATE_TRAPPED`] if it kept the CPU busy through it (a runaway
//!   loop dies ≈ 2 ms after the deadline, e.g. 3–4 ms for a 300 µs budget). A call that was
//!   merely preempted is not killed.
//! * **Traps** (deadline, out-of-bounds access, `unreachable`, …) → [`STATE_TRAPPED`] at once:
//!   the output jumps back to the dry input through an offset that fades from the last output
//!   sample to zero over [`DECLICK_SAMPLES`].
//! * **Memory growth** after `init` is refused (the guest's `memory.grow` returns −1) and
//!   flagged → [`STATE_MEMORY`], same declick as a trap.
//!
//! Bypass is sticky: re-instantiate the module (hot swap through
//! [`crate::FxSlot::swap_effect`]) to run it again. The dry signal is the input delayed by the
//! module's latency, or silence for audio sources. Non-finite guest output samples become 0.

use crate::slot::{CompDelay, MAX_COMP};
use crate::{Ctx, Effect, MAX_BLOCK, ParamSpec, Smoother};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use wasmtime::{
    Config, Engine, ExternType, Instance, Memory, Module, OptLevel, ResourceLimiter, Store, StoreContextMut, Trap, TypedFunc, UpdateDeadline, ValType,
};

/// Newest block ABI version implemented by this host (v1 modules still load).
pub const ABI_VERSION: i32 = 2;
/// Channels passed to `init` (planar stereo).
pub const CHANNELS: usize = 2;
/// Header floats at the start of the params block.
pub const HEADER_LEN: usize = 8;
/// Patch param slots after the header.
pub const WASM_PARAM_SLOTS: usize = 64;
/// Total f32 values in the params block (ABI v1).
pub const PARAMS_LEN: usize = HEADER_LEN + WASM_PARAM_SLOTS;
/// First float of the trigger payload (ABI v2).
pub const PAYLOAD_BASE: usize = PARAMS_LEN;
/// Floats of the trigger payload block (ABI v2; the first 12 are used).
pub const PAYLOAD_LEN: usize = 16;
/// Total f32 values in the params block (ABI v2).
pub const PARAMS_LEN_V2: usize = PARAMS_LEN + PAYLOAD_LEN;
/// Header index: trigger envelope 0–1.
pub const H_ENV: usize = 0;
/// Header index: tempo (bpm).
pub const H_BPM: usize = 1;
/// Header index: beat phase 0–1 at the block start.
pub const H_BEAT_PHASE: usize = 2;
/// Header index: bar phase 0–1 at the block start.
pub const H_BAR_PHASE: usize = 3;
/// Header index: trigger-on edge count (wraps at 2²⁴, exact in f32).
pub const H_TRIGGERS: usize = 4;
/// Linear memory limit per instance.
pub const MEMORY_LIMIT: usize = 64 << 20;
/// Epoch tick period of the engine-wide deadline clock.
pub const EPOCH_TICK: Duration = Duration::from_millis(1);
/// Minimum hard deadline per call.
pub const MIN_DEADLINE_MS: u64 = 2;
/// Consecutive over-budget blocks that trigger the auto-bypass.
pub const OVERRUN_BLOCKS: u32 = 3;
/// Crossfade from the module's output to the dry input on an over-budget bypass.
pub const BYPASS_FADE_MS: f32 = 10.0;
/// Declick length after a trap or memory growth.
pub const DECLICK_SAMPLES: u32 = 64;

/// [`WasmStatus::state`]: running normally.
pub const STATE_OK: u8 = 0;
/// [`WasmStatus::state`]: bypassed after [`OVERRUN_BLOCKS`] over-budget blocks.
pub const STATE_OVER_BUDGET: u8 = 1;
/// [`WasmStatus::state`]: bypassed after a trap (including the hard deadline).
pub const STATE_TRAPPED: u8 = 2;
/// [`WasmStatus::state`]: bypassed after the module tried to grow its memory (or a table).
pub const STATE_MEMORY: u8 = 3;

/// Deadline for `init`, the buffer queries, and the ABI probe (off the audio thread).
const SETUP_DEADLINE: Duration = Duration::from_millis(500);
/// Native stack available to guest code (runs on the caller's thread stack).
const MAX_WASM_STACK: usize = 256 << 10;
const MAX_TABLE_ELEMENTS: usize = 1 << 16;
const MAX_TABLES: usize = 4;
/// `Trap` code stored when a call failed with a non-trap error.
const TRAP_OTHER: u8 = u8::MAX;
const TRIGGER_WRAP: u32 = 1 << 24;
/// Grace window after the deadline (epoch ticks; the check runs right after a tick, so the
/// window is ≥ `GRACE_TICKS` full periods) and the CPU time within it that proves the guest is
/// still spinning rather than preempted.
const GRACE_TICKS: u64 = 2;
const GRACE_CPU: Duration = Duration::from_micros(500);
/// Upper bound on grace windows for a call whose thread keeps getting preempted.
const MAX_GRACE_WINDOWS: u32 = 100;

/// Pre-allocate wasmtime's per-thread trap-handling state (signal stack) on the calling thread.
/// Optional: without it the first guest call on a thread does this once (`mmap` +
/// `sigaltstack`). Call it from the audio thread outside the real-time callback.
pub fn prepare_thread() {
    Engine::tls_eager_initialize();
}

/// Increments the engine epoch every [`EPOCH_TICK`] while any host, module, or instance is
/// alive (instances hold a reference, so their deadline keeps working after the host drops).
struct Ticker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Ticker {
    fn start(engine: Engine) -> Result<Ticker, String> {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("se-wasm-epoch".into())
            .spawn(move || {
                while !flag.load(Ordering::Relaxed) {
                    std::thread::sleep(EPOCH_TICK);
                    engine.increment_epoch();
                }
            })
            .map_err(|e| format!("cannot start the wasm epoch thread: {e}"))?;
        Ok(Ticker { stop, thread: Some(thread) })
    }
}

impl Drop for Ticker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Store data: memory/table limits during setup, then refuses (and records) any growth; plus
/// the deadline watchdog state of the current call.
#[derive(Default)]
struct HostState {
    frozen: bool,
    grew: bool,
    /// Thread CPU time at the start of the current grace window.
    cpu_mark: Option<Duration>,
    windows: u32,
}

impl ResourceLimiter for HostState {
    fn memory_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        if self.frozen {
            self.grew = true;
            return Ok(false);
        }
        Ok(desired <= MEMORY_LIMIT)
    }

    fn table_growing(&mut self, _current: usize, desired: usize, _maximum: Option<usize>) -> wasmtime::Result<bool> {
        if self.frozen {
            self.grew = true;
            return Ok(false);
        }
        Ok(desired <= MAX_TABLE_ELEMENTS)
    }

    fn instances(&self) -> usize {
        1
    }

    fn tables(&self) -> usize {
        MAX_TABLES
    }

    fn memories(&self) -> usize {
        1
    }
}

fn new_store(engine: &Engine) -> Store<HostState> {
    let mut store = Store::new(engine, HostState::default());
    store.limiter(|s| s as &mut dyn ResourceLimiter);
    store.epoch_deadline_callback(on_deadline);
    store
}

/// The epoch deadline passed while the guest runs. The epoch is wall-clock time, so a thread
/// preempted mid-call (loaded machine, non-RT audio thread) would look like a runaway guest.
/// Before killing, give the call a grace window of [`GRACE_TICKS`]: kill if the thread used
/// at least [`GRACE_CPU`] of CPU time in it (the guest is really spinning); otherwise it was
/// preempted, so open another window (at most [`MAX_GRACE_WINDOWS`]). No allocation.
fn on_deadline(mut cx: StoreContextMut<'_, HostState>) -> wasmtime::Result<UpdateDeadline> {
    let s = cx.data_mut();
    let cpu = thread_cpu_time();
    let spinning = s.cpu_mark.is_some_and(|m| cpu.saturating_sub(m) >= GRACE_CPU);
    if spinning || s.windows >= MAX_GRACE_WINDOWS {
        return Ok(UpdateDeadline::Interrupt);
    }
    s.cpu_mark = Some(cpu);
    s.windows += 1;
    Ok(UpdateDeadline::Continue(GRACE_TICKS))
}

/// Arm the watchdog for one call: the first check comes after `ticks` epoch ticks.
fn arm(store: &mut Store<HostState>, ticks: u64) {
    let s = store.data_mut();
    s.cpu_mark = None;
    s.windows = 0;
    store.set_epoch_deadline(ticks);
}

/// CPU time consumed by the calling thread (`clock_gettime(CLOCK_THREAD_CPUTIME_ID)`).
fn thread_cpu_time() -> Duration {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `ts` is a valid out-pointer; this clock exists on every supported platform.
    unsafe { libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut ts) };
    Duration::new(ts.tv_sec as u64, ts.tv_nsec as u32)
}

/// Epoch ticks until the first deadline check for a limit of `d`. The epoch may tick right
/// after arming, so the check comes after `(ticks − 1, ticks]` periods; the grace window adds
/// [`GRACE_TICKS`] full periods, so no call is killed before `d`.
fn deadline_ticks(d: Duration) -> u64 {
    d.as_micros().div_ceil(EPOCH_TICK.as_micros()).max(1) as u64
}

/// Owns the wasmtime engine (cranelift, epoch interruption) and its epoch ticker thread.
pub struct WasmHost {
    engine: Engine,
    ticker: Arc<Ticker>,
}

impl WasmHost {
    pub fn new() -> Result<WasmHost, String> {
        let mut c = Config::new();
        c.cranelift_opt_level(OptLevel::Speed)
            .epoch_interruption(true)
            .wasm_backtrace_max_frames(None)
            .max_wasm_stack(MAX_WASM_STACK)
            .wasm_memory64(false)
            .wasm_multi_memory(false)
            .wasm_custom_page_sizes(false);
        let engine = Engine::new(&c).map_err(|e| format!("wasm engine: {e:#}"))?;
        let ticker = Arc::new(Ticker::start(engine.clone())?);
        Ok(WasmHost { engine, ticker })
    }

    /// Compile (AOT, cranelift) and validate a module against the block ABI: no imports, the
    /// required exports with exact signatures, a 32-bit unshared memory within
    /// [`MEMORY_LIMIT`], and `se_dsp_abi()` 1 or 2. Call off the audio thread.
    pub fn compile(&self, wasm: &[u8]) -> Result<WasmModule, String> {
        let module = Module::new(&self.engine, wasm).map_err(|e| format!("invalid wasm module: {e:#}"))?;
        if let Some(i) = module.imports().next() {
            return Err(format!("dsp modules must not have imports; found `{}.{}`", i.module(), i.name()));
        }
        match module.get_export("memory") {
            Some(ExternType::Memory(m)) => {
                if m.is_64() || m.is_shared() {
                    return Err("export `memory` must be a 32-bit, unshared memory".into());
                }
                let min = m.minimum().saturating_mul(m.page_size());
                if min > MEMORY_LIMIT as u64 {
                    return Err(format!("initial memory {} KiB exceeds the {} MiB limit", min >> 10, MEMORY_LIMIT >> 20));
                }
            }
            Some(_) => return Err("export `memory` must be a memory".into()),
            None => return Err("missing export `memory`".into()),
        }
        use Ty::{F32, I32};
        func_export(&module, "se_dsp_abi", &[], &[I32], true)?;
        func_export(&module, "init", &[F32, I32, I32], &[I32], true)?;
        func_export(&module, "input_buffer", &[], &[I32], true)?;
        func_export(&module, "output_buffer", &[], &[I32], true)?;
        func_export(&module, "params_buffer", &[], &[I32], true)?;
        func_export(&module, "process", &[I32, I32, I32, I32], &[], true)?;
        let has_reset = func_export(&module, "reset", &[], &[], false)?;
        let has_latency = func_export(&module, "latency", &[], &[I32], false)?;

        let mut store = new_store(&self.engine);
        arm(&mut store, deadline_ticks(SETUP_DEADLINE));
        let instance = Instance::new(&mut store, &module, &[]).map_err(|e| format!("instantiation failed: {e:#}"))?;
        let abi = call0(&mut store, &instance, "se_dsp_abi")?;
        if !(1..=ABI_VERSION).contains(&abi) {
            return Err(format!("se_dsp_abi() returned {abi}; this host implements ABI 1–{ABI_VERSION}"));
        }
        let params_len = if abi >= 2 { PARAMS_LEN_V2 } else { PARAMS_LEN };
        Ok(WasmModule { engine: self.engine.clone(), module, ticker: self.ticker.clone(), has_reset, has_latency, params_len })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Ty {
    I32,
    F32,
}

impl Ty {
    fn matches(self, v: &ValType) -> bool {
        match self {
            Ty::I32 => v.is_i32(),
            Ty::F32 => v.is_f32(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Ty::I32 => "i32",
            Ty::F32 => "f32",
        }
    }
}

/// Check function export `name` against `(params) -> (results)`. `Ok(false)` if an optional
/// export is absent.
fn func_export(module: &Module, name: &str, params: &[Ty], results: &[Ty], required: bool) -> Result<bool, String> {
    let want = || {
        let list = |t: &[Ty]| t.iter().map(|t| t.name()).collect::<Vec<_>>().join(", ");
        format!("({}) -> ({})", list(params), list(results))
    };
    match module.get_export(name) {
        None if required => Err(format!("missing export `{name}`: {}", want())),
        None => Ok(false),
        Some(ExternType::Func(f)) => {
            let same = |want: &[Ty], got: Vec<ValType>| got.len() == want.len() && got.iter().zip(want).all(|(g, w)| w.matches(g));
            if same(params, f.params().collect()) && same(results, f.results().collect()) {
                Ok(true)
            } else {
                Err(format!("export `{name}` must be {}; found {f}", want()))
            }
        }
        Some(_) => Err(format!("export `{name}` must be a function {}", want())),
    }
}

/// Call a `() -> i32` export during setup.
fn call0(store: &mut Store<HostState>, instance: &Instance, name: &str) -> Result<i32, String> {
    let f = instance.get_typed_func::<(), i32>(&mut *store, name).map_err(|e| format!("export `{name}`: {e:#}"))?;
    f.call(&mut *store, ()).map_err(|e| format!("{name}() failed: {e:#}"))
}

/// A compiled, validated `dsp` module; cheap to clone and instantiate.
#[derive(Clone)]
pub struct WasmModule {
    engine: Engine,
    module: Module,
    ticker: Arc<Ticker>,
    has_reset: bool,
    has_latency: bool,
    /// Params block length of the module's ABI.
    params_len: usize,
}

impl WasmModule {
    /// Create an instance: runs `init(sr, 2, MAX_BLOCK)`, validates the buffer offsets and the
    /// reported latency, then freezes memory. `source`: audio-source layer (the guest's input
    /// is silence and bypass fades to silence). `budget_us`: per-block CPU budget in µs
    /// (0 = no soft budget; the [`MIN_DEADLINE_MS`] hard deadline still applies). Call off
    /// the audio thread.
    pub fn instantiate(&self, sr: f32, source: bool, budget_us: u32) -> Result<WasmFx, String> {
        if !(sr.is_finite() && sr > 0.0) {
            return Err(format!("bad sample rate {sr}"));
        }
        let mut store = new_store(&self.engine);
        arm(&mut store, deadline_ticks(SETUP_DEADLINE));
        let instance = Instance::new(&mut store, &self.module, &[]).map_err(|e| format!("instantiation failed: {e:#}"))?;
        let memory = instance.get_memory(&mut store, "memory").ok_or("missing export `memory`")?;
        let init = instance.get_typed_func::<(f32, i32, i32), i32>(&mut store, "init").map_err(|e| format!("export `init`: {e:#}"))?;
        let rc = init.call(&mut store, (sr, CHANNELS as i32, MAX_BLOCK as i32)).map_err(|e| format!("init() failed: {e:#}"))?;
        if rc < 0 {
            return Err(format!("init({sr}, {CHANNELS}, {MAX_BLOCK}) returned error {rc}"));
        }
        store.data_mut().frozen = true;
        let mem_len = memory.data_size(&store);
        let audio_bytes = CHANNELS * MAX_BLOCK * 4;
        let region = |store: &mut Store<HostState>, name: &str, len: usize| -> Result<(i32, usize), String> {
            let raw = call0(store, &instance, name)?;
            let off = raw as u32 as usize;
            if !off.is_multiple_of(4) {
                return Err(format!("{name}() = {off} is not 4-byte aligned"));
            }
            if off.checked_add(len).is_none_or(|end| end > mem_len) {
                return Err(format!("{name}() = {off}: {len} bytes do not fit in the {mem_len}-byte memory"));
            }
            Ok((raw, off))
        };
        let input = region(&mut store, "input_buffer", audio_bytes)?;
        let output = region(&mut store, "output_buffer", audio_bytes)?;
        let params_len = self.params_len;
        let params = region(&mut store, "params_buffer", params_len * 4)?;
        let overlaps = |a: usize, alen: usize, b: usize, blen: usize| a < b + blen && b < a + alen;
        if input.1 != output.1 && overlaps(input.1, audio_bytes, output.1, audio_bytes) {
            return Err("input and output buffers partially overlap".into());
        }
        if overlaps(params.1, params_len * 4, input.1, audio_bytes) || overlaps(params.1, params_len * 4, output.1, audio_bytes) {
            return Err("the params buffer overlaps the audio buffers".into());
        }
        let latency = if self.has_latency {
            let l = call0(&mut store, &instance, "latency")?;
            if l < 0 || l as usize > MAX_COMP {
                return Err(format!("latency() = {l} is outside 0–{MAX_COMP} samples"));
            }
            l as usize
        } else {
            0
        };
        if store.data().grew {
            return Err("memory grew after init (allocate everything in `init`)".into());
        }
        let process = instance.get_typed_func::<(i32, i32, i32, i32), ()>(&mut store, "process").map_err(|e| format!("export `process`: {e:#}"))?;
        let reset =
            if self.has_reset { Some(instance.get_typed_func::<(), ()>(&mut store, "reset").map_err(|e| format!("export `reset`: {e:#}"))?) } else { None };

        let hard = Duration::from_micros((budget_us as u64 * 4).max(MIN_DEADLINE_MS * 1000));
        let ticks = deadline_ticks(hard);
        let mut delay = CompDelay::new(latency + 1);
        delay.set_delay(latency);
        Ok(WasmFx {
            store,
            memory,
            process,
            reset,
            in_ptr: input,
            out_ptr: output,
            params_ptr: params,
            mem_len,
            params: [0.0; PARAMS_LEN_V2],
            params_len,
            triggers: 0,
            trig_on: false,
            source,
            latency,
            budget: Duration::from_micros(budget_us as u64),
            deadline_ticks: ticks,
            over: 0,
            mode: Mode::Running,
            fade: Smoother::with_ms(1.0, BYPASS_FADE_MS, sr),
            offset: [0.0; CHANNELS],
            declick_left: 0,
            last_out: [0.0; CHANNELS],
            delay,
            dry_l: vec![0.0; MAX_BLOCK],
            dry_r: vec![0.0; MAX_BLOCK],
            failure: None,
            status: Arc::new(WasmStatus::new(budget_us, hard.as_millis() as u32)),
            _ticker: self.ticker.clone(),
        })
    }
}

/// Live health of one instance, shared with the UI (lock-free; written by the audio thread).
#[derive(Debug)]
pub struct WasmStatus {
    state: AtomicU8,
    trap: AtomicU8,
    last_us: AtomicU32,
    max_us: AtomicU32,
    overruns: AtomicU32,
    calls: AtomicU64,
    budget_us: u32,
    deadline_ms: u32,
}

impl WasmStatus {
    fn new(budget_us: u32, deadline_ms: u32) -> WasmStatus {
        WasmStatus {
            state: AtomicU8::new(STATE_OK),
            trap: AtomicU8::new(TRAP_OTHER),
            last_us: AtomicU32::new(0),
            max_us: AtomicU32::new(0),
            overruns: AtomicU32::new(0),
            calls: AtomicU64::new(0),
            budget_us,
            deadline_ms,
        }
    }

    /// [`STATE_OK`], [`STATE_OVER_BUDGET`], [`STATE_TRAPPED`], or [`STATE_MEMORY`].
    pub fn state(&self) -> u8 {
        self.state.load(Ordering::Relaxed)
    }

    /// Duration of the latest `process` call (µs).
    pub fn last_us(&self) -> u32 {
        self.last_us.load(Ordering::Relaxed)
    }

    /// Longest `process` call so far (µs).
    pub fn max_us(&self) -> u32 {
        self.max_us.load(Ordering::Relaxed)
    }

    /// Calls that exceeded the budget (in total, not only consecutive ones).
    pub fn overruns(&self) -> u32 {
        self.overruns.load(Ordering::Relaxed)
    }

    /// Guest `process` calls so far.
    pub fn calls(&self) -> u64 {
        self.calls.load(Ordering::Relaxed)
    }

    /// Per-block budget (µs; 0 = none).
    pub fn budget_us(&self) -> u32 {
        self.budget_us
    }

    /// Why the instance is bypassed (`None` while it runs). Allocates: call off the audio thread.
    pub fn error_text(&self) -> Option<String> {
        match self.state() {
            STATE_OVER_BUDGET => Some(format!(
                "over budget: {OVERRUN_BLOCKS} blocks in a row took longer than {} µs (last {} µs, max {} µs); bypassed",
                self.budget_us,
                self.last_us(),
                self.max_us()
            )),
            STATE_TRAPPED => Some(match Trap::from_u8(self.trap.load(Ordering::Relaxed)) {
                Some(Trap::Interrupt) => format!("killed: a call ran past the {} ms hard deadline ({}); bypassed", self.deadline_ms, Trap::Interrupt),
                Some(t) => format!("trapped: {t}; bypassed"),
                None => "trapped: the call failed in the host; bypassed".to_string(),
            }),
            STATE_MEMORY => Some("tried to grow memory after init (allocate everything in `init`); bypassed".to_string()),
            _ => None,
        }
    }

    fn fail(&self, state: u8, trap: u8) {
        self.trap.store(trap, Ordering::Relaxed);
        self.state.store(state, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    Running,
    /// Over budget: crossfading from the guest output to the dry signal.
    Fading,
    Bypassed,
}

/// One running module instance: an [`Effect`] of kind `"wasm"`.
///
/// Params: index 0 = `env` (header `[0]`), index `1 + k` = slot `p<k>` (params `[8 + k]`).
pub struct WasmFx {
    store: Store<HostState>,
    memory: Memory,
    process: TypedFunc<(i32, i32, i32, i32), ()>,
    reset: Option<TypedFunc<(), ()>>,
    /// (value passed to the guest, byte offset)
    in_ptr: (i32, usize),
    out_ptr: (i32, usize),
    params_ptr: (i32, usize),
    mem_len: usize,
    params: [f32; PARAMS_LEN_V2],
    /// Floats of `params` the module's ABI version reads.
    params_len: usize,
    triggers: u32,
    trig_on: bool,
    source: bool,
    latency: usize,
    budget: Duration,
    deadline_ticks: u64,
    /// Consecutive over-budget blocks.
    over: u32,
    mode: Mode,
    fade: Smoother,
    /// Declick offset per channel (last output − dry at the failure).
    offset: [f32; CHANNELS],
    declick_left: u32,
    last_out: [f32; CHANNELS],
    /// Aligns the dry signal with the module's latency.
    delay: CompDelay,
    dry_l: Vec<f32>,
    dry_r: Vec<f32>,
    /// Error of the failed call, parked so it is freed off the audio thread (on drop).
    failure: Option<wasmtime::Error>,
    status: Arc<WasmStatus>,
    _ticker: Arc<Ticker>,
}

impl WasmFx {
    pub fn status(&self) -> Arc<WasmStatus> {
        self.status.clone()
    }

    /// Run the guest on `l`/`r` (replaced by its output). Returns the failure state on error.
    fn call_guest(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) -> Result<(), (u8, u8)> {
        let n = l.len();
        let stride = MAX_BLOCK * 4;
        let t = &ctx.transport;
        self.params[H_BPM] = t.bpm;
        self.params[H_BEAT_PHASE] = t.phase();
        self.params[H_BAR_PHASE] = t.bar_phase();
        self.params[H_TRIGGERS] = self.triggers as f32;
        let mem = self.memory.data_mut(&mut self.store);
        let inp = self.in_ptr.1;
        if self.source {
            mem[inp..inp + n * 4].fill(0);
            mem[inp + stride..inp + stride + n * 4].fill(0);
        } else {
            put(mem, inp, l);
            put(mem, inp + stride, r);
        }
        put(mem, self.params_ptr.1, &self.params[..self.params_len]);

        arm(&mut self.store, self.deadline_ticks);
        let start = Instant::now();
        let res = self.process.call(&mut self.store, (self.in_ptr.0, self.out_ptr.0, n as i32, self.params_ptr.0));
        let took = start.elapsed();
        let us = took.as_micros().min(u32::MAX as u128) as u32;
        let st = &self.status;
        st.calls.fetch_add(1, Ordering::Relaxed);
        st.last_us.store(us, Ordering::Relaxed);
        st.max_us.fetch_max(us, Ordering::Relaxed);
        if let Err(e) = res {
            let code = e.downcast_ref::<Trap>().map_or(TRAP_OTHER, |t| trap_code(*t));
            self.failure.get_or_insert(e);
            return Err((STATE_TRAPPED, code));
        }
        if self.store.data().grew || self.memory.data_size(&self.store) != self.mem_len {
            return Err((STATE_MEMORY, TRAP_OTHER));
        }
        if !self.budget.is_zero() && took > self.budget {
            st.overruns.fetch_add(1, Ordering::Relaxed);
            self.over += 1;
        } else {
            self.over = 0;
        }
        let mem = self.memory.data(&self.store);
        let out = self.out_ptr.1;
        get(mem, out, l);
        get(mem, out + stride, r);
        Ok(())
    }

    /// Switch to pass-through with a declick from the last output sample to the current
    /// block's dry signal (already in `dry_l`/`dry_r`).
    fn fail(&mut self, state: u8, trap: u8) {
        self.mode = Mode::Bypassed;
        self.status.fail(state, trap);
        self.declick_left = DECLICK_SAMPLES;
        self.offset = [self.last_out[0] - self.dry_l[0], self.last_out[1] - self.dry_r[0]];
    }

    /// Dry output (+ declick) into `l`/`r`.
    fn bypass_out(&mut self, l: &mut [f32], r: &mut [f32]) {
        let n = l.len();
        l.copy_from_slice(&self.dry_l[..n]);
        r.copy_from_slice(&self.dry_r[..n]);
        if self.declick_left == 0 {
            return;
        }
        let d = DECLICK_SAMPLES as f32;
        for i in 0..n {
            if self.declick_left == 0 {
                break;
            }
            self.declick_left -= 1;
            let g = self.declick_left as f32 / d;
            l[i] += self.offset[0] * g;
            r[i] += self.offset[1] * g;
        }
    }
}

fn trap_code(t: Trap) -> u8 {
    (0..TRAP_OTHER).find(|&c| Trap::from_u8(c) == Some(t)).unwrap_or(TRAP_OTHER)
}

#[inline]
fn put(mem: &mut [u8], off: usize, src: &[f32]) {
    for (b, x) in mem[off..off + src.len() * 4].as_chunks_mut::<4>().0.iter_mut().zip(src) {
        *b = x.to_le_bytes();
    }
}

#[inline]
fn get(mem: &[u8], off: usize, dst: &mut [f32]) {
    let src = &mem[off..off + dst.len() * 4];
    for (x, b) in dst.iter_mut().zip(src.as_chunks::<4>().0) {
        let v = f32::from_le_bytes(*b);
        *x = if v.is_finite() { v } else { 0.0 };
    }
}

static PARAMS: [ParamSpec; 1 + WASM_PARAM_SLOTS] = {
    const NAMES: [&str; WASM_PARAM_SLOTS] = [
        "p0", "p1", "p2", "p3", "p4", "p5", "p6", "p7", "p8", "p9", "p10", "p11", "p12", "p13", "p14", "p15", "p16", "p17", "p18", "p19", "p20", "p21", "p22",
        "p23", "p24", "p25", "p26", "p27", "p28", "p29", "p30", "p31", "p32", "p33", "p34", "p35", "p36", "p37", "p38", "p39", "p40", "p41", "p42", "p43",
        "p44", "p45", "p46", "p47", "p48", "p49", "p50", "p51", "p52", "p53", "p54", "p55", "p56", "p57", "p58", "p59", "p60", "p61", "p62", "p63",
    ];
    const SLOT_DESC: &str = "Patch parameter slot (patch.toml params in alphabetical order)";
    let mut t = [ParamSpec::float("env", 0.0, 1.0, 0.0, "", "Trigger envelope 0–1 passed to the module"); 1 + WASM_PARAM_SLOTS];
    let mut k = 0;
    while k < WASM_PARAM_SLOTS {
        t[1 + k] = ParamSpec::float(NAMES[k], -1e6, 1e6, 0.0, "", SLOT_DESC);
        k += 1;
    }
    t
};

impl Effect for WasmFx {
    fn kind(&self) -> &'static str {
        "wasm"
    }

    fn params(&self) -> &'static [ParamSpec] {
        &PARAMS
    }

    fn set_param(&mut self, idx: usize, value: f32) {
        match idx {
            0 => self.params[H_ENV] = value,
            i if i <= WASM_PARAM_SLOTS => self.params[HEADER_LEN + i - 1] = value,
            _ => {}
        }
    }

    fn trigger(&mut self, on: bool) {
        if on && !self.trig_on {
            self.triggers = (self.triggers + 1) % TRIGGER_WRAP;
        }
        self.trig_on = on;
    }

    fn set_payload(&mut self, k: usize, value: f32) {
        if k < PAYLOAD_LEN {
            self.params[PAYLOAD_BASE + k] = value;
        }
    }

    fn process(&mut self, ctx: &Ctx, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len()).min(MAX_BLOCK);
        let (l, r) = (&mut l[..n], &mut r[..n]);
        if n == 0 {
            return;
        }
        let (dl, dr) = (&mut self.dry_l[..n], &mut self.dry_r[..n]);
        if self.source {
            dl.fill(0.0);
            dr.fill(0.0);
        } else {
            dl.copy_from_slice(l);
            dr.copy_from_slice(r);
            self.delay.process(dl, dr);
        }
        if self.mode == Mode::Bypassed {
            self.bypass_out(l, r);
            return;
        }
        if let Err((state, trap)) = self.call_guest(ctx, l, r) {
            self.fail(state, trap);
            self.bypass_out(l, r);
            return;
        }
        if self.mode == Mode::Running && self.over >= OVERRUN_BLOCKS {
            self.mode = Mode::Fading;
            self.fade.reset(1.0);
            self.fade.set(0.0);
            self.status.fail(STATE_OVER_BUDGET, TRAP_OTHER);
        }
        if self.mode == Mode::Fading {
            for i in 0..n {
                let g = self.fade.next();
                l[i] = self.dry_l[i] + (l[i] - self.dry_l[i]) * g;
                r[i] = self.dry_r[i] + (r[i] - self.dry_r[i]) * g;
            }
            if self.fade.settled() {
                self.mode = Mode::Bypassed;
            }
        }
        self.last_out = [l[n - 1], r[n - 1]];
    }

    fn latency(&self) -> usize {
        self.latency
    }

    fn reset(&mut self) {
        self.delay.clear();
        self.declick_left = 0;
        self.last_out = [0.0; CHANNELS];
        self.over = 0;
        match self.mode {
            Mode::Running => {
                if let Some(f) = &self.reset {
                    arm(&mut self.store, self.deadline_ticks);
                    if let Err(e) = f.call(&mut self.store, ()) {
                        let code = e.downcast_ref::<Trap>().map_or(TRAP_OTHER, |t| trap_code(*t));
                        self.failure.get_or_insert(e);
                        self.mode = Mode::Bypassed;
                        self.status.fail(STATE_TRAPPED, code);
                    } else if self.store.data().grew {
                        self.mode = Mode::Bypassed;
                        self.status.fail(STATE_MEMORY, TRAP_OTHER);
                    }
                }
            }
            Mode::Fading => self.mode = Mode::Bypassed,
            Mode::Bypassed => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wasmtime::AsContextMut;

    #[test]
    fn watchdog_kills_spinning_calls_but_not_preempted_ones() {
        let host = WasmHost::new().unwrap();
        let mut store = new_store(&host.engine);
        arm(&mut store, 1);
        let check = |store: &mut Store<HostState>| on_deadline(store.as_context_mut()).unwrap();
        // first expiry: always a grace window
        assert!(matches!(check(&mut store), UpdateDeadline::Continue(GRACE_TICKS)));
        // almost no CPU used since (the thread was preempted): another window
        assert!(matches!(check(&mut store), UpdateDeadline::Continue(GRACE_TICKS)));
        // the thread spent the window on the CPU: kill
        let start = thread_cpu_time();
        while thread_cpu_time() - start < GRACE_CPU + Duration::from_micros(100) {
            std::hint::spin_loop();
        }
        assert!(matches!(check(&mut store), UpdateDeadline::Interrupt));
        // re-arming starts over; endless preemption is bounded
        arm(&mut store, 1);
        for _ in 0..MAX_GRACE_WINDOWS {
            assert!(matches!(check(&mut store), UpdateDeadline::Continue(_)));
        }
        assert!(matches!(check(&mut store), UpdateDeadline::Interrupt));
    }
}
