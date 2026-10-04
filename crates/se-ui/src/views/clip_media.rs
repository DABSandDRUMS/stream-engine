//! Demand-driven posters and one silent file preview for the clipping library.
//!
//! All filesystem and decoder work stays on two bounded workers. Preview loops reopen the
//! selected range, rather than retaining an entire clip in memory. Dropping this owner or
//! ending a frame without a preview request cancels decoding; workers kill and reap their
//! own children, so the UI never waits for a process.

use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use egui::{ColorImage, Context, TextureHandle, TextureOptions};
use std::collections::{HashMap, VecDeque};
use std::io::{self, Read};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

const SIDE: usize = 480;
const FRAME_BYTES: usize = SIDE * SIDE * 3;
const CACHE_SIZE: usize = 96;
const POLL: Duration = Duration::from_millis(20);
const FRAME_TIME: Duration = Duration::from_nanos(1_000_000_000 / 12);
const DECODE_TIMEOUT: Duration = Duration::from_secs(8);
const RETRY_AFTER: Duration = Duration::from_secs(30);
const LOADING: u8 = 0;
const PLAYING: u8 = 1;
const FAILED: u8 = 2;

#[derive(Default)]
pub struct ClipMedia {
    posters: HashMap<String, Poster>,
    thumbnails: Option<ThumbnailWorker>,
    previews: Option<PreviewWorker>,
    active: Option<ActivePreview>,
    frame: u64,
    preview_used: bool,
}

struct Poster {
    version: i64,
    seen: u64,
    state: PosterState,
}

enum PosterState {
    Loading(Arc<AtomicBool>),
    Ready(TextureHandle),
    Failed(Instant),
}

impl Poster {
    fn cancel(&self) {
        if let PosterState::Loading(cancel) = &self.state {
            cancel.store(true, Ordering::Release);
        }
    }
}

struct ThumbnailJob {
    path: String,
    version: i64,
    cancel: Arc<AtomicBool>,
}

struct ThumbnailResult {
    job: ThumbnailJob,
    image: Option<ColorImage>,
}

struct ThumbnailWorker {
    jobs: Sender<ThumbnailJob>,
    results: Receiver<ThumbnailResult>,
    stop: Arc<AtomicBool>,
}

#[derive(Clone, PartialEq, Eq)]
struct PreviewKey {
    path: String,
    version: i64,
    start: u64,
    end: u64,
}

impl PreviewKey {
    fn matches(&self, path: &str, version: i64, start: f64, end: f64) -> bool {
        self.path == path && self.version == version && self.start == start.to_bits() && self.end == end.to_bits()
    }
}

struct ActivePreview {
    key: PreviewKey,
    generation: u64,
    status: Arc<AtomicU8>,
    texture: Option<TextureHandle>,
}

struct PreviewJob {
    key: PreviewKey,
    generation: u64,
    status: Arc<AtomicU8>,
}

struct PreviewWorker {
    jobs: Sender<PreviewJob>,
    pending: Receiver<PreviewJob>,
    frames: Receiver<Frame>,
    recycled: Sender<Frame>,
    generation: Arc<AtomicU64>,
    stop: Arc<AtomicBool>,
}

struct Frame {
    pixels: Vec<u8>,
    size: [usize; 2],
    generation: u64,
}

impl Frame {
    fn new() -> Self {
        Self { pixels: vec![0; FRAME_BYTES], size: [0, 0], generation: 0 }
    }

    fn image(&self) -> ColorImage {
        ColorImage::from_rgb(self.size, &self.pixels[..self.size[0] * self.size[1] * 3])
    }
}

impl ClipMedia {
    /// Call before drawing visible cards or the focused clip.
    pub fn begin_frame(&mut self) {
        self.frame = self.frame.wrapping_add(1);
        self.preview_used = false;
    }

