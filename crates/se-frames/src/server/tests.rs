use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use super::*;
use crate::client::{ClientMsg, FramesClient, wait_fence};
use crate::proto::{
    CANVAS_ATLAS, CANVAS_TALL, CANVAS_WIDE, CLIENT_OBS, CLIENT_OTHER, CLIENT_UI, DRM_FORMAT_ABGR8888, GOODBYE_CANVAS_REMOVED, GOODBYE_DEVICE_LOST,
    GOODBYE_SHUTDOWN,
};
use crate::shm::{ShmBuffer, ShmView};

const WAIT: Duration = Duration::from_secs(5);
const W: u32 = 16;
const H: u32 = 8;
const STRIDE: u32 = W * 4;

struct Fixture {
    _dir: tempfile::TempDir,
    path: PathBuf,
    server: FramesServer,
}

fn fixture_with(options: ServerOptions) -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("rt/frames.sock");
    let server = FramesServer::start_with(&path, options).unwrap();
    Fixture { _dir: dir, path, server }
}

fn fixture() -> Fixture {
    fixture_with(ServerOptions::default())
}

impl Fixture {
    fn client(&self, kind: u32, want: u32, dmabuf: bool) -> FramesClient {
        let c = FramesClient::connect(&self.path).unwrap();
        c.hello(kind, want, dmabuf).unwrap();
        c
    }
}

/// `n` shm buffers; buffer `i` is filled with byte `fill + i`.
fn buffers(n: usize, fill: u8) -> Vec<ShmBuffer> {
    (0..n)
        .map(|i| {
            let mut b = ShmBuffer::new(STRIDE, H).unwrap();
            b.as_mut_slice().fill(fill + i as u8);
            b
        })
        .collect()
}

fn dups(bufs: &[ShmBuffer]) -> Vec<OwnedFd> {
    bufs.iter().map(|b| b.try_clone_fd().unwrap()).collect()
}

fn shm_ring(bufs: &[ShmBuffer]) -> RingDesc {
    RingDesc::shm(W, H, STRIDE, dups(bufs))
}

/// memfds standing in for dmabufs (the server only passes fds through).
fn dmabuf_ring(bufs: &[ShmBuffer]) -> RingDesc {
    RingDesc { drm_fourcc: DRM_FORMAT_ABGR8888, modifier: 0, ..shm_ring(bufs) }
}

fn wait_until(what: &str, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(2));
    }
}

fn next(c: &mut FramesClient) -> ClientMsg {
    c.recv(WAIT).unwrap().expect("message before timeout")
}

fn expect_canvas(c: &mut FramesClient) -> (CanvasMsg, Vec<OwnedFd>) {
    match next(c) {
        ClientMsg::Canvas { msg, fds } => (msg, fds),
        other => panic!("expected se_canvas, got {other:?}"),
    }
}

fn expect_frame(c: &mut FramesClient) -> (FrameMsg, Option<OwnedFd>) {
    match next(c) {
        ClientMsg::Frame { msg, fence } => (msg, fence),
        other => panic!("expected se_frame, got {other:?}"),
    }
}

fn expect_goodbye(c: &mut FramesClient) -> Goodbye {
    match next(c) {
        ClientMsg::Goodbye(g) => g,
        other => panic!("expected se_goodbye, got {other:?}"),
    }
}

fn expect_silence(c: &mut FramesClient, ms: u64) {
    if let Some(m) = c.recv(Duration::from_millis(ms)).unwrap() {
        panic!("expected no message, got {m:?}");
    }
}

/// Acquires and presents one frame without fence; returns the buffer.
fn frame(s: &FramesServer, canvas: u32, kind: RingKind, seq: u64) -> u32 {
    let b = s.acquire(canvas, kind).expect("free buffer");
    s.present(canvas, kind, b, seq, seq * 1000, None);
    b
}

/// Every buffer `acquire` hands out right now (then abandoned again), in order.
fn free_buffers(s: &FramesServer, canvas: u32, kind: RingKind) -> Vec<u32> {
    let mut got = Vec::new();
    while let Some(b) = s.acquire(canvas, kind) {
        got.push(b);
    }
    for &b in &got {
        s.abandon(canvas, kind, b);
    }
    got
}

