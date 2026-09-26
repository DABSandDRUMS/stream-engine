//! Offline render tests of the example `dsp` patch `project-example/patches/ringmod`
//! (null test, known outputs, beat sync, tremolo gate) and click-free hot swap in an `FxSlot`.
#![cfg(feature = "wasm")]

mod wasm_support;

use se_dsp::slot::SWAP_MS;
use se_dsp::wasm::WasmFx;
use se_dsp::{Effect, FxSlot, Transport};
use std::f64::consts::TAU;
use wasm_support::*;

/// Deterministic white noise in [-1, 1).
fn noise(n: usize, seed: u32) -> Vec<f32> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (s >> 8) as f32 / (1u32 << 23) as f32 - 1.0
        })
        .collect()
}

fn ringmod_fx(depth: f32, freq: f32, sync: bool, tremolo: f32) -> WasmFx {
    let mut fx = instantiate(&ringmod(), 300);
    fx.set_param(RM_DEPTH, depth);
    fx.set_param(RM_FREQ, freq);
    fx.set_param(RM_SYNC, if sync { 1.0 } else { 0.0 });
    fx.set_param(RM_TREMOLO, tremolo);
    fx
}

#[test]
fn depth_zero_is_an_exact_null() {
    let (l, r) = (noise(9000, 1), noise(9000, 2));
    for sync in [false, true] {
        let mut fx = ringmod_fx(0.0, 777.0, sync, 0.0);
        let mut t = Transport { bpm: 97.0, beat: 1.3, beats_per_bar: 4 };
        // odd block sizes, a trigger edge and a reset in between
        let (a_l, a_r) = run_from(&mut fx, &l[..3000], &r[..3000], 333, &mut t);
        fx.trigger(true);
        let (b_l, b_r) = run_from(&mut fx, &l[3000..6000], &r[3000..6000], 1024, &mut t);
        fx.reset();
        let (c_l, c_r) = run_from(&mut fx, &l[6000..], &r[6000..], 17, &mut t);
        assert_eq!([a_l, b_l, c_l].concat(), l, "left, sync={sync}");
        assert_eq!([a_r, b_r, c_r].concat(), r, "right, sync={sync}");
    }
}

#[test]
fn dc_input_yields_the_scaled_carrier() {
    let mut fx = ringmod_fx(1.0, 1000.0, false, 0.0);
    let n = 8192;
    let (l, r) = run(&mut fx, &vec![0.5; n], &vec![-0.25; n], 256);
    for i in 0..n {
        let c = (TAU * 1000.0 * i as f64 / SR as f64).sin() as f32;
        assert!((l[i] - 0.5 * c).abs() < 1e-5, "left[{i}] = {} vs {}", l[i], 0.5 * c);
        assert!((r[i] + 0.25 * c).abs() < 1e-5, "right[{i}] = {} vs {}", r[i], -0.25 * c);
    }
}

#[test]
fn half_depth_mixes_dry_and_carrier() {
    let mut fx = ringmod_fx(0.5, 300.0, false, 0.0);
    let n = 2048;
    let (l, _) = run(&mut fx, &vec![1.0; n], &vec![1.0; n], 128);
    for (i, &y) in l.iter().enumerate() {
        let c = (TAU * 300.0 * i as f64 / SR as f64).sin() as f32;
        assert!((y - (0.5 + 0.5 * c)).abs() < 1e-5, "[{i}] {y}");
    }
}

#[test]
fn trigger_restarts_the_carrier_phase() {
    let mut fx = ringmod_fx(1.0, 440.0, false, 0.0);
    let dc = vec![1.0f32; 1000];
    run(&mut fx, &dc, &dc, 250);
    fx.trigger(true);
    let (l, _) = run(&mut fx, &dc, &dc, 250);
    for (i, &y) in l.iter().enumerate() {
        let c = (TAU * 440.0 * i as f64 / SR as f64).sin() as f32;
        assert!((y - c).abs() < 1e-5, "after trigger [{i}] {y} vs {c}");
    }
}

