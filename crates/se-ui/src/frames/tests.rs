//! Fake-engine tests: a real `SOCK_SEQPACKET` socketpair drives the IO thread and the UI state
//! machine, whose GPU side is a recording fake (the real device path is covered by
//! `gpu_tests`).

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};

use super::core::{Core, Gpu, GpuBuffer, STALE_FREE, WANT_HOLD};
use super::io::{self, Connector};
use super::proto::testing::{ClientMsg, decode_client};
use super::proto::{self, CanvasDesc, Frame, Goodbye, Hello, Release};
use super::testutil::{memfd_with, open_fds_linking, open_fds_named};
use super::{Canvas, FrameStats, FrameTexture, Transport};

const WAIT: Duration = Duration::from_secs(3);

pub(crate) fn socketpair() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: fills two fresh descriptors, wrapped immediately.
    assert_eq!(unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0, fds.as_mut_ptr()) }, 0);
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

pub(crate) fn pipe() -> (OwnedFd, OwnedFd) {
    let mut fds = [0; 2];
    // SAFETY: fills two fresh descriptors, wrapped immediately.
    assert_eq!(unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
}

pub(crate) fn fd_link(fd: BorrowedFd<'_>) -> String {
    std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())).unwrap().to_string_lossy().into_owned()
}

/// Hands out pre-made connections; refuses when none is queued.
pub(crate) struct PairConnector(pub Receiver<OwnedFd>);

impl Connector for PairConnector {
    fn connect(&mut self) -> std::io::Result<OwnedFd> {
        self.0.try_recv().map_err(|_| std::io::Error::from(std::io::ErrorKind::ConnectionRefused))
    }
}

/// The engine end of a connection.
pub(crate) struct Server(pub OwnedFd);

impl Server {
    pub fn send(&self, bytes: &[u8], fds: &[BorrowedFd<'_>]) {
        let mut iov = libc::iovec { iov_base: bytes.as_ptr() as *mut _, iov_len: bytes.len() };
        let mut cmsg = [0u64; 32];
        // SAFETY: msghdr/cmsg buffers are live for the call; CMSG_* stay within `cmsg`.
        unsafe {
            let mut msg: libc::msghdr = std::mem::zeroed();
            msg.msg_iov = &raw mut iov;
            msg.msg_iovlen = 1;
            if !fds.is_empty() {
                let payload = (fds.len() * size_of::<libc::c_int>()) as u32;
                msg.msg_control = cmsg.as_mut_ptr().cast();
                msg.msg_controllen = libc::CMSG_SPACE(payload) as _;
                let c = libc::CMSG_FIRSTHDR(&raw const msg);
                (*c).cmsg_level = libc::SOL_SOCKET;
                (*c).cmsg_type = libc::SCM_RIGHTS;
                (*c).cmsg_len = libc::CMSG_LEN(payload) as _;
                let data = libc::CMSG_DATA(c).cast::<libc::c_int>();
                for (i, fd) in fds.iter().enumerate() {
                    data.add(i).write_unaligned(fd.as_raw_fd());
                }
            }
            assert_eq!(libc::sendmsg(self.0.as_raw_fd(), &raw const msg, libc::MSG_NOSIGNAL), bytes.len() as isize);
        }
    }

    pub fn send_owned(&self, bytes: &[u8], fds: Vec<OwnedFd>) {
        let borrowed: Vec<BorrowedFd<'_>> = fds.iter().map(|f| f.as_fd()).collect();
        self.send(bytes, &borrowed);
    }

    /// Next client message within `timeout`; `None` on timeout.
    pub fn recv(&self, timeout: Duration) -> Option<ClientMsg> {
        let mut p = libc::pollfd { fd: self.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
        // SAFETY: one valid pollfd.
        if unsafe { libc::poll(&raw mut p, 1, timeout.as_millis() as i32) } <= 0 {
            return None;
        }
        let mut buf = [0u8; 64];
        // SAFETY: buf is writable for its length.
        let n = unsafe { libc::recv(self.0.as_raw_fd(), buf.as_mut_ptr().cast(), buf.len(), libc::MSG_DONTWAIT) };
        assert!(n > 0, "client closed the connection");
        Some(decode_client(&buf[..n as usize]).expect("client sent a malformed message"))
    }

    pub fn expect(&self) -> ClientMsg {
        self.recv(WAIT).expect("expected a client message")
    }

    pub fn expect_hello(&self) -> Hello {
        match self.expect() {
            ClientMsg::Hello(h) => h,
            other => panic!("expected hello, got {other:?}"),
        }
    }

    pub fn expect_release(&self) -> Release {
        match self.expect() {
            ClientMsg::Release(r) => r,
            other => panic!("expected release, got {other:?}"),
        }
    }

    pub fn expect_silence(&self, d: Duration) {
        if let Some(m) = self.recv(d) {
            panic!("unexpected client message {m:?}");
        }
    }

    /// True once the client closed its end.
    pub fn closed_within(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            let mut p = libc::pollfd { fd: self.0.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd.
            if unsafe { libc::poll(&raw mut p, 1, 50) } > 0 {
                let mut b = [0u8; 64];
                // SAFETY: b is writable for its length.
                if unsafe { libc::recv(self.0.as_raw_fd(), b.as_mut_ptr().cast(), b.len(), libc::MSG_DONTWAIT) } == 0 {
                    return true;
                }
            }
        }
        false
    }
}

pub(crate) fn frame(canvas: u32, buffer: u32, seq: u64, generation: u32, has_fence: bool) -> [u8; proto::FRAME_LEN] {
    Frame { canvas, buffer, seq, monotonic_ns: seq * 16_666_667, generation, has_fence }.encode()
}

pub(crate) fn desc(canvas: u32, generation: u32, width: u32, height: u32, stride: u32, fourcc: u32, buffers: u32) -> CanvasDesc {
    CanvasDesc {
        canvas,
        width,
        height,
        drm_fourcc: fourcc,
        modifier: 0,
        offsets: [0; 4],
        strides: [stride, 0, 0, 0],
        planes: 1,
        buffer_count: buffers,
        generation,
    }
}

/// memfds named `{name}-{i}`, each filled with `colors[i]` rows of `stride` bytes.
fn shm_buffers(name: &str, d: &CanvasDesc, colors: &[[u8; 4]]) -> Vec<OwnedFd> {
    colors
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let mut data = vec![0u8; (d.strides[0] * d.height) as usize];
            for row in data.chunks_mut(d.strides[0] as usize) {
                for px in row[..(d.width * 4) as usize].chunks_mut(4) {
                    px.copy_from_slice(c);
                }
            }
            memfd_with(&format!("{name}-{i}"), &data)
        })
        .collect()
}