fn pipe() -> (OwnedFd, OwnedFd) {
    let mut p = [0; 2];
    // SAFETY: p is writable; results are fresh fds.
    assert_eq!(unsafe { libc::pipe2(p.as_mut_ptr(), libc::O_CLOEXEC) }, 0);
    // SAFETY: fresh fds owned by nobody else.
    unsafe { (OwnedFd::from_raw_fd(p[0]), OwnedFd::from_raw_fd(p[1])) }
}

#[test]
fn hello_gets_canvas_with_mapped_fds() {
    let f = fixture();
    let mut bufs = buffers(4, 10);
    f.server.set_ring(CANVAS_WIDE, RingKind::Shm, Some(shm_ring(&bufs))).unwrap();
    assert_eq!(f.server.generation(CANVAS_WIDE, RingKind::Shm), 1);

    let mut shm = f.client(CLIENT_OTHER, 1 << CANVAS_WIDE, false);
    let mut dma = f.client(CLIENT_OBS, 1 << CANVAS_WIDE, true);
    let (msg, fds) = expect_canvas(&mut shm);
    assert_eq!(
        msg,
        CanvasMsg {
            canvas: CANVAS_WIDE,
            width: W,
            height: H,
            drm_fourcc: 0,
            modifier: 0,
            offsets: [0; 4],
            strides: [STRIDE, 0, 0, 0],
            planes: 1,
            buffer_count: 4,
            generation: 1,
        }
    );
    assert_eq!(fds.len(), 4);
    let views: Vec<ShmView> = fds.iter().map(|fd| ShmView::map(fd.as_fd(), msg.min_buffer_len() as usize).unwrap()).collect();
    for (i, v) in views.iter().enumerate() {
        assert!(v.as_slice().iter().all(|&x| x == 10 + i as u8));
    }
    // The mapping is live shared memory, not a copy.
    bufs[2].as_mut_slice()[5] = 0xEE;
    assert_eq!(views[2].as_slice()[5], 0xEE);

    // The dmabuf client gets nothing: no dmabuf ring exists.
    expect_silence(&mut dma, 100);
    wait_until("demand", || f.server.demand(CANVAS_WIDE) == Demand { dmabuf: true, shm: true });
    assert_eq!(f.server.demand(CANVAS_TALL), Demand::default());
    let st = f.server.stats();
    assert_eq!((st.clients, st.dmabuf_clients, st.shm_clients), (2, 1, 1));
}

#[test]
fn present_delivers_frame_with_live_fence() {
    let f = fixture();
    let bufs = buffers(3, 0);
    f.server.set_ring(CANVAS_TALL, RingKind::Dmabuf, Some(dmabuf_ring(&bufs))).unwrap();
    let mut c = f.client(CLIENT_UI, 1 << CANVAS_TALL, true);
    let (canvas, fds) = expect_canvas(&mut c);
    assert_eq!(canvas.drm_fourcc, DRM_FORMAT_ABGR8888);
    assert_eq!((canvas.buffer_count, fds.len()), (3, 3));

    let b = f.server.acquire(CANVAS_TALL, RingKind::Dmabuf).unwrap();
    let ev = sys::eventfd().unwrap();
    f.server.present(CANVAS_TALL, RingKind::Dmabuf, b, 77, 123_456, Some(ev.try_clone().unwrap()));
    let (msg, fence) = expect_frame(&mut c);
    assert_eq!(msg, FrameMsg { canvas: CANVAS_TALL, buffer: b, seq: 77, monotonic_ns: 123_456, generation: 1, has_fence: true });
    let fence = fence.expect("fence fd");
    assert_ne!(fence.as_raw_fd(), ev.as_raw_fd());
    assert!(!wait_fence(&fence, Duration::ZERO).unwrap(), "not signalled yet");
    sys::eventfd_signal(ev.as_fd()).unwrap();
    assert!(wait_fence(&fence, Duration::from_secs(1)).unwrap(), "the received fd is the live fence object");

    c.release(CANVAS_TALL, msg.buffer, msg.seq).unwrap();
    let b2 = f.server.acquire(CANVAS_TALL, RingKind::Dmabuf).unwrap();
    f.server.present(CANVAS_TALL, RingKind::Dmabuf, b2, 78, 0, None);
    let (msg, fence) = expect_frame(&mut c);
    assert!(!msg.has_fence && fence.is_none());
    assert_eq!(msg.seq, 78);
    assert_eq!(f.server.stats().frames_sent, 2);
}

