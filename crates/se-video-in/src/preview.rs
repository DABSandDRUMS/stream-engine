//! Temporary camera previews for Settings → Devices (§3.5: a newly plugged camera shows a live
//! thumbnail before any source uses it).
//!
//! The page leases a preview per camera with the action `video_in.preview {identity, on, lease}`
//! and renews it while the camera is on screen; the query `video_in.preview` returns small JPEG
//! thumbnails (base64) with a sequence number so unchanged pictures aren't sent again.
//!
//! * A camera a source is capturing is never reopened: that source's capture thread leaves a
//!   thumbnail in its [`Tap`] every [`THUMB_PERIOD_NS`] while someone is looking.
//! * A camera a source is starting on (or waiting for) is left alone ([`Plan::Wait`]).
//! * Any other camera is opened by a preview thread at its lightest mode ([`preview_mode`]) and
//!   closed when the lease runs out ([`DEFAULT_LEASE`], clamped to [`MIN_LEASE`]..=[`MAX_LEASE`])
//!   or the page sends `on = false`, so a UI that closed or crashed can't keep a camera open. A
//!   source that starts using the camera takes it over: the preview thread is stopped first.

use crate::frame::{MjpegDecoder, SignalDetector, rgba_stats, yuyv_stats};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TryRecvError};
use parking_lot::Mutex;
use se_devices::DeviceInfo;
use se_devices::v4l2::{self, Device, Mode, Stream};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// Thumbnails are at most this wide.
pub const THUMB_W: u32 = 320;
/// A thumbnail at most every 250 ms (4 per second).
pub const THUMB_PERIOD_NS: u64 = 250_000_000;
/// A picture older than this counts as "nothing coming in".
const FRESH_NS: u64 = 2_000_000_000;
pub const DEFAULT_LEASE: Duration = Duration::from_secs(6);
pub const MIN_LEASE: Duration = Duration::from_secs(1);
pub const MAX_LEASE: Duration = Duration::from_secs(60);
/// A preview thread that failed tries again after this long.
const RETRY: Duration = Duration::from_secs(3);
const JPEG_QUALITY: i32 = 70;

/// Sequence numbers are unique across taps, so a client that switches from a preview thread's
/// pictures to a source's never mistakes a new picture for one it already has.
static SEQ: AtomicU64 = AtomicU64::new(0);

// ---- leases ----------------------------------------------------------------------------------

/// Camera identity → when its preview lease runs out.
#[derive(Default, Debug)]
pub struct Leases {
    until: BTreeMap<String, Instant>,
}

impl Leases {
    /// Take or extend a lease; a shorter renewal never cuts a longer lease short.
    pub fn renew(&mut self, identity: &str, lease: Duration, now: Instant) {
        let end = now + lease.clamp(MIN_LEASE, MAX_LEASE);
        let e = self.until.entry(identity.to_string()).or_insert(end);
        *e = (*e).max(end);
    }

    pub fn release(&mut self, identity: &str) -> bool {
        self.until.remove(identity).is_some()
    }

    /// Drop and return leases that ran out.
    pub fn expire(&mut self, now: Instant) -> Vec<String> {
        let gone: Vec<String> = self.until.iter().filter(|(_, t)| **t <= now).map(|(k, _)| k.clone()).collect();
        for k in &gone {
            self.until.remove(k);
        }
        gone
    }

    pub fn contains(&self, identity: &str) -> bool {
        self.until.contains_key(identity)
    }

    pub fn is_empty(&self) -> bool {
        self.until.is_empty()
    }

    pub fn identities(&self) -> impl Iterator<Item = &String> {
        self.until.keys()
    }
}

// ---- who may touch a camera ------------------------------------------------------------------

