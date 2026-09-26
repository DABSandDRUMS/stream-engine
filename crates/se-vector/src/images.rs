//! Project-asset images for `DrawOp::Image`.
//!
//! The render thread owns [`ImageCache`] outright (no locks). Cache misses are sent to a
//! dedicated loader thread over a wait-free SPSC ring; decoded images come back over a
//! second ring and are picked up by [`ImageCache::poll`] at the start of the next render.
//! All file IO, decoding and logging happens on the loader thread.

use std::collections::HashMap;
use std::path::{Component, Path};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, Thread};
use std::time::Duration;

use anyhow::Context;
use rtrb::{Consumer, Producer, PushError, RingBuffer};
use vello::peniko::{Blob, ImageAlphaType, ImageData, ImageFormat};

/// Decoded images kept at most (LRU beyond this).
pub(crate) const MAX_ENTRIES: usize = 64;
/// Decoded bytes kept at most (LRU beyond this).
pub(crate) const MAX_BYTES: usize = 256 << 20;
/// Longest side of a decoded image; larger images are downscaled on load (vello's image
/// atlas tops out at 8192², and nothing in a layer needs more).
pub(crate) const MAX_SIDE: u32 = 4096;
/// Images requested but not decoded yet, at most; further misses retry on later renders.
const MAX_PENDING: usize = 32;
/// Remembered failing paths (so each is logged once); the set is forgotten when it overflows.
const MAX_FAILED: usize = 1024;
/// Largest image accepted from disk before downscaling.
const MAX_DECODE_SIDE: u32 = 16384;
const MAX_DECODE_ALLOC: u64 = 1 << 30;

const _: () = assert!((MAX_SIDE as usize) * (MAX_SIDE as usize) * 4 <= MAX_BYTES, "one image must always fit the byte cap");

/// Cache capacity.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Limits {
    pub entries: usize,
    pub bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self { entries: MAX_ENTRIES, bytes: MAX_BYTES }
    }
}

enum Request {
    Load {
        generation: u64,
        root: Arc<Path>,
        path: String,
    },
    /// A path rejected on the render thread; the loader only logs it.
    Rejected {
        path: String,
    },
}

struct Loaded {
    generation: u64,
    path: String,
    image: Option<ImageData>,
}

enum Slot {
    Pending,
    Ready { image: ImageData, bytes: usize, last_used: u64 },
    Failed,
}

pub(crate) struct ImageCache {
    limits: Limits,
    root: Arc<Path>,
    /// Bumped on every root change; results from older generations are dropped.
    generation: u64,
    slots: HashMap<String, Slot>,
    /// LRU clock; every hit and insert takes a fresh value, so ticks are unique.
    tick: u64,
    ready: usize,
    bytes: usize,
    pending: usize,
    failed: usize,
    requests: Producer<Request>,
    results: Consumer<Loaded>,
    stop: Arc<AtomicBool>,
    loader: Thread,
}

impl ImageCache {
    pub(crate) fn new(root: &Path, limits: Limits) -> anyhow::Result<Self> {
        let (requests, rx) = RingBuffer::new(MAX_PENDING);
        let (tx, results) = RingBuffer::new(MAX_PENDING * 2);
        let stop = Arc::new(AtomicBool::new(false));
        let loader_stop = stop.clone();
        let loader = thread::Builder::new()
            .name("se-vector-images".into())
            .spawn(move || run_loader(rx, tx, &loader_stop))
            .context("spawning the se-vector image loader thread")?
            .thread()
            .clone();
        Ok(Self {
            limits,
            root: Arc::from(root),
            generation: 0,
            slots: HashMap::with_capacity(limits.entries + MAX_PENDING + MAX_FAILED + 1),
            tick: 0,
            ready: 0,
            bytes: 0,
            pending: 0,
            failed: 0,
            requests,
            results,
            stop,
            loader,
        })
    }