#[test]
fn acquire_skips_held_last_rendering_and_pending() {
    let f = fixture();
    let bufs = buffers(4, 0);
    let s = &f.server;
    assert_eq!(s.acquire(CANVAS_WIDE, RingKind::Shm), None, "no ring yet");
    s.set_ring(CANVAS_WIDE, RingKind::Shm, Some(shm_ring(&bufs))).unwrap();
    let mut c = f.client(CLIENT_OTHER, 1 << CANVAS_WIDE, false);
    expect_canvas(&mut c);
    wait_until("hello", || s.demand(CANVAS_WIDE).shm);

    // An acquired (rendering) buffer is not handed out twice.
    assert_eq!(s.acquire(CANVAS_WIDE, RingKind::Shm), Some(0));
    assert_eq!(s.acquire(CANVAS_WIDE, RingKind::Shm), Some(1));
    s.abandon(CANVAS_WIDE, RingKind::Shm, 1);

    // The client never releases: every sent buffer stays held.
    for (seq, want) in [(1, 0), (2, 1), (3, 2)] {
        let b = if seq == 1 { 0 } else { s.acquire(CANVAS_WIDE, RingKind::Shm).unwrap() };
        assert_eq!(b, want, "round-robin after the last presented buffer");
        s.present(CANVAS_WIDE, RingKind::Shm, b, seq, 0, None);
        assert_eq!(expect_frame(&mut c).0.buffer, want);
    }
    let b3 = s.acquire(CANVAS_WIDE, RingKind::Shm).unwrap();
    assert_eq!(b3, 3);
    assert_eq!(s.acquire(CANVAS_WIDE, RingKind::Shm), None, "0..2 held, 3 rendering");
    s.present(CANVAS_WIDE, RingKind::Shm, 3, 4, 0, None);
    assert_eq!(expect_frame(&mut c).0.buffer, 3);
    assert_eq!(s.acquire(CANVAS_WIDE, RingKind::Shm), None, "all held");

    // A release with a mismatching seq is ignored; a matching one frees the buffer.
    c.release(CANVAS_WIDE, 0, 999).unwrap();
    c.release(CANVAS_WIDE, 1, 2).unwrap();
    wait_until("release", || !free_buffers(s, CANVAS_WIDE, RingKind::Shm).is_empty());
    assert_eq!(free_buffers(s, CANVAS_WIDE, RingKind::Shm), vec![1]);

    // Releasing the most recently presented buffer does not make it acquirable.
    c.release(CANVAS_WIDE, 3, 4).unwrap();
    c.release(CANVAS_WIDE, 0, 1).unwrap();
    wait_until("release", || free_buffers(s, CANVAS_WIDE, RingKind::Shm).len() == 2);
    assert_eq!(free_buffers(s, CANVAS_WIDE, RingKind::Shm), vec![0, 1]);

    // Pending: queued frames the server thread has not handled yet are not handed out.
    c.release(CANVAS_WIDE, 2, 3).unwrap();
    wait_until("release", || free_buffers(s, CANVAS_WIDE, RingKind::Shm).len() == 3);
    let resume = s.pause();
    let p0 = frame(s, CANVAS_WIDE, RingKind::Shm, 5); // 0 (after last = 3)
    let p1 = frame(s, CANVAS_WIDE, RingKind::Shm, 6); // 1
    assert_eq!((p0, p1), (0, 1));
    assert_eq!(s.acquire(CANVAS_WIDE, RingKind::Shm), Some(2));
    assert_eq!(s.acquire(CANVAS_WIDE, RingKind::Shm), Some(3));
    assert_eq!(s.acquire(CANVAS_WIDE, RingKind::Shm), None, "0 is pending, 1 is last, 2 and 3 are rendering");
    drop(resume);
    assert_eq!(expect_frame(&mut c).0.seq, 5);
    assert_eq!(expect_frame(&mut c).0.seq, 6);
    c.release(CANVAS_WIDE, 0, 5).unwrap();
    wait_until("release", || s.acquire(CANVAS_WIDE, RingKind::Shm) == Some(0));
}

