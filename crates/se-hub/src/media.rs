//! Lock-free media handoff between producers and the real-time consumers.
//!
//! * [`VideoSlots`]: named CPU video frames (CEF pages, media decoders, CPU-drawn patches) →
//!   the render thread. One triple buffer per slot: the producer never blocks, the consumer
//!   always sees the newest complete frame, and buffers are reused (allocation only when the
//!   frame size changes).
//! * [`VideoTap`]: bounded, drop-oldest side channels of one slot's frames for recorders
//!   ([`crate::Hub::tap_video`]). A slot without taps pays one relaxed atomic load per frame;
//!   with taps the writer makes one pooled copy per frame (shared by every tap) before
//!   publishing, and never blocks on a slow tap.
//! * [`AudioSlots`]: named interleaved `f32` sample rings (CEF audio, media files, TTS,
//!   sound effects) → the audio graph. Real-time safe (`rtrb`).
//!
//! Registration takes a mutex; the per-frame/per-block paths do not.

use arc_swap::ArcSwap;
use crossbeam_channel::{Receiver, Sender, TrySendError};
use parking_lot::Mutex;
use se_proto::Ts;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

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
    taps: Arc<TapSet>,
    /// Tap frames, reused once every tap released them (no per-frame allocation).
    pool: Vec<Arc<VideoFrame>>,
}

/// Tap frames kept for reuse per writer: a full 8-frame tap, the frame its reader holds,
/// and the one being filled, plus slack for a second tap.
const TAP_POOL: usize = 12;

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
        if self.taps.count.load(Ordering::Relaxed) != 0 {
            self.taps.offer(&mut self.pool, self.input.input_buffer());
        } else if !self.pool.is_empty() {
            // last tap gone: free the copies
            self.pool = Vec::new();
        }
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

/// One tap's bounded queue. The writer side both sends and evicts (drop-oldest).
struct TapQueue {
    tx: Sender<Arc<VideoFrame>>,
    rx: Receiver<Arc<VideoFrame>>,
    dropped: AtomicU64,
}

