//! Engine ↔ `stream-engine-web` host protocol.
//!
//! * **Control:** one `AF_UNIX` / `SOCK_SEQPACKET` socket pair; the engine keeps one end and the
//!   host inherits the other as fd 3 (`--se-ipc-fd=3`). Every message is one JSON datagram
//!   ([`ToHost`], [`FromHost`]); shared-memory file descriptors travel with their message as
//!   `SCM_RIGHTS`.
//! * **Video:** per browser, the host allocates a sealed memfd *surface* holding
//!   [`FRAME_SLOTS`] BGRA frames of the current view size. `on_paint` copies the view into a free
//!   slot and sends [`FromHost::Frame`]; the engine copies the slot into the hub video slot and
//!   answers [`ToHost::FrameDone`], which frees it. A resize allocates a new surface
//!   (`surface` generation + 1).
//! * **Audio:** per browser, a sealed memfd single-producer/single-consumer ring of interleaved
//!   `f32` samples ([`AudioRing`]); the host notifies with [`FromHost::Audio`] after each packet.
//!
//! Memfds are sealed against shrinking before they are shared, and the receiver verifies the seal
//! and size before mapping, so a misbehaving host can never make the engine fault on a truncated
//! mapping.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use std::io;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;
use std::sync::atomic::{AtomicU64, Ordering};

pub const PROTOCOL_VERSION: u32 = 1;
/// Frames per surface: one being copied by the engine, one being painted, one spare.
pub const FRAME_SLOTS: u32 = 3;
/// Sample rate requested from CEF and delivered to the audio slots.
pub const AUDIO_RATE: u32 = 48_000;
/// Interleaved channels in every audio ring (CEF streams are mixed to stereo).
pub const AUDIO_CHANNELS: u32 = 2;
/// Ring capacity in samples (power of two; 0.68 s of stereo at 48 kHz).
pub const AUDIO_RING_SAMPLES: u32 = 1 << 16;
/// Largest control datagram.
pub const MAX_MESSAGE: usize = 64 * 1024;
/// Largest view side in pixels (Chromium's own limit is far higher; this bounds shm sizes).
pub const MAX_SIDE: u32 = 8192;
/// CEF caps `windowless_frame_rate` at 60.
pub const MAX_FPS: u32 = 60;
/// Default user-visible fd number of the host's control socket.
pub const HOST_IPC_FD: i32 = 3;

/// Engine → host.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum ToHost {
    /// Open an off-screen browser (`id` is chosen by the engine, unique per host run).
    Open {
        id: u32,
        url: String,
        width: u32,
        height: u32,
        fps: u32,
    },
    Close {
        id: u32,
    },
    Resize {
        id: u32,
        width: u32,
        height: u32,
    },
    SetFps {
        id: u32,
        fps: u32,
    },
    Navigate {
        id: u32,
        url: String,
    },
    /// Reload bypassing the cache (patch files changed, `web.reload`).
    Reload {
        id: u32,
    },
    /// The engine finished copying `slot` of surface generation `surface`.
    FrameDone {
        id: u32,
        surface: u32,
        slot: u32,
    },
    /// Liveness probe; the host answers from its UI thread.
    Ping {
        seq: u64,
    },
    /// Close every browser and exit.
    Shutdown,
}

/// Host → engine.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum FromHost {
    /// CEF is initialized and the host accepts commands.
    Hello {
        protocol: u32,
        cef: String,
        chromium: String,
        pid: u32,
        gpu: bool,
    },
    /// New frame surface (memfd attached): `slots` frames of `stride * height` bytes.
    Surface {
        id: u32,
        surface: u32,
        width: u32,
        height: u32,
        stride: u32,
        slots: u32,
    },
    /// A painted frame is ready in `slot`; `paint_ns` is CLOCK_MONOTONIC (the master clock).
    Frame {
        id: u32,
        surface: u32,
        slot: u32,
        paint_ns: u64,
    },
    /// Audio ring for this browser (memfd attached).
    AudioRing {
        id: u32,
        channels: u32,
        rate: u32,
        capacity: u32,
    },
    /// Samples were appended to the browser's audio ring.
    Audio {
        id: u32,
    },
    /// The main frame started loading.
    Loading {
        id: u32,
    },
    /// The main frame finished loading with this HTTP status (0 for non-HTTP URLs).
    Loaded {
        id: u32,
        status: i32,
    },
    /// The main frame failed to load (`code` is a Chromium net error).
    LoadFailed {
        id: u32,
        url: String,
        code: i32,
        text: String,
    },
    /// The renderer process died; the host reloads the page after `retry_ms`.
    RendererGone {
        id: u32,
        reason: String,
        retry_ms: u64,
    },
    /// The browser was closed (after [`ToHost::Close`]).
    Closed {
        id: u32,
    },
    Pong {
        seq: u64,
    },
    Log {
        level: String,
        msg: String,
    },
    /// A page `console.error` (rate limited per browser).
    Console {
        id: u32,
        msg: String,
    },
}