/// A source whose capture thread owns a camera.
#[derive(Clone)]
pub struct Holder {
    pub source: String,
    pub capturing: bool,
    pub tap: Arc<Tap>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Plan {
    /// Take thumbnails from this source's capture thread.
    Tap(String),
    /// This source is (re)starting on the camera: don't open it.
    Wait(String),
    /// Nobody uses it: open it ourselves.
    Open,
}

/// How to show `identity`. `holders`: camera identity → the source that owns it.
pub fn plan(identity: &str, holders: &HashMap<String, Holder>) -> Plan {
    match holders.get(identity) {
        Some(h) if h.capturing => Plan::Tap(h.source.clone()),
        Some(h) => Plan::Wait(h.source.clone()),
        None => Plan::Open,
    }
}

// ---- mode choice -----------------------------------------------------------------------------

/// The lightest mode that still makes a sharp thumbnail: the smallest YUYV/MJPEG size at least
/// [`THUMB_W`] wide (YUYV on a tie: no decoding), at the lowest rate of 5 fps or more.
pub fn preview_mode(modes: &[Mode]) -> Option<(u32, u32, u32, f64)> {
    let usable = |m: &&Mode| (m.fourcc == v4l2::PIX_YUYV || m.fourcc == v4l2::PIX_MJPEG) && !m.fps.is_empty() && m.width > 0 && m.height > 0;
    let wide_enough = |m: &&Mode| m.width >= THUMB_W;
    let area = |m: &Mode| u64::from(m.width) * u64::from(m.height);
    let pick = modes
        .iter()
        .filter(usable)
        .filter(wide_enough)
        .min_by_key(|m| (area(m), m.fourcc != v4l2::PIX_YUYV))
        .or_else(|| modes.iter().filter(usable).max_by_key(|m| (area(m), m.fourcc == v4l2::PIX_YUYV)))?;
    let fps = pick.fps.iter().copied().filter(|f| *f >= 5.0).fold(None, |a: Option<f64>, f| Some(a.map_or(f, |a| a.min(f))));
    let fps = fps.unwrap_or_else(|| pick.fps.iter().copied().fold(0.0, f64::max));
    Some((pick.fourcc, pick.width, pick.height, fps))
}

// ---- thumbnails ------------------------------------------------------------------------------

/// Thumbnail size for a `w`×`h` picture: [`THUMB_W`] wide (never upscaled), height by aspect,
/// both even.
pub fn thumb_size(w: u32, h: u32) -> (u32, u32) {
    let tw = w.clamp(2, THUMB_W) & !1;
    let th = ((u64::from(h) * u64::from(tw) / u64::from(w.max(1))) as u32).max(2) & !1;
    (tw, th)
}

/// YCbCr → RGB for one picture's matrix and range.
#[derive(Clone, Copy, Debug)]
pub struct Yuv {
    y_off: f32,
    y_scale: f32,
    c_scale: f32,
    r_cr: f32,
    g_cb: f32,
    g_cr: f32,
    b_cb: f32,
}

impl Yuv {
    /// `matrix` = `bt709` | `bt601`; `range` = `full` | `limited` (as `PixFormat` reports them).
    pub fn new(matrix: &str, range: &str) -> Yuv {
        let (kr, kb) = if matrix == "bt709" { (0.2126, 0.0722) } else { (0.299, 0.114) };
        let kg = 1.0 - kr - kb;
        let (y_off, y_scale, c_scale) = if range == "full" { (0.0, 1.0, 1.0) } else { (16.0, 255.0 / 219.0, 255.0 / 224.0) };
        Yuv { y_off, y_scale, c_scale, r_cr: 2.0 * (1.0 - kr), g_cb: 2.0 * kb * (1.0 - kb) / kg, g_cr: 2.0 * kr * (1.0 - kr) / kg, b_cb: 2.0 * (1.0 - kb) }
    }