#[test]
fn disconnect_frees_holds() {
    let f = fixture();
    let bufs = buffers(4, 0);
    let s = &f.server;
    s.set_ring(CANVAS_ATLAS, RingKind::Shm, Some(shm_ring(&bufs))).unwrap();
    let mut c = f.client(CLIENT_UI, 1 << CANVAS_ATLAS, false);
    expect_canvas(&mut c);
    wait_until("hello", || s.demand(CANVAS_ATLAS).shm);
    for seq in 1..=4 {
        frame(s, CANVAS_ATLAS, RingKind::Shm, seq);
        expect_frame(&mut c);
    }
    assert_eq!(s.acquire(CANVAS_ATLAS, RingKind::Shm), None);
    drop(c);
    wait_until("disconnect", || s.stats().disconnects == 1);
    assert_eq!(free_buffers(s, CANVAS_ATLAS, RingKind::Shm), vec![0, 1, 2]);
    assert_eq!(s.stats().clients, 0);
    assert_eq!(s.demand(CANVAS_ATLAS), Demand::default());
}

#[test]
fn rehello_changes_want_and_transport() {
    let f = fixture();
    let s = &f.server;
    let (wide_shm, tall_shm, wide_dma, tall_dma) = (buffers(4, 0), buffers(4, 0), buffers(3, 0), buffers(3, 0));
    s.set_ring(CANVAS_WIDE, RingKind::Shm, Some(shm_ring(&wide_shm))).unwrap();
    s.set_ring(CANVAS_TALL, RingKind::Shm, Some(shm_ring(&tall_shm))).unwrap();
    s.set_ring(CANVAS_WIDE, RingKind::Dmabuf, Some(dmabuf_ring(&wide_dma))).unwrap();
    s.set_ring(CANVAS_TALL, RingKind::Dmabuf, Some(dmabuf_ring(&tall_dma))).unwrap();

    let mut c = f.client(CLIENT_UI, 1 << CANVAS_WIDE, false);
    let (m, _) = expect_canvas(&mut c);
    assert_eq!((m.canvas, m.drm_fourcc, m.generation), (CANVAS_WIDE, 0, 1));
    wait_until("hello", || s.demand(CANVAS_WIDE).shm);
    let b = frame(s, CANVAS_WIDE, RingKind::Shm, 1);
    expect_frame(&mut c);
    frame(s, CANVAS_WIDE, RingKind::Shm, 2);
    expect_frame(&mut c);
    assert_eq!(free_buffers(s, CANVAS_WIDE, RingKind::Shm), vec![2, 3]);

    // Switch to tall only: wide holds are released, tall se_canvas arrives.
    c.hello(CLIENT_UI, 1 << CANVAS_TALL, false).unwrap();
    let (m, _) = expect_canvas(&mut c);
    assert_eq!((m.canvas, m.drm_fourcc), (CANVAS_TALL, 0));
    wait_until("re-hello", || !s.demand(CANVAS_WIDE).shm);
    assert!(s.demand(CANVAS_TALL).shm);
    assert_eq!(free_buffers(s, CANVAS_WIDE, RingKind::Shm), vec![2, 3, b]);
    frame(s, CANVAS_WIDE, RingKind::Shm, 3);
    expect_silence(&mut c, 50);
    frame(s, CANVAS_TALL, RingKind::Shm, 4);
    assert_eq!(expect_frame(&mut c).0.canvas, CANVAS_TALL);

    // Switch the transport to dmabuf: shm holds go, dmabuf canvases arrive, demand moves.
    c.hello(CLIENT_UI, (1 << CANVAS_WIDE) | (1 << CANVAS_TALL), true).unwrap();
    let (a, fa) = expect_canvas(&mut c);
    let (b2, _) = expect_canvas(&mut c);
    assert_eq!((a.canvas, a.drm_fourcc, a.generation, fa.len()), (CANVAS_WIDE, DRM_FORMAT_ABGR8888, 2, 3));
    assert_eq!((b2.canvas, b2.drm_fourcc, b2.generation), (CANVAS_TALL, DRM_FORMAT_ABGR8888, 2));
    wait_until("transport switch", || s.demand(CANVAS_TALL).dmabuf);
    assert_eq!(s.demand(CANVAS_WIDE), Demand { dmabuf: true, shm: false });
    assert_eq!(s.stats().shm_clients, 0);
    assert_eq!(s.stats().dmabuf_clients, 1);
    assert_eq!(free_buffers(s, CANVAS_TALL, RingKind::Shm).len(), 3);
    frame(s, CANVAS_TALL, RingKind::Shm, 5);
    expect_silence(&mut c, 50);
    frame(s, CANVAS_TALL, RingKind::Dmabuf, 6);
    let (m, _) = expect_frame(&mut c);
    assert_eq!((m.canvas, m.seq, m.generation), (CANVAS_TALL, 6, 2));
}