fn eventually(what: &str, mut cond: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !cond() {
        assert!(Instant::now() < deadline, "timed out waiting for: {what}");
        std::thread::sleep(Duration::from_millis(2));
    }
}

fn names(name: &str, n: usize) -> Vec<String> {
    (0..n).map(|i| format!("{name}-{i}")).collect()
}

fn assert_all_closed(names: &[String]) {
    for n in names {
        assert_eq!(open_fds_named(n), 0, "{n} leaked");
    }
}

const RED: [u8; 4] = [255, 0, 0, 255];
const GREEN: [u8; 4] = [0, 255, 0, 255];
const BLUE: [u8; 4] = [0, 0, 255, 255];
const WHITE: [u8; 4] = [255, 255, 255, 255];

struct FakeBuf {
    id: egui::TextureId,
    /// A fake "dmabuf": the fd the client handed to the import.
    _fd: Option<OwnedFd>,
}

impl GpuBuffer for FakeBuf {
    fn texture_id(&self) -> egui::TextureId {
        self.id
    }
}

#[derive(Default)]
struct FakeGpu {
    dmabuf: bool,
    fail_import: bool,
    next_id: u64,
    live: usize,
    acquires: Vec<egui::TextureId>,
    /// (texture, first pixel, rows length, stride)
    uploads: Vec<(egui::TextureId, [u8; 4], usize, u32)>,
    pending: Vec<Box<dyn FnOnce() + Send>>,
    submitted: Vec<Box<dyn FnOnce() + Send>>,
}

impl FakeGpu {
    fn buf(&mut self, fd: Option<OwnedFd>) -> FakeBuf {
        self.next_id += 1;
        self.live += 1;
        FakeBuf { id: egui::TextureId::User(self.next_id), _fd: fd }
    }

    /// Completes all GPU work submitted so far.
    fn complete_work(&mut self) {
        for cb in self.submitted.drain(..) {
            cb();
        }
    }
}

impl Gpu for FakeGpu {
    type Buffer = FakeBuf;

    fn maintain(&mut self) {}

    fn dmabuf_capable(&self) -> bool {
        self.dmabuf
    }

