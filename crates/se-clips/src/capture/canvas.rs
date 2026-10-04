//! Bounded CPU export consumer. Frame timestamps, not arrival counts, drive the
//! Matroska input timeline; ffmpeg's fps filter repeats pictures across dropped frames.

use super::mkv::{self, LOSS_TIMEOUT, Layout};
use se_frames::{CanvasMsg, ClientMsg, FramesClient, ShmView, proto};
use std::os::fd::AsFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

pub(super) struct Canvas {
    client: FramesClient,
    desc: CanvasMsg,
    views: Vec<ShmView>,
}

fn views(desc: &CanvasMsg, fds: &[std::os::fd::OwnedFd]) -> Result<Vec<ShmView>, String> {
    if desc.drm_fourcc != 0 || desc.planes != 1 || desc.modifier != proto::DRM_FORMAT_MOD_LINEAR {
        return Err("recording requires the RGBA shared-memory canvas export".into());
    }
    if desc.width == 0 || desc.height == 0 || desc.width > 16384 || desc.height > 16384 || desc.strides[0] < desc.width * 4 {
        return Err("invalid canvas dimensions or stride".into());
    }
    let len = desc.offsets[0] as usize + desc.strides[0] as usize * desc.height as usize;
    fds.iter().map(|fd| ShmView::map(fd.as_fd(), len).map_err(|e| e.to_string())).collect()
}

impl Canvas {
    pub(super) fn connect(path: &Path, id: u32, stop: &AtomicBool) -> Result<Self, String> {
        let mut client = FramesClient::connect(path).map_err(|e| format!("cannot reach the engine canvas export: {e}"))?;
        client.hello(proto::CLIENT_OTHER, 1 << id, false).map_err(|e| e.to_string())?;
        let deadline = Instant::now() + LOSS_TIMEOUT;
        while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
            match client.recv(Duration::from_millis(100)).map_err(|e| e.to_string())? {
                Some(ClientMsg::Canvas { msg, fds }) if msg.canvas == id => {
                    let views = views(&msg, &fds)?;
                    return Ok(Self { client, desc: msg, views });
                }
                Some(ClientMsg::Frame { msg, .. }) => client.release(msg.canvas, msg.buffer, msg.seq).map_err(|e| e.to_string())?,
                Some(ClientMsg::Goodbye(_)) => return Err("canvas export stopped during recording startup".into()),
                _ => {}
            }
        }
        Err(if stop.load(Ordering::Relaxed) { "recording cancelled".into() } else { format!("canvas {} did not become available within 10 seconds", proto::canvas_name(id).unwrap_or("unknown")) })
    }

    /// Reader + writer threads. `dropped` counts pictures skipped because the encoder was busy.
    pub(super) fn spawn(self, output: UnixStream, start_ns: u64, fps: u32, stop: Arc<AtomicBool>, dropped: Arc<AtomicU64>) -> Vec<JoinHandle<Result<(), String>>> {
        // Exactly three owned pictures: the pool bounds the queue, reader and writer
        // together. Exhausting it drops frames without retaining renderer leases.
        let (free_tx, free_rx) = mpsc::sync_channel(3);
        let (tx, rx) = mpsc::sync_channel::<Picture>(3);
        let size = Layout::Rgba.size(self.desc.width, self.desc.height);
        for _ in 0..3 {
            free_tx.send(vec![0; size]).expect("new frame pool");
        }
        let desc = self.desc;
        let writer_stop = stop.clone();
        let writer = thread::spawn(move || {
            let mut output = output;
            output.set_write_timeout(Some(Duration::from_millis(100))).map_err(|e| e.to_string())?;
            mkv::write_cancel(&mut output, &mkv::header(desc.width, desc.height, fps, Layout::Rgba), &writer_stop)?;
            while !writer_stop.load(Ordering::Relaxed) {
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(picture) => {
                        let result = mkv::write_picture(&mut output, picture.time_us, &picture.rgba, &writer_stop);
                        let _ = free_tx.try_send(picture.rgba);
                        result?;
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            Ok(())
        });
        let reader = thread::spawn(move || self.consume(start_ns, fps, stop, tx, free_rx, dropped));
        vec![reader, writer]
    }

    fn consume(mut self, start_ns: u64, fps: u32, stop: Arc<AtomicBool>, tx: mpsc::SyncSender<Picture>, free: mpsc::Receiver<Vec<u8>>, dropped: Arc<AtomicU64>) -> Result<(), String> {
        let mut last_frame = Instant::now();
        let mut last_tick = None;
        while !stop.load(Ordering::Relaxed) {
            match self.client.recv(Duration::from_millis(100)).map_err(|e| e.to_string())? {
                Some(ClientMsg::Canvas { msg, fds }) => {
                    if msg.width != self.desc.width || msg.height != self.desc.height {
                        return Err("canvas dimensions changed during recording; continuing in a new file".into());
                    }
                    self.views = views(&msg, &fds)?;
                    self.desc = msg;
                }
                Some(ClientMsg::Frame { msg, fence }) => {
                    // Release even malformed/stale frames and error paths. No ring buffer
                    // is held during pipe IO or ffmpeg backpressure.
                    let result = (|| {
                        if msg.generation != self.desc.generation { return Ok(()); }
                        let view = self.views.get(msg.buffer as usize).ok_or("invalid canvas buffer index")?;
                        if let Some(fence) = fence && !se_frames::wait_fence(&fence, Duration::from_millis(100)).map_err(|e| e.to_string())? {
                            return Err("canvas completion fence timed out".to_string());
                        }
                        last_frame = Instant::now();
                        if msg.monotonic_ns < start_ns { return Ok(()); }
                        let ns = msg.monotonic_ns - start_ns;
                        let tick = (ns as u128 * fps as u128 / 1_000_000_000) as u64;
                        if last_tick.is_some_and(|previous| tick <= previous) { return Ok(()); }
                        let Ok(mut rgba) = free.try_recv() else {
                            dropped.fetch_add(1, Ordering::Relaxed);
                            return Ok(());
                        };
                        let row = self.desc.width as usize * 4;
                        let stride = self.desc.strides[0] as usize;
                        let bytes = &view.as_slice()[self.desc.offsets[0] as usize..];
                        for (dst, src) in rgba.chunks_exact_mut(row).zip(bytes.chunks(stride)) {
                            dst.copy_from_slice(&src[..row]);
                        }
                        // Each queued/writing frame owns one of the three pool slots,
                        // so a three-slot queue cannot be full while we own this one.
                        match tx.try_send(Picture { time_us: ns / 1000, rgba }) {
                            Ok(()) => { last_tick = Some(tick); Ok(()) }
                            Err(mpsc::TrySendError::Disconnected(_)) => Err("canvas encoder input closed".into()),
                            Err(mpsc::TrySendError::Full(_)) => Err("canvas frame queue invariant violated".into()),
                        }
                    })();
                    self.client.release(msg.canvas, msg.buffer, msg.seq).map_err(|e| e.to_string())?;
                    result?;
                }
                Some(ClientMsg::Goodbye(_)) => return Err("canvas export disconnected during recording".into()),
                None => {}
            }
            if last_frame.elapsed() > LOSS_TIMEOUT {
                return Err("canvas input stopped delivering frames for 10 seconds".into());
            }
        }
        Ok(())
    }
}

struct Picture {
    time_us: u64,
    rgba: Vec<u8>,
}