    /// An asynchronously decoded JPEG/image or video poster. A changed version invalidates it.
    /// Call only for visible items: queued work that leaves the viewport is cancelled.
    pub fn thumbnail(&mut self, ctx: &Context, path: &str, version: i64) -> Option<TextureHandle> {
        if path.is_empty() {
            return None;
        }
        self.drain_posters(ctx);
        if let Some(poster) = self.posters.get_mut(path) {
            if poster.version == version {
                poster.seen = self.frame;
                match &poster.state {
                    PosterState::Ready(texture) => return Some(texture.clone()),
                    PosterState::Loading(_) => return None,
                    PosterState::Failed(at) if at.elapsed() < RETRY_AFTER => return None,
                    PosterState::Failed(_) => {}
                }
            }
            poster.cancel();
            self.posters.remove(path);
        }
        if self.posters.len() >= CACHE_SIZE {
            let oldest = self.posters.iter().filter(|(_, p)| p.seen != self.frame).min_by_key(|(_, p)| p.seen).map(|(path, _)| path.clone());
            let oldest = oldest?;
            if let Some(poster) = self.posters.remove(&oldest) {
                poster.cancel();
            }
        }
        if self.thumbnails.is_none() {
            self.thumbnails = ThumbnailWorker::start(ctx.clone());
        }
        let cancel = Arc::new(AtomicBool::new(false));
        let job = ThumbnailJob { path: path.to_owned(), version, cancel: cancel.clone() };
        let state = match self.thumbnails.as_ref().map(|worker| worker.jobs.try_send(job)) {
            Some(Ok(())) => PosterState::Loading(cancel),
            Some(Err(TrySendError::Full(_))) => {
                ctx.request_repaint_after(POLL);
                return None;
            }
            _ => PosterState::Failed(Instant::now()),
        };
        self.posters.insert(path.to_owned(), Poster { version, seen: self.frame, state });
        None
    }

    /// One muted, looping preview of a finite range in a local media file.
    /// Keep the poster visible when this returns None. Repeated calls with the same key
    /// retain playback; changing file, version or range cancels the old decoder first.
    pub fn preview(&mut self, ctx: &Context, path: &str, version: i64, start_s: f64, end_s: f64) -> Option<TextureHandle> {
        if path.is_empty() || !start_s.is_finite() || !end_s.is_finite() {
            return None;
        }
        let start_s = start_s.max(0.0);
        if end_s <= start_s {
            return None;
        }
        self.preview_used = true;
        if self.previews.is_none() {
            self.previews = PreviewWorker::start(ctx.clone());
        }
        let worker = self.previews.as_ref()?;
        if self.active.as_ref().is_none_or(|active| !active.key.matches(path, version, start_s, end_s)) {
            let generation = worker.generation.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
            let key = PreviewKey { path: path.to_owned(), version, start: start_s.to_bits(), end: end_s.to_bits() };
            let status = Arc::new(AtomicU8::new(LOADING));
            let job = PreviewJob { key: key.clone(), generation, status: status.clone() };
            // A single pending request, with newest-wins replacement during rapid hover/seek.
            if let Err(TrySendError::Full(job)) = worker.jobs.try_send(job) {
                let _ = worker.pending.try_recv();
                let _ = worker.jobs.try_send(job);
            }
            self.active = Some(ActivePreview { key, generation, status, texture: None });
        }
        let active = self.active.as_mut()?;
        while let Ok(frame) = worker.frames.try_recv() {
            if frame.generation == active.generation && active.status.load(Ordering::Acquire) != FAILED {
                let image = frame.image();
                if let Some(texture) = &mut active.texture {
                    texture.set(image, TextureOptions::LINEAR);
                } else {
                    active.texture = Some(ctx.load_texture("clip-preview", image, TextureOptions::LINEAR));
                }
            }
            let _ = worker.recycled.try_send(frame);
        }
        if active.status.load(Ordering::Acquire) == FAILED { None } else { active.texture.clone() }
    }

