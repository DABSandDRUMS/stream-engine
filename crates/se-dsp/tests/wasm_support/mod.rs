//! Shared helpers for the `wasm*` integration tests.
#![allow(dead_code)]

use se_dsp::wasm::{WasmFx, WasmHost, WasmModule};
use se_dsp::{Ctx, Effect, Transport};
use std::sync::LazyLock;

pub const SR: f32 = 48000.0;

static HOST: LazyLock<WasmHost> = LazyLock::new(|| WasmHost::new().expect("wasm host"));

pub fn host() -> &'static WasmHost {
    &HOST
}

/// The built, opt-in ring-modulator template.
pub fn ringmod() -> WasmModule {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/patches/dsp/ringmod/main.wasm");
    let bytes = std::fs::read(path).expect("ringmod main.wasm (run templates/patches/dsp/ringmod/build.sh)");
    host().compile(&bytes).expect("ringmod compiles")
}

/// Ringmod param slots (patch.toml params, alphabetical) → `WasmFx` param index `1 + slot`.
pub const RM_DEPTH: usize = 1;
pub const RM_FREQ: usize = 2;
pub const RM_SYNC: usize = 3;
pub const RM_TREMOLO: usize = 4;

pub fn compile(wat: &str) -> Result<WasmModule, String> {
    host().compile(&wat::parse_str(wat).expect("valid wat"))
}

/// A complete ABI v1 module: input at 0, output at 8192, params at 16384 (one 64 KiB page).
/// `body` runs at the start of `process` (locals `$in $out $n $p $i $k`), followed by
/// `tail`; `extra` adds module fields.
pub fn abi_module(body: &str, tail: &str, extra: &str) -> String {
    format!(
        r#"(module
  (memory (export "memory") 1)
  (global $acc (mut i32) (i32.const 0))
  (func (export "se_dsp_abi") (result i32) i32.const 1)
  (func (export "init") (param f32 i32 i32) (result i32) i32.const 0)
  (func (export "input_buffer") (result i32) i32.const 0)
  (func (export "output_buffer") (result i32) i32.const 8192)
  (func (export "params_buffer") (result i32) i32.const 16384)
  (func (export "process") (param $in i32) (param $out i32) (param $n i32) (param $p i32) (local $i i32) (local $k i32)
    {body}
    {tail})
  {extra})"#
    )
}

/// `out = -in` on both planar channels (stride 1024 frames = 4096 bytes).
pub const INVERT: &str = r#"
    (block $done
      (loop $l
        (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
        (local.set $k (i32.shl (local.get $i) (i32.const 2)))
        (f32.store (i32.add (local.get $out) (local.get $k)) (f32.neg (f32.load (i32.add (local.get $in) (local.get $k)))))
        (f32.store offset=4096 (i32.add (local.get $out) (local.get $k)) (f32.neg (f32.load offset=4096 (i32.add (local.get $in) (local.get $k)))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))"#;

/// Guard on patch param slot 0 (`params[8]`, byte offset 32): runs `then` when it is > 0.5.
pub fn when_p0(then: &str) -> String {
    format!("(if (f32.gt (f32.load offset=32 (local.get $p)) (f32.const 0.5)) (then {then}))")
}

/// Busy loop of `p0` iterations (slot 0 = iteration count).
pub const BUSY: &str = r#"
    (local.set $k (i32.trunc_sat_f32_u (f32.load offset=32 (local.get $p))))
    (block $b
      (loop $w
        (br_if $b (i32.eqz (local.get $k)))
        (global.set $acc (i32.add (global.get $acc) (local.get $k)))
        (local.set $k (i32.sub (local.get $k) (i32.const 1)))
        (br $w)))"#;

pub fn ctx(transport: Transport) -> Ctx<'static> {
    Ctx { sr: SR, transport, key: &[] }
}

/// Run `fx` over stereo `l`/`r` in blocks of `block` frames with a 120 bpm transport starting
/// at beat 0; returns the processed signals.
pub fn run(fx: &mut dyn Effect, l: &[f32], r: &[f32], block: usize) -> (Vec<f32>, Vec<f32>) {
    let mut t = Transport::default();
    run_from(fx, l, r, block, &mut t)
}

pub fn run_from(fx: &mut dyn Effect, l: &[f32], r: &[f32], block: usize, t: &mut Transport) -> (Vec<f32>, Vec<f32>) {
    let (mut ol, mut or) = (l.to_vec(), r.to_vec());
    for (cl, cr) in ol.chunks_mut(block).zip(or.chunks_mut(block)) {
        fx.process(&ctx(*t), cl, cr);
        t.beat = t.beat_at(cl.len(), SR);
    }
    (ol, or)
}

pub fn max_step(x: &[f32]) -> f32 {
    x.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max)
}

pub fn instantiate(m: &WasmModule, budget_us: u32) -> WasmFx {
    m.instantiate(SR, false, budget_us).expect("instantiate")
}