#[test]
fn sync_locks_the_carrier_to_the_beat() {
    // 120 bpm = 2 Hz beats; 443 Hz rounds to harmonic 222 → 444 Hz, phase = 222 · beat.
    let mut fx = ringmod_fx(1.0, 443.0, true, 0.0);
    let start = 3.25;
    let mut t = Transport { bpm: 120.0, beat: start, beats_per_bar: 4 };
    let n = 4096;
    let (l, _) = run_from(&mut fx, &vec![0.5; n], &vec![0.5; n], 256, &mut t);
    for (i, &y) in l.iter().enumerate() {
        let beat = start + i as f64 * 2.0 / SR as f64;
        let want = 0.5 * (TAU * (beat * 222.0).fract()).sin() as f32;
        assert!((y - want).abs() < 2e-4, "[{i}] {y} vs {want}");
    }
}

#[test]
fn tremolo_gates_sixteenth_notes_without_clicks() {
    // 120 bpm: a 16th = 6000 samples, open for the first 3000; 2 ms (96 sample) edges.
    let mut fx = ringmod_fx(0.0, 440.0, false, 1.0);
    let n = 24000;
    let (l, _) = run(&mut fx, &vec![1.0; n], &vec![1.0; n], 512);
    for k in 0..4 {
        let base = k * 6000;
        assert!(l[base + 100..base + 2990].iter().all(|&x| x == 1.0), "16th {k} open");
        assert!(l[base + 3100..base + 5990].iter().all(|&x| x == 0.0), "16th {k} closed");
    }
    assert!(max_step(&l) <= 1.0 / 96.0 + 1e-6, "gate edge step {}", max_step(&l));
}

#[test]
fn hot_swap_in_a_slot_crossfades_without_clicks() {
    let invert = compile(&abi_module(INVERT, "", "")).unwrap();
    let mut slot = FxSlot::new(Box::new(instantiate(&invert, 300)), SR, false);
    slot.set_wet(1.0);
    slot.set_dry(0.0);
    let dc = vec![0.5f32; 4096];
    let (before, _) = run(&mut slot_fx(&mut slot), &dc, &dc, 256);
    assert_eq!(before[4095], -0.5, "old instance: inverted");

    let replacement = ringmod_fx(0.0, 440.0, false, 0.0); // identity
    assert!(slot.swap_effect(Box::new(replacement)).is_none());
    let n = 8192;
    let (after, _) = run(&mut slot_fx(&mut slot), &dc.repeat(2), &dc.repeat(2), 256);
    let swap_len = SWAP_MS * 0.001 * SR;
    let step = max_step(&[&before[4000..], &after[..]].concat());
    assert!(step <= 1.0 / swap_len + 1e-5, "swap step {step} (ramp {swap_len} samples)");
    assert_eq!(after[n - 1], 0.5, "new instance: identity");
    let retired = slot.take_retired().expect("old instance retired after the fade");
    // dropped here, off the audio path
    assert_eq!(retired.kind(), "wasm");
}

/// Adapts an `FxSlot` to the `Effect`-based `run` helpers.
struct SlotFx<'a>(&'a mut FxSlot);

fn slot_fx(slot: &mut FxSlot) -> SlotFx<'_> {
    SlotFx(slot)
}

impl Effect for SlotFx<'_> {
    fn kind(&self) -> &'static str {
        self.0.kind()
    }
    fn params(&self) -> &'static [se_dsp::ParamSpec] {
        self.0.params()
    }
    fn set_param(&mut self, idx: usize, value: f32) {
        self.0.set_param(idx, value);
    }
    fn process(&mut self, ctx: &se_dsp::Ctx, l: &mut [f32], r: &mut [f32]) {
        self.0.process(ctx, l, r);
    }
    fn reset(&mut self) {
        self.0.reset();
    }
}
