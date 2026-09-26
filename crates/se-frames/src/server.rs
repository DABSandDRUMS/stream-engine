//! The engine side of `frames.sock`.
//!
//! [`FramesServer`] owns the listening socket and a `se-frames` thread that runs a
//! `poll()` loop over the listener, a wakeup eventfd and every client. The render thread
//! talks to it only through atomics and a preallocated bounded queue:
//!
//! - [`FramesServer::demand`], [`FramesServer::acquire`], [`FramesServer::present`] and
//!   [`FramesServer::abandon`] never block, never lock and never allocate.
//! - Ring changes ([`FramesServer::set_ring`], [`FramesServer::device_lost`]) go through a
//!   command channel to the server thread, which alone performs socket IO.
//!
//! Each canvas has two independent rings: [`RingKind::Dmabuf`] for clients whose hello set
//! [`HELLO_FLAG_DMABUF`], [`RingKind::Shm`] for the rest. Buffer state lives in one
//! `AtomicU64` per buffer: the ring generation in the high half, `RENDERING | PENDING |
//! HELD` flags in the low half. Every transition is a compare-and-swap that also checks the
//! generation, so updates that race with a ring reset fail instead of corrupting the new
//! ring.

use std::array;
use std::fs;
use std::io;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError, TrySendError};
use parking_lot::Mutex;

use crate::proto::{
    self, ALL_CANVASES, ALL_CANVASES_MASK, CLIENT_OBS, CLIENT_OTHER, CLIENT_UI, CanvasMsg, FrameMsg, GOODBYE_CANVAS_REMOVED, GOODBYE_DEVICE_LOST,
    GOODBYE_SHUTDOWN, Goodbye, HELLO_FLAG_DMABUF, Hello, MAX_BUFFERS, MAX_CANVASES, MAX_MSG_SIZE, MIN_BUFFERS, MSG_HELLO, MSG_RELEASE, Release, canvas_name,
};
use crate::sys::{self, RecvOutcome, SendOutcome};

/// Which buffer ring of a canvas a client consumes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RingKind {
    /// dmabufs (`drm_fourcc != 0`), for clients whose hello set [`HELLO_FLAG_DMABUF`].
    Dmabuf,
    /// memfds with packed RGBA8 rows (`drm_fourcc == 0`), for every other client.
    Shm,
}

impl RingKind {
    pub const ALL: [RingKind; 2] = [RingKind::Dmabuf, RingKind::Shm];

    /// The ring a client with these `se_hello.flags` consumes.
    pub fn from_hello_flags(flags: u32) -> RingKind {
        if flags & HELLO_FLAG_DMABUF != 0 { RingKind::Dmabuf } else { RingKind::Shm }
    }

    pub fn name(self) -> &'static str {
        match self {
            RingKind::Dmabuf => "dmabuf",
            RingKind::Shm => "shm",
        }
    }

    fn index(self) -> usize {
        match self {
            RingKind::Dmabuf => 0,
            RingKind::Shm => 1,
        }
    }
}

/// The buffers of one canvas ring, as announced in `se_canvas`.
///
/// `fds` holds one fd per buffer ([`MIN_BUFFERS`]..=[`MAX_BUFFERS`]); buffer index `i` in
/// `acquire`/`present` is `fds[i]`. The server keeps the fds until the ring is replaced.
#[derive(Debug)]
pub struct RingDesc {
    pub width: u32,
    pub height: u32,
    /// [`proto::DRM_FORMAT_ABGR8888`] for dmabuf rings, 0 for shm rings.
    pub drm_fourcc: u32,
    /// DRM format modifier of the dmabufs; 0 for shm rings.
    pub modifier: u64,
    pub offsets: [u32; 4],
    pub strides: [u32; 4],
    /// Always 1 (single-plane buffers).
    pub planes: u32,
    pub fds: Vec<OwnedFd>,
}

impl RingDesc {
    /// An shm ring of `width × height` RGBA8 with row pitch `stride` (bytes).
    pub fn shm(width: u32, height: u32, stride: u32, fds: Vec<OwnedFd>) -> RingDesc {
        RingDesc { width, height, drm_fourcc: 0, modifier: 0, offsets: [0; 4], strides: [stride, 0, 0, 0], planes: 1, fds }
    }

    fn validate(&self, kind: RingKind) -> io::Result<()> {
        let invalid = |msg: String| Err(io::Error::new(io::ErrorKind::InvalidInput, msg));
        if !(MIN_BUFFERS..=MAX_BUFFERS).contains(&self.fds.len()) {
            return invalid(format!("a ring needs {MIN_BUFFERS} to {MAX_BUFFERS} buffers, got {}", self.fds.len()));
        }
        if self.width == 0 || self.height == 0 {
            return invalid(format!("empty canvas {}x{}", self.width, self.height));
        }
        if self.planes != 1 {
            return invalid(format!("buffers must be single-plane, got {} planes", self.planes));
        }
        if self.strides[0] == 0 {
            return invalid("strides[0] is 0".into());
        }
        match kind {
            RingKind::Shm => {
                if self.drm_fourcc != 0 || self.modifier != 0 {
                    return invalid("shm rings must have drm_fourcc = 0 and modifier = 0".into());
                }
                if u64::from(self.strides[0]) < u64::from(self.width) * 4 {
                    return invalid(format!("shm stride {} is shorter than {} RGBA8 pixels", self.strides[0], self.width));
                }
            }
            RingKind::Dmabuf => {
                if self.drm_fourcc == 0 {
                    return invalid("dmabuf rings need a DRM fourcc".into());
                }
            }
        }
        let need = u64::from(self.offsets[0]) + u64::from(self.strides[0]) * u64::from(self.height);
        for (i, fd) in self.fds.iter().enumerate() {
            let size = sys::fd_size(fd.as_fd()).map_err(|e| io::Error::new(e.kind(), format!("sizing buffer {i}: {e}")))?;
            if size < need {
                return invalid(format!("buffer {i} is {size} bytes, needs {need}"));
            }
        }
        Ok(())
    }
}