/// CLOCK_MONOTONIC in nanoseconds (the engine's master clock, `se_clock::now`).
pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec; CLOCK_MONOTONIC is always available on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

fn cvt(r: libc::c_int) -> io::Result<libc::c_int> {
    if r < 0 { Err(io::Error::last_os_error()) } else { Ok(r) }
}

/// A connected `SOCK_SEQPACKET` pair (both ends close-on-exec).
pub fn socketpair() -> io::Result<(OwnedFd, OwnedFd)> {
    let mut fds = [0 as libc::c_int; 2];
    // SAFETY: `fds` has room for the two descriptors socketpair writes.
    cvt(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) })?;
    // SAFETY: socketpair succeeded, so both descriptors are open and owned by nobody else.
    Ok(unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) })
}

/// Enlarge the socket's send buffer (frame/audio notifications are sent without blocking).
pub fn set_send_buffer(fd: BorrowedFd<'_>, bytes: usize) {
    let v = bytes as libc::c_int;
    // SAFETY: plain setsockopt on an open socket with a correctly sized int option.
    unsafe {
        libc::setsockopt(fd.as_raw_fd(), libc::SOL_SOCKET, libc::SO_SNDBUF, (&v as *const libc::c_int).cast(), size_of::<libc::c_int>() as libc::socklen_t);
    }
}

