//! Offline harness: build a graph from TOML and run the RT engine against in-memory ports.
#![allow(dead_code)]

use se_audio::builder::{self, Built, PortTable};
use se_audio::config::{self, AudioConfig};
use se_audio::graph::{Cycle, Engine, PortIo, RtMsg, RtOut, RtStats, Target};
use se_audio::sounds::{self, SoundCache};
use se_audio::wasmfx::DspPatches;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

pub const SR: f32 = 48000.0;

pub struct FakeIo {
    pub ins: Vec<Vec<f32>>,
    pub outs: Vec<Vec<f32>>,
}

impl PortIo for FakeIo {
    fn input(&self, port: usize) -> Option<&[f32]> {
        self.ins.get(port).map(|v| v.as_slice())
    }
    fn output(&mut self, port: usize) -> Option<&mut [f32]> {
        self.outs.get_mut(port).map(|v| v.as_mut_slice())
    }
}

pub struct Rig {
    pub cfg: AudioConfig,
    pub ports: PortTable,
    pub engine: Engine,
    pub tx: rtrb::Producer<RtMsg>,
    pub rx: rtrb::Consumer<RtOut>,
    pub stats: Arc<RtStats>,
    pub io: FakeIo,
    pub generation: u32,
    pub built: Built,
    pub t: u64,
    pub quantum: usize,
    /// Absolute sample position of `run` generators (continuous across calls).
    pub pos: u64,
}

pub fn cfg(toml_src: &str) -> AudioConfig {
    let t: toml::Table = toml::from_str(toml_src).unwrap();
    config::parse(&t, &BTreeMap::new()).unwrap()
}

impl Rig {
    pub fn new(toml_src: &str) -> Rig {
        Rig::with_root(toml_src, Path::new("/nonexistent"))
    }

    pub fn with_root(toml_src: &str, root: &Path) -> Rig {
        let cfg = cfg(toml_src);
        let (bank, errs) = sounds::build_bank(&cfg, root, &mut SoundCache::default());
        assert!(errs.is_empty(), "{errs:?}");
        let mut ports = PortTable::default();
        let mut dsp = DspPatches::new(1000);
        dsp.scan(root);
        let mut built = builder::build(&cfg, 1, &mut ports, bank, &dsp);
        assert!(built.errors.is_empty(), "{:?}", built.errors);
        let (mut tx, rx_rt) = rtrb::RingBuffer::new(16384);
        let (tx_rt, rx) = rtrb::RingBuffer::new(16384);
        let stats = Arc::new(RtStats::default());
        let engine = Engine::new(SR, rx_rt, tx_rt, stats.clone());
        tx.push(RtMsg::Swap(built.graph.take().unwrap())).ok().unwrap();
        let quantum = cfg.quantum as usize;
        let io = FakeIo { ins: vec![vec![0.0; quantum]; ports.ins.len()], outs: vec![vec![0.0; quantum]; ports.outs.len()] };
        Rig { cfg, ports, engine, tx, rx, stats, io, generation: 1, built, t: 1_000_000_000, quantum, pos: 0 }
    }

    pub fn in_port(&self, name: &str) -> usize {
        self.ports.ins.iter().position(|p| p == name).unwrap_or_else(|| panic!("no input port {name}: {:?}", self.ports.ins))
    }

    pub fn out_port(&self, name: &str) -> usize {
        self.ports.outs.iter().position(|p| p == name).unwrap_or_else(|| panic!("no output port {name}: {:?}", self.ports.outs))
    }

    pub fn param(&mut self, target: Target, value: f32) {
        self.tx.push(RtMsg::Param { generation: self.generation, target, value }).ok().unwrap();
    }

    pub fn send(&mut self, m: RtMsg) {
        assert!(self.tx.push(m).is_ok());
    }

    /// Run one cycle with `fill(port, i)` for inputs; returns nothing (read `io.outs`).
    pub fn cycle(&mut self, mut fill: impl FnMut(usize, usize) -> f32) {
        for (p, buf) in self.io.ins.iter_mut().enumerate() {
            for (i, x) in buf.iter_mut().enumerate() {
                *x = fill(p, i);
            }
        }
        for o in &mut self.io.outs {
            o.fill(0.0);
        }
        self.engine.process(&mut self.io, self.quantum, Cycle { nsec: self.t, rate: SR as u32 });
        self.t += (self.quantum as f64 * 1e9 / SR as f64) as u64;
    }

    /// Run `n` cycles feeding a per-sample generator (absolute sample index), collecting
    /// output port `out`.
    pub fn run(&mut self, n: usize, input: &[usize], mut g: impl FnMut(u64) -> f32, out: usize) -> Vec<f32> {
        let mut v = Vec::with_capacity(n * self.quantum);
        let mut s = self.pos;
        let q = self.quantum;
        for _ in 0..n {
            let base = s;
            let ins = input.to_vec();
            self.cycle(|p, i| if ins.contains(&p) { g(base + i as u64) } else { 0.0 });
            v.extend_from_slice(&self.io.outs[out]);
            s += q as u64;
        }
        self.pos = s;
        v
    }

    pub fn drain(&mut self) -> Vec<RtOut> {
        let mut v = Vec::new();
        while let Ok(o) = self.rx.pop() {
            v.push(o);
        }
        v
    }
}

pub fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

pub fn max_abs(x: &[f32]) -> f32 {
    x.iter().fold(0.0, |m, v| m.max(v.abs()))
}

/// Largest |second difference| (click detector).
pub fn max_d2(x: &[f32]) -> f32 {
    x.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs()).fold(0.0, f32::max)
}

pub fn sine(freq: f32, amp: f32) -> impl FnMut(u64) -> f32 {
    move |i| amp * (std::f32::consts::TAU * freq * i as f32 / SR).sin()
}