/// Whether connected clients (that completed hello) want a canvas on each path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Demand {
    pub dmabuf: bool,
    pub shm: bool,
}

impl Demand {
    pub fn any(self) -> bool {
        self.dmabuf || self.shm
    }

    pub fn wants(self, kind: RingKind) -> bool {
        match kind {
            RingKind::Dmabuf => self.dmabuf,
            RingKind::Shm => self.shm,
        }
    }
}

/// Server counters. `clients` counts every connection, `*_clients` those past hello.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ServerStats {
    pub clients: u32,
    pub dmabuf_clients: u32,
    pub shm_clients: u32,
    /// `se_frame` messages sent (one per client per frame).
    pub frames_sent: u64,
    /// Presented frames that reached no client: queue full, invalid/stale buffer, or the
    /// ring was replaced before the server thread handled them.
    pub frames_dropped: u64,
    /// Closed client connections (any cause).
    pub disconnects: u64,
}

/// Tunables of [`FramesServer::start_with`].
#[derive(Debug, Clone)]
pub struct ServerOptions {
    /// A client whose socket keeps unread data (or stays unwritable) this long is
    /// disconnected.
    pub write_timeout: Duration,
    /// Capacity of the render → server present queue (preallocated).
    pub queue_capacity: usize,
}

impl Default for ServerOptions {
    fn default() -> Self {
        ServerOptions { write_timeout: Duration::from_secs(2), queue_capacity: 64 }
    }
}

const RENDERING: u64 = 1;
const PENDING: u64 = 2;
const HELD: u64 = 4;
const NO_BUFFER: u32 = u32::MAX;

/// Kernel send buffer per client (doubled by the kernel): bounds how much a stuck client
/// can queue before sends report WouldBlock.
const CLIENT_SNDBUF: u32 = 32 * 1024;
/// How often stalled clients are re-sampled while nothing else wakes the loop.
const STALL_SAMPLE: Duration = Duration::from_millis(50);
/// Pause after accept() fails for lack of resources.
const ACCEPT_BACKOFF: Duration = Duration::from_millis(100);
/// Messages read from one client per wakeup (fairness).
const MAX_READS_PER_WAKE: usize = 64;

fn word(generation: u32, flags: u64) -> u64 {
    (u64::from(generation) << 32) | flags
}

/// Render-visible state of one (canvas, kind) ring.
struct RingSlot {
    /// 0 = no ring.
    generation: AtomicU32,
    count: AtomicU32,
    /// Most recently presented buffer or [`NO_BUFFER`].
    last: AtomicU32,
    /// Clients (past hello) of this kind that want this canvas.
    demand: AtomicU32,
    bufs: [AtomicU64; MAX_BUFFERS],
}

impl RingSlot {
    fn new() -> RingSlot {
        RingSlot {
            generation: AtomicU32::new(0),
            count: AtomicU32::new(0),
            last: AtomicU32::new(NO_BUFFER),
            demand: AtomicU32::new(0),
            bufs: array::from_fn(|_| AtomicU64::new(0)),
        }
    }

    /// Unpublishes the ring and prepares `count` free buffers of `generation`; the caller
    /// publishes by storing `generation`.
    fn reset(&self, generation: u32, count: usize) {
        self.generation.store(0, Ordering::Release);
        self.count.store(count as u32, Ordering::Relaxed);
        self.last.store(NO_BUFFER, Ordering::Relaxed);
        for (i, b) in self.bufs.iter().enumerate() {
            let w = if i < count { word(generation, 0) } else { 0 };
            b.store(w, Ordering::Release);
        }
    }

    /// Sets/clears flags of `buffer` if it still belongs to `generation`.
    fn update(&self, buffer: usize, generation: u32, set: u64, clear: u64) -> bool {
        let w = &self.bufs[buffer];
        let mut cur = w.load(Ordering::Acquire);
        loop {
            if (cur >> 32) as u32 != generation {
                return false;
            }
            match w.compare_exchange_weak(cur, (cur | set) & !clear, Ordering::AcqRel, Ordering::Acquire) {
                Ok(_) => return true,
                Err(v) => cur = v,
            }
        }
    }
}

struct Shared {
    rings: [[RingSlot; 2]; MAX_CANVASES],
    wake: OwnedFd,
    clients: AtomicU32,
    dmabuf_clients: AtomicU32,
    shm_clients: AtomicU32,
    frames_sent: AtomicU64,
    frames_dropped: AtomicU64,
    disconnects: AtomicU64,
}

impl Shared {
    fn slot(&self, canvas: u32, kind: RingKind) -> Option<&RingSlot> {
        self.rings.get(canvas as usize).map(|pair| &pair[kind.index()])
    }

    fn wake(&self) {
        // Cannot fail short of a closed fd; a saturated counter is already a wakeup.
        let _ = sys::eventfd_signal(self.wake.as_fd());
    }

    fn dropped(&self) {
        self.frames_dropped.fetch_add(1, Ordering::Relaxed);
    }
}

struct Present {
    canvas: u8,
    kind: RingKind,
    buffer: u8,
    generation: u32,
    seq: u64,
    monotonic_ns: u64,
    fence: Option<OwnedFd>,
}