/// Send one message, optionally passing a file descriptor. `nonblocking` fails with
/// `WouldBlock` instead of waiting when the peer's queue is full.
pub fn send<T: Serialize>(sock: BorrowedFd<'_>, msg: &T, pass: Option<BorrowedFd<'_>>, nonblocking: bool) -> io::Result<()> {
    let bytes = serde_json::to_vec(msg).map_err(io::Error::other)?;
    if bytes.len() > MAX_MESSAGE {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, format!("message of {} bytes exceeds {MAX_MESSAGE}", bytes.len())));
    }
    let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut libc::c_void, iov_len: bytes.len() };
    // Room for exactly one fd, 8-byte aligned as cmsghdr requires.
    let mut cbuf = [0u64; 4];
    // SAFETY: an all-zero msghdr is a valid "no name, no control" header.
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    if let Some(fd) = pass {
        // SAFETY: CMSG_SPACE is a pure size computation.
        let space = unsafe { libc::CMSG_SPACE(size_of::<libc::c_int>() as u32) } as usize;
        debug_assert!(space <= size_of_val(&cbuf));
        hdr.msg_control = cbuf.as_mut_ptr().cast();
        hdr.msg_controllen = space;
        // SAFETY: msg_control points at `space` zeroed, aligned bytes, so CMSG_FIRSTHDR yields a
        // valid header inside `cbuf` with room for one int of data.
        unsafe {
            let c = libc::CMSG_FIRSTHDR(&hdr);
            (*c).cmsg_level = libc::SOL_SOCKET;
            (*c).cmsg_type = libc::SCM_RIGHTS;
            (*c).cmsg_len = libc::CMSG_LEN(size_of::<libc::c_int>() as u32) as usize;
            std::ptr::write_unaligned(libc::CMSG_DATA(c).cast::<libc::c_int>(), fd.as_raw_fd());
        }
    }
    let flags = libc::MSG_NOSIGNAL | if nonblocking { libc::MSG_DONTWAIT } else { 0 };
    loop {
        // SAFETY: `hdr` references `iov`/`cbuf`, which outlive the call.
        let n = unsafe { libc::sendmsg(sock.as_raw_fd(), &hdr, flags) };
        if n >= 0 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

/// Receive one message (blocking). `Ok(None)` when the peer closed the socket. A descriptor
/// passed with the message is returned close-on-exec.
pub fn recv<T: DeserializeOwned>(sock: BorrowedFd<'_>, buf: &mut Vec<u8>) -> io::Result<Option<(T, Option<OwnedFd>)>> {
    buf.resize(MAX_MESSAGE, 0);
    let mut iov = libc::iovec { iov_base: buf.as_mut_ptr().cast(), iov_len: buf.len() };
    let mut cbuf = [0u64; 8];
    // SAFETY: an all-zero msghdr is valid; the pointers set below outlive the call.
    let mut hdr: libc::msghdr = unsafe { std::mem::zeroed() };
    hdr.msg_iov = &mut iov;
    hdr.msg_iovlen = 1;
    hdr.msg_control = cbuf.as_mut_ptr().cast();
    hdr.msg_controllen = size_of_val(&cbuf);
    let n = loop {
        // SAFETY: `hdr` references `iov` (into `buf`) and `cbuf`, both valid for writes.
        let n = unsafe { libc::recvmsg(sock.as_raw_fd(), &mut hdr, libc::MSG_CMSG_CLOEXEC) };
        if n >= 0 {
            break n as usize;
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    };
    // Take ownership of every passed descriptor first so none leaks on an error path.
    let mut fds: Vec<OwnedFd> = Vec::new();
    // SAFETY: the kernel filled `msg_control`; the CMSG_* macros walk it within
    // `msg_controllen`, and each SCM_RIGHTS entry holds `(cmsg_len - CMSG_LEN(0)) / 4` fds.
    unsafe {
        let mut c = libc::CMSG_FIRSTHDR(&hdr);
        while !c.is_null() {
            if (*c).cmsg_level == libc::SOL_SOCKET && (*c).cmsg_type == libc::SCM_RIGHTS {
                let count = ((*c).cmsg_len - libc::CMSG_LEN(0) as usize) / size_of::<libc::c_int>();
                let data = libc::CMSG_DATA(c).cast::<libc::c_int>();
                for i in 0..count {
                    fds.push(OwnedFd::from_raw_fd(std::ptr::read_unaligned(data.add(i))));
                }
            }
            c = libc::CMSG_NXTHDR(&hdr, c);
        }
    }
    if n == 0 {
        return Ok(None);
    }
    if hdr.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "truncated control message"));
    }
    let msg = serde_json::from_slice(&buf[..n]).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let fd = if fds.is_empty() { None } else { Some(fds.swap_remove(0)) };
    Ok(Some((msg, fd)))
}

/// A shared memory mapping backed by a sealed memfd.
///
/// Access goes through copies (`read_at` / `write_at`) because the other process writes the
/// same memory concurrently; the frame/ring protocols decide who may touch which bytes when.
pub struct Shm {
    ptr: NonNull<u8>,
    len: usize,
    fd: OwnedFd,
}

// SAFETY: the mapping is plain shared memory owned by this value; all access is via raw copies.
unsafe impl Send for Shm {}
// SAFETY: see above; concurrent `&Shm` access only performs raw copies and atomic operations.
unsafe impl Sync for Shm {}

impl Shm {
    /// Create, size, seal (no shrink/grow), and map a new memfd.
    pub fn create(name: &str, len: usize) -> io::Result<Shm> {
        let cname = std::ffi::CString::new(name).map_err(io::Error::other)?;
        // SAFETY: valid NUL-terminated name; flags are valid memfd flags.
        let raw = cvt(unsafe { libc::memfd_create(cname.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING) })?;
        // SAFETY: memfd_create returned a fresh descriptor we exclusively own.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        // SAFETY: ftruncate/fcntl on our own open memfd.
        unsafe {
            cvt(libc::ftruncate(fd.as_raw_fd(), len as libc::off_t))?;
            cvt(libc::fcntl(fd.as_raw_fd(), libc::F_ADD_SEALS, libc::F_SEAL_SHRINK | libc::F_SEAL_GROW | libc::F_SEAL_SEAL))?;
        }
        Self::mmap(fd, len)
    }

