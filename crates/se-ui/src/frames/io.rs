//! The frames.sock IO thread: connects with backoff, sends hello/release, receives engine
//! messages with their `SCM_RIGHTS` descriptors and publishes them into [`Shared`] mailboxes
//! (bounded: one pending canvas description and one pending frame per canvas).
//!
//! Ownership bookkeeping ("leases"): every hold the engine attributes to this client belongs to
//! the canvas lease current when the frame was received. A lease ends when the connection
//! (re)starts, a canvas is re-described, a goodbye arrives, the canvas stops being wanted, or the
//! IO thread pauses delivery because the UI stopped updating. Releases carrying an ended lease are
//! dropped — the engine has already forgotten those holds.

use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};

use super::proto::{self, CanvasDesc, EngineMsg};

pub(crate) const BACKOFF_MIN: Duration = Duration::from_millis(100);
pub(crate) const BACKOFF_MAX: Duration = Duration::from_secs(2);
/// Delivery is paused (hello with an empty want mask) when the UI has not called `update()` for
/// this long while frames keep arriving — e.g. the window is hidden and not repainting. The
/// engine then releases everything this client held, so a frozen UI never starves other clients.
pub(crate) const SUSPEND_AFTER: Duration = Duration::from_secs(1);
const SEND_TIMEOUT: Duration = Duration::from_millis(500);
/// Messages handled per poll wakeup before commands get a turn.
const READ_BATCH: usize = 64;
/// Receive buffer: every valid message is at most `CANVAS_LEN`; anything longer is truncated
/// and rejected.
const RECV_BUF: usize = 256;
/// Control buffer (u64s for alignment): room for 32 descriptors.
const CMSG_WORDS: usize = 24;

pub(crate) fn bit(canvas: usize) -> u32 {
    1 << canvas
}

/// Opens the transport to the engine; production connects to a socket path.
pub(crate) trait Connector: Send + 'static {
    fn connect(&mut self) -> io::Result<OwnedFd>;
}

pub(crate) struct PathConnector(pub PathBuf);

impl Connector for PathConnector {
    fn connect(&mut self) -> io::Result<OwnedFd> {
        connect_seqpacket(&self.0)
    }
}

fn cvt(ret: libc::c_int) -> io::Result<libc::c_int> {
    if ret < 0 { Err(io::Error::last_os_error()) } else { Ok(ret) }
}

/// Connects a `SOCK_SEQPACKET` Unix socket to `path` (close-on-exec, bounded send timeout).
pub(crate) fn connect_seqpacket(path: &Path) -> io::Result<OwnedFd> {
    let bytes = path.as_os_str().as_bytes();
    // SAFETY: sockaddr_un is plain old data; all-zero is a valid value.
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    if bytes.len() >= addr.sun_path.len() || bytes.contains(&0) {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("unusable socket path {}", path.display())));
    }
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (dst, src) in addr.sun_path.iter_mut().zip(bytes) {
        *dst = *src as libc::c_char;
    }
    // SAFETY: plain socket(2) call; the result is checked and wrapped immediately.
    let fd = unsafe { OwnedFd::from_raw_fd(cvt(libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0))?) };
    let len = std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1;
    // SAFETY: addr is a valid sockaddr_un and len does not exceed its size.
    cvt(unsafe { libc::connect(fd.as_raw_fd(), (&raw const addr).cast(), len as libc::socklen_t) })?;
    set_send_timeout(fd.as_fd(), SEND_TIMEOUT)?;
    Ok(fd)
}

pub(crate) fn set_send_timeout(fd: BorrowedFd<'_>, timeout: Duration) -> io::Result<()> {
    let tv = libc::timeval { tv_sec: timeout.as_secs() as libc::time_t, tv_usec: timeout.subsec_micros() as libc::suseconds_t };
    // SAFETY: tv outlives the call and the length matches its type.
    cvt(unsafe {
        libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDTIMEO, (&raw const tv).cast(), size_of::<libc::timeval>() as libc::socklen_t)
    })?;
    Ok(())
}