    /// Switch to a new assets root and forget everything cached or in flight.
    pub(crate) fn reset(&mut self, root: &Path) {
        self.root = Arc::from(root);
        self.generation += 1;
        self.slots.clear();
        self.ready = 0;
        self.bytes = 0;
        self.pending = 0;
        self.failed = 0;
    }

    /// Images requested but not decoded yet.
    pub(crate) fn pending(&self) -> usize {
        self.pending
    }

    #[cfg(test)]
    fn ready(&self) -> (usize, usize) {
        (self.ready, self.bytes)
    }

    #[cfg(test)]
    fn is_ready(&self, path: &str) -> bool {
        matches!(self.slots.get(path), Some(Slot::Ready { .. }))
    }

    /// Take finished loads from the loader thread (non-blocking).
    pub(crate) fn poll(&mut self) {
        while let Ok(done) = self.results.pop() {
            if done.generation != self.generation || !matches!(self.slots.get(done.path.as_str()), Some(Slot::Pending)) {
                continue;
            }
            self.pending -= 1;
            let slot = match done.image {
                Some(image) => {
                    let bytes = image.data.len();
                    self.evict_for(bytes);
                    self.tick += 1;
                    self.ready += 1;
                    self.bytes += bytes;
                    Slot::Ready { image, bytes, last_used: self.tick }
                }
                None => {
                    self.failed += 1;
                    Slot::Failed
                }
            };
            if let Some(s) = self.slots.get_mut(done.path.as_str()) {
                *s = slot;
            }
        }
    }

    /// The decoded image for `path`, or `None` while it loads / if it failed. A miss queues
    /// the load; nothing here blocks or touches the filesystem.
    pub(crate) fn get(&mut self, path: &str) -> Option<&ImageData> {
        if !self.slots.contains_key(path) {
            self.request(path);
            return None;
        }
        match self.slots.get_mut(path) {
            Some(Slot::Ready { image, last_used, .. }) => {
                self.tick += 1;
                *last_used = self.tick;
                Some(image)
            }
            _ => None,
        }
    }

    fn request(&mut self, path: &str) {
        // Queue full or too much in flight: retry on a later render.
        if self.pending >= MAX_PENDING || self.requests.slots() == 0 {
            return;
        }
        if self.failed >= MAX_FAILED {
            self.slots.retain(|_, s| !matches!(s, Slot::Failed));
            self.failed = 0;
        }
        let (request, slot) = if is_safe_relative(path) {
            self.pending += 1;
            (Request::Load { generation: self.generation, root: self.root.clone(), path: path.to_owned() }, Slot::Pending)
        } else {
            self.failed += 1;
            (Request::Rejected { path: path.to_owned() }, Slot::Failed)
        };
        // Single producer and `slots() > 0` was checked above, so this cannot fail.
        if self.requests.push(request).is_ok() {
            self.slots.insert(path.to_owned(), slot);
            self.loader.unpark();
        }
    }

    /// Evict least-recently-used images until one of `incoming` bytes fits.
    fn evict_for(&mut self, incoming: usize) {
        while self.ready > 0 && (self.ready >= self.limits.entries || self.bytes + incoming > self.limits.bytes) {
            let Some((oldest, bytes)) = self
                .slots
                .values()
                .filter_map(|s| match s {
                    Slot::Ready { last_used, bytes, .. } => Some((*last_used, *bytes)),
                    _ => None,
                })
                .min_by_key(|(t, _)| *t)
            else {
                break;
            };
            self.slots.retain(|_, s| !matches!(s, Slot::Ready { last_used, .. } if *last_used == oldest));
            self.ready -= 1;
            self.bytes -= bytes;
        }
    }
}

impl Drop for ImageCache {
    fn drop(&mut self) {
        // The loader may be mid-decode; it notices the flag (or the dropped ring) and exits.
        self.stop.store(true, Ordering::Release);
        self.loader.unpark();
    }
}

/// A non-empty relative path that cannot leave the assets root lexically (no root, prefix
/// or `..` components). Symlinks inside the assets folder are followed.
pub(crate) fn is_safe_relative(path: &str) -> bool {
    let mut normal = false;
    for c in Path::new(path).components() {
        match c {
            Component::Normal(_) => normal = true,
            Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => return false,
        }
    }
    normal && !path.contains('\0')
}