    fn rgb(&self, y: u8, u: u8, v: u8) -> [u8; 3] {
        let y = (f32::from(y) - self.y_off) * self.y_scale;
        let cb = (f32::from(u) - 128.0) * self.c_scale;
        let cr = (f32::from(v) - 128.0) * self.c_scale;
        let c = |x: f32| x.round().clamp(0.0, 255.0) as u8;
        [c(y + self.r_cr * cr), c(y - self.g_cb * cb - self.g_cr * cr), c(y + self.b_cb * cb)]
    }
}

/// Nearest-neighbour thumbnail of a packed YUYV picture into RGB `out` (resized as needed).
pub fn yuyv_thumb(src: &[u8], w: u32, h: u32, stride: u32, yuv: Yuv, out: &mut Vec<u8>) -> (u32, u32) {
    let (tw, th) = thumb_size(w, h);
    out.resize((tw * th * 3) as usize, 0);
    for ty in 0..th {
        let row = (u64::from(ty) * u64::from(h) / u64::from(th)) as usize * stride as usize;
        for tx in 0..tw {
            let sx = (u64::from(tx) * u64::from(w) / u64::from(tw)) as usize;
            let pair = row + (sx & !1) * 2;
            let Some(p) = src.get(pair..pair + 4) else { continue };
            let px = yuv.rgb(p[(sx & 1) * 2], p[1], p[3]);
            let o = ((ty * tw + tx) * 3) as usize;
            out[o..o + 3].copy_from_slice(&px);
        }
    }
    (tw, th)
}

/// Nearest-neighbour thumbnail of an RGBA picture into RGB `out` (resized as needed).
pub fn rgba_thumb(src: &[u8], w: u32, h: u32, stride: u32, out: &mut Vec<u8>) -> (u32, u32) {
    let (tw, th) = thumb_size(w, h);
    out.resize((tw * th * 3) as usize, 0);
    for ty in 0..th {
        let row = (u64::from(ty) * u64::from(h) / u64::from(th)) as usize * stride as usize;
        for tx in 0..tw {
            let sx = (u64::from(tx) * u64::from(w) / u64::from(tw)) as usize;
            let Some(p) = src.get(row + sx * 4..row + sx * 4 + 3) else { continue };
            let o = ((ty * tw + tx) * 3) as usize;
            out[o..o + 3].copy_from_slice(p);
        }
    }
    (tw, th)
}

#[derive(Default)]
struct Thumb {
    width: u32,
    height: u32,
    rgb: Vec<u8>,
    seq: u64,
    at_ns: u64,
}

/// Where a capture thread leaves thumbnails for the preview query.
#[derive(Default)]
pub struct Tap {
    /// Someone is looking: produce thumbnails.
    pub want: AtomicBool,
    last_ns: AtomicU64,
    thumb: Mutex<Thumb>,
    /// (seq, JPEG of that thumbnail): encoded once per new thumbnail.
    jpeg: Mutex<(u64, Vec<u8>)>,
    /// The input carries no picture (HDMI without a signal delivers blank frames): the
    /// thumbnail would be a flat colour, so none is shown.
    pub no_signal: AtomicBool,
}

/// One thumbnail as the query hands it out.
#[derive(Clone, Debug)]
pub struct Picture {
    pub seq: u64,
    pub width: u32,
    pub height: u32,
    /// When it was taken (master clock ns).
    pub at_ns: u64,
    pub jpeg: Vec<u8>,
}

impl Tap {
    /// Wanted, and the last thumbnail is at least [`THUMB_PERIOD_NS`] old.
    pub fn due(&self, now_ns: u64) -> bool {
        self.want.load(Ordering::Relaxed) && now_ns.saturating_sub(self.last_ns.load(Ordering::Relaxed)) >= THUMB_PERIOD_NS
    }

    /// Store a thumbnail drawn by `draw` into the reused RGB buffer. Never blocks the capture
    /// thread: skipped while the query is reading.
    pub fn offer(&self, now_ns: u64, draw: impl FnOnce(&mut Vec<u8>) -> (u32, u32)) {
        let Some(mut t) = self.thumb.try_lock() else { return };
        let (w, h) = draw(&mut t.rgb);
        t.width = w;
        t.height = h;
        t.at_ns = now_ns;
        t.seq = SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        self.last_ns.store(now_ns, Ordering::Relaxed);
    }

    /// The newest thumbnail as JPEG.
    pub fn picture(&self) -> Option<Picture> {
        let t = self.thumb.lock();
        if t.seq == 0 {
            return None;
        }
        let mut cache = self.jpeg.lock();
        if cache.0 != t.seq {
            let img = turbojpeg::Image {
                pixels: &t.rgb[..],
                width: t.width as usize,
                pitch: t.width as usize * 3,
                height: t.height as usize,
                format: turbojpeg::PixelFormat::RGB,
            };
            cache.1 = turbojpeg::compress(img, JPEG_QUALITY, turbojpeg::Subsamp::Sub2x2).ok()?.to_vec();
            cache.0 = t.seq;
        }
        Some(Picture { seq: t.seq, width: t.width, height: t.height, at_ns: t.at_ns, jpeg: cache.1.clone() })
    }

