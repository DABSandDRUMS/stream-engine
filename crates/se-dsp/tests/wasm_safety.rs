//! Budget overrun, hard deadline, traps, and memory growth: each bypasses the module without
//! clicks and reports why through `WasmStatus`.
#![cfg(feature = "wasm")]

mod wasm_support;

use se_dsp::wasm::{BYPASS_FADE_MS, DECLICK_SAMPLES, STATE_MEMORY, STATE_OK, STATE_OVER_BUDGET, STATE_TRAPPED, WasmFx};
use se_dsp::{Effect, Transport};
use std::time::{Duration, Instant};
use wasm_support::*;

const BLOCK: usize = 256;

fn sine(n: usize, hz: f32, amp: f32) -> Vec<f32> {
    (0..n).map(|i| amp * (std::f32::consts::TAU * hz * i as f32 / SR).sin()).collect()
}

/// Module that inverts its input and misbehaves (`bad`) once param slot 0 is set.
fn faulty(bad: &str, extra: &str, budget_us: u32) -> WasmFx {
    let m = compile(&abi_module(&when_p0(bad), INVERT, extra)).expect("compiles");
    instantiate(&m, budget_us)
}

/// Runs `fx` on `input` (both channels) block by block; before block `fault_at`, sets param
/// slot 0 to 1. Returns the left output and the wall time of the faulting block.
fn run_fault(fx: &mut WasmFx, input: &[f32], fault_at: usize) -> (Vec<f32>, Duration) {
    let mut out = input.to_vec();
    let mut right = input.to_vec();
    let mut t = Transport::default();
    let mut took = Duration::ZERO;
    for (b, (l, r)) in out.chunks_mut(BLOCK).zip(right.chunks_mut(BLOCK)).enumerate() {
        if b == fault_at {
            fx.set_param(1, 1.0);
        }
        let start = Instant::now();
        fx.process(&ctx(t), l, r);
        if b == fault_at {
            took = start.elapsed();
        }
        t.beat = t.beat_at(l.len(), SR);
    }
    (out, took)
}

/// Output is `-input` before the fault block, then the input with a declick offset that
/// fades out over `DECLICK_SAMPLES`, then exactly the input.
fn assert_declicked_bypass(out: &[f32], input: &[f32], fault_block: usize) {
    let at = fault_block * BLOCK;
    for i in 0..at {
        assert_eq!(out[i], -input[i], "guest output before the fault [{i}]");
    }
    let offset = -input[at - 1] - input[at];
    let d = DECLICK_SAMPLES as usize;
    for k in 0..d {
        let want = input[at + k] + offset * (d - 1 - k) as f32 / d as f32;
        assert!((out[at + k] - want).abs() < 1e-6, "declick [{k}] {} vs {want}", out[at + k]);
    }
    assert_eq!(&out[at + d..], &input[at + d..], "pass-through after the declick");
    // no step larger than the signal's own slope plus the declick slope
    let natural = max_step(input);
    let step = max_step(&out[at - 1..]);
    assert!(step <= natural + offset.abs() / d as f32 + 1e-6, "step {step} at the switch");
}