impl TapQueue {
    fn push(&self, frame: Arc<VideoFrame>) {
        let mut frame = frame;
        // Only the reader removes frames besides us, so one eviction always makes room.
        for _ in 0..2 {
            match self.tx.try_send(frame) {
                Ok(()) => return,
                Err(TrySendError::Full(back)) => {
                    frame = back;
                    if self.rx.try_recv().is_ok() {
                        self.dropped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                Err(TrySendError::Disconnected(_)) => return,
            }
        }
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// The taps on one slot name; shared by the slot's writer (across re-registrations) and its
/// [`VideoTap`]s.
struct TapSet {
    /// Number of live taps; the writer checks only this when nobody taps.
    count: AtomicUsize,
    taps: ArcSwap<Vec<Arc<TapQueue>>>,
    /// Serializes tap add/remove (the writer reads `taps` lock-free).
    edit: Mutex<()>,
    changed: Arc<Notify>,
}

impl TapSet {
    fn new(changed: Arc<Notify>) -> TapSet {
        TapSet { count: AtomicUsize::new(0), taps: ArcSwap::from_pointee(Vec::new()), edit: Mutex::new(()), changed }
    }

    /// Copy `src` once into a pooled frame and hand it to every tap.
    fn offer(&self, pool: &mut Vec<Arc<VideoFrame>>, src: &VideoFrame) {
        let taps = self.taps.load();
        if taps.is_empty() {
            return;
        }
        let frame = match pool.iter_mut().position(|f| Arc::get_mut(f).is_some()) {
            Some(i) => {
                let f = Arc::get_mut(&mut pool[i]).expect("unique");
                f.width = src.width;
                f.height = src.height;
                f.stride = src.stride;
                f.format = src.format;
                f.seq = src.seq;
                f.ts = src.ts;
                f.data.clear();
                f.data.extend_from_slice(&src.data);
                pool[i].clone()
            }
            None => {
                let f = Arc::new(src.clone());
                if pool.len() < TAP_POOL {
                    pool.push(f.clone());
                }
                f
            }
        };
        for t in taps.iter() {
            t.push(frame.clone());
        }
    }

    fn add(&self, q: Arc<TapQueue>) {
        let _g = self.edit.lock();
        let mut v = Vec::clone(&self.taps.load());
        v.push(q);
        self.taps.store(Arc::new(v));
        self.count.fetch_add(1, Ordering::Relaxed);
        self.changed.notify_one();
    }

    fn remove(&self, q: &Arc<TapQueue>) {
        let _g = self.edit.lock();
        let mut v = Vec::clone(&self.taps.load());
        v.retain(|x| !Arc::ptr_eq(x, q));
        self.taps.store(Arc::new(v));
        self.count.fetch_sub(1, Ordering::Relaxed);
        self.changed.notify_one();
    }
}

/// A recorder's view of one video slot: every frame its producer publishes, as the same
/// pixels, size, stride, format, `seq` and master-clock capture `ts` the renderer receives.
/// Bounded; when full the oldest queued frame is dropped (counted in [`VideoTap::dropped`]),
/// so a slow consumer never delays the producer or the renderer. Holding a tap adds capture
/// demand for the source (`se-video-in` captures it even when no scene shows it); dropping
/// the last tap releases that demand.
pub struct VideoTap {
    queue: Arc<TapQueue>,
    set: Arc<TapSet>,
}

impl VideoTap {
    /// The oldest queued frame, waiting up to `d` for one.
    pub fn recv_timeout(&self, d: Duration) -> Option<Arc<VideoFrame>> {
        self.queue.rx.recv_timeout(d).ok()
    }

    /// Frames this tap lost because its queue was full.
    pub fn dropped(&self) -> u64 {
        self.queue.dropped.load(Ordering::Relaxed)
    }
}

impl Drop for VideoTap {
    fn drop(&mut self) {
        self.set.remove(&self.queue);
    }
}

#[derive(Default)]
pub struct VideoSlots {
    readers: Mutex<HashMap<String, VideoReader>>,
    names: Mutex<Vec<String>>,
    /// Bumped on every registration so the consumer knows to pick up new readers.
    pub generation: AtomicU64,
    taps: Mutex<HashMap<String, Arc<TapSet>>>,
    taps_changed: Arc<Notify>,
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
        VideoWriter { input, seq: 0, taps: self.tap_set(name), pool: Vec::new() }
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

    fn tap_set(&self, name: &str) -> Arc<TapSet> {
        self.taps.lock().entry(name.to_string()).or_insert_with(|| Arc::new(TapSet::new(self.taps_changed.clone()))).clone()
    }

    /// Tap slot `name` (registered now or later) with a queue of `capacity` frames (clamped
    /// to 1..=8).
    pub fn tap(&self, name: &str, capacity: usize) -> VideoTap {
        let (tx, rx) = crossbeam_channel::bounded(capacity.clamp(1, 8));
        let queue = Arc::new(TapQueue { tx, rx, dropped: AtomicU64::new(0) });
        let set = self.tap_set(name);
        set.add(queue.clone());
        VideoTap { queue, set }
    }

    /// Live taps on slot `name`.
    pub fn tap_count(&self, name: &str) -> usize {
        self.taps.lock().get(name).map_or(0, |s| s.count.load(Ordering::Relaxed))
    }

    /// Slot names with at least one live tap.
    pub fn tapped(&self) -> Vec<String> {
        self.taps.lock().iter().filter(|(_, s)| s.count.load(Ordering::Relaxed) != 0).map(|(n, _)| n.clone()).collect()
    }

    /// Resolves once a tap was added or dropped since the previous wait (single waiter).
    pub async fn taps_changed(&self) {
        self.taps_changed.notified().await
    }
}

impl crate::Hub {
    /// Tap the frames of video source `source` (its `hub.video` slot name: a camera's
    /// `sources/<id>.toml` stem) for recording. See [`VideoTap`]. `capacity` is clamped to
    /// 1..=8. Never blocks the capture thread.
    pub fn tap_video(&self, source: &str, capacity: usize) -> VideoTap {
        self.video.tap(source, capacity)
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

    const NOW: Duration = Duration::ZERO;

    /// 2x1 YUYV with a padded stride, like a capture card's bytesperline.
    fn yuyv(w: &mut VideoWriter, ts: Ts, v: u8) {
        w.write(2, 1, 8, PixelFormat::Yuyv, ts, &[v; 8]);
    }

    #[test]
    fn tap_is_bounded_drop_oldest_with_renderer_fields() {
        let slots = VideoSlots::default();
        let mut w = slots.register("cam_kit");
        let tap = slots.tap("cam_kit", 2);
        for i in 1..=5u8 {
            yuyv(&mut w, 1000 + i as u64, i);
        }
        let a = tap.recv_timeout(NOW).unwrap();
        let b = tap.recv_timeout(NOW).unwrap();
        assert!(tap.recv_timeout(Duration::from_millis(5)).is_none());
        assert_eq!((a.seq, a.ts, a.data[0]), (4, 1004, 4), "oldest frames were dropped");
        assert_eq!((b.seq, b.ts, b.data[0]), (5, 1005, 5));
        assert_eq!((b.width, b.height, b.stride, b.format, b.data.len()), (2, 1, 8, PixelFormat::Yuyv, 8));
        assert_eq!(tap.dropped(), 3);
    }

    #[test]
    fn capacity_is_clamped() {
        let slots = VideoSlots::default();
        let mut w = slots.register("cam");
        let (zero, big) = (slots.tap("cam", 0), slots.tap("cam", 100));
        for i in 0..20u8 {
            yuyv(&mut w, i as u64, i);
        }
        assert_eq!((zero.dropped(), big.dropped()), (19, 12));
    }

    #[test]
    fn taps_share_one_copy_and_drop_independently() {
        let slots = VideoSlots::default();
        let mut w = slots.register("cam");
        let slow = slots.tap("cam", 1);
        let fast = slots.tap("cam", 8);
        assert_eq!(slots.tap_count("cam"), 2);
        for i in 1..=4u8 {
            yuyv(&mut w, i as u64, i);
        }
        let s = slow.recv_timeout(NOW).unwrap();
        let mut last = None;
        while let Some(f) = fast.recv_timeout(NOW) {
            last = Some(f);
        }
        assert!(Arc::ptr_eq(&s, last.as_ref().unwrap()), "one copy per frame for all taps");
        assert_eq!((slow.dropped(), fast.dropped()), (3, 0));
    }

    #[test]
    fn renderer_slot_unaffected_by_slow_tap() {
        let slots = VideoSlots::default();
        let mut w = slots.register("cam");
        let mut r = slots.take_reader("cam").unwrap();
        let _stuck = slots.tap("cam", 1);
        for i in 1..=50u8 {
            yuyv(&mut w, i as u64, i);
            let f = r.fresh().unwrap();
            assert_eq!((f.seq, f.ts, f.data[0], f.stride, f.format), (i as u64, i as u64, i, 8, PixelFormat::Yuyv));
        }
    }

    #[test]
    fn tap_copies_are_pooled_and_freed_with_the_last_tap() {
        let slots = VideoSlots::default();
        let mut w = slots.register("cam");
        let tap = slots.tap("cam", 4);
        for i in 0..100u8 {
            yuyv(&mut w, i as u64, i);
            drop(tap.recv_timeout(NOW).unwrap());
        }
        assert_eq!(w.pool.len(), 1, "a released copy is reused");
        drop(tap);
        yuyv(&mut w, 0, 0);
        assert!(w.pool.is_empty());
    }

    #[tokio::test]
    async fn tap_demand_follows_tap_lifetime_and_survives_reregistration() {
        let slots = VideoSlots::default();
        let wait = || tokio::time::timeout(Duration::from_secs(1), slots.taps_changed());
        let tap = slots.tap("cam", 2);
        wait().await.expect("tap added");
        assert_eq!(slots.tapped(), vec!["cam".to_string()]);
        let second = slots.tap("cam", 2);
        drop(second);
        assert_eq!(slots.tap_count("cam"), 1);
        // the writer registered after the tap, and re-registered (capture thread restart)
        yuyv(&mut slots.register("cam"), 1, 1);
        yuyv(&mut slots.register("cam"), 2, 2);
        assert_eq!(tap.recv_timeout(NOW).unwrap().data[0], 1);
        assert_eq!(tap.recv_timeout(NOW).unwrap().data[0], 2);
        wait().await.expect("changes noticed");
        drop(tap);
        wait().await.expect("tap dropped");
        assert!(slots.tapped().is_empty());
        assert_eq!(slots.tap_count("cam"), 0);
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
