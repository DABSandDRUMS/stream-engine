//! Camera ISO feeder: the native frames the engine already captures for a source (YUYV or
//! decoded RGBA, master-clock capture timestamps) streamed to an encoder as timestamped raw
//! Matroska. Holding the tap keeps the camera captured even when no scene shows it; the
//! renderer's own frames are unaffected. A busy encoder drops the oldest queued frames.

use super::mkv::{self, LOSS_TIMEOUT, Layout};
use se_hub::media::{PixelFormat, VideoFrame, VideoTap};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

/// Frames queued between the capture thread and this feeder before the oldest is dropped.
pub(super) const TAP_CAPACITY: usize = 4;

/// A bounded frame source (the hub tap in the engine, a channel in tests).
pub(super) trait Frames: Send + 'static {
    fn recv(&mut self, timeout: Duration) -> Option<Arc<VideoFrame>>;
    /// Frames lost because this consumer fell behind.
    fn dropped(&self) -> u64;
}

impl Frames for VideoTap {
    fn recv(&mut self, timeout: Duration) -> Option<Arc<VideoFrame>> {
        self.recv_timeout(timeout)
    }
    fn dropped(&self) -> u64 {
        VideoTap::dropped(self)
    }
}

fn layout(format: PixelFormat) -> Layout {
    match format {
        PixelFormat::Rgba8 => Layout::Rgba,
        PixelFormat::Bgra8 => Layout::Bgra,
        PixelFormat::Yuyv => Layout::Yuy2,
        PixelFormat::Nv12 => Layout::Nv12,
    }
}

/// A camera whose first picture has arrived, so the encoder knows its size and layout.
pub(super) struct Camera<F: Frames> {
    name: String,
    frames: F,
    pub(super) width: u32,
    pub(super) height: u32,
    pub(super) layout: Layout,
}

/// Bytes a frame must hold for its planes at `stride`.
fn required(frame: &VideoFrame, layout: Layout) -> usize {
    let stride = frame.stride as usize;
    layout.planes(frame.width, frame.height).iter().map(|(_, rows)| stride * rows).sum()
}

fn valid(frame: &VideoFrame) -> bool {
    let layout = layout(frame.format);
    let row = layout.planes(frame.width, frame.height)[0].0;
    frame.width > 0
        && frame.height > 0
        && frame.width <= 16384
        && frame.height <= 16384
        && frame.width.is_multiple_of(2)
        && (frame.stride as usize) >= row
        && frame.data.len() >= required(frame, layout)
}