#[test]
fn set_ring_bumps_generation_and_resets_holds() {
    let f = fixture();
    let s = &f.server;
    let first = buffers(4, 1);
    s.set_ring(CANVAS_WIDE, RingKind::Shm, Some(shm_ring(&first))).unwrap();
    let mut c = f.client(CLIENT_OTHER, 1 << CANVAS_WIDE, false);
    assert_eq!(expect_canvas(&mut c).0.generation, 1);
    wait_until("hello", || s.demand(CANVAS_WIDE).shm);
    assert_eq!(frame(s, CANVAS_WIDE, RingKind::Shm, 1), 0);
    expect_frame(&mut c);

    // Resize: a new ring with 3 buffers and a new generation; old holds are forgotten.
    let second = buffers(3, 50);
    s.set_ring(CANVAS_WIDE, RingKind::Shm, Some(shm_ring(&second))).unwrap();
    assert_eq!(s.generation(CANVAS_WIDE, RingKind::Shm), 2);
    let (m, fds) = expect_canvas(&mut c);
    assert_eq!((m.generation, m.buffer_count, fds.len()), (2, 3, 3));
    let v = ShmView::map(fds[0].as_fd(), m.min_buffer_len() as usize).unwrap();
    assert_eq!(v.as_slice()[0], 50, "fds of the new ring");
    assert_eq!(frame(s, CANVAS_WIDE, RingKind::Shm, 10), 0, "old hold forgotten");
    let (fm, _) = expect_frame(&mut c);
    assert_eq!((fm.buffer, fm.generation), (0, 2));

    assert_eq!(frame(s, CANVAS_WIDE, RingKind::Shm, 11), 1);
    expect_frame(&mut c);
    assert_eq!(frame(s, CANVAS_WIDE, RingKind::Shm, 12), 2);
    expect_frame(&mut c);
    // A late release for the old generation's buffer 0 (seq 1) must not free the new
    // buffer 0 (held for seq 10); the following valid release frees buffer 1.
    c.release(CANVAS_WIDE, 0, 1).unwrap();
    c.release(CANVAS_WIDE, 1, 11).unwrap();
    wait_until("release", || !free_buffers(s, CANVAS_WIDE, RingKind::Shm).is_empty());
    assert_eq!(free_buffers(s, CANVAS_WIDE, RingKind::Shm), vec![1]);

    // The counter is per canvas and shared by both kinds.
    s.set_ring(CANVAS_WIDE, RingKind::Dmabuf, Some(dmabuf_ring(&first))).unwrap();
    assert_eq!(s.generation(CANVAS_WIDE, RingKind::Dmabuf), 3);
    assert_eq!(s.generation(CANVAS_WIDE, RingKind::Shm), 2);
    assert_eq!(s.generation(CANVAS_TALL, RingKind::Shm), 0);
    expect_silence(&mut c, 50);

    // Removing the ring sends goodbye(canvas removed) and disables acquire.
    s.set_ring(CANVAS_WIDE, RingKind::Shm, None).unwrap();
    assert_eq!(expect_goodbye(&mut c), Goodbye { reason: GOODBYE_CANVAS_REMOVED, canvas: CANVAS_WIDE });
    assert_eq!(s.acquire(CANVAS_WIDE, RingKind::Shm), None);
    assert!(s.demand(CANVAS_WIDE).shm, "demand is about clients, not rings");
    s.set_ring(CANVAS_WIDE, RingKind::Shm, Some(shm_ring(&second))).unwrap();
    assert_eq!(expect_canvas(&mut c).0.generation, 4);
}

