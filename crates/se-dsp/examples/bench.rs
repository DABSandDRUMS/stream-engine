//! Per-block cost of every effect and the sampler (PLAN §23).
//! `cargo run --release -p se-dsp --example bench`

use se_dsp::sampler::{LayerDef, Pick, SampleBank, SampleData, Sampler, SoundDef};
use se_dsp::{Ctx, FxSlot, Transport, create, kinds};
use std::time::Instant;

const SR: f32 = 48000.0;
const BLOCKS: usize = 2000;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.total_cmp(b));
    v[v.len() / 2]
}

fn main() {
    println!("{:<12} {:>14} {:>14} {:>14}", "effect", "64 fr ns (%)", "128 fr ns (%)", "256 fr ns (%)");
    for kind in kinds() {
        let mut row = format!("{kind:<12}");
        for n in [64usize, 128, 256] {
            let mut slot = FxSlot::new(create(kind, SR).unwrap(), SR, true);
            slot.set_trigger(true);
            slot.set_env(1.0);
            let mut l: Vec<f32> = (0..n).map(|i| (i as f32 * 0.05).sin() * 0.5).collect();
            let mut r = l.clone();
            let mut times = Vec::with_capacity(BLOCKS);
            let mut beat = 0.0;
            for _ in 0..BLOCKS {
                let t = Instant::now();
                slot.process(&Ctx { sr: SR, transport: Transport { bpm: 128.0, beat, beats_per_bar: 4 }, key: &[] }, &mut l, &mut r);
                times.push(t.elapsed().as_nanos() as f64);
                beat += n as f64 * 128.0 / 60.0 / SR as f64;
            }
            let ns = median(times);
            let period = n as f64 / SR as f64 * 1e9;
            row.push_str(&format!(" {:>8.0} ({:>4.2})", ns, ns / period * 100.0));
        }
        println!("{row}");
    }
    let mut bank = SampleBank {
        samples: vec![SampleData { name: "s".into(), l: vec![0.1; 480000], r: vec![0.1; 480000] }],
        sounds: vec![SoundDef {
            name: "s".into(),
            layers: vec![LayerDef { vel: [0.0, 1.0], samples: vec![0] }],
            pick: Pick::RoundRobin,
            gain: 1.0,
            choke: None,
            max_voices: 16,
            next: 0,
        }],
    };
    let mut row = format!("{:<12}", "sampler x16");
    for n in [64usize, 128, 256] {
        let mut s = Sampler::new(16, SR);
        for k in 0..16 {
            s.play(&mut bank, 0, 1.0, 1.0, 0.0, k as f32 * 0.1);
        }
        let mut l = vec![0.0f32; n];
        let mut r = vec![0.0f32; n];
        let mut times = Vec::with_capacity(BLOCKS);
        for _ in 0..BLOCKS {
            let t = Instant::now();
            s.process(&bank, &mut l, &mut r);
            times.push(t.elapsed().as_nanos() as f64);
        }
        let ns = median(times);
        row.push_str(&format!(" {:>8.0} ({:>4.2})", ns, ns / (n as f64 / SR as f64 * 1e9) * 100.0));
    }
    println!("{row}");
}