    /// Forget the picture (the camera closed).
    fn clear(&self) {
        self.want.store(false, Ordering::Relaxed);
        self.no_signal.store(false, Ordering::Relaxed);
        let mut t = self.thumb.lock();
        t.seq = 0;
        self.last_ns.store(0, Ordering::Relaxed);
    }
}

// ---- preview threads -------------------------------------------------------------------------

struct Worker {
    stop: Sender<()>,
    handle: std::thread::JoinHandle<()>,
}

/// A thread that is being stopped; join it off the async runtime.
pub struct Stopping(Worker);

impl Stopping {
    pub fn join(self) {
        let _ = self.0.handle.join();
    }
}

fn spawn(path: String, tap: Arc<Tap>, error: Arc<Mutex<String>>) -> std::io::Result<Worker> {
    let (stop, rx) = crossbeam_channel::bounded(1);
    let handle = std::thread::Builder::new().name("se-cam-preview".into()).spawn(move || {
        tap.want.store(true, Ordering::Relaxed);
        loop {
            match stream(&path, &tap, &rx) {
                Ok(()) => break,
                Err(e) => {
                    *error.lock() = e;
                    match rx.recv_timeout(RETRY) {
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => break,
                        Err(RecvTimeoutError::Timeout) => {}
                    }
                }
            }
        }
        tap.clear();
    })?;
    Ok(Worker { stop, handle })
}

/// Stream `path` at its preview mode until told to stop (Ok) or the camera fails (Err).
fn stream(path: &str, tap: &Tap, stop: &Receiver<()>) -> Result<(), String> {
    let busy = |e: std::io::Error| {
        if e.raw_os_error() == Some(libc::EBUSY) { "the camera is in use by another program".to_string() } else { e.to_string() }
    };
    let dev = Device::open(path, true).map_err(|e| format!("open {path}: {e}"))?;
    if !dev.caps().map_err(|e| e.to_string())?.is_capture() {
        return Err(format!("{path} is not a capture device"));
    }
    // HDMI inputs: lock onto the detected timing first (as a source would)
    if let Ok(t) = dev.query_dv_timing()
        && dev.current_dv_timing().ok() != Some(t)
    {
        let _ = dev.set_dv_timing(&t);
    }
    let modes = dev.modes().map_err(|e| e.to_string())?;
    let (fourcc, w, h, fps) = preview_mode(&modes).ok_or("no YUYV or MJPEG mode")?;
    let fmt = dev.set_format(fourcc, w, h).map_err(busy)?;
    if fmt.fourcc != fourcc {
        return Err(format!("{path} did not accept {}", v4l2::format_name(fourcc)));
    }
    let _ = dev.set_fps(fps);
    let mut s = Stream::start(&dev, 3).map_err(busy)?;
    let (w, h) = (fmt.width, fmt.height);
    let stride = if fourcc == v4l2::PIX_YUYV { fmt.bytesperline.max(w * 2) } else { w * 4 };
    let yuv = Yuv::new(fmt.matrix(), fmt.range());
    let mut dec = if fourcc == v4l2::PIX_MJPEG { Some(MjpegDecoder::new()?) } else { None };
    let mut rgba = if dec.is_some() { vec![0u8; (stride * h) as usize] } else { Vec::new() };
    // same no-signal rules as a camera source (blank or frozen-flat frames, input status)
    let cfg = crate::config::SignalCfg::default();
    let mut det = SignalDetector::new(cfg.timeout_ms, cfg.black_level, cfg.hold_ms);
    let mut last_input = 0u64;
    let opened = se_clock::now();
    loop {
        let ready = s.wait(100).map_err(|e| e.to_string())?;
        if ready.frame {
            while let Some(f) = s.dequeue().map_err(|e| e.to_string())? {
                let now = se_clock::now();
                if !f.is_error() {
                    let data = s.data(&f);
                    let due = tap.due(now);
                    match &mut dec {
                        None if data.len() >= (stride * (h - 1) + w * 2) as usize => {
                            det.frame(now, yuyv_stats(data, w, h, stride));
                            if due {
                                tap.offer(now, |out| yuyv_thumb(data, w, h, stride, yuv, out));
                            }
                        }
                        None => {}
                        // MJPEG is only decoded when a thumbnail is due
                        Some(d) if due => {
                            if d.header(data) == Some((w, h)) && d.decode_rgba(data, &mut rgba, w, h, stride).is_ok() {
                                det.frame(now, rgba_stats(&rgba, w, h, stride));
                                tap.offer(now, |out| rgba_thumb(&rgba, w, h, stride, out));
                            }
                        }
                        // between decoded thumbnails: the frame still arrived, judge it like the last
                        Some(_) => det.frame(now, det.last_stats),
                    }
                }
                s.requeue(f.index).map_err(|e| e.to_string())?;
            }
        } else if ready.error {
            return Err(format!("{path} went away"));
        }
        let now = se_clock::now();
        if now.saturating_sub(last_input) >= 1_000_000_000 {
            last_input = now;
            if let Ok(inp) = dev.input_status() {
                det.input(!v4l2::input_has_no_signal(inp));
            }
        }
        // give a fresh stream the detector's hold time before calling it blank
        let settled = now.saturating_sub(opened) >= (cfg.hold_ms + cfg.timeout_ms) * 1_000_000;
        tap.no_signal.store(settled && !det.tick(now), Ordering::Relaxed);
        match stop.try_recv() {
            Ok(()) | Err(TryRecvError::Disconnected) => return Ok(()),
            Err(TryRecvError::Empty) => {}
        }
    }
}

// ---- the set of previews ---------------------------------------------------------------------

struct Entry {
    /// Our own preview thread's tap (the source's tap is used while it captures).
    tap: Arc<Tap>,
    worker: Option<Worker>,
    plan: Plan,
    error: Arc<Mutex<String>>,
}

/// Leased previews and the threads serving them.
#[derive(Default)]
pub struct Previews {
    pub leases: Leases,
    entries: BTreeMap<String, Entry>,
}

impl Previews {
    /// Any preview thread running (then starting sources must check for conflicts).
    pub fn has_workers(&self) -> bool {
        self.entries.values().any(|e| e.worker.is_some())
    }