#[test]
fn set_ring_rejects_invalid_descriptions() {
    let f = fixture();
    let s = &f.server;
    let two = buffers(2, 0);
    let err = s.set_ring(CANVAS_WIDE, RingKind::Shm, Some(shm_ring(&two))).unwrap_err();
    assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
    let four = buffers(4, 0);
    assert!(s.set_ring(CANVAS_WIDE, RingKind::Dmabuf, Some(shm_ring(&four))).is_err(), "dmabuf ring without fourcc");
    assert!(s.set_ring(CANVAS_WIDE, RingKind::Shm, Some(dmabuf_ring(&four))).is_err(), "shm ring with a fourcc");
    let mut tall = shm_ring(&four);
    tall.height = H + 1;
    assert!(s.set_ring(CANVAS_WIDE, RingKind::Shm, Some(tall)).is_err(), "buffers smaller than stride * height");
    assert!(s.set_ring(4, RingKind::Shm, Some(shm_ring(&four))).is_err(), "canvas id out of range");
    assert_eq!(s.generation(CANVAS_WIDE, RingKind::Shm), 0);
}

#[test]
fn device_lost_sends_goodbye_and_drops_rings() {
    let f = fixture();
    let s = &f.server;
    let a = buffers(4, 0);
    let b = buffers(4, 0);
    s.set_ring(CANVAS_WIDE, RingKind::Dmabuf, Some(dmabuf_ring(&a))).unwrap();
    s.set_ring(CANVAS_TALL, RingKind::Shm, Some(shm_ring(&b))).unwrap();
    let mut dma = f.client(CLIENT_OBS, 1 << CANVAS_WIDE, true);
    let mut shm = f.client(CLIENT_UI, 1 << CANVAS_TALL, false);
    assert_eq!(expect_canvas(&mut dma).0.generation, 1);
    assert_eq!(expect_canvas(&mut shm).0.generation, 1);
    s.device_lost();
    for c in [&mut dma, &mut shm] {
        assert_eq!(expect_goodbye(c), Goodbye { reason: GOODBYE_DEVICE_LOST, canvas: ALL_CANVASES });
    }
    assert_eq!(s.acquire(CANVAS_WIDE, RingKind::Dmabuf), None);
    assert_eq!(s.acquire(CANVAS_TALL, RingKind::Shm), None);
    s.set_ring(CANVAS_WIDE, RingKind::Dmabuf, Some(dmabuf_ring(&a))).unwrap();
    assert_eq!(expect_canvas(&mut dma).0.generation, 2);
    expect_silence(&mut shm, 50);
}