    /// Required even when there are no visible cards. Leaving hover/detail stops playback.
    pub fn end_frame(&mut self) {
        if !self.preview_used && self.active.take().is_some()
            && let Some(worker) = &self.previews
        {
            worker.generation.fetch_add(1, Ordering::AcqRel);
            let _ = worker.pending.try_recv();
            while let Ok(frame) = worker.frames.try_recv() {
                let _ = worker.recycled.try_send(frame);
            }
        }
        self.posters.retain(|_, poster| {
            if poster.seen != self.frame && matches!(poster.state, PosterState::Loading(_)) {
                poster.cancel();
                false
            } else {
                true
            }
        });
    }

    fn drain_posters(&mut self, ctx: &Context) {
        let Some(worker) = &self.thumbnails else { return };
        while let Ok(result) = worker.results.try_recv() {
            let Some(poster) = self.posters.get_mut(&result.job.path) else { continue };
            if poster.version != result.job.version || result.job.cancel.load(Ordering::Acquire) {
                continue;
            }
            if !matches!(&poster.state, PosterState::Loading(cancel) if Arc::ptr_eq(cancel, &result.job.cancel)) {
                continue;
            }
            poster.state = match result.image {
                Some(image) => PosterState::Ready(ctx.load_texture(format!("clip-poster:{}:{}", result.job.path, result.job.version), image, TextureOptions::LINEAR)),
                None => PosterState::Failed(Instant::now()),
            };
        }
    }
}

impl Drop for ClipMedia {
    fn drop(&mut self) {
        if let Some(worker) = &self.thumbnails {
            worker.stop.store(true, Ordering::Release);
        }
        if let Some(worker) = &self.previews {
            worker.stop.store(true, Ordering::Release);
            worker.generation.fetch_add(1, Ordering::AcqRel);
        }
        // Thread handles are deliberately detached: cancellation and child reaping happen
        // in the workers, never in an egui frame or this destructor.
    }
}

impl ThumbnailWorker {
    fn start(ctx: Context) -> Option<Self> {
        let (jobs, incoming) = bounded::<ThumbnailJob>(8);
        let (outgoing, results) = bounded(2);
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::Builder::new().name("clip-posters".into()).spawn(move || {
            let mut frame = Frame::new();
            while !stopped.load(Ordering::Acquire) {
                let job = match incoming.recv_timeout(POLL) {
                    Ok(job) => job,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                };
                let cancelled = || stopped.load(Ordering::Acquire) || job.cancel.load(Ordering::Acquire);
                if cancelled() {
                    continue;
                }
                let image = poster_image(&job.path, &mut frame, &cancelled);
                if cancelled() {
                    continue;
                }
                let mut result = ThumbnailResult { job, image };
                loop {
                    match outgoing.send_timeout(result, POLL) {
                        Ok(()) => { ctx.request_repaint(); break; }
                        Err(crossbeam_channel::SendTimeoutError::Timeout(pending)) => {
                            if stopped.load(Ordering::Acquire) || pending.job.cancel.load(Ordering::Acquire) {
                                break;
                            }
                            result = pending;
                        }
                        Err(_) => return,
                    }
                }
            }
        }).ok()?;
        Some(Self { jobs, results, stop })
    }
}