    /// No leases and nothing left to wind down.
    pub fn is_idle(&self) -> bool {
        self.leases.is_empty() && self.entries.is_empty()
    }

    /// Stop our preview threads on these cameras (a source is about to open them).
    pub fn yield_to_sources(&mut self, identities: &[String]) -> Vec<Stopping> {
        let mut out = Vec::new();
        for id in identities {
            if let Some(e) = self.entries.get_mut(id)
                && let Some(w) = e.worker.take()
            {
                let _ = w.stop.try_send(());
                out.push(Stopping(w));
                e.plan = Plan::Wait(String::new());
            }
        }
        out
    }

    /// Bring threads and taps in line with the leases. `holders`: camera identity → source that
    /// owns it; `cameras`: cameras present now. Returns threads to join.
    pub fn reconcile(&mut self, holders: &HashMap<String, Holder>, cameras: &[DeviceInfo]) -> Vec<Stopping> {
        let mut stopping = Vec::new();
        let mut stop = |e: &mut Entry| {
            if let Some(w) = e.worker.take() {
                let _ = w.stop.try_send(());
                stopping.push(Stopping(w));
            }
        };
        // released or expired leases
        let gone: Vec<String> = self.entries.keys().filter(|k| !self.leases.contains(k)).cloned().collect();
        for k in gone {
            if let Some(mut e) = self.entries.remove(&k) {
                stop(&mut e);
            }
        }
        for id in self.leases.identities() {
            let p = plan(id, holders);
            let e = self.entries.entry(id.clone()).or_insert_with(|| Entry {
                tap: Arc::new(Tap::default()),
                worker: None,
                plan: p.clone(),
                error: Arc::new(Mutex::new(String::new())),
            });
            e.plan = p.clone();
            let path = cameras.iter().find(|c| c.identity == *id).map(|c| c.path.clone());
            match (&p, path) {
                (Plan::Open, Some(path)) => {
                    if e.worker.is_none() {
                        e.error.lock().clear();
                        match spawn(path, e.tap.clone(), e.error.clone()) {
                            Ok(w) => e.worker = Some(w),
                            Err(err) => *e.error.lock() = format!("can't start the preview: {err}"),
                        }
                    }
                }
                (Plan::Open, None) => {
                    stop(e);
                    *e.error.lock() = "not plugged in".into();
                }
                _ => stop(e),
            }
        }
        // sources show thumbnails only while someone is looking at them
        for (id, h) in holders {
            h.tap.want.store(self.leases.contains(id) && h.capturing, Ordering::Relaxed);
        }
        stopping
    }

