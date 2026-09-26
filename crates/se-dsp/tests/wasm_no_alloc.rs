//! The real-time path of `WasmFx` (`process`, `set_param`, `trigger`, `reset`) performs no
//! heap allocation or free: normal running, the over-budget fade and bypass, memory-growth
//! bypass, pass-through after a trap, and inside an `FxSlot`. Runs on fresh threads, so the
//! lazy per-thread wasmtime setup on the first call is covered too.
#![cfg(feature = "wasm")]

mod wasm_support;

#[global_allocator]
static A: se_alloc::Counting = se_alloc::Counting;

use se_dsp::wasm::{STATE_MEMORY, STATE_OVER_BUDGET, STATE_TRAPPED};
use se_dsp::{Effect, FxSlot, Transport};
use wasm_support::*;

const CALLS: usize = 256;
const BLOCK: usize = 256;

/// Runs `body` on a new thread (first wasm call on that thread happens inside it).
fn on_fresh_thread<T: Send + 'static>(body: impl FnOnce() -> T + Send + 'static) -> T {
    std::thread::spawn(body).join().expect("test thread")
}

/// `CALLS` blocks of `process` with parameter changes and trigger edges, all inside one
/// allocation scope. Returns (allocs, frees).
fn drive(fx: &mut dyn Effect) -> (u64, u64) {
    let mut l = vec![0.0f32; BLOCK];
    let mut r = vec![0.0f32; BLOCK];
    let mut t = Transport::default();
    let scope = se_alloc::Scope::begin();
    for b in 0..CALLS {
        for (i, (x, y)) in l.iter_mut().zip(r.iter_mut()).enumerate() {
            let s = ((b * BLOCK + i) as f32 * 0.03).sin() * 0.5;
            (*x, *y) = (s, -s);
        }
        fx.set_param(0, (b % 7) as f32 / 7.0);
        fx.set_param(RM_DEPTH, (b % 5) as f32 / 4.0);
        fx.set_param(RM_FREQ, 100.0 + b as f32);
        fx.set_param(RM_SYNC, (b / 32 % 2) as f32);
        fx.set_param(RM_TREMOLO, (b % 3) as f32 / 2.0);
        if b % 16 == 0 {
            fx.trigger(b % 32 == 0);
        }
        fx.process(&ctx(t), &mut l, &mut r);
        t.beat = t.beat_at(BLOCK, SR);
        assert!(l.iter().chain(&r).all(|x| x.is_finite()));
    }
    fx.reset();
    (scope.allocs(), scope.frees())
}

#[test]
fn ringmod_process_path_does_not_allocate() {
    assert!(se_alloc::installed(), "counting allocator must be the global allocator");
    let mut fx = instantiate(&ringmod(), 300);
    let status = fx.status();
    let (allocs, frees) = on_fresh_thread(move || drive(&mut fx));
    assert_eq!((allocs, frees), (0, 0), "allocations/frees on the audio path");
    assert!(status.calls() >= CALLS as u64);
}

#[test]
fn ringmod_in_a_slot_does_not_allocate() {
    assert!(se_alloc::installed());
    let mut slot = FxSlot::new(Box::new(instantiate(&ringmod(), 300)), SR, true);
    let (allocs, frees) = on_fresh_thread(move || {
        let mut l = vec![0.25f32; BLOCK];
        let mut r = vec![0.25f32; BLOCK];
        let scope = se_alloc::Scope::begin();
        for b in 0..CALLS {
            slot.set_param(RM_FREQ, 200.0 + b as f32);
            slot.set_env(if b % 40 < 20 { 1.0 } else { 0.0 });
            slot.set_trigger(b % 40 < 20);
            slot.process(&ctx(Transport::default()), &mut l, &mut r);
        }
        (scope.allocs(), scope.frees())
    });
    assert_eq!((allocs, frees), (0, 0));
}

#[test]
fn over_budget_fade_and_bypass_do_not_allocate() {
    assert!(se_alloc::installed());
    let m = compile(&abi_module(BUSY, INVERT, "")).unwrap();
    let mut fx = instantiate(&m, 1);
    let status = fx.status();
    let (allocs, frees) = on_fresh_thread(move || {
        let mut l = vec![0.5f32; BLOCK];
        let mut r = vec![0.5f32; BLOCK];
        let scope = se_alloc::Scope::begin();
        for _ in 0..CALLS {
            fx.set_param(1, 20_000.0);
            fx.process(&ctx(Transport::default()), &mut l, &mut r);
        }
        (scope.allocs(), scope.frees())
    });
    assert_eq!(status.state(), STATE_OVER_BUDGET);
    assert_eq!((allocs, frees), (0, 0));
}

#[test]
fn memory_growth_bypass_does_not_allocate() {
    assert!(se_alloc::installed());
    let m = compile(&abi_module(&when_p0("(drop (memory.grow (i32.const 1)))"), INVERT, "")).unwrap();
    let mut fx = instantiate(&m, 300);
    let status = fx.status();
    let (allocs, frees) = on_fresh_thread(move || {
        let mut l = vec![0.5f32; BLOCK];
        let mut r = vec![0.5f32; BLOCK];
        let scope = se_alloc::Scope::begin();
        for b in 0..CALLS {
            fx.set_param(1, if b >= 10 { 1.0 } else { 0.0 });
            fx.process(&ctx(Transport::default()), &mut l, &mut r);
        }
        (scope.allocs(), scope.frees())
    });
    assert_eq!(status.state(), STATE_MEMORY);
    assert_eq!((allocs, frees), (0, 0));
}

#[test]
fn pass_through_after_a_trap_does_not_allocate() {
    assert!(se_alloc::installed());
    let m = compile(&abi_module(&when_p0("unreachable"), INVERT, "")).unwrap();
    let mut fx = instantiate(&m, 300);
    let status = fx.status();
    let (allocs, frees) = on_fresh_thread(move || {
        let mut l = vec![0.5f32; BLOCK];
        let mut r = vec![0.5f32; BLOCK];
        let c = ctx(Transport::default());
        fx.process(&c, &mut l, &mut r);
        // the trap itself allocates once inside wasmtime (boxed trap error, kept in `fx`)
        fx.set_param(1, 1.0);
        fx.process(&c, &mut l, &mut r);
        let scope = se_alloc::Scope::begin();
        for _ in 0..CALLS {
            fx.process(&c, &mut l, &mut r);
            fx.trigger(true);
            fx.trigger(false);
        }
        fx.reset();
        (scope.allocs(), scope.frees())
    });
    assert_eq!(status.state(), STATE_TRAPPED);
    assert_eq!((allocs, frees), (0, 0));
}