fn run_loader(mut requests: Consumer<Request>, mut results: Producer<Loaded>, stop: &AtomicBool) {
    loop {
        while let Ok(request) = requests.pop() {
            if stop.load(Ordering::Acquire) {
                return;
            }
            match request {
                Request::Rejected { path } => {
                    tracing::warn!(target: "se_vector", path, "draw image rejected: path must be relative and inside the assets folder");
                }
                Request::Load { generation, root, path } => {
                    let image = match decode(&root.join(&path)) {
                        Ok(image) => Some(image),
                        Err(e) => {
                            tracing::warn!(target: "se_vector", path, root = %root.display(), "draw image failed to load: {e:#}");
                            None
                        }
                    };
                    let mut done = Loaded { generation, path, image };
                    loop {
                        match results.push(done) {
                            Ok(()) => break,
                            Err(PushError::Full(back)) => {
                                if stop.load(Ordering::Acquire) || results.is_abandoned() {
                                    return;
                                }
                                done = back;
                                thread::sleep(Duration::from_millis(2));
                            }
                        }
                    }
                }
            }
        }
        if stop.load(Ordering::Acquire) || requests.is_abandoned() {
            return;
        }
        thread::park();
    }
}

fn decode(path: &Path) -> anyhow::Result<ImageData> {
    let mut reader = image::ImageReader::open(path).with_context(|| format!("opening {}", path.display()))?.with_guessed_format()?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_DECODE_SIDE);
    limits.max_image_height = Some(MAX_DECODE_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    reader.limits(limits);
    let mut img = reader.decode().with_context(|| format!("decoding {}", path.display()))?;
    if img.width() > MAX_SIDE || img.height() > MAX_SIDE {
        img = img.resize(MAX_SIDE, MAX_SIDE, image::imageops::FilterType::Triangle);
    }
    let rgba = img.into_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(ImageData { data: Blob::new(Arc::new(rgba.into_raw())), format: ImageFormat::Rgba8, alpha_type: ImageAlphaType::Alpha, width, height })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    fn write_png(dir: &Path, name: &str, w: u32, h: u32) {
        image::RgbaImage::from_pixel(w, h, image::Rgba([1, 2, 3, 255])).save(dir.join(name)).unwrap();
    }

    fn settle(cache: &mut ImageCache) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while cache.pending() > 0 {
            assert!(Instant::now() < deadline, "loader did not finish");
            thread::sleep(Duration::from_millis(2));
            cache.poll();
        }
    }

    #[test]
    fn safe_paths() {
        assert!(is_safe_relative("a.png"));
        assert!(is_safe_relative("./logos/a.png"));
        assert!(is_safe_relative("logos//a.png"));
        assert!(!is_safe_relative(""));
        assert!(!is_safe_relative("."));
        assert!(!is_safe_relative("../x.png"));
        assert!(!is_safe_relative("logos/../../x.png"));
        assert!(!is_safe_relative("logos/.."));
        assert!(!is_safe_relative("/etc/passwd"));
        assert!(!is_safe_relative("a\0.png"));
    }

    #[test]
    fn loads_off_thread_then_hits() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "a.png", 3, 2);
        let mut cache = ImageCache::new(dir.path(), Limits::default()).unwrap();
        assert!(cache.get("a.png").is_none());
        assert_eq!(cache.pending(), 1);
        // A second miss for the same path does not queue it twice.
        assert!(cache.get("a.png").is_none());
        assert_eq!(cache.pending(), 1);
        settle(&mut cache);
        let img = cache.get("a.png").expect("decoded");
        assert_eq!((img.width, img.height, img.data.len()), (3, 2, 24));
        assert_eq!(&img.data.data()[..4], &[1, 2, 3, 255]);
    }

    #[test]
    fn failures_are_terminal_until_reset() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("junk.png"), b"not a png").unwrap();
        let mut cache = ImageCache::new(dir.path(), Limits::default()).unwrap();
        for p in ["missing.png", "junk.png", "../x.png", "/abs.png"] {
            assert!(cache.get(p).is_none());
        }
        // Rejected paths never go pending.
        assert_eq!(cache.pending(), 2);
        settle(&mut cache);
        for p in ["missing.png", "junk.png", "../x.png", "/abs.png"] {
            assert!(cache.get(p).is_none());
        }
        assert_eq!(cache.pending(), 0, "failed paths are not re-requested");

        // After a root change the file may exist; it is requested again.
        write_png(dir.path(), "missing.png", 1, 1);
        cache.reset(dir.path());
        assert!(cache.get("missing.png").is_none());
        assert_eq!(cache.pending(), 1);
        settle(&mut cache);
        assert!(cache.get("missing.png").is_some());
    }

    #[test]
    fn stale_generation_results_are_dropped() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        write_png(a.path(), "x.png", 2, 2);
        let mut cache = ImageCache::new(a.path(), Limits::default()).unwrap();
        assert!(cache.get("x.png").is_none());
        cache.reset(b.path());
        assert_eq!(cache.pending(), 0);
        thread::sleep(Duration::from_millis(200));
        cache.poll();
        assert!(!cache.is_ready("x.png"), "result for the old root must not land in the new cache");
        assert_eq!(cache.ready(), (0, 0));
    }

    #[test]
    fn lru_by_count() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..5 {
            write_png(dir.path(), &format!("{i}.png"), 1, 1);
        }
        let mut cache = ImageCache::new(dir.path(), Limits { entries: 3, bytes: MAX_BYTES }).unwrap();
        for i in 0..3 {
            cache.get(&format!("{i}.png"));
            settle(&mut cache);
        }
        assert_eq!(cache.ready(), (3, 12));
        // Touch 0 so 1 is the least recently used.
        assert!(cache.get("0.png").is_some());
        cache.get("3.png");
        settle(&mut cache);
        assert_eq!(cache.ready(), (3, 12));
        assert!(cache.is_ready("0.png") && cache.is_ready("2.png") && cache.is_ready("3.png"));
        assert!(!cache.is_ready("1.png"));
        // An evicted image is simply loaded again on its next use.
        assert!(cache.get("1.png").is_none());
        settle(&mut cache);
        assert!(cache.is_ready("1.png") && !cache.is_ready("2.png"));
    }

    #[test]
    fn lru_by_bytes() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "big.png", 10, 10); // 400 bytes
        write_png(dir.path(), "s1.png", 5, 5); // 100 bytes
        write_png(dir.path(), "s2.png", 5, 5);
        let mut cache = ImageCache::new(dir.path(), Limits { entries: 64, bytes: 500 }).unwrap();
        for p in ["s1.png", "s2.png", "big.png"] {
            cache.get(p);
            settle(&mut cache);
        }
        // 100 + 100 + 400 > 500: only the oldest small one had to go.
        assert_eq!(cache.ready(), (2, 500));
        assert!(!cache.is_ready("s1.png") && cache.is_ready("s2.png") && cache.is_ready("big.png"));
    }

    #[test]
    fn oversized_images_are_downscaled() {
        let dir = tempfile::tempdir().unwrap();
        write_png(dir.path(), "wide.png", MAX_SIDE * 2, 4);
        let mut cache = ImageCache::new(dir.path(), Limits::default()).unwrap();
        cache.get("wide.png");
        settle(&mut cache);
        let img = cache.get("wide.png").expect("decoded");
        assert_eq!((img.width, img.height), (MAX_SIDE, 2));
    }

    #[test]
    fn pending_is_bounded() {
        let dir = tempfile::tempdir().unwrap();
        let mut cache = ImageCache::new(dir.path(), Limits::default()).unwrap();
        for i in 0..(MAX_PENDING * 3) {
            cache.get(&format!("nope{i}.png"));
        }
        assert!(cache.pending() <= MAX_PENDING);
        settle(&mut cache);
    }
}