    /// Map a memfd received from the peer after checking it is sealed against shrinking and at
    /// least `min_len` bytes long.
    pub fn open(fd: OwnedFd, min_len: usize) -> io::Result<Shm> {
        // SAFETY: fcntl(F_GET_SEALS) on an open descriptor.
        let seals = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GET_SEALS) };
        if seals < 0 || seals & libc::F_SEAL_SHRINK == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "shared memory is not sealed against shrinking"));
        }
        // SAFETY: fstat into a zeroed stat buffer.
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        cvt(unsafe { libc::fstat(fd.as_raw_fd(), &mut st) })?;
        let len = st.st_size as usize;
        if len < min_len || min_len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, format!("shared memory is {len} bytes, expected ≥ {min_len}")));
        }
        Self::mmap(fd, len)
    }

    fn mmap(fd: OwnedFd, len: usize) -> io::Result<Shm> {
        if len == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "empty shared memory"));
        }
        // SAFETY: mapping `len` bytes of a memfd whose size is at least `len` and can no longer
        // shrink; the mapping is released in Drop.
        let p = unsafe { libc::mmap(std::ptr::null_mut(), len, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if p == libc::MAP_FAILED {
            return Err(io::Error::last_os_error());
        }
        let ptr = NonNull::new(p.cast::<u8>()).ok_or_else(|| io::Error::other("mmap returned null"))?;
        Ok(Shm { ptr, len, fd })
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }

    /// Copy `dst.len()` bytes starting at `offset` out of the mapping.
    pub fn read_at(&self, offset: usize, dst: &mut [u8]) -> bool {
        if offset.checked_add(dst.len()).is_none_or(|end| end > self.len) {
            return false;
        }
        // SAFETY: bounds checked above; the source is inside our live mapping and `dst` is a
        // distinct, exclusively borrowed buffer.
        unsafe { std::ptr::copy_nonoverlapping(self.ptr.as_ptr().add(offset), dst.as_mut_ptr(), dst.len()) };
        true
    }

    /// Copy `src` into the mapping at `offset`.
    pub fn write_at(&self, offset: usize, src: &[u8]) -> bool {
        if offset.checked_add(src.len()).is_none_or(|end| end > self.len) {
            return false;
        }
        // SAFETY: bounds checked above; the destination is inside our live, writable mapping.
        unsafe { std::ptr::copy_nonoverlapping(src.as_ptr(), self.ptr.as_ptr().add(offset), src.len()) };
        true
    }

    fn atomic_u64(&self, offset: usize) -> &AtomicU64 {
        assert!(offset.is_multiple_of(8) && offset + 8 <= self.len);
        // SAFETY: in bounds and 8-byte aligned (mmap is page aligned); AtomicU64 has the same
        // layout as u64 and is lock-free on the supported targets, so it works across processes.
        unsafe { &*self.ptr.as_ptr().add(offset).cast::<AtomicU64>() }
    }
}

impl Drop for Shm {
    fn drop(&mut self) {
        // SAFETY: unmapping exactly the region mapped in `mmap`.
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len) };
    }
}

impl std::fmt::Debug for Shm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Shm").field("len", &self.len).field("fd", &self.fd.as_raw_fd()).finish()
    }
}

const RING_MAGIC: u64 = u64::from_le_bytes(*b"seaudio1");
const RING_OFF_MAGIC: usize = 0;
const RING_OFF_FORMAT: usize = 8;
const RING_OFF_WRITE: usize = 64;
const RING_OFF_READ: usize = 128;
const RING_OFF_DATA: usize = 192;

/// Single-producer/single-consumer ring of interleaved `f32` samples in shared memory.
///
/// Header (cache-line separated): magic, `capacity << 32 | channels`, the producer's `write`
/// sample counter, the consumer's `read` sample counter. Counters increase monotonically in
/// whole frames (`channels` samples); the sample for counter `n` lives at `n % capacity`.
#[derive(Debug)]
pub struct AudioRing {
    shm: Shm,
    capacity: u64,
    channels: u64,
}

impl AudioRing {
    fn bytes(capacity: u32) -> usize {
        RING_OFF_DATA + capacity as usize * 4
    }

