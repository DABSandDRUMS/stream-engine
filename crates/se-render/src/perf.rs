//! Always-on render instrumentation (§21): GPU timestamps per pass group, CPU frame time,
//! dropped/late frames, VRAM. The render thread writes plain atomics; the IO side publishes
//! them as `perf.*` state at a few Hz.

use crate::resources::MappedRing;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Pass groups timed on the GPU (`perf.pass.<name>_ms`).
pub const PASSES: [&str; 6] = ["sources", "wide", "tall", "preview", "atlas", "output"];
pub const P_SOURCES: usize = 0;
pub const P_WIDE: usize = 1;
pub const P_TALL: usize = 2;
pub const P_PREVIEW: usize = 3;
pub const P_ATLAS: usize = 4;
pub const P_OUTPUT: usize = 5;
const SLOTS: u32 = (PASSES.len() * 2) as u32;

fn f(a: &AtomicU32) -> f32 {
    f32::from_bits(a.load(Ordering::Relaxed))
}

fn set(a: &AtomicU32, v: f32) {
    a.store(v.to_bits(), Ordering::Relaxed);
}

/// Shared between the render thread (writer) and the IO task (reader).
#[derive(Default)]
pub struct Stats {
    pub frames: AtomicU64,
    pub dropped: AtomicU64,
    pub late: AtomicU64,
    frame_ms: AtomicU32,
    frame_ms_max: AtomicU32,
    gpu_ms: AtomicU32,
    fps: AtomicU32,
    pass_ms: [AtomicU32; PASSES.len()],
    pub vram_bytes: AtomicU64,
    pub vram_budget: AtomicU64,
    /// Own allocations (textures + buffers), bytes.
    pub own_bytes: AtomicU64,
    pub used_sources: [AtomicU64; 4],
    pub used_generation: AtomicU64,
    limit: AtomicU32,
    flash_rate: AtomicU32,
    pub limited: AtomicBool,
    pub exporting: AtomicBool,
    pub export_error: AtomicBool,
    pub device_ok: AtomicBool,
    pub recoveries: AtomicU64,
    pub alloc_violations: AtomicU64,
    pub arena_overflow: AtomicBool,
    /// Last frame sequence per canvas (frames.sock `seq`, monotonic across device loss).
    pub canvas_seq: [AtomicU64; 4],
    /// Effect passes in the last frame, and effects that ran inside fused passes.
    fx_passes: AtomicU32,
    fx_fused: AtomicU32,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct StatsView {
    pub frames: u64,
    pub dropped: u64,
    pub late: u64,
    pub frame_ms: f32,
    pub frame_ms_max: f32,
    pub gpu_ms: f32,
    pub fps: f32,
    pub pass_ms: [f32; PASSES.len()],
    pub vram_mb: f32,
    pub vram_budget_mb: f32,
    pub own_mb: f32,
    pub limit: f32,
    pub flash_rate: f32,
    pub limited: bool,
    pub exporting: bool,
    pub export_error: bool,
    pub device_ok: bool,
    pub recoveries: u64,
    pub alloc_violations: u64,
    pub fx_passes: u32,
    pub fx_fused: u32,
}

impl Stats {
    pub fn set_frame(&self, ms: f32) {
        set(&self.frame_ms, ms);
        if ms > f(&self.frame_ms_max) {
            set(&self.frame_ms_max, ms);
        }
    }
    pub fn set_fx(&self, passes: u32, fused: u32) {
        self.fx_passes.store(passes, Ordering::Relaxed);
        self.fx_fused.store(fused, Ordering::Relaxed);
    }
    pub fn set_fps(&self, v: f32) {
        set(&self.fps, v);
    }
    pub fn set_gpu(&self, total: f32, passes: &[f32; PASSES.len()]) {
        set(&self.gpu_ms, total);
        for (a, v) in self.pass_ms.iter().zip(passes) {
            set(a, *v);
        }
    }
    pub fn set_flash(&self, limit: f32, rate: f32, limited: bool) {
        set(&self.limit, limit);
        set(&self.flash_rate, rate);
        self.limited.store(limited, Ordering::Relaxed);
    }
    pub fn set_used(&self, bits: &[u64; 4]) {
        let mut changed = false;
        for (a, b) in self.used_sources.iter().zip(bits) {
            if a.swap(*b, Ordering::Relaxed) != *b {
                changed = true;
            }
        }
        if changed {
            self.used_generation.fetch_add(1, Ordering::Release);
        }
    }
    pub fn used(&self) -> [u64; 4] {
        [0, 1, 2, 3].map(|i| self.used_sources[i].load(Ordering::Relaxed))
    }
    /// Snapshot and reset the per-interval maximum.
    pub fn view(&self) -> StatsView {
        let mb = |b: u64| b as f32 / (1024.0 * 1024.0);
        let v = StatsView {
            frames: self.frames.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            late: self.late.load(Ordering::Relaxed),
            frame_ms: f(&self.frame_ms),
            frame_ms_max: f(&self.frame_ms_max),
            gpu_ms: f(&self.gpu_ms),
            fps: f(&self.fps),
            pass_ms: std::array::from_fn(|i| f(&self.pass_ms[i])),
            vram_mb: mb(self.vram_bytes.load(Ordering::Relaxed)),
            vram_budget_mb: mb(self.vram_budget.load(Ordering::Relaxed)),
            own_mb: mb(self.own_bytes.load(Ordering::Relaxed)),
            limit: f(&self.limit),
            flash_rate: f(&self.flash_rate),
            limited: self.limited.load(Ordering::Relaxed),
            exporting: self.exporting.load(Ordering::Relaxed),
            export_error: self.export_error.load(Ordering::Relaxed),
            device_ok: self.device_ok.load(Ordering::Relaxed),
            recoveries: self.recoveries.load(Ordering::Relaxed),
            alloc_violations: self.alloc_violations.load(Ordering::Relaxed),
            fx_passes: self.fx_passes.load(Ordering::Relaxed),
            fx_fused: self.fx_fused.load(Ordering::Relaxed),
        };
        set(&self.frame_ms_max, 0.0);
        v
    }
}

/// GPU timestamp queries for the pass groups, read back a few frames later.
pub struct Timestamps {
    sets: Vec<wgpu::QuerySet>,
    resolve: wgpu::Buffer,
    readback: MappedRing,
    masks: [u64; 4],
    slot: usize,
    written: [bool; PASSES.len()],
    period_ns: f32,
    pub last: [f32; PASSES.len()],
    pub total: f32,
}

impl Timestamps {
    pub fn new(device: &wgpu::Device, period_ns: f32) -> Timestamps {
        let sets = (0..3)
            .map(|i| {
                device.create_query_set(&wgpu::QuerySetDescriptor { label: Some(&format!("timestamps#{i}")), ty: wgpu::QueryType::Timestamp, count: SLOTS })
            })
            .collect();
        let size = SLOTS as u64 * 8;
        let resolve = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("timestamp resolve"),
            size,
            usage: wgpu::BufferUsages::QUERY_RESOLVE | wgpu::BufferUsages::COPY_SRC,
            mapped_at_creation: false,
        });
        Timestamps {
            sets,
            resolve,
            readback: MappedRing::new(device, "timestamps", 4, size, false),
            masks: [0; 4],
            slot: 0,
            written: [false; PASSES.len()],
            period_ns,
            last: [0.0; PASSES.len()],
            total: 0.0,
        }
    }

    pub fn begin_frame(&mut self) {
        self.slot = (self.slot + 1) % self.sets.len();
        self.written = [false; PASSES.len()];
    }

    pub fn start(&mut self, enc: &mut wgpu::CommandEncoder, pass: usize) {
        let _p = se_alloc::Pause::new();
        enc.write_timestamp(&self.sets[self.slot], (pass * 2) as u32);
    }

    pub fn end(&mut self, enc: &mut wgpu::CommandEncoder, pass: usize) {
        let _p = se_alloc::Pause::new();
        enc.write_timestamp(&self.sets[self.slot], (pass * 2 + 1) as u32);
        self.written[pass] = true;
    }

    /// Resolve this frame's queries into a readback buffer (skipped when none is free).
    pub fn resolve(&mut self, enc: &mut wgpu::CommandEncoder, frame: u64) {
        let _p = se_alloc::Pause::new();
        let Some(i) = self.readback.acquire_read(frame) else { return };
        enc.resolve_query_set(&self.sets[self.slot], 0..SLOTS, &self.resolve, 0);
        enc.copy_buffer_to_buffer(&self.resolve, 0, &self.readback.buffers[i], 0, SLOTS as u64 * 8);
        let mut mask = 0u64;
        for (p, w) in self.written.iter().enumerate() {
            if *w {
                mask |= 1 << p;
            }
        }
        self.masks[i] = mask;
    }

    pub fn after_submit(&mut self) {
        self.readback.after_submit();
    }

    /// Consume finished readbacks; returns true when new numbers arrived.
    pub fn poll(&mut self) -> bool {
        let _p = se_alloc::Pause::new();
        let mut got = false;
        while let Some(i) = self.readback.ready_read() {
            let mask = self.masks[i];
            if let Ok(view) = self.readback.buffers[i].slice(..).get_mapped_range() {
                let ticks: &[u64] = bytemuck::cast_slice(&view);
                let (mut first, mut last) = (u64::MAX, 0u64);
                for p in 0..PASSES.len() {
                    if mask & (1 << p) == 0 {
                        self.last[p] = 0.0;
                        continue;
                    }
                    let (a, b) = (ticks[p * 2], ticks[p * 2 + 1]);
                    self.last[p] = b.saturating_sub(a) as f32 * self.period_ns / 1e6;
                    first = first.min(a);
                    last = last.max(b);
                }
                self.total = if last > first { (last - first) as f32 * self.period_ns / 1e6 } else { 0.0 };
            }
            self.readback.release_read(i);
            got = true;
        }
        got
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stats_roundtrip_and_used_generation() {
        let s = Stats::default();
        s.set_frame(3.5);
        s.set_frame(1.0);
        s.set_gpu(2.0, &[0.5; PASSES.len()]);
        let v = s.view();
        assert_eq!((v.frame_ms, v.frame_ms_max, v.gpu_ms), (1.0, 3.5, 2.0));
        assert_eq!(s.view().frame_ms_max, 0.0, "max resets per view");
        let g = s.used_generation.load(Ordering::Relaxed);
        s.set_used(&[1, 0, 0, 0]);
        s.set_used(&[1, 0, 0, 0]);
        assert_eq!(s.used_generation.load(Ordering::Relaxed), g + 1);
        assert_eq!(s.used(), [1, 0, 0, 0]);
    }
}