    fn import_dmabuf(&mut self, _desc: &CanvasDesc, fds: Vec<OwnedFd>) -> Result<Vec<FakeBuf>, String> {
        if self.fail_import {
            return Err("injected import failure".into());
        }
        Ok(fds.into_iter().map(|fd| self.buf(Some(fd))).collect())
    }

    fn create_shm(&mut self, _w: u32, _h: u32) -> Result<FakeBuf, String> {
        Ok(self.buf(None))
    }

    fn acquire(&mut self, buffer: &FakeBuf) {
        self.acquires.push(buffer.id);
    }

    fn upload(&mut self, buffer: &FakeBuf, rows: &[u8], stride: u32, _size: [u32; 2]) {
        self.uploads.push((buffer.id, [rows[0], rows[1], rows[2], rows[3]], rows.len(), stride));
    }

    fn submit(&mut self) {}

    fn on_work_done(&mut self, callback: Box<dyn FnOnce() + Send + 'static>) {
        // Everything recorded before this call belongs to work "submitted" now.
        self.pending.push(callback);
        self.submitted.append(&mut self.pending);
    }

    fn free(&mut self, _buffer: FakeBuf) {
        self.live -= 1;
    }
}

struct Harness {
    core: Core<FakeBuf>,
    thread: Option<JoinHandle<()>>,
    gpu: FakeGpu,
    conns: Sender<OwnedFd>,
}

impl Harness {
    /// A client connected to a fresh fake engine; the initial hello is consumed.
    fn new(dmabuf: bool) -> (Harness, Server) {
        let (conns, rx) = crossbeam_channel::unbounded();
        let (core, thread) = Core::start(Box::new(PairConnector(rx)), egui::Context::default());
        let mut h = Harness { core, thread, gpu: FakeGpu { dmabuf, ..Default::default() }, conns };
        let srv = h.connect();
        assert_eq!(srv.expect_hello(), Hello { client: proto::CLIENT_UI, want: 0, flags: 0 });
        (h, srv)
    }

    fn connect(&mut self) -> Server {
        let (client, server) = socketpair();
        self.conns.send(client).unwrap();
        Server(server)
    }

    fn step(&mut self) {
        self.core.step(&mut self.gpu, Instant::now());
    }

    fn want(&mut self, c: Canvas, hz: f32) {
        self.core.want(c.index(), hz, Instant::now());
    }

    fn texture(&self, c: Canvas) -> Option<FrameTexture> {
        self.core.texture(c.index(), Instant::now())
    }

    fn stats(&self) -> FrameStats {
        self.core.stats(std::path::Path::new("test"), Instant::now())
    }