#[test]
fn over_budget_crossfades_to_dry_then_stops_calling() {
    // 1 µs budget: 20k loop iterations are far over it, and far below the 2 ms deadline.
    let m = compile(&abi_module(BUSY, INVERT, "")).unwrap();
    let mut fx = instantiate(&m, 1);
    fx.set_param(1, 20_000.0);
    let status = fx.status();
    let n = 16 * BLOCK;
    let input = vec![0.5f32; n];
    let (out, _) = run(&mut fx, &input, &input, BLOCK);

    assert_eq!(status.state(), STATE_OVER_BUDGET);
    let text = status.error_text().expect("reason");
    assert!(text.contains("over budget"), "{text}");
    // blocks 0–1 run the guest, block 2 is the third overrun: the fade starts there and
    // lasts 10 ms (480 samples = the rest of block 2 and most of block 3)
    let fade = (BYPASS_FADE_MS * 0.001 * SR) as usize;
    assert!(out[..2 * BLOCK].iter().all(|&x| x == -0.5), "guest output before the bypass");
    assert!(out[2 * BLOCK + fade..].iter().all(|&x| x == 0.5), "dry after the fade");
    let step = max_step(&out);
    assert!(step <= 1.0 / fade as f32 + 1e-5, "switch step {step}");
    assert_eq!(status.calls(), 4, "guest runs during the fade only");
    assert!(status.overruns() >= 3);
    assert!(status.max_us() >= 1);

    run(&mut fx, &input, &input, BLOCK);
    assert_eq!(status.calls(), 4, "bypassed: guest no longer called");
    fx.reset();
    run(&mut fx, &input, &input, BLOCK);
    assert_eq!(status.state(), STATE_OVER_BUDGET, "bypass is sticky across reset");
}

#[test]
fn within_budget_keeps_running() {
    let m = compile(&abi_module(INVERT, "", "")).unwrap();
    let mut fx = instantiate(&m, 5_000);
    let status = fx.status();
    let input = vec![0.25f32; 64 * BLOCK];
    let (out, _) = run(&mut fx, &input, &input, BLOCK);
    assert_eq!(status.state(), STATE_OK);
    assert_eq!(status.error_text(), None);
    assert_eq!(status.calls(), 64);
    assert!(out.iter().all(|&x| x == -0.25));
}

#[test]
fn infinite_loop_is_killed_at_the_deadline() {
    // budget 300 µs → hard deadline max(1.2 ms, 2 ms) = 2 ms, then one grace window
    let mut fx = faulty("(loop $forever (br $forever))", "", 300);
    let status = fx.status();
    let input = sine(12 * BLOCK, 110.0, 0.5);
    let (out, took) = run_fault(&mut fx, &input, 5);
    assert_eq!(status.state(), STATE_TRAPPED);
    let text = status.error_text().expect("reason");
    assert!(text.contains("hard deadline") && text.contains("interrupt"), "{text}");
    assert!(took >= Duration::from_millis(2), "killed before the hard deadline: {took:?}");
    // ≈ 3–4 ms on an idle machine; generous for a loaded one (a preempted guest gets extra windows)
    assert!(took < Duration::from_secs(1), "deadline not enforced: {took:?}");
    assert_declicked_bypass(&out, &input, 5);
    assert_eq!(status.calls(), 6, "no calls after the trap");

    // reset after a trap must not enter the guest again
    fx.reset();
    let (again, _) = run(&mut fx, &input, &input, BLOCK);
    assert_eq!(again, input);
    assert_eq!(status.calls(), 6);
}

#[test]
fn out_of_bounds_access_traps() {
    let mut fx = faulty("(f32.store (i32.const -8) (f32.const 1))", "", 300);
    let status = fx.status();
    let input = sine(8 * BLOCK, 70.0, 0.8);
    let (out, _) = run_fault(&mut fx, &input, 3);
    assert_eq!(status.state(), STATE_TRAPPED);
    let text = status.error_text().expect("reason");
    assert!(text.contains("out of bounds memory access"), "{text}");
    assert_declicked_bypass(&out, &input, 3);
}

#[test]
fn unreachable_traps() {
    let mut fx = faulty("unreachable", "", 300);
    let status = fx.status();
    let input = sine(8 * BLOCK, 200.0, 0.3);
    let (out, _) = run_fault(&mut fx, &input, 2);
    assert_eq!(status.state(), STATE_TRAPPED);
    assert!(status.error_text().unwrap().contains("unreachable"));
    assert_declicked_bypass(&out, &input, 2);
}