    /// Producer side (host).
    pub fn create(name: &str, capacity: u32, channels: u32) -> io::Result<AudioRing> {
        if !capacity.is_power_of_two() || channels == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "ring capacity must be a power of two"));
        }
        let shm = Shm::create(name, Self::bytes(capacity))?;
        shm.atomic_u64(RING_OFF_FORMAT).store((capacity as u64) << 32 | channels as u64, Ordering::Relaxed);
        shm.atomic_u64(RING_OFF_MAGIC).store(RING_MAGIC, Ordering::Release);
        Ok(AudioRing { shm, capacity: capacity as u64, channels: channels as u64 })
    }

    /// Consumer side (engine): map and validate a ring received from the host.
    pub fn open(fd: OwnedFd, capacity: u32, channels: u32) -> io::Result<AudioRing> {
        if !capacity.is_power_of_two() || channels == 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "bad ring format"));
        }
        let shm = Shm::open(fd, Self::bytes(capacity))?;
        let ok = shm.atomic_u64(RING_OFF_MAGIC).load(Ordering::Acquire) == RING_MAGIC
            && shm.atomic_u64(RING_OFF_FORMAT).load(Ordering::Relaxed) == ((capacity as u64) << 32 | channels as u64);
        if !ok {
            return Err(io::Error::new(io::ErrorKind::InvalidData, "ring header mismatch"));
        }
        Ok(AudioRing { shm, capacity: capacity as u64, channels: channels as u64 })
    }

    pub fn fd(&self) -> BorrowedFd<'_> {
        self.shm.fd()
    }

    pub fn capacity(&self) -> u32 {
        self.capacity as u32
    }

    fn sample_ptr(&self, index: u64) -> *mut f32 {
        let i = (index & (self.capacity - 1)) as usize;
        // SAFETY: `i < capacity` and the mapping holds `capacity` f32 after the header.
        unsafe { self.shm.ptr.as_ptr().add(RING_OFF_DATA + i * 4).cast::<f32>() }
    }

    /// Append up to `n` samples produced by `sample(i)` (whole frames only); returns how many
    /// fit (the rest are dropped: the consumer is behind).
    pub fn push_with(&self, n: usize, mut sample: impl FnMut(usize) -> f32) -> usize {
        let w = self.shm.atomic_u64(RING_OFF_WRITE).load(Ordering::Relaxed);
        let r = self.shm.atomic_u64(RING_OFF_READ).load(Ordering::Acquire);
        let used = w.wrapping_sub(r).min(self.capacity);
        let free = (self.capacity - used) as usize;
        let ch = self.channels as usize;
        let count = n.min(free) / ch * ch;
        for i in 0..count {
            // SAFETY: slots between `write` and `read + capacity` belong to the producer.
            unsafe { self.sample_ptr(w + i as u64).write(sample(i)) };
        }
        self.shm.atomic_u64(RING_OFF_WRITE).store(w + count as u64, Ordering::Release);
        count
    }

    /// Consume everything available: `sink` receives up to two contiguous runs of samples.
    /// Returns the number of samples consumed. Garbage counters from a misbehaving producer are
    /// resynchronized instead of trusted.
    pub fn pop_with(&self, mut sink: impl FnMut(&[f32])) -> usize {
        let w = self.shm.atomic_u64(RING_OFF_WRITE).load(Ordering::Acquire);
        let r = self.shm.atomic_u64(RING_OFF_READ).load(Ordering::Relaxed);
        let avail = w.wrapping_sub(r);
        let whole = avail / self.channels * self.channels;
        if avail == 0 {
            return 0;
        }
        if avail > self.capacity {
            // producer counter is inconsistent: skip to its position
            self.shm.atomic_u64(RING_OFF_READ).store(w, Ordering::Release);
            return 0;
        }
        if whole == 0 {
            return 0;
        }
        let start = (r & (self.capacity - 1)) as usize;
        let first = (whole as usize).min(self.capacity as usize - start);
        // SAFETY: the samples in [read, write) were published by the producer's Release store
        // (observed by our Acquire load) and it won't touch them until `read` advances.
        unsafe {
            sink(std::slice::from_raw_parts(self.sample_ptr(r), first));
            if first < whole as usize {
                sink(std::slice::from_raw_parts(self.sample_ptr(r + first as u64), whole as usize - first));
            }
        }
        self.shm.atomic_u64(RING_OFF_READ).store(r + whole, Ordering::Release);
        whole as usize
    }
}

