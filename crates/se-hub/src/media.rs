//! Lock-free media handoff between producers and the real-time consumers.
//!
//! * [`VideoSlots`]: named CPU video frames (CEF pages, media decoders, CPU-drawn patches) →
//!   the render thread. One triple buffer per slot: the producer never blocks, the consumer
//!   always sees the newest complete frame, and buffers are reused (allocation only when the
//!   frame size changes).
//! * [`AudioSlots`]: named interleaved `f32` sample rings (CEF audio, media files, TTS,
//!   sound effects) → the audio graph. Real-time safe (`rtrb`).
//!
//! Registration takes a mutex; the per-frame/per-block paths do not.

use parking_lot::Mutex;
use se_proto::Ts;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub enum PixelFormat {
    #[default]
    Rgba8,
    Bgra8,
    /// Packed 4:2:2 (V4L2 YUYV).
    Yuyv,
    /// Planar 4:2:0 with interleaved chroma (Y plane then UV plane).
    Nv12,
}

impl PixelFormat {
    pub fn bytes_per_pixel(self) -> f32 {
        match self {
            PixelFormat::Rgba8 | PixelFormat::Bgra8 => 4.0,
            PixelFormat::Yuyv => 2.0,
            PixelFormat::Nv12 => 1.5,
        }
    }
}

#[derive(Clone, Debug, Default)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row of the first plane.
    pub stride: u32,
    pub format: PixelFormat,
    pub data: Vec<u8>,
    /// Increments per published frame (0 = never written).
    pub seq: u64,
    /// Master-clock capture/paint time.
    pub ts: Ts,
}

pub struct VideoWriter {
    input: triple_buffer::Input<VideoFrame>,
    seq: u64,
}

impl VideoWriter {
    /// Fill the back buffer in place and publish it. `fill` receives a buffer of exactly
    /// `stride * height * plane factor` bytes (reused between frames).
    pub fn write_with(&mut self, width: u32, height: u32, stride: u32, format: PixelFormat, ts: Ts, fill: impl FnOnce(&mut [u8])) {
        let len = match format {
            PixelFormat::Nv12 => (stride * height + stride * height.div_ceil(2)) as usize,
            _ => (stride * height) as usize,
        };
        let f = self.input.input_buffer_mut();
        if f.data.len() != len {
            f.data.resize(len, 0);
        }
        f.width = width;
        f.height = height;
        f.stride = stride;
        f.format = format;
        f.ts = ts;
        self.seq += 1;
        f.seq = self.seq;
        fill(&mut f.data);
        self.input.publish();
    }

    /// Copy a complete frame.
    pub fn write(&mut self, width: u32, height: u32, stride: u32, format: PixelFormat, ts: Ts, data: &[u8]) {
        self.write_with(width, height, stride, format, ts, |dst| {
            let n = dst.len().min(data.len());
            dst[..n].copy_from_slice(&data[..n]);
        });
    }
}

pub struct VideoReader {
    output: triple_buffer::Output<VideoFrame>,
    last: u64,
}

impl VideoReader {
    /// The newest frame if it changed since the last call.
    pub fn fresh(&mut self) -> Option<&VideoFrame> {
        self.output.update();
        let f = self.output.output_buffer();
        if f.seq != 0 && f.seq != self.last {
            self.last = f.seq;
            Some(f)
        } else {
            None
        }
    }

    /// The newest frame regardless of whether it was seen.
    pub fn current(&mut self) -> Option<&VideoFrame> {
        self.output.update();
        let f = self.output.output_buffer();
        (f.seq != 0).then_some(f)
    }
}

#[derive(Default)]
pub struct VideoSlots {
    readers: Mutex<HashMap<String, VideoReader>>,
    names: Mutex<Vec<String>>,
    /// Bumped on every registration so the consumer knows to pick up new readers.
    pub generation: AtomicU64,
}

impl VideoSlots {
    /// Create (or replace) the slot `name` and return its writer.
    pub fn register(&self, name: &str) -> VideoWriter {
        let (input, output) = triple_buffer::TripleBuffer::new(&VideoFrame::default()).split();
        self.readers.lock().insert(name.to_string(), VideoReader { output, last: 0 });
        let mut n = self.names.lock();
        if !n.iter().any(|x| x == name) {
            n.push(name.to_string());
        }
        self.generation.fetch_add(1, Ordering::Release);
        VideoWriter { input, seq: 0 }
    }

    /// Take the reader for `name` (single consumer). Call again after `generation` changes to
    /// pick up re-registrations.
    pub fn take_reader(&self, name: &str) -> Option<VideoReader> {
        self.readers.lock().remove(name)
    }

    /// Take every reader registered since the last call.
    pub fn take_new(&self) -> Vec<(String, VideoReader)> {
        self.readers.lock().drain().collect()
    }

    pub fn names(&self) -> Vec<String> {
        self.names.lock().clone()
    }
}

/// Interleaved f32 audio at the graph rate (48 kHz).
pub struct AudioStream {
    pub channels: u16,
    pub rate: u32,
    pub consumer: rtrb::Consumer<f32>,
}

#[derive(Default)]
pub struct AudioSlots {
    streams: Mutex<HashMap<String, AudioStream>>,
    names: Mutex<Vec<(String, u16, u32)>>,
    pub generation: AtomicU64,
}

impl AudioSlots {
    /// Register a named audio producer; `seconds` of buffering.
    pub fn register(&self, name: &str, channels: u16, rate: u32, seconds: f32) -> rtrb::Producer<f32> {
        let cap = ((rate as f32 * seconds) as usize).max(1024) * channels as usize;
        let (p, c) = rtrb::RingBuffer::new(cap);
        self.streams.lock().insert(name.to_string(), AudioStream { channels, rate, consumer: c });
        let mut n = self.names.lock();
        n.retain(|(x, _, _)| x != name);
        n.push((name.to_string(), channels, rate));
        self.generation.fetch_add(1, Ordering::Release);
        p
    }

    pub fn take_new(&self) -> Vec<(String, AudioStream)> {
        self.streams.lock().drain().collect()
    }

    pub fn names(&self) -> Vec<(String, u16, u32)> {
        self.names.lock().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn video_newest_wins_and_reuses_buffers() {
        let slots = VideoSlots::default();
        let mut w = slots.register("web.player");
        let mut r = slots.take_reader("web.player").unwrap();
        assert!(r.fresh().is_none());
        w.write(2, 1, 8, PixelFormat::Rgba8, 1, &[1; 8]);
        w.write(2, 1, 8, PixelFormat::Rgba8, 2, &[2; 8]);
        let f = r.fresh().unwrap();
        assert_eq!((f.seq, f.data[0]), (2, 2));
        assert!(r.fresh().is_none(), "no new frame");
        assert!(r.current().is_some());
    }

    #[test]
    fn audio_ring() {
        let a = AudioSlots::default();
        let mut p = a.register("tts", 2, 48000, 0.1);
        let mut s = a.take_new().pop().unwrap().1;
        p.push(0.5).unwrap();
        assert_eq!(s.consumer.pop().unwrap(), 0.5);
        assert_eq!(s.channels, 2);
    }
}