    /// The query's view: one entry per leased camera. `have`: identity → the sequence number
    /// the client already shows (its picture is left out). `now_ns`: master clock.
    pub fn report(&self, holders: &HashMap<String, Holder>, have: &HashMap<String, u64>, now_ns: u64) -> Vec<Report> {
        self.entries
            .iter()
            .map(|(id, e)| {
                let (tap, source) = match &e.plan {
                    Plan::Tap(s) => (holders.get(id).map(|h| h.tap.clone()), Some(s.clone())),
                    Plan::Wait(s) => (None, Some(s.clone()).filter(|s| !s.is_empty())),
                    Plan::Open => (Some(e.tap.clone()), None),
                };
                let blank = tap.as_ref().is_some_and(|t| t.no_signal.load(Ordering::Relaxed));
                let pic = if blank { None } else { tap.and_then(|t| t.picture()) };
                let error = if matches!(e.plan, Plan::Open) { e.error.lock().clone() } else { String::new() };
                let state = match (&e.plan, &pic) {
                    (Plan::Wait(_), _) => "waiting",
                    _ if blank => "no_signal",
                    (_, Some(p)) if now_ns.saturating_sub(p.at_ns) <= FRESH_NS => "live",
                    _ if !error.is_empty() => "error",
                    (_, Some(_)) => "no_picture",
                    (_, None) => "starting",
                };
                let known = have.get(id).copied();
                Report {
                    identity: id.clone(),
                    state,
                    source,
                    error,
                    picture: pic.map(|p| if known == Some(p.seq) { Picture { jpeg: Vec::new(), ..p } } else { p }),
                }
            })
            .collect()
    }
}

/// One leased camera in the `video_in.preview` query.
#[derive(Debug)]
pub struct Report {
    pub identity: String,
    /// `live` | `starting` | `no_picture` | `no_signal` (frames arrive but carry no picture) |
    /// `waiting` (a source is opening it) | `error`.
    pub state: &'static str,
    /// The source whose frames are shown (or that is opening the camera).
    pub source: Option<String>,
    pub error: String,
    /// Empty `jpeg` = the client already has this picture.
    pub picture: Option<Picture>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_devices::v4l2::{PIX_MJPEG, PIX_YUYV};

    #[test]
    fn leases_extend_clamp_and_expire() {
        let t0 = Instant::now();
        let s = Duration::from_secs;
        let mut l = Leases::default();
        l.renew("cam", s(6), t0);
        l.renew("cam", s(6), t0 + s(5)); // renewed before it ran out: until 11 s
        assert!(l.expire(t0 + s(10)).is_empty());
        assert_eq!(l.expire(t0 + s(11)), ["cam"]);
        assert!(l.is_empty());

        // a crashed UI's lease is capped; a zero lease still lasts the minimum
        l.renew("a", s(3600), t0);
        l.renew("b", Duration::ZERO, t0);
        assert_eq!(l.expire(t0 + MIN_LEASE), ["b"]);
        assert!(l.expire(t0 + MAX_LEASE - s(1)).is_empty());
        assert_eq!(l.expire(t0 + MAX_LEASE), ["a"]);

        // a second viewer asking for less doesn't shorten the first one's lease
        l.renew("c", s(30), t0);
        l.renew("c", s(2), t0 + s(1));
        assert!(l.expire(t0 + s(20)).is_empty());
        // turning a preview off ends it at once
        assert!(l.release("c"));
        assert!(!l.release("c"));
        assert!(l.is_empty());
    }

    #[test]
    fn a_camera_a_source_uses_is_never_opened_for_a_preview() {
        let holder = |source: &str, capturing: bool| Holder { source: source.into(), capturing, tap: Arc::new(Tap::default()) };
        let holders = HashMap::from([("hdmi1".to_string(), holder("cam_kit", true)), ("hdmi2".to_string(), holder("cam_room", false))]);
        assert_eq!(plan("hdmi1", &holders), Plan::Tap("cam_kit".into()));
        assert_eq!(plan("hdmi2", &holders), Plan::Wait("cam_room".into()), "a source still opening the camera");
        assert_eq!(plan("usb", &holders), Plan::Open);

        // reconcile: only the tap of the watched, capturing source produces thumbnails
        let mut p = Previews::default();
        let now = Instant::now();
        p.leases.renew("hdmi1", DEFAULT_LEASE, now);
        p.leases.renew("hdmi2", DEFAULT_LEASE, now);
        assert!(p.reconcile(&holders, &[]).is_empty());
        assert!(!p.has_workers());
        assert!(holders["hdmi1"].tap.want.load(Ordering::Relaxed));
        assert!(!holders["hdmi2"].tap.want.load(Ordering::Relaxed));
        let r = p.report(&holders, &HashMap::new(), 0);
        let states: Vec<(&str, &str)> = r.iter().map(|x| (x.identity.as_str(), x.state)).collect();
        assert_eq!(states, [("hdmi1", "starting"), ("hdmi2", "waiting")]);

        // the lease ends: the source stops making thumbnails
        p.leases.release("hdmi1");
        p.reconcile(&holders, &[]);
        assert!(!holders["hdmi1"].tap.want.load(Ordering::Relaxed));
        assert_eq!(p.report(&holders, &HashMap::new(), 0).len(), 1);
    }

