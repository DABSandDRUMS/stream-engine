//! Per-block time of `dsp` WebAssembly patches (§23): host overhead with a minimal module
//! (copies input to output) and the ringmod example, 256-frame stereo blocks at 48 kHz.
//!
//! `cargo bench -p se-dsp --features wasm --bench wasm`

use se_dsp::wasm::{WasmHost, WasmModule};
use se_dsp::{Ctx, Effect, Transport};
use std::time::{Duration, Instant};

const FRAMES: usize = 256;
const BLOCKS: usize = 20_000;

/// Smallest ABI v1 module: `process` copies both input channels to the output.
const COPY: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "se_dsp_abi") (result i32) i32.const 1)
  (func (export "init") (param f32 i32 i32) (result i32) i32.const 0)
  (func (export "input_buffer") (result i32) i32.const 0)
  (func (export "output_buffer") (result i32) i32.const 8192)
  (func (export "params_buffer") (result i32) i32.const 16384)
  (func (export "process") (param $in i32) (param $out i32) (param $n i32) (param $p i32)
    (memory.copy (local.get $out) (local.get $in) (i32.shl (local.get $n) (i32.const 2)))
    (memory.copy (i32.add (local.get $out) (i32.const 4096)) (i32.add (local.get $in) (i32.const 4096)) (i32.shl (local.get $n) (i32.const 2)))))
"#;

fn bench(name: &str, module: &WasmModule) {
    let mut fx = module.instantiate(48000.0, false, 0).expect("instantiate");
    for (i, v) in [1.0, 440.0, 0.0, 0.5].into_iter().enumerate() {
        fx.set_param(1 + i, v);
    }
    let mut l: Vec<f32> = (0..FRAMES).map(|i| (i as f32 * 0.05).sin()).collect();
    let mut r = l.clone();
    let mut t = Transport::default();
    let mut times = Vec::with_capacity(BLOCKS);
    let status = fx.status();
    for b in 0..BLOCKS + 1000 {
        let ctx = Ctx { sr: 48000.0, transport: t, key: &[] };
        let start = Instant::now();
        fx.process(&ctx, &mut l, &mut r);
        let took = start.elapsed();
        if let Some(e) = status.error_text() {
            // a preempted call can hit the wall-clock deadline on a loaded, non-RT thread
            println!("{name:<8} bypassed after {b} blocks: {e}");
            break;
        }
        if b >= 1000 {
            times.push(took);
        }
        t.beat = t.beat_at(FRAMES, 48000.0);
    }
    if times.is_empty() {
        return;
    }
    times.sort();
    let mean = times.iter().sum::<Duration>() / times.len() as u32;
    let pct = |p: f64| times[((times.len() - 1) as f64 * p) as usize];
    let us = |d: Duration| d.as_secs_f64() * 1e6;
    println!(
        "{name:<8} {FRAMES}-frame block, {} blocks: mean {:.2} µs  min {:.2}  p50 {:.2}  p99 {:.2}  p99.9 {:.2}  max {:.2}",
        times.len(),
        us(mean),
        us(times[0]),
        us(pct(0.5)),
        us(pct(0.99)),
        us(pct(0.999)),
        us(pct(1.0)),
    );
}

fn main() {
    let host = WasmHost::new().expect("wasm host");
    let copy = host.compile(&wat::parse_str(COPY).expect("wat")).expect("compile copy");
    let ringmod_path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../templates/patches/dsp/ringmod/main.wasm");
    let ringmod = host.compile(&std::fs::read(ringmod_path).expect("ringmod main.wasm")).expect("compile ringmod");
    bench("copy", &copy);
    bench("ringmod", &ringmod);
}