impl PreviewWorker {
    fn start(ctx: Context) -> Option<Self> {
        let (jobs, incoming) = bounded::<PreviewJob>(1);
        let pending = incoming.clone();
        let (outgoing, frames) = bounded::<Frame>(1);
        let replace = frames.clone();
        let (recycled, available) = bounded::<Frame>(3);
        let recycle = recycled.clone();
        let generation = Arc::new(AtomicU64::new(0));
        let current = generation.clone();
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        std::thread::Builder::new().name("clip-preview".into()).spawn(move || {
            // Exactly three reusable pixel buffers: decoder, latest frame and UI upload.
            for _ in 0..3 {
                if recycle.try_send(Frame::new()).is_err() {
                    return;
                }
            }
            let mut failures: VecDeque<(PreviewKey, Instant)> = VecDeque::new();
            while !stopped.load(Ordering::Acquire) {
                let job = match incoming.recv_timeout(POLL) {
                    Ok(job) => job,
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                    Err(_) => break,
                };
                let cancelled = || stopped.load(Ordering::Acquire) || current.load(Ordering::Acquire) != job.generation;
                if cancelled() {
                    continue;
                }
                failures.retain(|(_, at)| at.elapsed() < RETRY_AFTER);
                let failed = failures.iter().any(|(key, _)| key == &job.key)
                    || !play_preview(&job, &ctx, &available, &outgoing, &replace, &recycle, &cancelled);
                if failed && !cancelled() {
                    job.status.store(FAILED, Ordering::Release);
                    if !failures.iter().any(|(key, _)| key == &job.key) {
                        if failures.len() == 16 {
                            failures.pop_front();
                        }
                        failures.push_back((job.key, Instant::now()));
                    }
                    ctx.request_repaint();
                }
            }
        }).ok()?;
        Some(Self { jobs, pending, frames, recycled, generation, stop })
    }
}

fn local_file(path: &str) -> Option<PathBuf> {
    let path = std::fs::canonicalize(path).ok()?;
    path.is_file().then_some(path)
}

fn poster_image(path: &str, frame: &mut Frame, cancelled: &impl Fn() -> bool) -> Option<ColorImage> {
    let path = local_file(path)?;
    let image = path.extension().and_then(|ext| ext.to_str()).is_some_and(|ext| {
        ["jpg", "jpeg", "png", "webp", "gif"].iter().any(|known| ext.eq_ignore_ascii_case(known))
    });
    let seeks: &[f64] = if image { &[0.0] } else { &[1.0, 0.0] };
    for &seek in seeks {
        if cancelled() {
            return None;
        }
        let mut decoder = Decoder::start(&path, seek, None).ok()?;
        if decoder.frame(frame, cancelled).is_ok() {
            return Some(frame.image());
        }
    }
    None
}

fn play_preview(
    job: &PreviewJob,
    ctx: &Context,
    available: &Receiver<Frame>,
    outgoing: &Sender<Frame>,
    replace: &Receiver<Frame>,
    recycle: &Sender<Frame>,
    cancelled: &impl Fn() -> bool,
) -> bool {
    let Some(path) = local_file(&job.key.path) else { return false };
    let start = f64::from_bits(job.key.start);
    let duration = f64::from_bits(job.key.end) - start;
    while !cancelled() {
        let Ok(mut decoder) = Decoder::start(&path, start, Some(duration)) else { return false };
        let mut delivered = false;
        let mut next_frame = Instant::now();
        loop {
            if cancelled() {
                return true;
            }
            let mut frame = match available.recv_timeout(POLL) {
                Ok(frame) => frame,
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => continue,
                Err(_) => return false,
            };
            if let Err(error) = decoder.frame(&mut frame, cancelled) {
                let _ = recycle.try_send(frame);
                if cancelled() {
                    return true;
                }
                if delivered && error.kind() == io::ErrorKind::UnexpectedEof {
                    while Instant::now() < next_frame && !cancelled() {
                        std::thread::sleep(next_frame.saturating_duration_since(Instant::now()).min(POLL));
                    }
                    break;
                }
                return false;
            }
            while Instant::now() < next_frame && !cancelled() {
                std::thread::sleep(next_frame.saturating_duration_since(Instant::now()).min(POLL));
            }
            if cancelled() {
                let _ = recycle.try_send(frame);
                return true;
            }
            frame.generation = job.generation;
            if let Err(TrySendError::Full(frame)) = outgoing.try_send(frame) {
                if let Ok(previous) = replace.try_recv() {
                    let _ = recycle.try_send(previous);
                }
                if let Err(error) = outgoing.try_send(frame) {
                    let _ = recycle.try_send(error.into_inner());
                }
            }
            delivered = true;
            job.status.store(PLAYING, Ordering::Release);
            ctx.request_repaint();
            next_frame = (next_frame + FRAME_TIME).max(Instant::now());
        }
        // Drop kills/reaps before the next loop or a different requested file starts.
    }
    true
}