    #[test]
    fn unplugged_camera_reports_an_error_without_a_thread() {
        let mut p = Previews::default();
        p.leases.renew("gone", DEFAULT_LEASE, Instant::now());
        p.reconcile(&HashMap::new(), &[]);
        assert!(!p.has_workers());
        let r = p.report(&HashMap::new(), &HashMap::new(), 0);
        assert_eq!((r[0].state, r[0].error.as_str()), ("error", "not plugged in"));
    }

    #[test]
    fn preview_mode_is_the_lightest_sharp_mode() {
        let m = |f, w, h, fps: &[f64]| Mode { fourcc: f, width: w, height: h, fps: fps.to_vec() };
        let sonix = [
            m(PIX_MJPEG, 1920, 1080, &[30.0, 25.0]),
            m(PIX_MJPEG, 640, 480, &[30.0, 15.0, 5.0, 1.0]),
            m(PIX_YUYV, 640, 480, &[30.0, 15.0]),
            m(PIX_YUYV, 160, 120, &[30.0]),
            m(PIX_MJPEG, 320, 240, &[30.0, 10.0]),
        ];
        assert_eq!(preview_mode(&sonix), Some((PIX_MJPEG, 320, 240, 10.0)));
        // same size in both: YUYV needs no decoding
        let both = [m(PIX_MJPEG, 640, 480, &[30.0]), m(PIX_YUYV, 640, 480, &[30.0])];
        assert_eq!(preview_mode(&both), Some((PIX_YUYV, 640, 480, 30.0)));
        // a capture card with one mode takes it
        assert_eq!(preview_mode(&[m(PIX_YUYV, 1920, 1080, &[60.0])]), Some((PIX_YUYV, 1920, 1080, 60.0)));
        // nothing wide enough: the largest; nothing ≥ 5 fps: the fastest
        assert_eq!(preview_mode(&[m(PIX_YUYV, 160, 120, &[2.0, 3.0]), m(PIX_YUYV, 176, 144, &[1.0])]), Some((PIX_YUYV, 176, 144, 1.0)));
        assert_eq!(preview_mode(&[m(v4l2::PIX_NV12, 640, 480, &[30.0])]), None);
    }

    #[test]
    fn thumbnail_sizes_keep_the_aspect_and_never_upscale() {
        assert_eq!(thumb_size(1920, 1080), (320, 180));
        assert_eq!(thumb_size(640, 480), (320, 240));
        assert_eq!(thumb_size(160, 120), (160, 120));
        assert_eq!(thumb_size(1280, 721), (320, 180));
    }

    #[test]
    fn yuyv_colors_follow_matrix_and_range() {
        let px = |y: u8, u: u8, v: u8, yuv: Yuv| {
            let src = [y, u, y, v];
            let mut out = Vec::new();
            yuyv_thumb(&src, 2, 2, 0, yuv, &mut out);
            [out[0], out[1], out[2]]
        };
        let lim601 = Yuv::new("bt601", "limited");
        assert_eq!(px(16, 128, 128, lim601), [0, 0, 0]);
        assert_eq!(px(235, 128, 128, lim601), [255, 255, 255]);
        let red = px(81, 90, 240, lim601);
        assert!(red[0] >= 250 && red[1] <= 5 && red[2] <= 5, "{red:?}");
        assert_eq!(px(255, 128, 128, Yuv::new("bt601", "full")), [255, 255, 255]);
        // BT.709 red: Y 63, Cb 102, Cr 240
        let red709 = px(63, 102, 240, Yuv::new("bt709", "limited"));
        assert!(red709[0] >= 250 && red709[1] <= 5 && red709[2] <= 5, "{red709:?}");
    }

