//! Engine-side sinks: shared-memory frames → `hub.video` slot, shared-memory audio ring →
//! `hub.audio` slot. One [`Media`] per source; it outlives host restarts so the renderer keeps
//! the same slot (and the last frame) while the host comes back.

use crate::protocol::{self, AUDIO_CHANNELS, AUDIO_RATE, AudioRing, MAX_SIDE, Shm};
use se_hub::Hub;
use se_hub::media::{PixelFormat, VideoWriter};
use std::os::fd::OwnedFd;

/// Buffered audio per slot (the audio graph normally drains it within one quantum).
const AUDIO_SLOT_SECONDS: f32 = 0.5;

struct Surface {
    generation: u32,
    width: u32,
    height: u32,
    stride: u32,
    slots: u32,
    shm: Shm,
}

pub struct Media {
    video: VideoWriter,
    audio: rtrb::Producer<f32>,
    surface: Option<Surface>,
    ring: Option<AudioRing>,
    /// Size of the last published frame.
    pub size: (u32, u32),
    /// Frames published since the service started.
    pub frames: u64,
    /// Paint → publish latency, exponential moving average (ms).
    pub latency_ms: f64,
    pub audio_samples: u64,
    /// Samples discarded because the audio slot was full (nobody consuming).
    pub audio_dropped: u64,
}

impl Media {
    pub fn new(hub: &Hub, slot: &str) -> Media {
        Media {
            video: hub.video.register(slot),
            audio: hub.audio.register(slot, AUDIO_CHANNELS as u16, AUDIO_RATE, AUDIO_SLOT_SECONDS),
            surface: None,
            ring: None,
            size: (0, 0),
            frames: 0,
            latency_ms: 0.0,
            audio_samples: 0,
            audio_dropped: 0,
        }
    }

    /// Map a new frame surface announced by the host.
    pub fn surface(&mut self, generation: u32, width: u32, height: u32, stride: u32, slots: u32, fd: Option<OwnedFd>) -> Result<(), String> {
        let fd = fd.ok_or("surface without shared memory")?;
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || stride < width * 4 || slots == 0 || slots > 8 {
            return Err(format!("bad surface {width}x{height} stride {stride} × {slots}"));
        }
        let need = protocol::slot_range(stride, height, slots - 1).end;
        let shm = Shm::open(fd, need).map_err(|e| format!("surface: {e}"))?;
        self.surface = Some(Surface { generation, width, height, stride, slots, shm });
        Ok(())
    }

    /// Copy frame `slot` into the video slot. `false` if it doesn't belong to the current surface.
    pub fn frame(&mut self, generation: u32, slot: u32, paint_ns: u64) -> bool {
        let Some(s) = self.surface.as_ref().filter(|s| s.generation == generation && slot < s.slots) else { return false };
        let range = protocol::slot_range(s.stride, s.height, slot);
        let now = se_clock::now();
        // `paint_ns` is the host's CLOCK_MONOTONIC = the master clock; never trust a future stamp.
        let ts = if paint_ns == 0 || paint_ns > now { now } else { paint_ns };
        let shm = &s.shm;
        self.video.write_with(s.width, s.height, s.stride, PixelFormat::Bgra8, ts, |dst| {
            shm.read_at(range.start, dst);
        });
        self.size = (s.width, s.height);
        self.frames += 1;
        let lat = (se_clock::now().saturating_sub(ts)) as f64 / 1e6;
        self.latency_ms = if self.frames == 1 { lat } else { self.latency_ms * 0.9 + lat * 0.1 };
        true
    }

    pub fn audio_ring(&mut self, channels: u32, rate: u32, capacity: u32, fd: Option<OwnedFd>) -> Result<(), String> {
        let fd = fd.ok_or("audio ring without shared memory")?;
        if channels != AUDIO_CHANNELS || rate != AUDIO_RATE {
            return Err(format!("unsupported audio format {channels} ch @ {rate} Hz"));
        }
        self.ring = Some(AudioRing::open(fd, capacity, channels).map_err(|e| format!("audio ring: {e}"))?);
        Ok(())
    }

    /// Move everything the host wrote into the audio slot (whole stereo frames; overflow is
    /// dropped because nobody is consuming the slot).
    pub fn drain_audio(&mut self) {
        let Some(ring) = &self.ring else { return };
        let prod = &mut self.audio;
        let (mut moved, mut dropped) = (0u64, 0u64);
        ring.pop_with(|run| {
            let n = prod.slots().min(run.len()) / AUDIO_CHANNELS as usize * AUDIO_CHANNELS as usize;
            if let Ok(chunk) = prod.write_chunk_uninit(n) {
                chunk.fill_from_iter(run[..n].iter().map(|&x| if x.is_finite() { x.clamp(-8.0, 8.0) } else { 0.0 }));
            }
            moved += n as u64;
            dropped += (run.len() - n) as u64;
        });
        self.audio_samples += moved;
        self.audio_dropped += dropped;
    }

    /// The host went away: release its shared memory, keep the last frame on screen.
    pub fn detach(&mut self) {
        self.surface = None;
        self.ring = None;
    }

    /// The source closed (patch disabled/removed): stop output with a transparent frame.
    pub fn clear(&mut self) {
        self.detach();
        let (w, h) = if self.size == (0, 0) { (2, 2) } else { self.size };
        self.video.write_with(w, h, w * 4, PixelFormat::Bgra8, se_clock::now(), |dst| dst.fill(0));
    }
}