/// PPM provides each frame's real dimensions without a separate blocking probe. The
/// decoder's pipe is nonblocking so cancellation also interrupts a corrupt/stalled file.
struct Decoder {
    child: Child,
    stdout: ChildStdout,
}

impl Decoder {
    fn start(path: &Path, start: f64, duration: Option<f64>) -> io::Result<Self> {
        let mut command = Command::new("ffmpeg");
        command.args(["-v", "error", "-nostdin", "-threads", "1", "-filter_threads", "1", "-protocol_whitelist", "file,pipe"]);
        // Seeking even to zero can exhaust a single-image demuxer before its first frame.
        if start > 0.0 {
            command.arg("-ss").arg(start.to_string());
        }
        command.arg("-i").arg(path).args(["-map", "0:v:0", "-an", "-sn", "-dn"]);
        if let Some(duration) = duration {
            command.args(["-t", &duration.to_string(), "-vf", "fps=12:eof_action=pass,scale=480:480:force_original_aspect_ratio=decrease,setsar=1"]);
        } else {
            command.args(["-frames:v", "1", "-vf", "scale=480:480:force_original_aspect_ratio=decrease,setsar=1"]);
        }
        let mut child = command.args(["-threads", "1", "-c:v", "ppm", "-pix_fmt", "rgb24", "-f", "image2pipe", "pipe:1"])
            .stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn()?;
        let stdout = child.stdout.take().expect("piped ffmpeg stdout");
        let decoder = Self { child, stdout };
        // SAFETY: stdout owns this live descriptor; only this worker changes its flags.
        let flags = unsafe { libc::fcntl(decoder.stdout.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 || unsafe { libc::fcntl(decoder.stdout.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(decoder)
    }

    fn frame(&mut self, frame: &mut Frame, cancelled: &impl Fn() -> bool) -> io::Result<()> {
        let deadline = Instant::now() + DECODE_TIMEOUT;
        let mut token = [0u8; 24];
        let len = self.token(&mut token, deadline, cancelled)?;
        if &token[..len] != b"P6" {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid preview frame"));
        }
        let mut values = [0usize; 3];
        for value in &mut values {
            let len = self.token(&mut token, deadline, cancelled)?;
            *value = std::str::from_utf8(&token[..len]).ok().and_then(|s| s.parse().ok())
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid preview dimensions"))?;
        }
        let [width, height, depth] = values;
        if width == 0 || height == 0 || width > SIDE || height > SIDE || depth != 255 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "oversized preview frame"));
        }
        self.read_exact(&mut frame.pixels[..width * height * 3], deadline, cancelled)?;
        frame.size = [width, height];
        Ok(())
    }

    fn token(&mut self, token: &mut [u8], deadline: Instant, cancelled: &impl Fn() -> bool) -> io::Result<usize> {
        let mut len = 0;
        loop {
            let mut byte = [0];
            self.read_exact(&mut byte, deadline, cancelled)?;
            if byte[0].is_ascii_whitespace() {
                if len != 0 {
                    return Ok(len);
                }
            } else {
                if len == token.len() {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "invalid preview header"));
                }
                token[len] = byte[0];
                len += 1;
            }
        }
    }

    fn read_exact(&mut self, mut bytes: &mut [u8], deadline: Instant, cancelled: &impl Fn() -> bool) -> io::Result<()> {
        while !bytes.is_empty() {
            if cancelled() {
                return Err(io::Error::new(io::ErrorKind::Interrupted, "preview cancelled"));
            }
            if Instant::now() >= deadline {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "preview decoder timed out"));
            }
            match self.stdout.read(bytes) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::UnexpectedEof)),
                Ok(count) => bytes = &mut bytes[count..],
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    let mut fd = libc::pollfd { fd: self.stdout.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                    // SAFETY: fd is a valid, owned pipe and poll receives one initialized entry.
                    unsafe { libc::poll(&mut fd, 1, POLL.as_millis() as i32); }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