    #[test]
    fn a_blank_input_reports_no_signal_instead_of_a_flat_picture() {
        // an HDMI input without a signal: all-zero YUYV (would draw as solid green)
        let (w, h) = (64u32, 36u32);
        let src = vec![0u8; (w * 2 * h) as usize];
        let tap = Arc::new(Tap::default());
        tap.want.store(true, Ordering::Relaxed);
        tap.offer(1_000_000_000, |out| yuyv_thumb(&src, w, h, w * 2, Yuv::new("bt601", "limited"), out));
        tap.no_signal.store(true, Ordering::Relaxed);
        let mut prev = Previews::default();
        let holders = HashMap::from([("cam".to_string(), Holder { source: "cam_3".into(), capturing: true, tap: tap.clone() })]);
        prev.leases.renew("cam", DEFAULT_LEASE, Instant::now());
        prev.reconcile(&holders, &[]);
        let r = prev.report(&holders, &HashMap::new(), 1_100_000_000);
        assert_eq!(r[0].state, "no_signal");
        assert!(r[0].picture.is_none(), "no flat-colour thumbnail");
        // the picture comes back with the signal
        tap.no_signal.store(false, Ordering::Relaxed);
        let r = prev.report(&holders, &HashMap::new(), 1_100_000_000);
        assert_eq!(r[0].state, "live");
        assert!(r[0].picture.is_some());
    }

    #[test]
    fn thumbnails_sample_the_picture_and_encode_once() {
        // 640×360 YUYV: left half black, right half white
        let (w, h) = (640u32, 360u32);
        let stride = w * 2;
        let mut src = vec![0u8; (stride * h) as usize];
        for y in 0..h as usize {
            for x in 0..w as usize {
                let o = y * stride as usize + x * 2;
                src[o] = if x < 320 { 16 } else { 235 };
                src[o + 1] = 128;
            }
        }
        let tap = Tap::default();
        assert!(!tap.due(1_000_000_000), "nobody is looking");
        tap.want.store(true, Ordering::Relaxed);
        assert!(tap.due(1_000_000_000));
        tap.offer(1_000_000_000, |out| yuyv_thumb(&src, w, h, stride, Yuv::new("bt601", "limited"), out));
        assert!(!tap.due(1_100_000_000), "at most every 250 ms");
        assert!(tap.due(1_250_000_000));
        let p = tap.picture().unwrap();
        assert_eq!((p.width, p.height), (320, 180));
        let img = turbojpeg::decompress(&p.jpeg, turbojpeg::PixelFormat::RGB).unwrap();
        assert_eq!((img.width, img.height), (320, 180));
        let at = |x: usize| img.pixels[(90 * 320 + x) * 3];
        assert!(at(40) < 20 && at(280) > 235, "left {} right {}", at(40), at(280));
        // same picture again: same sequence number
        assert_eq!(tap.picture().unwrap().seq, p.seq);

        // a client that already has it gets no bytes; a new thumbnail gets a new number
        let mut prev = Previews::default();
        let tap = Arc::new(tap);
        let holders = HashMap::from([("cam".to_string(), Holder { source: "cam_kit".into(), capturing: true, tap: tap.clone() })]);
        prev.leases.renew("cam", DEFAULT_LEASE, Instant::now());
        prev.reconcile(&holders, &[]);
        let r = prev.report(&holders, &HashMap::from([("cam".to_string(), p.seq)]), 1_500_000_000);
        let pic = r[0].picture.as_ref().unwrap();
        assert_eq!((r[0].state, pic.seq, pic.jpeg.len()), ("live", p.seq, 0));
        assert_eq!(prev.report(&holders, &HashMap::new(), 9_000_000_000)[0].state, "no_picture");
        tap.offer(9_000_000_000, |out| rgba_thumb(&[200; 64 * 36 * 4], 64, 36, 64 * 4, out));
        let r = prev.report(&holders, &HashMap::from([("cam".to_string(), p.seq)]), 9_100_000_000);
        let pic = r[0].picture.as_ref().unwrap();
        assert!(pic.seq > p.seq && !pic.jpeg.is_empty() && (pic.width, pic.height) == (64, 36));
    }

    #[test]
    fn a_busy_reader_makes_the_capture_thread_skip_not_wait() {
        let tap = Tap::default();
        tap.want.store(true, Ordering::Relaxed);
        let held = tap.thumb.lock();
        tap.offer(1, |out| rgba_thumb(&[0; 16], 2, 2, 8, out));
        drop(held);
        assert!(tap.picture().is_none());
        assert!(tap.due(THUMB_PERIOD_NS), "a skipped thumbnail is tried again on the next frame");
    }
}