#[test]
fn stalled_client_is_disconnected_but_reading_client_is_not() {
    let f = fixture_with(ServerOptions { write_timeout: Duration::from_millis(300), ..Default::default() });
    let s = &f.server;
    let bufs = buffers(4, 0);
    s.set_ring(CANVAS_WIDE, RingKind::Shm, Some(shm_ring(&bufs))).unwrap();

    // A reading client that releases each previous frame survives a steady stream.
    let path = f.path.clone();
    let reader = thread::spawn(move || {
        let mut c = FramesClient::connect(&path).unwrap();
        c.hello(CLIENT_UI, 1 << CANVAS_WIDE, false).unwrap();
        let mut prev: Option<FrameMsg> = None;
        let mut frames = 0;
        loop {
            match c.recv(Duration::from_secs(2)) {
                Ok(Some(ClientMsg::Frame { msg, .. })) => {
                    frames += 1;
                    if let Some(p) = prev.replace(msg) {
                        c.release(p.canvas, p.buffer, p.seq).unwrap();
                    }
                }
                Ok(Some(ClientMsg::Goodbye(_))) | Ok(None) | Err(_) => return frames,
                Ok(Some(ClientMsg::Canvas { .. })) => {}
            }
        }
    });
    wait_until("reader hello", || s.demand(CANVAS_WIDE).shm);
    let start = Instant::now();
    let mut seq = 0;
    while start.elapsed() < Duration::from_millis(1000) {
        seq += 1;
        if let Some(b) = s.acquire(CANVAS_WIDE, RingKind::Shm) {
            s.present(CANVAS_WIDE, RingKind::Shm, b, seq, 0, None);
        }
        thread::sleep(Duration::from_millis(4));
    }
    assert_eq!(s.stats().disconnects, 0, "a reading client is never kicked");

    // A client that stops reading gets disconnected after the timeout.
    let mut stuck = f.client(CLIENT_OTHER, 1 << CANVAS_WIDE, false);
    wait_until("stuck hello", || s.stats().shm_clients == 2);
    let stuck_since = Instant::now();
    while s.stats().disconnects == 0 {
        assert!(stuck_since.elapsed() < WAIT, "stuck client was never disconnected");
        seq += 1;
        if let Some(b) = s.acquire(CANVAS_WIDE, RingKind::Shm) {
            s.present(CANVAS_WIDE, RingKind::Shm, b, seq, 0, None);
        }
        thread::sleep(Duration::from_millis(4));
    }
    assert!(stuck_since.elapsed() >= Duration::from_millis(300));
    assert_eq!(s.stats().clients, 1, "only the stuck client was dropped");
    // The stuck client drains what was queued, then sees the connection closed.
    let err = loop {
        match stuck.recv(WAIT) {
            Ok(Some(_)) => {}
            Ok(None) => panic!("no EOF"),
            Err(e) => break e,
        }
    };
    assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);

    // Its holds are gone: frames keep flowing to the reader.
    let before = s.stats().frames_sent;
    for _ in 0..20 {
        seq += 1;
        if let Some(b) = s.acquire(CANVAS_WIDE, RingKind::Shm) {
            s.present(CANVAS_WIDE, RingKind::Shm, b, seq, 0, None);
        }
        thread::sleep(Duration::from_millis(4));
    }
    assert!(s.stats().frames_sent >= before + 15);
    f.server.set_ring(CANVAS_WIDE, RingKind::Shm, None).unwrap();
    assert!(reader.join().unwrap() > 100);
}

#[test]
fn present_never_blocks_when_the_queue_is_full() {
    let f = fixture_with(ServerOptions { queue_capacity: 2, ..Default::default() });
    let s = &f.server;
    let bufs = buffers(4, 0);
    s.set_ring(CANVAS_WIDE, RingKind::Shm, Some(shm_ring(&bufs))).unwrap();
    let mut c = f.client(CLIENT_OTHER, 1 << CANVAS_WIDE, false);
    expect_canvas(&mut c);
    wait_until("hello", || s.demand(CANVAS_WIDE).shm);

    let resume = s.pause();
    assert_eq!(frame(s, CANVAS_WIDE, RingKind::Shm, 1), 0);
    assert_eq!(frame(s, CANVAS_WIDE, RingKind::Shm, 2), 1);
    let b = s.acquire(CANVAS_WIDE, RingKind::Shm).unwrap();
    assert_eq!(b, 2);
    let (rd, wr) = pipe();
    let t = Instant::now();
    s.present(CANVAS_WIDE, RingKind::Shm, b, 3, 0, Some(wr));
    assert!(t.elapsed() < Duration::from_millis(50), "present returned at once");
    assert_eq!(s.stats().frames_dropped, 1);
    // The fence was closed: the pipe's only write end is gone.
    assert!(sys::wait_readable(rd.as_fd(), Duration::ZERO).unwrap());
    let mut byte = 0u8;
    // SAFETY: reads one byte into a live u8.
    let n = unsafe { libc::read(rd.as_raw_fd(), (&raw mut byte).cast(), 1) };
    assert_eq!(n, 0);
    // The dropped buffer is free again; the last presented one is still 1.
    assert_eq!(s.acquire(CANVAS_WIDE, RingKind::Shm), Some(2));
    s.abandon(CANVAS_WIDE, RingKind::Shm, 2);

    drop(resume);
    assert_eq!(expect_frame(&mut c).0.seq, 1);
    assert_eq!(expect_frame(&mut c).0.seq, 2);
    expect_silence(&mut c, 100);
}