#[test]
fn memory_growth_in_process_bypasses() {
    let mut fx = faulty("(drop (memory.grow (i32.const 1)))", "", 300);
    let status = fx.status();
    let input = sine(8 * BLOCK, 50.0, 0.9);
    let (out, _) = run_fault(&mut fx, &input, 4);
    assert_eq!(status.state(), STATE_MEMORY);
    assert!(status.error_text().unwrap().contains("grow memory"), "{:?}", status.error_text());
    assert_declicked_bypass(&out, &input, 4);
    assert_eq!(status.calls(), 5);
}

#[test]
fn memory_size_zero_growth_is_not_growth() {
    // `memory.grow 0` only queries the size
    let mut fx = faulty("(drop (memory.grow (i32.const 0)))", "", 300);
    let status = fx.status();
    let input = sine(4 * BLOCK, 50.0, 0.9);
    let (out, _) = run_fault(&mut fx, &input, 1);
    assert_eq!(status.state(), STATE_OK);
    assert!(out.iter().zip(&input).all(|(o, i)| *o == -*i));
}

#[test]
fn trap_in_reset_bypasses() {
    let m = compile(&abi_module(INVERT, "", r#"(func (export "reset") unreachable)"#)).unwrap();
    let mut fx = instantiate(&m, 300);
    let status = fx.status();
    fx.reset();
    assert_eq!(status.state(), STATE_TRAPPED);
    let input = sine(2 * BLOCK, 100.0, 0.5);
    let (out, _) = run(&mut fx, &input, &input, BLOCK);
    assert_eq!(out, input);
    assert_eq!(status.calls(), 0);
}

#[test]
fn bypass_keeps_the_reported_latency() {
    // latency() = 100: after a failure the dry path is the input delayed by 100 samples
    let mut fx = faulty("(drop (memory.grow (i32.const 1)))", r#"(func (export "latency") (result i32) i32.const 100)"#, 300);
    assert_eq!(fx.latency(), 100);
    let input = sine(8 * BLOCK, 300.0, 0.5);
    let (out, _) = run_fault(&mut fx, &input, 2);
    let at = 2 * BLOCK + DECLICK_SAMPLES as usize;
    for i in at..input.len() {
        assert_eq!(out[i], input[i - 100], "[{i}]");
    }
}

#[test]
fn audio_source_bypass_fades_to_silence() {
    // constant 0.5 generator (ignores its input), then grows memory once armed
    let gen_body = r#"
        (block $done
          (loop $l
            (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
            (local.set $k (i32.shl (local.get $i) (i32.const 2)))
            (f32.store (i32.add (local.get $out) (local.get $k)) (f32.const 0.5))
            (f32.store offset=4096 (i32.add (local.get $out) (local.get $k)) (f32.const 0.5))
            (local.set $i (i32.add (local.get $i) (i32.const 1)))
            (br $l)))"#;
    let m = compile(&abi_module(&when_p0("(drop (memory.grow (i32.const 1)))"), gen_body, "")).unwrap();
    let mut fx = m.instantiate(SR, true, 300).unwrap();
    let bus = sine(4 * BLOCK, 440.0, 0.7);
    let (out, _) = run_fault(&mut fx, &bus, 2);
    let at = 2 * BLOCK;
    assert!(out[..at].iter().all(|&x| x == 0.5));
    assert!(max_step(&out) <= 0.5 / DECLICK_SAMPLES as f32 + 1e-6);
    assert!(out[at + DECLICK_SAMPLES as usize..].iter().all(|&x| x == 0.0), "bypassed source is silent");
}

#[test]
fn non_finite_guest_output_is_zeroed() {
    let nan = INVERT.replace("(f32.neg (f32.load (i32.add (local.get $in) (local.get $k))))", "(f32.const nan)");
    let m = compile(&abi_module(&nan, "", "")).unwrap();
    let mut fx = instantiate(&m, 300);
    let input = vec![0.5f32; BLOCK];
    let (out, _) = run(&mut fx, &input, &input, BLOCK);
    assert!(out.iter().all(|&x| x == 0.0));
}