enum Command {
    SetRing {
        canvas: usize,
        kind: RingKind,
        generation: u32,
        desc: RingDesc,
    },
    RemoveRing {
        canvas: usize,
        kind: RingKind,
    },
    DeviceLost,
    Shutdown,
    #[cfg(test)]
    Pause {
        paused: Sender<()>,
        resume: Receiver<()>,
    },
}

/// The `frames.sock` server. `Send + Sync`; share it through an `Arc`.
pub struct FramesServer {
    shared: Arc<Shared>,
    presents: Sender<Present>,
    commands: Sender<Command>,
    /// Per-canvas generation counters; also serializes ring changes.
    generations: Mutex<[u32; MAX_CANVASES]>,
    thread: Option<JoinHandle<()>>,
    path: PathBuf,
}

fn canvas_index(canvas: u32) -> io::Result<usize> {
    if (canvas as usize) < MAX_CANVASES {
        Ok(canvas as usize)
    } else {
        Err(io::Error::new(io::ErrorKind::InvalidInput, format!("canvas id {canvas} out of range")))
    }
}

impl FramesServer {
    /// Binds `path` (see [`sys::bind_seqpacket`]) and starts the `se-frames` thread.
    pub fn start(path: &Path) -> io::Result<FramesServer> {
        FramesServer::start_with(path, ServerOptions::default())
    }

    pub fn start_with(path: &Path, options: ServerOptions) -> io::Result<FramesServer> {
        let listener = sys::bind_seqpacket(path)?;
        let meta = fs::symlink_metadata(path)?;
        let socket_id = (meta.dev(), meta.ino());
        let shared = Arc::new(Shared {
            rings: array::from_fn(|_| array::from_fn(|_| RingSlot::new())),
            wake: sys::eventfd()?,
            clients: AtomicU32::new(0),
            dmabuf_clients: AtomicU32::new(0),
            shm_clients: AtomicU32::new(0),
            frames_sent: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
            disconnects: AtomicU64::new(0),
        });
        let (presents, present_rx) = crossbeam_channel::bounded(options.queue_capacity.max(1));
        let (commands, command_rx) = crossbeam_channel::unbounded();
        let worker = Worker {
            shared: shared.clone(),
            listener,
            presents: present_rx,
            commands: command_rx,
            rings: Default::default(),
            clients: Vec::new(),
            pollfds: Vec::new(),
            // SAFETY: geteuid has no preconditions.
            uid: unsafe { libc::geteuid() },
            path: path.to_path_buf(),
            socket_id,
            accept_paused_until: None,
            next_id: 1,
            stopping: false,
            options,
        };
        let thread = match std::thread::Builder::new().name("se-frames".into()).spawn(move || worker.run()) {
            Ok(t) => t,
            Err(e) => {
                let _ = fs::remove_file(path);
                return Err(e);
            }
        };
        tracing::info!(path = %path.display(), "frames server listening");
        Ok(FramesServer { shared, presents, commands, generations: Mutex::new([0; MAX_CANVASES]), thread: Some(thread), path: path.to_path_buf() })
    }

    /// The socket path this server listens on.
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn command(&self, cmd: Command) -> io::Result<()> {
        self.commands.send(cmd).map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the frames server thread has stopped"))?;
        self.shared.wake();
        Ok(())
    }

    /// Installs (`Some`) or removes (`None`) the `kind` ring of `canvas`.
    ///
    /// `Some` gives the canvas a new generation (one counter per canvas; the first ring is
    /// generation 1), forgets every hold/pending mark of that ring and sends `se_canvas`
    /// (with the fds) to every client of that kind that wants the canvas. `None` sends
    /// `se_goodbye{canvas removed}` to those clients. May allocate; must not race with
    /// `acquire`/`present` on the same ring (call it from the render thread or while the
    /// render thread holds no acquired buffer of that ring).
    pub fn set_ring(&self, canvas: u32, kind: RingKind, ring: Option<RingDesc>) -> io::Result<()> {
        let ci = canvas_index(canvas)?;
        if let Some(desc) = &ring {
            desc.validate(kind)?;
        }
        let slot = &self.shared.rings[ci][kind.index()];
        let mut generations = self.generations.lock();
        match ring {
            Some(desc) => {
                let generation = generations[ci].checked_add(1).ok_or_else(|| io::Error::other("canvas generation counter exhausted"))?;
                generations[ci] = generation;
                slot.reset(generation, desc.fds.len());
                // Enqueued before the generation is published, so the server thread knows
                // the ring before any frame of it can be presented.
                self.command(Command::SetRing { canvas: ci, kind, generation, desc })?;
                slot.generation.store(generation, Ordering::Release);
            }
            None => {
                slot.reset(0, 0);
                self.command(Command::RemoveRing { canvas: ci, kind })?;
            }
        }
        Ok(())
    }

    /// Current generation of a ring (0 = no ring).
    pub fn generation(&self, canvas: u32, kind: RingKind) -> u32 {
        self.shared.slot(canvas, kind).map_or(0, |s| s.generation.load(Ordering::Acquire))
    }

    /// Render thread: whether any client past hello wants `canvas` on each path.
    pub fn demand(&self, canvas: u32) -> Demand {
        let d = |kind| self.shared.slot(canvas, kind).is_some_and(|s| s.demand.load(Ordering::Relaxed) > 0);
        Demand { dmabuf: d(RingKind::Dmabuf), shm: d(RingKind::Shm) }
    }