/// Sends one datagram (no descriptors).
pub(crate) fn send_msg(fd: BorrowedFd<'_>, bytes: &[u8]) -> io::Result<()> {
    loop {
        // SAFETY: bytes is a valid readable buffer of the given length.
        let n = unsafe { libc::send(fd.as_raw_fd(), bytes.as_ptr().cast(), bytes.len(), libc::MSG_NOSIGNAL) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e);
        }
        if n as usize != bytes.len() {
            return Err(io::Error::new(io::ErrorKind::WriteZero, "short send on frames.sock"));
        }
        return Ok(());
    }
}

pub(crate) struct Received {
    pub len: usize,
    pub fds: Vec<OwnedFd>,
    /// Data or control data did not fit: the message is invalid (descriptors that did not fit
    /// were never installed by the kernel).
    pub truncated: bool,
}

/// Receives one datagram without blocking. `Ok(None)` = nothing queued.
pub(crate) fn recv_msg(fd: BorrowedFd<'_>, buf: &mut [u8]) -> io::Result<Option<Received>> {
    let mut cmsg = [0u64; CMSG_WORDS];
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    // SAFETY: msghdr is plain old data; all-zero is a valid value.
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = &raw mut iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsg.as_mut_ptr().cast();
    msg.msg_controllen = size_of_val(&cmsg) as _;
    let n = loop {
        // SAFETY: msg points at live buffers for the duration of the call.
        let n = unsafe { libc::recvmsg(fd.as_raw_fd(), &raw mut msg, libc::MSG_CMSG_CLOEXEC | libc::MSG_DONTWAIT) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        match e.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => return Ok(None),
            _ => return Err(e),
        }
    };
    let mut fds = Vec::new();
    // SAFETY: the kernel filled msg_control/msg_controllen; CMSG_* walk only within it, and every
    // SCM_RIGHTS descriptor is a fresh fd owned by this process, wrapped exactly once.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&raw const msg);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let data = libc::CMSG_DATA(c);
                let payload = (*c).cmsg_len as usize - libc::CMSG_LEN(0) as usize;
                for i in 0..payload / size_of::<libc::c_int>() {
                    let raw = std::ptr::read_unaligned(data.add(i * size_of::<libc::c_int>()).cast::<libc::c_int>());
                    fds.push(OwnedFd::from_raw_fd(raw));
                }
            }
            c = libc::CMSG_NXTHDR(&raw const msg, c);
        }
    }
    let truncated = msg.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0;
    Ok(Some(Received { len: n, fds, truncated }))
}

/// A se_canvas waiting for the UI thread to import it.
pub(crate) struct NewCanvas {
    pub lease: u64,
    pub epoch: u64,
    pub desc: CanvasDesc,
    pub fds: Vec<OwnedFd>,
}

/// The newest se_frame of a canvas not yet taken by the UI thread (the engine counts it as held).
pub(crate) struct MailFrame {
    pub lease: u64,
    pub frame: proto::Frame,
    pub fence: Option<Arc<OwnedFd>>,
}

#[derive(Default)]
pub(crate) struct Mailbox {
    pub lease: u64,
    pub desc: Option<NewCanvas>,
    pub frame: Option<MailFrame>,
    /// Goodbye reason received for this canvas since the UI last looked.
    pub goodbye: Option<u32>,
    pub frames_received: u64,
    /// Frames replaced by a newer one before the UI took them (released immediately).
    pub frames_superseded: u64,
    /// Frames of a stale generation or for a canvas without a description (ignored).
    pub frames_stale: u64,
    pub last_seq: Option<u64>,
}

pub(crate) struct SharedState {
    pub connected: bool,
    /// Time of the last connect/disconnect transition.
    pub changed_at: Instant,
    /// Number of successful connections so far.
    pub epoch: u64,
    /// Delivery was paused because the UI stopped updating; the UI must drop its textures
    /// (the engine may now overwrite those buffers) and send its hello again.
    pub suspended: bool,
    pub last_error: Option<String>,
    pub canvases: [Mailbox; 4],
}

pub(crate) struct Shared {
    state: Mutex<SharedState>,
    start: Instant,
    last_update_ms: AtomicU64,
    pub releases_sent: AtomicU64,
    pub hellos_sent: AtomicU64,
    pub reconnects: AtomicU64,
}

