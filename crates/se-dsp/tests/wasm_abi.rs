//! ABI v1 validation (compile and instantiate errors) and the params-block contract.
#![cfg(feature = "wasm")]

mod wasm_support;

use se_dsp::wasm::{H_BAR_PHASE, H_BEAT_PHASE, H_BPM, H_ENV, H_TRIGGERS, HEADER_LEN, PARAMS_LEN, WASM_PARAM_SLOTS};
use se_dsp::{Effect, MAX_BLOCK, Transport};
use wasm_support::*;

fn compile_err(wat: &str) -> String {
    match compile(wat) {
        Ok(_) => panic!("module unexpectedly accepted"),
        Err(e) => e,
    }
}

fn instantiate_err(wat: &str) -> String {
    match compile(wat).expect("compiles").instantiate(SR, false, 0) {
        Ok(_) => panic!("instance unexpectedly created"),
        Err(e) => e,
    }
}

fn assert_contains(err: &str, needle: &str) {
    assert!(err.contains(needle), "error `{err}` should mention `{needle}`");
}

#[test]
fn well_formed_module_is_accepted() {
    let m = compile(&abi_module(INVERT, "", "")).expect("valid module");
    let fx = m.instantiate(SR, false, 300).expect("instance");
    assert_eq!(fx.kind(), "wasm");
    assert_eq!(fx.latency(), 0);
}

#[test]
fn rejects_non_wasm_bytes() {
    let err = host().compile(b"not a wasm module").err().expect("rejected");
    assert_contains(&err, "invalid wasm module");
}