    /// Render thread: a buffer to render into — not the most recently presented one, not
    /// held by any client, not pending in the server queue and not already acquired —
    /// searched round-robin after the last presented one. `None` when every buffer is busy
    /// or the ring does not exist (skip the export this frame).
    pub fn acquire(&self, canvas: u32, kind: RingKind) -> Option<u32> {
        let slot = self.shared.slot(canvas, kind)?;
        let generation = slot.generation.load(Ordering::Acquire);
        if generation == 0 {
            return None;
        }
        let count = (slot.count.load(Ordering::Relaxed) as usize).min(MAX_BUFFERS);
        let last = slot.last.load(Ordering::Acquire);
        let start = if (last as usize) < count { last as usize + 1 } else { 0 };
        let free = word(generation, 0);
        for k in 0..count {
            let i = (start + k) % count;
            if i as u32 == last {
                continue;
            }
            if slot.bufs[i].compare_exchange(free, free | RENDERING, Ordering::AcqRel, Ordering::Relaxed).is_ok() {
                return Some(i as u32);
            }
        }
        None
    }

    /// Render thread: hands an acquired buffer to the server thread, which sends
    /// `se_frame` (plus `fence`, a sync_file fd, when given) to every client of `kind`
    /// that wants `canvas`. Never blocks: when the queue is full the frame is dropped,
    /// the fence closed, the buffer freed and the drop counted.
    pub fn present(&self, canvas: u32, kind: RingKind, buffer: u32, seq: u64, monotonic_ns: u64, fence: Option<OwnedFd>) {
        let Some(slot) = self.shared.slot(canvas, kind) else {
            self.shared.dropped();
            return;
        };
        let generation = slot.generation.load(Ordering::Acquire);
        let b = buffer as usize;
        if generation == 0 || b >= MAX_BUFFERS {
            self.shared.dropped();
            return;
        }
        let pending = word(generation, PENDING);
        if slot.bufs[b].compare_exchange(word(generation, RENDERING), pending, Ordering::AcqRel, Ordering::Relaxed).is_err() {
            // Not acquired, or the ring was replaced since.
            self.shared.dropped();
            return;
        }
        let item = Present { canvas: canvas as u8, kind, buffer: buffer as u8, generation, seq, monotonic_ns, fence };
        match self.presents.try_send(item) {
            Ok(()) => {
                slot.last.store(buffer, Ordering::Release);
                self.shared.wake();
            }
            Err(TrySendError::Full(item) | TrySendError::Disconnected(item)) => {
                drop(item); // closes the fence
                let _ = slot.bufs[b].compare_exchange(pending, word(generation, 0), Ordering::AcqRel, Ordering::Relaxed);
                self.shared.dropped();
            }
        }
    }

    /// Render thread: returns an acquired buffer without presenting it.
    pub fn abandon(&self, canvas: u32, kind: RingKind, buffer: u32) {
        let Some(slot) = self.shared.slot(canvas, kind) else {
            return;
        };
        let generation = slot.generation.load(Ordering::Acquire);
        if generation == 0 || buffer as usize >= MAX_BUFFERS {
            return;
        }
        let _ = slot.bufs[buffer as usize].compare_exchange(word(generation, RENDERING), word(generation, 0), Ordering::AcqRel, Ordering::Relaxed);
    }

    /// Sends `se_goodbye{device lost, all}` to every client and drops every ring. New
    /// rings (via `set_ring`) get fresh generations and new `se_canvas` messages.
    pub fn device_lost(&self) {
        let _generations = self.generations.lock();
        for pair in &self.shared.rings {
            for slot in pair {
                slot.reset(0, 0);
            }
        }
        if self.command(Command::DeviceLost).is_err() {
            tracing::warn!("device_lost on a stopped frames server");
        }
    }

    pub fn stats(&self) -> ServerStats {
        let s = &self.shared;
        ServerStats {
            clients: s.clients.load(Ordering::Relaxed),
            dmabuf_clients: s.dmabuf_clients.load(Ordering::Relaxed),
            shm_clients: s.shm_clients.load(Ordering::Relaxed),
            frames_sent: s.frames_sent.load(Ordering::Relaxed),
            frames_dropped: s.frames_dropped.load(Ordering::Relaxed),
            disconnects: s.disconnects.load(Ordering::Relaxed),
        }
    }

    /// Sends `se_goodbye{shutdown, all}`, stops the thread and unlinks the socket.
    /// Dropping the server does the same.
    pub fn shutdown(mut self) {
        self.stop();
    }

    fn stop(&mut self) {
        if let Some(thread) = self.thread.take() {
            let _ = self.command(Command::Shutdown);
            if thread.join().is_err() {
                tracing::error!("frames server thread panicked");
            }
        }
    }

    /// Blocks the server thread until the returned sender is dropped.
    #[cfg(test)]
    pub(crate) fn pause(&self) -> Sender<()> {
        let (paused, paused_rx) = crossbeam_channel::bounded(1);
        let (resume_tx, resume) = crossbeam_channel::bounded(0);
        self.command(Command::Pause { paused, resume }).unwrap();
        paused_rx.recv().unwrap();
        resume_tx
    }
}