#[test]
fn render_thread_calls_do_not_allocate() {
    let f = fixture();
    let s = &f.server;
    let bufs = buffers(4, 0);
    s.set_ring(CANVAS_WIDE, RingKind::Shm, Some(shm_ring(&bufs))).unwrap();
    let mut c = f.client(CLIENT_OTHER, 1 << CANVAS_WIDE, false);
    expect_canvas(&mut c);
    wait_until("hello", || s.demand(CANVAS_WIDE).shm);
    let fences: Vec<OwnedFd> = (0..8).map(|_| sys::eventfd().unwrap()).collect();

    let scope = se_alloc::Scope::begin();
    for (seq, fence) in fences.into_iter().enumerate() {
        let d = s.demand(CANVAS_WIDE);
        assert!(d.shm);
        if let Some(b) = s.acquire(CANVAS_WIDE, RingKind::Shm) {
            s.present(CANVAS_WIDE, RingKind::Shm, b, seq as u64, 0, Some(fence));
        }
        if let Some(b) = s.acquire(CANVAS_WIDE, RingKind::Shm) {
            s.abandon(CANVAS_WIDE, RingKind::Shm, b);
        }
        let _ = s.stats();
    }
    let allocs = scope.allocs();
    drop(scope);
    assert!(se_alloc::installed(), "counting allocator active");
    assert_eq!(allocs, 0, "render-thread API allocated");
}

#[test]
fn protocol_violations_disconnect() {
    let f = fixture();
    let s = &f.server;
    let mut buf = [0u8; 64];

    // Garbage header.
    let raw = sys::connect_seqpacket(&f.path).unwrap();
    sys::send_msg(raw.as_fd(), b"not a frames message", &[]).unwrap();
    // Unknown client kind.
    let mut bad_kind = FramesClient::connect(&f.path).unwrap();
    bad_kind.hello(9, 1, false).unwrap();
    // A server → client message type.
    let wrong_dir = sys::connect_seqpacket(&f.path).unwrap();
    let n = Goodbye::default().encode(&mut buf);
    sys::send_msg(wrong_dir.as_fd(), &buf[..n], &[]).unwrap();
    // Release of an out-of-range canvas.
    let range = FramesClient::connect(&f.path).unwrap();
    range.hello(CLIENT_OTHER, 1, false).unwrap();
    range.release(7, 0, 0).unwrap();
    // Release before hello is merely ignored.
    let early = FramesClient::connect(&f.path).unwrap();
    early.release(0, 0, 0).unwrap();

    wait_until("four disconnects", || s.stats().disconnects == 4);
    for fd in [&raw, &wrong_dir] {
        assert!(sys::wait_readable(fd.as_fd(), WAIT).unwrap());
        assert_eq!(sys::recv_msg(fd.as_fd(), &mut buf, None).unwrap(), sys::RecvOutcome::Closed);
    }
    assert_eq!(bad_kind.recv(WAIT).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    assert_eq!(s.stats().clients, 1, "the early releaser stays connected");
    drop(early);
}

#[test]
fn shutdown_says_goodbye_and_unlinks_the_socket() {
    let f = fixture();
    assert_eq!(FramesServer::start(&f.path).err().map(|e| e.kind()), Some(io::ErrorKind::AddrInUse));
    let mut c = f.client(CLIENT_OBS, 1, true);
    wait_until("hello", || f.server.stats().dmabuf_clients == 1);
    let Fixture { _dir: dir, path, server } = f;
    server.shutdown();
    assert_eq!(expect_goodbye(&mut c), Goodbye { reason: GOODBYE_SHUTDOWN, canvas: ALL_CANVASES });
    assert_eq!(c.recv(WAIT).unwrap_err().kind(), io::ErrorKind::UnexpectedEof);
    assert!(!path.exists(), "socket unlinked");
    // The path is immediately reusable.
    let again = FramesServer::start(&path).unwrap();
    drop(again);
    assert!(!path.exists());
    drop(dir);
}