impl<F: Frames> Camera<F> {
    /// Wait (up to 10 s) for the camera's first usable picture.
    pub(super) fn open(name: &str, mut frames: F, stop: &AtomicBool) -> Result<Self, String> {
        let deadline = Instant::now() + LOSS_TIMEOUT;
        while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
            if let Some(frame) = frames.recv(Duration::from_millis(100))
                && valid(&frame)
            {
                return Ok(Camera { name: name.into(), width: frame.width, height: frame.height, layout: layout(frame.format), frames });
            }
        }
        Err(if stop.load(Ordering::Relaxed) {
            "recording cancelled".into()
        } else {
            format!("camera {name} delivered no pictures for 10 seconds (is it connected and enabled?)")
        })
    }

    /// One thread: wait for a frame, write it. `dropped` mirrors frames lost to a busy encoder.
    pub(super) fn spawn(self, output: UnixStream, start_ns: u64, fps: u32, stop: Arc<AtomicBool>, dropped: Arc<AtomicU64>) -> JoinHandle<Result<(), String>> {
        thread::spawn(move || self.feed(output, start_ns, fps, &stop, &dropped))
    }

    fn feed(mut self, mut output: UnixStream, start_ns: u64, fps: u32, stop: &AtomicBool, dropped: &AtomicU64) -> Result<(), String> {
        output.set_write_timeout(Some(Duration::from_millis(100))).map_err(|e| e.to_string())?;
        mkv::write_cancel(&mut output, &mkv::header(self.width, self.height, fps, self.layout), stop)?;
        let planes = self.layout.planes(self.width, self.height);
        let mut packed = Vec::new();
        let mut last_frame = Instant::now();
        let mut last_tick = None;
        while !stop.load(Ordering::Relaxed) {
            dropped.store(self.frames.dropped(), Ordering::Relaxed);
            let Some(frame) = self.frames.recv(Duration::from_millis(100)) else {
                if last_frame.elapsed() > LOSS_TIMEOUT {
                    return Err(format!("camera {} stopped delivering pictures for 10 seconds", self.name));
                }
                continue;
            };
            last_frame = Instant::now();
            if !valid(&frame) {
                continue;
            }
            if frame.width != self.width || frame.height != self.height || layout(frame.format) != self.layout {
                return Err(format!("camera {} changed picture size or format; continuing in a new file", self.name));
            }
            if frame.ts < start_ns {
                continue;
            }
            let ns = frame.ts - start_ns;
            let tick = (ns as u128 * fps as u128 / 1_000_000_000) as u64;
            if last_tick.is_some_and(|previous| tick <= previous) {
                continue;
            }
            last_tick = Some(tick);
            let stride = frame.stride as usize;
            let bytes: &[u8] = if planes.iter().all(|(row, _)| *row == 0 || *row == stride) {
                &frame.data[..self.layout.size(self.width, self.height)]
            } else {
                packed.clear();
                let mut at = 0;
                for (row, rows) in planes {
                    for r in 0..rows {
                        packed.extend_from_slice(&frame.data[at + r * stride..at + r * stride + row]);
                    }
                    at += rows * stride;
                }
                &packed
            };
            mkv::write_picture(&mut output, ns / 1000, bytes, stop)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    struct Channel(mpsc::Receiver<Arc<VideoFrame>>);
    impl Frames for Channel {
        fn recv(&mut self, timeout: Duration) -> Option<Arc<VideoFrame>> {
            self.0.recv_timeout(timeout).ok()
        }
        fn dropped(&self) -> u64 {
            0
        }
    }

    /// YUYV with padded rows: top half white, bottom half black.
    fn yuyv(width: u32, height: u32, stride: u32, ts: u64) -> Arc<VideoFrame> {
        let mut data = vec![0u8; (stride * height) as usize];
        for y in 0..height as usize {
            let luma = if y < height as usize / 2 { 235 } else { 16 };
            for x in 0..width as usize / 2 {
                let at = y * stride as usize + x * 4;
                data[at..at + 4].copy_from_slice(&[luma, 128, luma, 128]);
            }
        }
        Arc::new(VideoFrame { width, height, stride, format: PixelFormat::Yuyv, data, seq: ts, ts })
    }

    #[test]
    fn padded_yuyv_frames_reach_ffmpeg_upright_on_their_capture_timestamps() {
        if std::process::Command::new("ffmpeg").arg("-version").output().is_err() {
            eprintln!("ffmpeg not available: skipped");
            return;
        }
        let (tx, rx) = mpsc::channel();
        let start = 1_000_000_000u64;
        // A frame from before the recording origin is consumed by open() and never written.
        tx.send(yuyv(64, 32, 160, start - 10)).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let camera = Camera::open("kit", Channel(rx), &stop).unwrap();
        assert_eq!((camera.width, camera.height, camera.layout), (64, 32, Layout::Yuy2));
        for i in 0..10u64 {
            tx.send(yuyv(64, 32, 160, start + i * 100_000_000)).unwrap();
        }
        drop(tx);
        let (read, write) = UnixStream::pair().unwrap();
        let feeder = camera.spawn(write, start, 10, stop.clone(), Arc::new(AtomicU64::new(0)));
        let child = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "matroska", "-i", "pipe:0", "-vf", "scale=out_range=full,format=gray", "-f", "rawvideo", "-"])
            .stdin(std::process::Stdio::from(std::os::fd::OwnedFd::from(read)))
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        // The feeder errors out after the channel closes and 10 s pass; stop it once ffmpeg has
        // the pictures by closing our end: give it a moment, then stop.
        thread::sleep(Duration::from_millis(500));
        stop.store(true, Ordering::Relaxed);
        assert!(feeder.join().unwrap().is_ok());
        let out = child.wait_with_output().unwrap();
        assert_eq!(out.stdout.len() % (64 * 32), 0);
        let frames = out.stdout.len() / (64 * 32);
        assert!((9..=10).contains(&frames), "decoded {frames} pictures");
        let first = &out.stdout[..64 * 32];
        assert!(first[0] > 200, "top row must be white (upright), got {}", first[0]);
        assert!(first[64 * 31] < 50, "bottom row must be black, got {}", first[64 * 31]);
    }
}