impl Drop for FramesServer {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------------------
// Server thread

struct Ring {
    generation: u32,
    desc: RingDesc,
    /// Number of clients holding each buffer.
    holds: [u32; MAX_BUFFERS],
}

impl Ring {
    fn canvas_msg(&self, canvas: usize) -> CanvasMsg {
        CanvasMsg {
            canvas: canvas as u32,
            width: self.desc.width,
            height: self.desc.height,
            drm_fourcc: self.desc.drm_fourcc,
            modifier: self.desc.modifier,
            offsets: self.desc.offsets,
            strides: self.desc.strides,
            planes: self.desc.planes,
            buffer_count: self.desc.fds.len() as u32,
            generation: self.generation,
        }
    }
}

type Rings = [[Option<Ring>; 2]; MAX_CANVASES];

#[derive(Debug, Clone, Copy)]
struct HelloState {
    client: u32,
    want: u32,
    kind: RingKind,
}

struct Client {
    id: u64,
    pid: i32,
    fd: OwnedFd,
    hello: Option<HelloState>,
    /// Generation of the last `se_canvas` sent per canvas (for the client's kind); 0 = none.
    sent_gen: [u32; MAX_CANVASES],
    /// Seq of the frame that made the client hold (canvas, buffer) of its kind's ring.
    holds: [[Option<u64>; MAX_BUFFERS]; MAX_CANVASES],
    owe_device_lost: bool,
    /// Canvases owed a `se_goodbye{canvas removed}`.
    owe_removed: u32,
    /// Last send hit a full socket; wait for POLLOUT before sending owed messages.
    blocked: bool,
    /// Since when the socket has had unread data / been unwritable.
    stall_since: Option<Instant>,
    dead: Option<String>,
}

fn client_kind_name(kind: u32) -> &'static str {
    match kind {
        CLIENT_OBS => "obs",
        CLIENT_UI => "ui",
        _ => "other",
    }
}

impl Client {
    fn wants(&self, canvas: usize, kind: RingKind) -> bool {
        self.dead.is_none() && self.hello.is_some_and(|h| h.kind == kind && h.want & (1 << canvas) != 0)
    }

    fn kill(&mut self, reason: impl Into<String>) {
        if self.dead.is_none() {
            self.dead = Some(reason.into());
        }
    }

    /// Sends one record; `true` when it was queued.
    fn send(&mut self, bytes: &[u8], fds: &[RawFd], now: Instant) -> bool {
        if self.dead.is_some() {
            return false;
        }
        match sys::send_msg(self.fd.as_fd(), bytes, fds) {
            Ok(SendOutcome::Sent) => {
                self.blocked = false;
                true
            }
            Ok(SendOutcome::WouldBlock) => {
                self.blocked = true;
                self.stall_since.get_or_insert(now);
                false
            }
            // Too many fds in flight for this user (clients not reading): skip this
            // message, retry owed ones on a later iteration.
            Err(e) if e.raw_os_error() == Some(libc::ETOOMANYREFS) => {
                self.stall_since.get_or_insert(now);
                false
            }
            Err(e) => {
                self.kill(format!("send failed: {e}"));
                false
            }
        }
    }
}

/// Drops `c`'s holds on `canvas` of its current ring kind.
fn drop_holds(rings: &mut Rings, shared: &Shared, c: &mut Client, canvas: usize) {
    let Some(h) = c.hello else {
        return;
    };
    let k = h.kind.index();
    for b in 0..MAX_BUFFERS {
        if c.holds[canvas][b].take().is_some()
            && let Some(ring) = rings[canvas][k].as_mut()
        {
            unhold(ring, &shared.rings[canvas][k], b);
        }
    }
}

fn unhold(ring: &mut Ring, slot: &RingSlot, b: usize) {
    debug_assert!(ring.holds[b] > 0);
    ring.holds[b] = ring.holds[b].saturating_sub(1);
    if ring.holds[b] == 0 {
        slot.update(b, ring.generation, 0, HELD);
    }
}

/// Sends what `c` is owed, in order: device-lost goodbye, canvas-removed goodbyes, then
/// `se_canvas` for every wanted canvas whose ring generation it has not seen. Stops at the
/// first send that does not go through.
fn sync_client(rings: &Rings, c: &mut Client, now: Instant) {
    if c.dead.is_some() || c.blocked {
        return;
    }
    let mut buf = [0u8; MAX_MSG_SIZE];
    if c.owe_device_lost {
        let n = Goodbye { reason: GOODBYE_DEVICE_LOST, canvas: ALL_CANVASES }.encode(&mut buf);
        if !c.send(&buf[..n], &[], now) {
            return;
        }
        c.owe_device_lost = false;
    }
    while c.owe_removed != 0 {
        let canvas = c.owe_removed.trailing_zeros();
        let n = Goodbye { reason: GOODBYE_CANVAS_REMOVED, canvas }.encode(&mut buf);
        if !c.send(&buf[..n], &[], now) {
            return;
        }
        c.owe_removed &= !(1 << canvas);
    }
    let Some(h) = c.hello else {
        return;
    };
    for (canvas, pair) in rings.iter().enumerate() {
        if h.want & (1 << canvas) == 0 {
            continue;
        }
        let Some(ring) = &pair[h.kind.index()] else {
            continue;
        };
        if c.sent_gen[canvas] == ring.generation {
            continue;
        }
        let n = ring.canvas_msg(canvas).encode(&mut buf);
        let mut raw = [0 as RawFd; MAX_BUFFERS];
        for (r, fd) in raw.iter_mut().zip(&ring.desc.fds) {
            *r = fd.as_raw_fd();
        }
        if !c.send(&buf[..n], &raw[..ring.desc.fds.len()], now) {
            return;
        }
        c.sent_gen[canvas] = ring.generation;
        tracing::debug!(
            client = c.id,
            canvas = canvas_name(canvas as u32).unwrap_or("?"),
            kind = h.kind.name(),
            generation = ring.generation,
            "sent se_canvas"
        );
    }
}