/// Frame surface layout helper: byte range of `slot`.
pub fn slot_range(stride: u32, height: u32, slot: u32) -> std::ops::Range<usize> {
    let size = stride as usize * height as usize;
    let start = slot as usize * size;
    start..start + size
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_and_fds_cross_the_socket() {
        let (a, b) = socketpair().unwrap();
        let shm = Shm::create("se-web-test", 4096).unwrap();
        assert!(shm.write_at(4000, b"hello"));
        assert!(!shm.write_at(4094, b"hello"), "out of bounds write must be refused");
        let msg = FromHost::Surface { id: 7, surface: 2, width: 16, height: 16, stride: 64, slots: 3 };
        send(a.as_fd(), &msg, Some(shm.fd()), false).unwrap();
        send(a.as_fd(), &FromHost::Pong { seq: 9 }, None, true).unwrap();
        let mut buf = Vec::new();
        let (got, fd) = recv::<FromHost>(b.as_fd(), &mut buf).unwrap().unwrap();
        assert_eq!(got, msg);
        let peer = Shm::open(fd.expect("fd passed"), 4096).unwrap();
        let mut out = [0u8; 5];
        assert!(peer.read_at(4000, &mut out));
        assert_eq!(&out, b"hello");
        let (got, fd) = recv::<FromHost>(b.as_fd(), &mut buf).unwrap().unwrap();
        assert_eq!(got, FromHost::Pong { seq: 9 });
        assert!(fd.is_none());
        drop(a);
        assert!(recv::<FromHost>(b.as_fd(), &mut buf).unwrap().is_none(), "EOF after the peer closes");
    }

    #[test]
    fn unsealed_or_short_memory_is_rejected() {
        let shm = Shm::create("se-web-test", 1024).unwrap();
        let dup = shm.fd().try_clone_to_owned().unwrap();
        assert!(Shm::open(dup, 2048).is_err(), "too small");
        // SAFETY: plain memfd without sealing support.
        let raw = unsafe { libc::memfd_create(c"unsealed".as_ptr(), libc::MFD_CLOEXEC) };
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        unsafe { libc::ftruncate(fd.as_raw_fd(), 4096) };
        assert!(Shm::open(fd, 1024).is_err(), "a shrinkable memfd could SIGBUS the engine");
    }

    #[test]
    fn audio_ring_wraps_and_drops_when_full() {
        let prod = AudioRing::create("se-web-ring", 8, 2).unwrap();
        let cons = AudioRing::open(prod.fd().try_clone_to_owned().unwrap(), 8, 2).unwrap();
        assert!(AudioRing::open(prod.fd().try_clone_to_owned().unwrap(), 16, 2).is_err(), "format mismatch");
        assert_eq!(prod.push_with(6, |i| i as f32), 6);
        let mut got = Vec::new();
        assert_eq!(cons.pop_with(|s| got.extend_from_slice(s)), 6);
        assert_eq!(got, [0.0, 1.0, 2.0, 3.0, 4.0, 5.0]);
        // wraps around the end of the buffer; only 8 fit, the rest is dropped
        assert_eq!(prod.push_with(10, |i| 10.0 + i as f32), 8);
        got.clear();
        let mut runs = 0;
        assert_eq!(
            cons.pop_with(|s| {
                runs += 1;
                got.extend_from_slice(s)
            }),
            8
        );
        assert_eq!(runs, 2, "wrapped data arrives as two runs");
        assert_eq!(got, (0..8).map(|i| 10.0 + i as f32).collect::<Vec<_>>());
        assert_eq!(cons.pop_with(|_| panic!("empty")), 0);
    }

    #[test]
    fn corrupt_ring_counters_resync() {
        let prod = AudioRing::create("se-web-ring", 8, 2).unwrap();
        let cons = AudioRing::open(prod.fd().try_clone_to_owned().unwrap(), 8, 2).unwrap();
        prod.shm.atomic_u64(RING_OFF_WRITE).store(1_000_000, Ordering::Release);
        assert_eq!(cons.pop_with(|_| panic!("garbage must not be delivered")), 0);
        assert_eq!(prod.push_with(4, |_| 1.0), 4);
        assert_eq!(cons.pop_with(|s| assert!(s.iter().all(|&x| x == 1.0))), 4);
    }
}