impl Shared {
    pub fn new(now: Instant) -> Self {
        Self {
            state: Mutex::new(SharedState { connected: false, changed_at: now, epoch: 0, suspended: false, last_error: None, canvases: Default::default() }),
            start: now,
            last_update_ms: AtomicU64::new(0),
            releases_sent: AtomicU64::new(0),
            hellos_sent: AtomicU64::new(0),
            reconnects: AtomicU64::new(0),
        }
    }

    pub fn lock(&self) -> MutexGuard<'_, SharedState> {
        // A panic while holding the lock leaves plain data behind; keep going with it.
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Records that the UI thread ran `update()` at `now`.
    pub fn touch(&self, now: Instant) {
        let ms = now.saturating_duration_since(self.start).as_millis() as u64;
        self.last_update_ms.store(ms, Ordering::Relaxed);
    }

    fn since_update(&self, now: Instant) -> Duration {
        let last = self.start + Duration::from_millis(self.last_update_ms.load(Ordering::Relaxed));
        now.saturating_duration_since(last)
    }

    pub fn set_error(&self, msg: String) {
        tracing::warn!(target: "se_ui::frames", "{msg}");
        self.lock().last_error = Some(msg);
    }
}

pub(crate) enum Cmd {
    Hello {
        want: u32,
        flags: u32,
    },
    /// Max frame rate the UI asked for, per canvas (drives repaint throttling).
    Rates([f32; 4]),
    Release {
        canvas: u32,
        buffer: u32,
        seq: u64,
        lease: u64,
    },
    Shutdown,
}

/// Command sender for the IO thread; also usable from GPU completion callbacks.
#[derive(Clone)]
pub(crate) struct IoTx {
    tx: Sender<Cmd>,
    wake: Arc<OwnedFd>,
}

impl IoTx {
    pub fn send(&self, cmd: Cmd) {
        if self.tx.send(cmd).is_ok() {
            let one = 1u64;
            // SAFETY: writes 8 bytes from a live u64 into our eventfd; EAGAIN (counter saturated)
            // still leaves the fd readable, which is all that matters.
            unsafe { libc::write(self.wake.as_raw_fd(), (&raw const one).cast(), 8) };
        }
    }
}

pub(crate) fn spawn(connector: Box<dyn Connector>, shared: Arc<Shared>, ctx: egui::Context) -> io::Result<(IoTx, JoinHandle<()>)> {
    // SAFETY: plain eventfd(2) call; checked and wrapped immediately.
    let wake = Arc::new(unsafe { OwnedFd::from_raw_fd(cvt(libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK))?) });
    let (tx, rx) = crossbeam_channel::unbounded();
    let io = Io {
        connector,
        shared,
        rx,
        wake: wake.clone(),
        ctx,
        desired: (0, 0),
        rates: [0.0; 4],
        next_repaint: [None; 4],
        conn: None,
        watch: Default::default(),
        backoff: BACKOFF_MIN,
        retry_in: None,
        last_connect_error: None,
    };
    let handle = std::thread::Builder::new().name("se-frames-io".into()).spawn(move || io.run())?;
    Ok((IoTx { tx, wake }, handle))
}

struct Conn {
    fd: OwnedFd,
    /// (want, flags) of the last hello sent on this connection.
    sent: (u32, u32),
    /// (generation, buffer_count) of the canvases described on this connection.
    canvases: [Option<(u32, u32)>; 4],
}

struct Io {
    connector: Box<dyn Connector>,
    shared: Arc<Shared>,
    rx: Receiver<Cmd>,
    wake: Arc<OwnedFd>,
    ctx: egui::Context,
    /// (want, flags) most recently requested by the UI thread.
    desired: (u32, u32),
    rates: [f32; 4],
    next_repaint: [Option<Instant>; 4],
    conn: Option<Conn>,
    /// Fence of the newest pending frame per canvas: wakes the UI once it signals.
    watch: [Option<Arc<OwnedFd>>; 4],
    backoff: Duration,
    retry_in: Option<Duration>,
    last_connect_error: Option<io::ErrorKind>,
}