struct Worker {
    shared: Arc<Shared>,
    listener: OwnedFd,
    presents: Receiver<Present>,
    commands: Receiver<Command>,
    options: ServerOptions,
    rings: Rings,
    clients: Vec<Client>,
    pollfds: Vec<libc::pollfd>,
    uid: u32,
    path: PathBuf,
    socket_id: (u64, u64),
    accept_paused_until: Option<Instant>,
    next_id: u64,
    stopping: bool,
}

impl Worker {
    fn run(mut self) {
        while !self.stopping {
            let now = Instant::now();
            let timeout = self.poll_timeout(now);
            self.build_pollfds();
            if let Err(e) = sys::poll(&mut self.pollfds, timeout) {
                tracing::error!(error = %e, "frames server poll failed");
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            let now = Instant::now();
            if self.pollfds[1].revents != 0 {
                let _ = sys::eventfd_drain(self.shared.wake.as_fd());
            }
            self.process_commands(now);
            if self.stopping {
                break;
            }
            for ci in 0..self.pollfds.len() - 2 {
                let revents = self.pollfds[2 + ci].revents;
                if revents == 0 {
                    continue;
                }
                if revents & libc::POLLNVAL != 0 {
                    self.clients[ci].kill("socket became invalid");
                    continue;
                }
                if revents & libc::POLLOUT != 0 {
                    self.clients[ci].blocked = false;
                }
                if revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
                    self.read_client(ci);
                }
            }
            if self.accept_paused_until.is_some_and(|t| now >= t) {
                self.accept_paused_until = None;
            }
            if self.pollfds[0].revents & libc::POLLIN != 0 {
                self.accept_clients(now);
            }
            self.sample_stalls(now);
            self.sync_all(now);
            self.process_presents(now);
            self.reap();
        }
        self.finish();
    }

    fn poll_timeout(&self, now: Instant) -> Option<Duration> {
        let mut timeout: Option<Duration> = None;
        if self.clients.iter().any(|c| c.stall_since.is_some()) {
            timeout = Some(STALL_SAMPLE.min(self.options.write_timeout));
        }
        if let Some(until) = self.accept_paused_until {
            let d = until.saturating_duration_since(now);
            timeout = Some(timeout.map_or(d, |t| t.min(d)));
        }
        timeout
    }