    /// Steps (wanting `wants` each frame, like a UI drawing them) until `done` holds.
    fn until(&mut self, what: &str, wants: &[(Canvas, f32)], mut done: impl FnMut(&mut Self) -> bool) {
        let deadline = Instant::now() + WAIT;
        loop {
            for &(c, hz) in wants {
                self.want(c, hz);
            }
            self.step();
            if done(self) {
                return;
            }
            assert!(Instant::now() < deadline, "timed out waiting for: {what}");
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn run_for(&mut self, d: Duration, wants: &[(Canvas, f32)]) {
        let end = Instant::now() + d;
        while Instant::now() < end {
            for &(c, hz) in wants {
                self.want(c, hz);
            }
            self.step();
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    fn seq(&self, c: Canvas) -> Option<u64> {
        self.texture(c).map(|t| t.seq)
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.core.shutdown();
        if let Some(t) = self.thread.take() {
            t.join().unwrap();
        }
    }
}

const WIDE: &[(Canvas, f32)] = &[(Canvas::Wide, 60.0)];

#[test]
fn shm_frames_upload_release_supersede_fence_and_rate() {
    let name = "se-t-shm-flow";
    let (mut h, srv) = Harness::new(false);
    h.want(Canvas::Wide, 60.0);
    h.step();
    assert_eq!(srv.expect_hello(), Hello { client: 2, want: 0b0001, flags: 0 });

    // 4x2 canvas with padded rows (stride 20) and three buffers of distinct colors.
    let d = desc(0, 1, 4, 2, 20, 0, 3);
    srv.send_owned(&d.encode(), shm_buffers(name, &d, &[RED, GREEN, BLUE]));
    h.until("shm canvas imported", WIDE, |h| h.stats().canvases[0].transport == Transport::Shm);
    // Mapped, then the memfds were closed: the client keeps no descriptors for shm buffers.
    assert_all_closed(&names(name, 3));

    srv.send(&frame(0, 0, 1, 1, false), &[]);
    h.until("frame 1 shown", WIDE, |h| h.seq(Canvas::Wide) == Some(1));
    let tex = h.texture(Canvas::Wide).unwrap();
    assert_eq!((tex.size, tex.transport, tex.stale), ([4, 2], Transport::Shm, false));
    assert_eq!(h.gpu.uploads.last().map(|u| (u.0, u.1, u.2, u.3)), Some((tex.id, RED, 40, 20)));
    // The rows were copied by the upload, so the buffer goes straight back.
    assert_eq!(srv.expect_release(), Release { canvas: 0, buffer: 0, seq: 1 });

    // Two frames before the UI looks: the older one is released unseen by the IO thread.
    srv.send(&frame(0, 1, 2, 1, false), &[]);
    srv.send(&frame(0, 2, 3, 1, false), &[]);
    assert_eq!(srv.expect_release(), Release { canvas: 0, buffer: 1, seq: 2 });
    h.until("frame 3 shown", WIDE, |h| h.seq(Canvas::Wide) == Some(3));
    assert_eq!(h.gpu.uploads.last().unwrap().1, BLUE);
    assert_eq!(srv.expect_release(), Release { canvas: 0, buffer: 2, seq: 3 });

    // A frame is not presentable before its fence signals.
    let (fence_r, fence_w) = pipe();
    let fence_link = fd_link(fence_r.as_fd());
    srv.send_owned(&frame(0, 0, 4, 1, true), vec![fence_r]);
    h.until("fence pending", WIDE, |h| h.stats().canvases[0].fence_pending);
    h.run_for(Duration::from_millis(50), WIDE);
    assert_eq!(h.seq(Canvas::Wide), Some(3));
    srv.expect_silence(Duration::from_millis(10));
    // SAFETY: writes one byte from a live buffer.
    assert_eq!(unsafe { libc::write(fence_w.as_raw_fd(), [1u8].as_ptr().cast(), 1) }, 1);
    h.until("fenced frame shown", WIDE, |h| h.seq(Canvas::Wide) == Some(4));
    assert_eq!(srv.expect_release(), Release { canvas: 0, buffer: 0, seq: 4 });
    assert!(!h.stats().canvases[0].fence_pending);
    // Both pipe ends link to the same inode: only the test's write end may remain open.
    eventually("fence closed by the client", || open_fds_linking(&fence_link) == 1);

    // Rate limit: at 4 Hz a new frame is swapped in no sooner than 0.8 * 250 ms after the last.
    let slow: &[(Canvas, f32)] = &[(Canvas::Wide, 4.0)];
    h.run_for(Duration::from_millis(5), slow);
    let last_swap = Instant::now();
    srv.send(&frame(0, 1, 5, 1, false), &[]);
    h.run_for(Duration::from_millis(100), slow);
    // (the swap to seq 4 happened at most a few ms before `last_swap`)
    assert_eq!(h.seq(Canvas::Wide), Some(4), "swapped faster than max_hz");
    h.until("rate-limited frame shown", slow, |h| h.seq(Canvas::Wide) == Some(5));
    assert!(last_swap.elapsed() >= Duration::from_millis(150));
    assert_eq!(srv.expect_release(), Release { canvas: 0, buffer: 1, seq: 5 });

    let s = h.stats();
    let c = &s.canvases[0];
    assert!(s.connected);
    assert_eq!((c.frames_received, c.frames_presented, c.frames_dropped_rate, c.last_seq, c.presented_seq), (5, 4, 1, Some(5), Some(5)));
    assert_eq!((s.shm_frames_presented, s.shm_bytes_uploaded, s.dmabuf_frames_presented, s.dmabuf_imports), (4, 4 * 4 * 2 * 4, 0, 0));
    h.until("release count", slow, |h| h.stats().releases_sent == 5);

    drop(fence_w);
    drop(h);
    assert!(srv.closed_within(WAIT), "shutdown must close the socket");
    assert_eq!(open_fds_linking(&fence_link), 0, "fence fd leaked");
}

#[test]
fn generation_change_goodbye_and_malformed_input() {
    let name = "se-t-gen";
    let (mut h, srv) = Harness::new(false);
    h.until("hello", WIDE, |_| true);
    assert_eq!(srv.expect_hello().want, 1);

    let d1 = desc(0, 1, 2, 2, 8, 0, 2);
    srv.send_owned(&d1.encode(), shm_buffers(&format!("{name}-g1"), &d1, &[RED, GREEN]));
    srv.send(&frame(0, 0, 1, 1, false), &[]);
    h.until("gen 1 frame", WIDE, |h| h.seq(Canvas::Wide) == Some(1));
    srv.expect_release();

    // Re-created canvas: the last gen-1 frame stays up until gen 2 presents.
    let d2 = desc(0, 2, 3, 3, 12, 0, 2);
    srv.send_owned(&d2.encode(), shm_buffers(&format!("{name}-g2"), &d2, &[BLUE, WHITE]));
    h.until("gen 2 described", WIDE, |h| h.stats().canvases[0].generation == 2);
    assert_eq!(h.texture(Canvas::Wide).map(|t| (t.seq, t.size)), Some((1, [2, 2])));
    // A late gen-1 frame is ignored (and not released: the engine dropped those buffers).
    srv.send(&frame(0, 1, 2, 1, false), &[]);
    srv.send(&frame(0, 1, 3, 2, false), &[]);
    h.until("gen 2 frame", WIDE, |h| h.seq(Canvas::Wide) == Some(3));
    let t = h.texture(Canvas::Wide).unwrap();
    assert_eq!(t.size, [3, 3]);
    assert_eq!(h.gpu.uploads.last().unwrap().1, WHITE);
    assert_eq!(h.gpu.live, 1, "gen-1 texture freed once gen 2 presented");
    assert_eq!(srv.expect_release(), Release { canvas: 0, buffer: 1, seq: 3 });
    assert_eq!(h.stats().canvases[0].frames_dropped_stale, 1);

    // A frame for a buffer index the canvas does not have is rejected.
    srv.send(&frame(0, 5, 4, 2, false), &[]);
    // Malformed message carrying descriptors: rejected, descriptors closed.
    let bad = "se-t-gen-bad";
    let mut bytes = desc(0, 3, 2, 2, 8, 0, 2).encode();
    bytes[0] ^= 0xff;
    srv.send_owned(&bytes, vec![memfd_with(&format!("{bad}-0"), &[0; 16]), memfd_with(&format!("{bad}-1"), &[0; 16])]);
    h.until("malformed message reported", WIDE, |h| h.stats().last_error.is_some_and(|e| e.contains("malformed")));
    assert_all_closed(&names(bad, 2));
    assert_eq!(h.seq(Canvas::Wide), Some(3));

    // Goodbye keeps the last frame, flagged stale; frames need a new description afterwards.
    srv.send(&Goodbye { reason: 2, canvas: None }.encode(), &[]);
    h.until("stale after goodbye", WIDE, |h| h.texture(Canvas::Wide).is_some_and(|t| t.stale));
    srv.send(&frame(0, 0, 5, 2, false), &[]);
    h.run_for(Duration::from_millis(50), WIDE);
    assert_eq!(h.seq(Canvas::Wide), Some(3));
    assert!(h.stats().canvases[0].stale);
    srv.expect_silence(Duration::from_millis(10));

    let d3 = desc(0, 3, 3, 3, 12, 0, 2);
    srv.send_owned(&d3.encode(), shm_buffers(&format!("{name}-g3"), &d3, &[GREEN, RED]));
    srv.send(&frame(0, 0, 6, 3, false), &[]);
    h.until("fresh frame after goodbye", WIDE, |h| h.seq(Canvas::Wide) == Some(6));
    assert!(!h.texture(Canvas::Wide).unwrap().stale);

    drop(h);
    for g in ["g1", "g2", "g3"] {
        assert_all_closed(&names(&format!("{name}-{g}"), 2));
    }
}

#[test]
fn want_hysteresis_sends_hello_updates() {
    let (mut h, srv) = Harness::new(false);
    let both: &[(Canvas, f32)] = &[(Canvas::Wide, 60.0), (Canvas::Atlas, 30.0)];
    h.until("hello", both, |_| true);
    assert_eq!(srv.expect_hello().want, 0b1001);
    // Wanting the same set again sends nothing.
    h.run_for(Duration::from_millis(30), both);
    srv.expect_silence(Duration::from_millis(10));

    // The atlas panel is hidden: its bit drops only after WANT_HOLD without being drawn.
    let hidden_at = Instant::now();
    h.run_for(WANT_HOLD - Duration::from_millis(200), WIDE);
    srv.expect_silence(Duration::from_millis(1));
    let hello = loop {
        h.run_for(Duration::from_millis(5), WIDE);
        if let Some(ClientMsg::Hello(hello)) = srv.recv(Duration::from_millis(5)) {
            break hello;
        }
        assert!(hidden_at.elapsed() < WANT_HOLD + WAIT, "no hello after the atlas stopped being drawn");
    };
    assert_eq!(hello.want, 0b0001);
    // (the atlas was last drawn a few ms before `hidden_at`)
    assert!(hidden_at.elapsed() >= WANT_HOLD - Duration::from_millis(20));
    // Frames of the dropped canvas that were already in flight are ignored, not released.
    srv.send_owned(&desc(3, 1, 2, 2, 8, 0, 1).encode(), shm_buffers("se-t-hyst", &desc(3, 1, 2, 2, 8, 0, 1), &[RED]));
    srv.send(&frame(3, 0, 1, 1, false), &[]);
    h.run_for(Duration::from_millis(50), WIDE);
    assert!(h.texture(Canvas::Atlas).is_none());
    srv.expect_silence(Duration::from_millis(10));
    assert_all_closed(&names("se-t-hyst", 1));
    assert_eq!(h.stats().hellos_sent, 3);
}

#[test]
fn dmabuf_displayed_buffer_is_released_only_after_gpu_work() {
    let name = "se-t-dmabuf";
    let (mut h, srv) = Harness::new(true);
    h.until("hello", WIDE, |_| true);
    assert_eq!(srv.expect_hello(), Hello { client: 2, want: 1, flags: proto::FLAG_DMABUF });

    let d = CanvasDesc { modifier: 0x0300_0000_0060_6015, ..desc(0, 1, 64, 32, 256, proto::DRM_FORMAT_ABGR8888, 4) };
    let fds: Vec<OwnedFd> = (0..4).map(|i| memfd_with(&format!("{name}-{i}"), &[0; 8])).collect();
    srv.send_owned(&d.encode(), fds);
    h.until("dmabuf imported", WIDE, |h| h.stats().canvases[0].transport == Transport::Dmabuf);
    for n in names(name, 4) {
        assert_eq!(open_fds_named(&n), 1, "import keeps exactly one fd per buffer");
    }
    let s = h.stats();
    assert_eq!((s.dmabuf_imports, s.canvases[0].modifier, s.canvases[0].drm_fourcc), (4, 0x0300_0000_0060_6015, proto::DRM_FORMAT_ABGR8888));

    srv.send(&frame(0, 0, 1, 1, false), &[]);
    h.until("frame 1", WIDE, |h| h.seq(Canvas::Wide) == Some(1));
    let first = h.texture(Canvas::Wide).unwrap().id;
    assert_eq!(h.gpu.acquires, vec![first], "acquired before display");
    h.gpu.complete_work();
    h.run_for(Duration::from_millis(20), WIDE);
    srv.expect_silence(Duration::from_millis(10)); // displayed: still held

    srv.send(&frame(0, 1, 2, 1, false), &[]);
    h.until("frame 2", WIDE, |h| h.seq(Canvas::Wide) == Some(2));
    assert_ne!(h.texture(Canvas::Wide).unwrap().id, first);
    // Buffer 0 may still be sampled by submitted work: nothing is released until it completes.
    h.run_for(Duration::from_millis(20), WIDE);
    srv.expect_silence(Duration::from_millis(10));
    h.gpu.complete_work();
    assert_eq!(srv.expect_release(), Release { canvas: 0, buffer: 0, seq: 1 });

    // Superseded before display: released immediately, never acquired.
    srv.send(&frame(0, 2, 3, 1, false), &[]);
    srv.send(&frame(0, 3, 4, 1, false), &[]);
    assert_eq!(srv.expect_release(), Release { canvas: 0, buffer: 2, seq: 3 });
    h.until("frame 4", WIDE, |h| h.seq(Canvas::Wide) == Some(4));
    assert_eq!(h.gpu.acquires.len(), 3);
    h.gpu.complete_work();
    assert_eq!(srv.expect_release(), Release { canvas: 0, buffer: 1, seq: 2 });
    assert_eq!(h.stats().dmabuf_frames_presented, 3);

    // A completion that arrives after the engine re-described the canvas must not release a
    // buffer of the new set.
    srv.send(&frame(0, 0, 5, 1, false), &[]);
    h.until("frame 5", WIDE, |h| h.seq(Canvas::Wide) == Some(5));
    let d2 = CanvasDesc { generation: 2, ..d };
    srv.send_owned(&d2.encode(), (0..4).map(|i| memfd_with(&format!("{name}-g2-{i}"), &[0; 8])).collect());
    h.until("gen 2 imported", WIDE, |h| h.stats().canvases[0].generation == 2);
    h.gpu.complete_work(); // releases buffer 3 (seq 4) of the ended lease: dropped by the IO thread
    srv.expect_silence(Duration::from_millis(50));
    assert_eq!(h.seq(Canvas::Wide), Some(5), "old generation stays on screen");

    drop(h);
    assert_all_closed(&names(name, 4));
    assert_all_closed(&names(&format!("{name}-g2"), 4));
}

#[test]
fn dmabuf_import_failure_switches_to_shm() {
    let (mut h, srv) = Harness::new(true);
    h.until("hello", WIDE, |_| true);
    assert_eq!(srv.expect_hello().flags, proto::FLAG_DMABUF);
    h.gpu.fail_import = true;
    let d = desc(0, 1, 16, 16, 64, proto::DRM_FORMAT_ABGR8888, 3);
    srv.send_owned(&d.encode(), (0..3).map(|i| memfd_with(&format!("se-t-fail-{i}"), &[0; 8])).collect());
    h.until("fell back", WIDE, |h| !h.stats().dmabuf_enabled);
    assert_eq!(srv.expect_hello(), Hello { client: 2, want: 1, flags: 0 });
    assert!(h.stats().last_error.unwrap().contains("injected import failure"));
    assert_all_closed(&names("se-t-fail", 3));
    // The engine answers with a shm description.
    let s = desc(0, 1, 16, 16, 64, 0, 3);
    srv.send_owned(&s.encode(), shm_buffers("se-t-fail-shm", &s, &[RED, GREEN, BLUE]));
    srv.send(&frame(0, 2, 1, 1, false), &[]);
    h.until("shm frame", WIDE, |h| h.seq(Canvas::Wide) == Some(1));
    assert_eq!(h.texture(Canvas::Wide).unwrap().transport, Transport::Shm);
    assert_eq!(h.gpu.uploads.last().unwrap().1, BLUE);
}

#[test]
fn reconnect_keeps_last_frame_then_frees_it() {
    let (mut h, srv) = Harness::new(false);
    h.until("hello", WIDE, |_| true);
    srv.expect_hello();
    let d = desc(0, 1, 2, 2, 8, 0, 2);
    srv.send_owned(&d.encode(), shm_buffers("se-t-recon", &d, &[RED, GREEN]));
    srv.send(&frame(0, 1, 1, 1, false), &[]);
    h.until("frame", WIDE, |h| h.seq(Canvas::Wide) == Some(1));
    srv.expect_release();

    // Engine goes away: last frame stays, flagged stale.
    drop(srv);
    h.until("disconnected", WIDE, |h| !h.stats().connected);
    assert!(h.texture(Canvas::Wide).is_some_and(|t| t.stale));

    // Engine comes back: the client reconnects and repeats its hello.
    let srv = h.connect();
    h.until("reconnected", WIDE, |h| h.stats().connected);
    assert_eq!(srv.expect_hello(), Hello { client: 2, want: 1, flags: 0 });
    assert_eq!(h.stats().reconnects, 1);
    assert_eq!(h.seq(Canvas::Wide), Some(1), "old frame still shown until the new engine delivers");
    let d = desc(0, 1, 2, 2, 8, 0, 2);
    srv.send_owned(&d.encode(), shm_buffers("se-t-recon2", &d, &[BLUE, WHITE]));
    srv.send(&frame(0, 0, 1, 1, false), &[]);
    h.until("new engine frame", WIDE, |h| h.texture(Canvas::Wide).is_some_and(|t| !t.stale));
    assert_eq!(h.gpu.uploads.last().unwrap().1, BLUE);
    assert_eq!(h.gpu.live, 1);

    // Gone for good: textures are dropped after STALE_FREE.
    drop(srv);
    h.until("disconnected again", WIDE, |h| !h.stats().connected);
    let gone = Instant::now();
    h.run_for(STALE_FREE - Duration::from_millis(300), WIDE);
    assert!(h.texture(Canvas::Wide).is_some());
    h.until("freed", WIDE, |h| h.texture(Canvas::Wide).is_none());
    assert!(gone.elapsed() >= STALE_FREE - Duration::from_millis(50));
    assert_eq!(h.gpu.live, 0);
    assert!(h.stats().canvases[0].stale);
}

#[test]
fn stops_holding_buffers_when_the_ui_stops_updating() {
    let (mut h, srv) = Harness::new(true);
    h.until("hello", WIDE, |_| true);
    srv.expect_hello();
    let d = desc(0, 1, 8, 8, 32, proto::DRM_FORMAT_ABGR8888, 4);
    srv.send_owned(&d.encode(), (0..4).map(|i| memfd_with(&format!("se-t-susp-{i}"), &[0; 8])).collect());
    srv.send(&frame(0, 0, 1, 1, false), &[]);
    h.until("frame 1", WIDE, |h| h.seq(Canvas::Wide) == Some(1));
    srv.send(&frame(0, 1, 2, 1, false), &[]);
    h.until("frame 2", WIDE, |h| h.seq(Canvas::Wide) == Some(2)); // buffer 0 now awaits GPU completion

    // The window is hidden: no more update() calls while the engine keeps sending frames.
    std::thread::sleep(io::SUSPEND_AFTER + Duration::from_millis(100));
    srv.send(&frame(0, 2, 3, 1, false), &[]);
    assert_eq!(srv.expect_hello(), Hello { client: 2, want: 0, flags: proto::FLAG_DMABUF }, "pause = hello with nothing wanted");

    // The UI runs again: its textures are gone (the engine may have rewritten them) and it asks again.
    h.until("resumed", WIDE, |_| true);
    assert!(h.texture(Canvas::Wide).is_none());
    assert_eq!(h.gpu.live, 0);
    assert_eq!(srv.expect_hello(), Hello { client: 2, want: 1, flags: proto::FLAG_DMABUF });
    srv.send_owned(&d.encode(), (0..4).map(|i| memfd_with(&format!("se-t-susp2-{i}"), &[0; 8])).collect());
    srv.send(&frame(0, 2, 4, 1, false), &[]);
    h.until("frame after resume", WIDE, |h| h.seq(Canvas::Wide) == Some(4));
    // Completion of work from before the pause must not release anything (the engine already
    // dropped those holds).
    h.gpu.complete_work();
    srv.expect_silence(Duration::from_millis(50));
    drop(h);
    assert_all_closed(&names("se-t-susp", 4));
    assert_all_closed(&names("se-t-susp2", 4));
}

#[test]
fn path_connector_retries_until_the_engine_listens() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("frames.sock");
    let (core, thread) = Core::<FakeBuf>::start(Box::new(io::PathConnector(path.clone())), egui::Context::default());
    let mut h = Harness { core, thread, gpu: FakeGpu::default(), conns: crossbeam_channel::unbounded().0 };
    h.run_for(Duration::from_millis(350), WIDE);
    assert!(!h.stats().connected);

    // SAFETY: plain socket/bind/listen/accept calls on fresh descriptors.
    let listener = unsafe { OwnedFd::from_raw_fd(libc::socket(libc::AF_UNIX, libc::SOCK_SEQPACKET | libc::SOCK_CLOEXEC, 0)) };
    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (d, s) in addr.sun_path.iter_mut().zip(path.as_os_str().as_encoded_bytes()) {
        *d = *s as libc::c_char;
    }
    assert_eq!(unsafe { libc::bind(listener.as_raw_fd(), (&raw const addr).cast(), size_of::<libc::sockaddr_un>() as u32) }, 0);
    assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 4) }, 0);
    let listening = Instant::now();
    let mut p = libc::pollfd { fd: listener.as_raw_fd(), events: libc::POLLIN, revents: 0 };
    assert!(unsafe { libc::poll(&raw mut p, 1, 3000) } > 0, "client never retried");
    // Backoff is capped at 2 s.
    assert!(listening.elapsed() <= io::BACKOFF_MAX + Duration::from_millis(200));
    let srv = Server(unsafe { OwnedFd::from_raw_fd(libc::accept4(listener.as_raw_fd(), std::ptr::null_mut(), std::ptr::null_mut(), libc::SOCK_CLOEXEC)) });
    // The UI already wanted the wide canvas: the first hello carries it.
    assert_eq!(srv.expect_hello(), Hello { client: proto::CLIENT_UI, want: 1, flags: 0 });
    h.until("connected", WIDE, |h| h.stats().connected);
}