#[test]
fn rejects_missing_exports() {
    let src = abi_module(INVERT, "", "");
    let err = compile_err(&src.replace(r#"(export "process")"#, ""));
    assert_contains(&err, "missing export `process`");
    let err = compile_err(&src.replace(r#"(func (export "params_buffer") (result i32) i32.const 16384)"#, ""));
    assert_contains(&err, "missing export `params_buffer`");
    let err = compile_err(&src.replace(r#"(memory (export "memory") 1)"#, "(memory 1)"));
    assert_contains(&err, "missing export `memory`");
}

#[test]
fn rejects_wrong_signatures() {
    let src = abi_module(INVERT, "", "");
    let err = compile_err(&src.replace(r#"(func (export "init") (param f32 i32 i32)"#, r#"(func (export "init") (param i32 i32 i32)"#));
    assert_contains(&err, "export `init` must be (f32, i32, i32) -> (i32)");
    // `process` with three params ($p demoted to a local)
    let err = compile_err(&src.replace("(param $p i32) (local $i i32)", "(local $p i32) (local $i i32)"));
    assert_contains(&err, "export `process` must be (i32, i32, i32, i32) -> ()");
    // optional exports are checked too when present
    let err = compile_err(&abi_module(INVERT, "", r#"(func (export "latency") (result f32) f32.const 0)"#));
    assert_contains(&err, "export `latency` must be () -> (i32)");
    let err = compile_err(&abi_module(INVERT, "", r#"(func (export "reset") (param i32))"#));
    assert_contains(&err, "export `reset` must be () -> ()");
    // the right name but not a function
    let err = compile_err(&src.replace(r#"(func (export "se_dsp_abi") (result i32) i32.const 1)"#, r#"(global (export "se_dsp_abi") i32 (i32.const 1))"#));
    assert_contains(&err, "export `se_dsp_abi` must be a function");
}

#[test]
fn rejects_any_import() {
    let src = abi_module(INVERT, "", "");
    let with_import = src.replace(r#"(memory (export "memory") 1)"#, r#"(import "env" "log" (func $log (param i32))) (memory (export "memory") 1)"#);
    let err = compile_err(&with_import);
    assert_contains(&err, "must not have imports");
    assert_contains(&err, "env.log");
}

#[test]
fn rejects_other_abi_versions() {
    let src = abi_module(INVERT, "", "");
    let err = compile_err(&src.replace(r#"(func (export "se_dsp_abi") (result i32) i32.const 1)"#, r#"(func (export "se_dsp_abi") (result i32) i32.const 2)"#));
    assert_contains(&err, "se_dsp_abi() returned 2");
}

#[test]
fn rejects_bad_buffer_offsets() {
    let src = abi_module(INVERT, "", "");
    // output region (8 KiB) would end past the single 64 KiB page
    let err = instantiate_err(&src.replace("i32.const 8192)", "i32.const 60000)"));
    assert_contains(&err, "output_buffer() = 60000");
    // negative offsets are huge u32 addresses
    let err = instantiate_err(&src.replace("i32.const 16384)", "i32.const -4)"));
    assert_contains(&err, "params_buffer()");
    let err =
        instantiate_err(&src.replace("(func (export \"input_buffer\") (result i32) i32.const 0)", "(func (export \"input_buffer\") (result i32) i32.const 2)"));
    assert_contains(&err, "not 4-byte aligned");
    // params inside the input buffer
    let err = instantiate_err(&src.replace("i32.const 16384)", "i32.const 4096)"));
    assert_contains(&err, "params buffer overlaps");
    // input and output partially overlapping
    let err = instantiate_err(&src.replace("i32.const 8192)", "i32.const 4096)"));
    assert_contains(&err, "partially overlap");
}

#[test]
fn in_place_buffers_are_allowed() {
    // input == output: the guest processes in place
    let m = compile(&abi_module("", "", "").replace("i32.const 8192)", "i32.const 0)")).expect("compiles");
    let mut fx = m.instantiate(SR, false, 0).expect("in-place buffers are valid");
    let input: Vec<f32> = (0..256).map(|i| i as f32 * 0.001).collect();
    let (l, r) = run(&mut fx, &input, &input, 256);
    assert_eq!(l, input, "an empty in-place process is the identity");
    assert_eq!(r, input);
}

#[test]
fn init_errors_and_memory_limit() {
    let src = abi_module(INVERT, "", "");
    let err = instantiate_err(&src.replace(
        r#"(func (export "init") (param f32 i32 i32) (result i32) i32.const 0)"#,
        r#"(func (export "init") (param f32 i32 i32) (result i32) i32.const -7)"#,
    ));
    assert_contains(&err, "returned error -7");
    // init may grow memory within the 64 MiB limit ...
    let grow_ok = r#"(func (export "init") (param f32 i32 i32) (result i32)
        (if (result i32) (i32.lt_s (memory.grow (i32.const 100)) (i32.const 0)) (then (i32.const -1)) (else (i32.const 0))))"#;
    let init = r#"(func (export "init") (param f32 i32 i32) (result i32) i32.const 0)"#;
    let m = compile(&src.replace(init, grow_ok)).expect("compiles");
    m.instantiate(SR, false, 0).expect("growth during init is allowed");
    // ... but not beyond it (1100 pages = 68.75 MiB): memory.grow fails inside the guest
    let err = instantiate_err(&src.replace(init, &grow_ok.replace("i32.const 100", "i32.const 1100")));
    assert_contains(&err, "returned error -1");
    // a declared initial memory above the limit is rejected at compile time
    let err = compile_err(&src.replace(r#"(memory (export "memory") 1)"#, r#"(memory (export "memory") 1025)"#));
    assert_contains(&err, "exceeds the 64 MiB limit");
}

#[test]
fn latency_export_is_validated_and_reported() {
    let lat = |n: i32| abi_module(INVERT, "", &format!(r#"(func (export "latency") (result i32) i32.const {n})"#));
    let fx = compile(&lat(100)).unwrap().instantiate(SR, false, 0).expect("instance");
    assert_eq!(fx.latency(), 100);
    assert_contains(&instantiate_err(&lat(-1)), "latency() = -1");
    assert_contains(&instantiate_err(&lat(1 << 20)), "latency() = 1048576");
}

/// Writes the whole params block into the left output and the input channels swapped into
/// the right output / rest of the left output, exposing the host's layout.
const PROBE: &str = r#"
    (block $done
      (loop $l
        (br_if $done (i32.ge_u (local.get $i) (local.get $n)))
        (local.set $k (i32.shl (local.get $i) (i32.const 2)))
        (f32.store (i32.add (local.get $out) (local.get $k))
          (if (result f32) (i32.lt_u (local.get $i) (i32.const 72))
            (then (f32.load (i32.add (local.get $p) (local.get $k))))
            (else (f32.load offset=4096 (i32.add (local.get $in) (local.get $k))))))
        (f32.store offset=4096 (i32.add (local.get $out) (local.get $k)) (f32.load (i32.add (local.get $in) (local.get $k))))
        (local.set $i (i32.add (local.get $i) (i32.const 1)))
        (br $l)))"#;

#[test]
fn params_block_and_planar_layout() {
    assert_eq!(PARAMS_LEN, 72);
    let m = compile(&abi_module(PROBE, "", "")).unwrap();
    let mut fx = m.instantiate(SR, false, 0).unwrap();
    assert_eq!(fx.params().len(), 1 + WASM_PARAM_SLOTS);
    assert_eq!(fx.params()[0].name, "env");
    assert_eq!(fx.params()[1].name, "p0");
    assert_eq!(fx.params()[64].name, "p63");
    fx.set_param(0, 0.25);
    for k in 0..WASM_PARAM_SLOTS {
        fx.set_param(1 + k, 100.0 + k as f32);
    }
    fx.set_param(1 + WASM_PARAM_SLOTS, 5.0); // out of range: ignored
    fx.trigger(true);
    fx.trigger(true); // still on: not a new edge
    fx.trigger(false);
    fx.trigger(true);
    let t = Transport { bpm: 128.0, beat: 9.75, beats_per_bar: 4 };
    let n = 200;
    let l_in: Vec<f32> = (0..n).map(|i| i as f32).collect();
    let r_in: Vec<f32> = (0..n).map(|i| -(i as f32) - 0.5).collect();
    let (mut l, mut r) = (l_in.clone(), r_in.clone());
    fx.process(&ctx(t), &mut l, &mut r);
    assert_eq!(l[H_ENV], 0.25);
    assert_eq!(l[H_BPM], 128.0);
    assert_eq!(l[H_BEAT_PHASE], 0.75);
    assert_eq!(l[H_BAR_PHASE], 0.4375); // beat 9.75 of a 4-beat bar = 1.75 / 4
    assert_eq!(l[H_TRIGGERS], 2.0);
    assert_eq!(&l[5..HEADER_LEN], &[0.0; 3], "reserved header fields are 0");
    for k in 0..WASM_PARAM_SLOTS {
        assert_eq!(l[HEADER_LEN + k], 100.0 + k as f32, "slot {k}");
    }
    assert_eq!(&l[PARAMS_LEN..], &r_in[PARAMS_LEN..], "right input is at in + max_frames*4");
    assert_eq!(r, l_in, "right output is at out + max_frames*4");
    assert_eq!(MAX_BLOCK, 1024, "the WAT modules assume a 4096-byte channel stride");
}

#[test]
fn source_layer_gets_silent_input() {
    let m = compile(&abi_module(INVERT, "", "")).unwrap();
    let mut fx = m.instantiate(SR, true, 0).unwrap();
    let input = vec![0.7f32; 512];
    let (l, r) = run(&mut fx, &input, &input, 256);
    assert!(l.iter().chain(&r).all(|&x| x == 0.0), "an audio source never sees the bus input");
}