    fn build_pollfds(&mut self) {
        self.pollfds.clear();
        let listener = if self.accept_paused_until.is_some() {
            -1 // poll ignores negative fds
        } else {
            self.listener.as_raw_fd()
        };
        self.pollfds.push(libc::pollfd { fd: listener, events: libc::POLLIN, revents: 0 });
        self.pollfds.push(libc::pollfd { fd: self.shared.wake.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        for c in &self.clients {
            let mut events = libc::POLLIN;
            if c.blocked {
                events |= libc::POLLOUT;
            }
            self.pollfds.push(libc::pollfd { fd: c.fd.as_raw_fd(), events, revents: 0 });
        }
    }

    fn process_commands(&mut self, now: Instant) {
        loop {
            match self.commands.try_recv() {
                Ok(cmd) => self.apply(cmd, now),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    self.stopping = true;
                    break;
                }
            }
        }
    }

    fn apply(&mut self, cmd: Command, _now: Instant) {
        match cmd {
            Command::SetRing { canvas, kind, generation, desc } => {
                tracing::info!(
                    canvas = canvas_name(canvas as u32).unwrap_or("?"),
                    kind = kind.name(),
                    generation,
                    width = desc.width,
                    height = desc.height,
                    fourcc = format_args!("{:#010x}", desc.drm_fourcc),
                    modifier = format_args!("{:#x}", desc.modifier),
                    buffers = desc.fds.len(),
                    "frames ring set"
                );
                for c in &mut self.clients {
                    if c.hello.is_some_and(|h| h.kind == kind) {
                        c.holds[canvas] = [None; MAX_BUFFERS];
                    }
                }
                self.rings[canvas][kind.index()] = Some(Ring { generation, desc, holds: [0; MAX_BUFFERS] });
            }
            Command::RemoveRing { canvas, kind } => {
                if self.rings[canvas][kind.index()].take().is_none() {
                    return;
                }
                tracing::info!(canvas = canvas_name(canvas as u32).unwrap_or("?"), kind = kind.name(), "frames ring removed");
                for c in &mut self.clients {
                    if c.hello.is_some_and(|h| h.kind == kind) {
                        c.holds[canvas] = [None; MAX_BUFFERS];
                        c.sent_gen[canvas] = 0;
                        if c.wants(canvas, kind) {
                            c.owe_removed |= 1 << canvas;
                        }
                    }
                }
            }
            Command::DeviceLost => {
                tracing::warn!("frames: device lost, dropping every ring");
                self.rings = Default::default();
                for c in &mut self.clients {
                    c.holds = [[None; MAX_BUFFERS]; MAX_CANVASES];
                    c.sent_gen = [0; MAX_CANVASES];
                    c.owe_removed = 0;
                    c.owe_device_lost = true;
                }
            }
            Command::Shutdown => self.stopping = true,
            #[cfg(test)]
            Command::Pause { paused, resume } => {
                let _ = paused.send(());
                let _ = resume.recv();
            }
        }
    }

    fn sync_all(&mut self, now: Instant) {
        for c in &mut self.clients {
            sync_client(&self.rings, c, now);
        }
    }

    fn accept_clients(&mut self, now: Instant) {
        loop {
            match sys::accept(self.listener.as_fd()) {
                Ok(Some(fd)) => self.admit(fd),
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "accepting a frames client failed");
                    if matches!(e.raw_os_error(), Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)) {
                        self.accept_paused_until = Some(now + ACCEPT_BACKOFF);
                    }
                    break;
                }
            }
        }
    }

    fn admit(&mut self, fd: OwnedFd) {
        let cred = match sys::peer_cred(fd.as_fd()) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "frames client without peer credentials rejected");
                return;
            }
        };
        if cred.uid != self.uid {
            tracing::warn!(pid = cred.pid, uid = cred.uid, "frames client of another user rejected");
            return;
        }
        if let Err(e) = sys::set_send_buffer(fd.as_fd(), CLIENT_SNDBUF) {
            tracing::debug!(error = %e, "setting SO_SNDBUF failed");
        }
        let id = self.next_id;
        self.next_id += 1;
        tracing::info!(client = id, pid = cred.pid, uid = cred.uid, "frames client connected");
        self.clients.push(Client {
            id,
            pid: cred.pid,
            fd,
            hello: None,
            sent_gen: [0; MAX_CANVASES],
            holds: [[None; MAX_BUFFERS]; MAX_CANVASES],
            owe_device_lost: false,
            owe_removed: 0,
            blocked: false,
            stall_since: None,
            dead: None,
        });
        self.recount();
    }

    fn read_client(&mut self, ci: usize) {
        let mut buf = [0u8; 2 * MAX_MSG_SIZE];
        for _ in 0..MAX_READS_PER_WAKE {
            let c = &mut self.clients[ci];
            if c.dead.is_some() {
                return;
            }
            match sys::recv_msg(c.fd.as_fd(), &mut buf, None) {
                Ok(RecvOutcome::Message { len, truncated }) => {
                    if truncated {
                        self.violation(ci, "oversized message or unexpected fds".into());
                        return;
                    }
                    self.handle(ci, &buf[..len]);
                }
                Ok(RecvOutcome::WouldBlock) => return,
                Ok(RecvOutcome::Closed) => {
                    c.kill("closed by client");
                    return;
                }
                Err(e) => {
                    c.kill(format!("receive failed: {e}"));
                    return;
                }
            }
        }
    }

    fn violation(&mut self, ci: usize, reason: String) {
        let c = &mut self.clients[ci];
        tracing::warn!(client = c.id, pid = c.pid, %reason, "frames protocol violation");
        c.kill(format!("protocol violation: {reason}"));
    }

    fn handle(&mut self, ci: usize, bytes: &[u8]) {
        let result = match proto::peek_type(bytes) {
            Ok(MSG_HELLO) => Hello::decode(bytes).map_err(|e| e.to_string()).and_then(|h| self.on_hello(ci, h)),
            Ok(MSG_RELEASE) => Release::decode(bytes).map_err(|e| e.to_string()).and_then(|r| self.on_release(ci, r)),
            Ok(ty) => Err(format!("unexpected message type {ty}")),
            Err(e) => Err(e.to_string()),
        };
        if let Err(reason) = result {
            self.violation(ci, reason);
        }
    }

    fn on_hello(&mut self, ci: usize, hello: Hello) -> Result<(), String> {
        if !(CLIENT_OBS..=CLIENT_OTHER).contains(&hello.client) {
            return Err(format!("unknown client kind {}", hello.client));
        }
        if hello.want & !ALL_CANVASES_MASK != 0 {
            return Err(format!("want mask {:#x} names unknown canvases", hello.want));
        }
        let new = HelloState { client: hello.client, want: hello.want, kind: RingKind::from_hello_flags(hello.flags) };
        let c = &mut self.clients[ci];
        if let Some(old) = c.hello {
            for canvas in 0..MAX_CANVASES {
                let bit = 1 << canvas;
                let was = old.want & bit != 0;
                let keeps = was && old.kind == new.kind && new.want & bit != 0;
                if was && !keeps {
                    drop_holds(&mut self.rings, &self.shared, c, canvas);
                }
                if !keeps {
                    c.sent_gen[canvas] = 0;
                }
            }
            if old.kind != new.kind {
                // Goodbyes for rings of the old kind no longer concern this client.
                c.owe_removed = 0;
            }
        }
        c.hello = Some(new);
        tracing::info!(
            client = c.id,
            pid = c.pid,
            kind = client_kind_name(new.client),
            want = format_args!("{:#06b}", new.want),
            path = new.kind.name(),
            "frames client hello"
        );
        self.recount();
        Ok(())
    }

    fn on_release(&mut self, ci: usize, r: Release) -> Result<(), String> {
        let (canvas, b) = (r.canvas as usize, r.buffer as usize);
        if canvas >= MAX_CANVASES || b >= MAX_BUFFERS {
            return Err(format!("release of canvas {} buffer {} is out of range", r.canvas, r.buffer));
        }
        let c = &mut self.clients[ci];
        let Some(h) = c.hello else {
            tracing::debug!(client = c.id, "release before hello ignored");
            return Ok(());
        };
        if c.holds[canvas][b] == Some(r.seq) {
            c.holds[canvas][b] = None;
            let k = h.kind.index();
            if let Some(ring) = self.rings[canvas][k].as_mut() {
                unhold(ring, &self.shared.rings[canvas][k], b);
            }
        } else {
            tracing::trace!(client = c.id, canvas, buffer = b, seq = r.seq, "stale release ignored");
        }
        Ok(())
    }

    /// Samples unread bytes of every client (before this iteration's frames are sent) and
    /// disconnects clients that stopped reading for longer than the write timeout.
    fn sample_stalls(&mut self, now: Instant) {
        for c in &mut self.clients {
            if c.dead.is_some() {
                continue;
            }
            match sys::unread_bytes(c.fd.as_fd()) {
                Ok(0) if !c.blocked => c.stall_since = None,
                Ok(_) => {
                    c.stall_since.get_or_insert(now);
                }
                Err(e) => {
                    c.kill(format!("SIOCOUTQ failed: {e}"));
                    continue;
                }
            }
            if let Some(since) = c.stall_since {
                let stalled = now.duration_since(since);
                if stalled > self.options.write_timeout {
                    tracing::warn!(client = c.id, pid = c.pid, stalled_ms = stalled.as_millis() as u64, "frames client stopped reading");
                    c.kill(format!("not reading for {} ms", stalled.as_millis()));
                }
            }
        }
    }

    fn process_presents(&mut self, now: Instant) {
        while !self.stopping {
            match self.presents.try_recv() {
                Ok(p) => self.deliver(p, now),
                Err(_) => break,
            }
        }
    }

    fn deliver(&mut self, p: Present, now: Instant) {
        let (canvas, k, b) = (p.canvas as usize, p.kind.index(), p.buffer as usize);
        if self.rings[canvas][k].as_ref().map(|r| r.generation) != Some(p.generation) {
            // set_ring enqueues its command before publishing the generation: apply it.
            self.process_commands(now);
            self.sync_all(now);
        }
        let slot = &self.shared.rings[canvas][k];
        let Some(ring) = self.rings[canvas][k].as_mut().filter(|r| r.generation == p.generation) else {
            slot.update(b, p.generation, 0, PENDING);
            self.shared.dropped();
            return;
        };
        let mut buf = [0u8; FrameMsg::SIZE];
        FrameMsg { canvas: canvas as u32, buffer: b as u32, seq: p.seq, monotonic_ns: p.monotonic_ns, generation: p.generation, has_fence: p.fence.is_some() }
            .encode(&mut buf);
        let fence = p.fence.as_ref().map(|f| [f.as_raw_fd()]);
        let fds: &[RawFd] = fence.as_ref().map_or(&[], |f| &f[..]);
        let mut sent = 0;
        for c in &mut self.clients {
            if !c.wants(canvas, p.kind) || c.sent_gen[canvas] != p.generation {
                continue;
            }
            if c.send(&buf, fds, now) {
                sent += 1;
                if c.holds[canvas][b].replace(p.seq).is_none() {
                    ring.holds[b] += 1;
                }
            }
        }
        self.shared.frames_sent.fetch_add(sent, Ordering::Relaxed);
        // HELD is set in the same step that clears PENDING, so the buffer is never
        // observably free while a client holds it.
        let set = if ring.holds[b] > 0 { HELD } else { 0 };
        slot.update(b, p.generation, set, PENDING);
        // `p.fence` closes here.
    }

    fn reap(&mut self) {
        let mut changed = false;
        let mut i = 0;
        while i < self.clients.len() {
            if self.clients[i].dead.is_none() {
                i += 1;
                continue;
            }
            let mut c = self.clients.swap_remove(i);
            for canvas in 0..MAX_CANVASES {
                drop_holds(&mut self.rings, &self.shared, &mut c, canvas);
            }
            tracing::info!(client = c.id, pid = c.pid, reason = c.dead.as_deref().unwrap_or(""), "frames client disconnected");
            self.shared.disconnects.fetch_add(1, Ordering::Relaxed);
            changed = true;
        }
        if changed {
            self.recount();
        }
    }

    /// Recomputes client counts and per-ring demand.
    fn recount(&self) {
        let mut demand = [[0u32; 2]; MAX_CANVASES];
        let mut per_kind = [0u32; 2];
        let mut live = 0;
        for c in self.clients.iter().filter(|c| c.dead.is_none()) {
            live += 1;
            let Some(h) = c.hello else {
                continue;
            };
            per_kind[h.kind.index()] += 1;
            for (canvas, d) in demand.iter_mut().enumerate() {
                if h.want & (1 << canvas) != 0 {
                    d[h.kind.index()] += 1;
                }
            }
        }
        for (pair, d) in self.shared.rings.iter().zip(demand) {
            for (slot, n) in pair.iter().zip(d) {
                slot.demand.store(n, Ordering::Relaxed);
            }
        }
        let s = &self.shared;
        s.clients.store(live, Ordering::Relaxed);
        s.dmabuf_clients.store(per_kind[RingKind::Dmabuf.index()], Ordering::Relaxed);
        s.shm_clients.store(per_kind[RingKind::Shm.index()], Ordering::Relaxed);
    }

    fn finish(&mut self) {
        let mut buf = [0u8; Goodbye::SIZE];
        let n = Goodbye { reason: GOODBYE_SHUTDOWN, canvas: ALL_CANVASES }.encode(&mut buf);
        for c in &self.clients {
            if c.dead.is_none() {
                // Best effort: a full socket just misses the goodbye and sees EOF.
                let _ = sys::send_msg(c.fd.as_fd(), &buf[..n], &[]);
            }
        }
        self.clients.clear();
        self.rings = Default::default();
        self.recount();
        match fs::symlink_metadata(&self.path) {
            Ok(meta) if (meta.dev(), meta.ino()) == self.socket_id => {
                if let Err(e) = fs::remove_file(&self.path) {
                    tracing::warn!(error = %e, path = %self.path.display(), "unlinking frames socket failed");
                }
            }
            _ => {}
        }
        tracing::info!(path = %self.path.display(), "frames server stopped");
    }
}

#[cfg(test)]
mod tests;