impl Io {
    fn run(mut self) {
        loop {
            if self.conn.is_none() {
                if let Some(wait) = self.retry_in.take()
                    && !self.idle(wait)
                {
                    return;
                }
                match self.connector.connect() {
                    Ok(fd) => self.on_connect(fd),
                    Err(e) => {
                        if self.last_connect_error != Some(e.kind()) {
                            self.last_connect_error = Some(e.kind());
                            tracing::debug!(target: "se_ui::frames", "frames.sock connect failed: {e}");
                        }
                        self.retry_in = Some(self.backoff);
                        self.backoff = (self.backoff * 2).min(BACKOFF_MAX);
                    }
                }
                continue;
            }
            if !self.pump() {
                return;
            }
        }
    }

    /// Waits `wait` while disconnected, still serving commands. Returns false on shutdown.
    fn idle(&mut self, wait: Duration) -> bool {
        let deadline = Instant::now() + wait;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return true;
            }
            let mut pfd = [libc::pollfd { fd: self.wake.as_raw_fd(), events: libc::POLLIN, revents: 0 }];
            let ms = left.as_millis().clamp(1, i32::MAX as u128) as libc::c_int;
            // SAFETY: pfd is a valid array of one pollfd.
            let n = unsafe { libc::poll(pfd.as_mut_ptr(), 1, ms) };
            if n > 0 && !self.drain_cmds() {
                return false;
            }
        }
    }

    /// One poll round while connected. Returns false on shutdown.
    fn pump(&mut self) -> bool {
        let Some(conn) = &self.conn else { return true };
        let mut pfds = Vec::with_capacity(6);
        pfds.push(libc::pollfd { fd: conn.fd.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        pfds.push(libc::pollfd { fd: self.wake.as_raw_fd(), events: libc::POLLIN, revents: 0 });
        let mut watched = Vec::with_capacity(4);
        for (c, w) in self.watch.iter().enumerate() {
            if let Some(fence) = w {
                pfds.push(libc::pollfd { fd: fence.as_raw_fd(), events: libc::POLLIN, revents: 0 });
                watched.push(c);
            }
        }
        // SAFETY: pfds is a valid pollfd array; every fd in it is kept open by self.
        let n = unsafe { libc::poll(pfds.as_mut_ptr(), pfds.len() as libc::nfds_t, -1) };
        if n <= 0 {
            return true; // EINTR
        }
        if pfds[1].revents != 0 && !self.drain_cmds() {
            return false;
        }
        for (i, &c) in watched.iter().enumerate() {
            if pfds[2 + i].revents != 0 {
                self.watch[c] = None;
                self.repaint_throttled(c);
            }
        }
        if pfds[0].revents != 0 {
            self.read_all();
        }
        true
    }

    fn drain_cmds(&mut self) -> bool {
        let mut counter = 0u64;
        // SAFETY: reads 8 bytes into a live u64 from our non-blocking eventfd.
        unsafe { libc::read(self.wake.as_raw_fd(), (&raw mut counter).cast(), 8) };
        loop {
            match self.rx.try_recv() {
                Ok(Cmd::Shutdown) | Err(TryRecvError::Disconnected) => return false,
                Err(TryRecvError::Empty) => return true,
                Ok(Cmd::Hello { want, flags }) => {
                    self.desired = (want, flags);
                    self.sync_hello();
                }
                Ok(Cmd::Rates(rates)) => self.rates = rates,
                Ok(Cmd::Release { canvas, buffer, seq, lease }) => {
                    let current = self.shared.lock().canvases.get(canvas as usize).is_some_and(|m| m.lease == lease);
                    if current && self.conn.is_some() {
                        self.send_release(canvas, buffer, seq);
                    }
                }
            }
        }
    }

    fn send(&mut self, bytes: &[u8]) -> bool {
        let Some(conn) = &self.conn else { return false };
        match send_msg(conn.fd.as_fd(), bytes) {
            Ok(()) => true,
            Err(e) => {
                self.disconnect(Some(format!("frames.sock send failed: {e}")));
                false
            }
        }
    }

    fn send_release(&mut self, canvas: u32, buffer: u32, seq: u64) {
        if self.send(&proto::Release { canvas, buffer, seq }.encode()) {
            self.shared.releases_sent.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn send_hello(&mut self, want: u32, flags: u32) {
        if self.send(&proto::Hello { client: proto::CLIENT_UI, want, flags }.encode()) {
            self.shared.hellos_sent.fetch_add(1, Ordering::Relaxed);
            if let Some(conn) = &mut self.conn {
                conn.sent = (want, flags);
            }
        }
    }

    /// Ends the leases of `canvases` (bitmask): pending descriptions/frames are dropped without
    /// release because the engine no longer counts them as held by us.
    fn end_leases(&mut self, canvases: u32) {
        let mut st = self.shared.lock();
        for c in 0..4 {
            if canvases & bit(c) != 0 {
                let m = &mut st.canvases[c];
                m.lease += 1;
                m.desc = None;
                m.frame = None;
                self.watch[c] = None;
                if let Some(conn) = &mut self.conn {
                    conn.canvases[c] = None;
                }
            }
        }
    }

    /// Sends a hello when the UI's desired (want, flags) differs from what the engine has.
    fn sync_hello(&mut self) {
        let Some(conn) = &self.conn else { return };
        let (sent_want, _) = conn.sent;
        if conn.sent == self.desired {
            return;
        }
        let (want, flags) = self.desired;
        // Canvases no longer wanted are implicitly released by the engine.
        self.end_leases(sent_want & !want);
        self.send_hello(want, flags);
    }

    fn on_connect(&mut self, fd: OwnedFd) {
        let now = Instant::now();
        {
            let mut st = self.shared.lock();
            if st.epoch > 0 {
                self.shared.reconnects.fetch_add(1, Ordering::Relaxed);
            }
            st.epoch += 1;
            st.connected = true;
            st.changed_at = now;
            tracing::info!(target: "se_ui::frames", epoch = st.epoch, "connected to frames.sock");
        }
        self.conn = Some(Conn { fd, sent: (u32::MAX, u32::MAX), canvases: [None; 4] });
        self.end_leases(0b1111);
        self.backoff = BACKOFF_MIN;
        self.last_connect_error = None;
        let (want, flags) = self.desired;
        self.send_hello(want, flags);
        self.ctx.request_repaint();
    }

    fn disconnect(&mut self, error: Option<String>) {
        if self.conn.take().is_none() {
            return;
        }
        self.end_leases(0b1111);
        {
            let mut st = self.shared.lock();
            st.connected = false;
            st.changed_at = Instant::now();
            if let Some(e) = &error {
                st.last_error = Some(e.clone());
            }
        }
        match error {
            Some(e) => tracing::warn!(target: "se_ui::frames", "{e}"),
            None => tracing::info!(target: "se_ui::frames", "frames.sock closed by the engine"),
        }
        self.retry_in = Some(BACKOFF_MIN);
        self.backoff = BACKOFF_MIN * 2;
        self.ctx.request_repaint();
    }

    fn read_all(&mut self) {
        let mut buf = [0u8; RECV_BUF];
        for _ in 0..READ_BATCH {
            let Some(conn) = &self.conn else { return };
            match recv_msg(conn.fd.as_fd(), &mut buf) {
                Ok(None) => return,
                Ok(Some(r)) if r.len == 0 && !r.truncated => {
                    self.disconnect(None);
                    return;
                }
                Ok(Some(r)) => {
                    if r.truncated {
                        self.shared.set_error(format!("frames.sock: dropped an oversized message ({} fds attached)", r.fds.len()));
                        continue;
                    }
                    match proto::decode_engine(&buf[..r.len], r.fds) {
                        Ok(msg) => self.handle(msg),
                        Err(e) => self.shared.set_error(format!("frames.sock: malformed message: {e}")),
                    }
                }
                Err(e) => {
                    self.disconnect(Some(format!("frames.sock receive failed: {e}")));
                    return;
                }
            }
        }
    }

    fn wanted(&self, canvas: usize) -> bool {
        self.conn.as_ref().is_some_and(|c| c.sent.0 & bit(canvas) != 0)
    }

    fn handle(&mut self, msg: EngineMsg) {
        let now = Instant::now();
        match msg {
            EngineMsg::Canvas(desc, fds) => {
                let c = desc.canvas as usize;
                if !self.wanted(c) {
                    return; // raced with a hello that dropped it; fds close here
                }
                if let Some(conn) = &mut self.conn {
                    conn.canvases[c] = Some((desc.generation, desc.buffer_count));
                }
                self.watch[c] = None;
                {
                    let mut st = self.shared.lock();
                    let epoch = st.epoch;
                    let m = &mut st.canvases[c];
                    m.lease += 1;
                    m.frame = None;
                    m.goodbye = None;
                    m.desc = Some(NewCanvas { lease: m.lease, epoch, desc, fds });
                }
                self.ctx.request_repaint();
            }
            EngineMsg::Frame(frame, fence) => {
                let c = frame.canvas as usize;
                if !self.wanted(c) {
                    return;
                }
                let described = self.conn.as_ref().and_then(|conn| conn.canvases[c]);
                let valid = match described {
                    Some((generation, count)) if generation == frame.generation => {
                        if frame.buffer >= count {
                            self.shared.set_error(format!("frames.sock: frame for buffer {} of a {count}-buffer canvas", frame.buffer));
                            return;
                        }
                        true
                    }
                    _ => false,
                };
                if self.shared.since_update(now) > SUSPEND_AFTER {
                    self.suspend();
                    return;
                }
                let fence = fence.map(Arc::new);
                let superseded = {
                    let mut st = self.shared.lock();
                    let m = &mut st.canvases[c];
                    m.frames_received += 1;
                    if !valid {
                        m.frames_stale += 1;
                        return;
                    }
                    m.last_seq = Some(frame.seq);
                    let old = m.frame.replace(MailFrame { lease: m.lease, frame, fence: fence.clone() });
                    let old = old.filter(|o| o.frame.buffer != frame.buffer);
                    if old.is_some() {
                        m.frames_superseded += 1;
                    }
                    old
                };
                if let Some(old) = superseded {
                    // Never displayed, never sampled: hand it back right away.
                    self.send_release(frame.canvas, old.frame.buffer, old.frame.seq);
                }
                match fence {
                    Some(f) => self.watch[c] = Some(f),
                    None => {
                        self.watch[c] = None;
                        self.repaint_throttled(c);
                    }
                }
            }
            EngineMsg::Goodbye(g) => {
                let mask = g.canvas.map_or(0b1111, |c| bit(c as usize));
                self.end_leases(mask);
                let mut st = self.shared.lock();
                for c in 0..4 {
                    if mask & bit(c) != 0 {
                        st.canvases[c].goodbye = Some(g.reason);
                    }
                }
                drop(st);
                tracing::info!(target: "se_ui::frames", reason = g.reason, canvas = ?g.canvas, "engine goodbye");
                self.ctx.request_repaint();
            }
        }
    }

    /// The UI stopped updating while frames keep coming: ask for nothing so the engine releases
    /// every buffer we hold. The UI re-sends its hello when it runs again.
    fn suspend(&mut self) {
        let Some(conn) = &self.conn else { return };
        let (want, flags) = conn.sent;
        self.end_leases(want);
        self.shared.lock().suspended = true;
        tracing::debug!(target: "se_ui::frames", "UI not updating; pausing frame delivery");
        self.send_hello(0, flags);
        // Wake the UI so it can resume as soon as it is able to run.
        self.ctx.request_repaint();
    }

    fn repaint_throttled(&mut self, c: usize) {
        let hz = self.rates[c];
        let hz = if hz.is_finite() && hz > 0.0 { hz } else { 60.0 };
        let now = Instant::now();
        match self.next_repaint[c] {
            Some(next) if now < next => self.ctx.request_repaint_after(next - now),
            _ => {
                self.ctx.request_repaint();
                self.next_repaint[c] = Some(now + Duration::from_secs_f32(1.0 / hz));
            }
        }
    }
}
