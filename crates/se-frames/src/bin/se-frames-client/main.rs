//! `se-frames-client`: a validating `frames.sock` consumer.
//!
//! Connects like the OBS plugin / UI would, validates every message and fd, waits on
//! every fence, releases buffers per protocol, and prints per-canvas stats once per second
//! plus one summary line per wanted canvas at the end. Exit code 0 = all checks passed,
//! 1 = a check failed, 2 = usage or connection error.

mod vk;

use std::collections::VecDeque;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use se_frames::proto::{
    ALL_CANVASES, CANVAS_NAMES, CLIENT_OTHER, DRM_FORMAT_ABGR8888, GOODBYE_CANVAS_REMOVED, GOODBYE_DEVICE_LOST, GOODBYE_SHUTDOWN, MAX_BUFFERS, MAX_CANVASES,
    MIN_BUFFERS, canvas_by_name,
};
use se_frames::{CanvasMsg, ClientMsg, FrameMsg, FramesClient, Goodbye, ShmView, sys, wait_fence};

const USAGE: &str = "\
usage: se-frames-client [options]
  --socket <path>        frames.sock path (default: $SE_FRAMES_SOCKET or
                         $XDG_RUNTIME_DIR/stream-engine/frames.sock)
  --want <list>          canvases: wide,tall,preview,atlas (default wide,tall)
  --shm                  request the shm fallback (no dmabuf flag in hello)
  --seconds <n>          run time (default 5)
  --import               import every dmabuf into Vulkan (NVIDIA or SE_GPU=<name>) and
                         read back the center 16x16 pixels about once per second
  --hold-ms <n>          keep each buffer n ms before releasing it (slow consumer)
  --expect-fps <f>       fail when a wanted canvas averages below f fps
  --check-pattern        expect the test pattern: the center pixel of frame `seq` is
                         RGBA(seq & 255, seq >> 8 & 255, seq >> 16 & 255, 255); shm buffers
                         are checked on every frame and again when released
  -h, --help             this text";

const FENCE_TIMEOUT: Duration = Duration::from_millis(100);
const REPORT_EVERY: Duration = Duration::from_secs(1);

struct Args {
    socket: PathBuf,
    want: u32,
    shm: bool,
    seconds: f64,
    import: bool,
    hold_ms: u64,
    expect_fps: Option<f64>,
    check_pattern: bool,
}

fn parse_args(mut it: impl Iterator<Item = String>) -> Result<Option<Args>, String> {
    let mut args = Args {
        socket: se_frames::default_socket_path(),
        want: 0b11,
        shm: false,
        seconds: 5.0,
        import: false,
        hold_ms: 0,
        expect_fps: None,
        check_pattern: false,
    };
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_string(), Some(v.to_string())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> { inline.clone().or_else(|| it.next()).ok_or_else(|| format!("{name} needs a value")) };
        match flag.as_str() {
            "-h" | "--help" => return Ok(None),
            "--socket" => args.socket = PathBuf::from(value("--socket")?),
            "--want" => {
                let list = value("--want")?;
                let mut mask = 0;
                for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                    let id = canvas_by_name(name).ok_or_else(|| format!("unknown canvas {name:?} in --want"))?;
                    mask |= 1 << id;
                }
                if mask == 0 {
                    return Err("--want names no canvas".into());
                }
                args.want = mask;
            }
            "--shm" => args.shm = true,
            "--import" => args.import = true,
            "--check-pattern" => args.check_pattern = true,
            "--seconds" => {
                args.seconds = value("--seconds")?.parse().ok().filter(|s: &f64| s.is_finite() && *s > 0.0).ok_or("--seconds needs a positive number")?;
            }
            "--hold-ms" => {
                args.hold_ms = value("--hold-ms")?.parse().map_err(|_| "--hold-ms needs a whole number of milliseconds")?;
            }
            "--expect-fps" => {
                args.expect_fps = Some(value("--expect-fps")?.parse().ok().filter(|f: &f64| f.is_finite() && *f >= 0.0).ok_or("--expect-fps needs a number")?);
            }
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    if args.import && args.shm {
        return Err("--import validates dmabufs; it cannot be combined with --shm".into());
    }
    Ok(Some(args))
}

/// Center pixel of the test pattern for frame `seq`.
fn pattern(seq: u64) -> [u8; 4] {
    [seq as u8, (seq >> 8) as u8, (seq >> 16) as u8, 0xff]
}

struct Held {
    buffer: u32,
    seq: u64,
    at: Instant,
}

struct Canvas {
    id: u32,
    name: &'static str,
    /// The current usable canvas (validated); `None` before se_canvas / after goodbye.
    msg: Option<CanvasMsg>,
    fds: Vec<OwnedFd>,
    views: Vec<ShmView>,
    images: Vec<vk::Image>,
    generation: u32,
    canvases: u32,
    held: VecDeque<Held>,
    frames: u64,
    frames_at_report: u64,
    first_frame: Option<Instant>,
    last_seq: Option<u64>,
    fence_timeouts: u64,
    max_fence_wait: Duration,
    last_sample: Option<Instant>,
    imports_ok: u64,
    import_errors: u64,
    invalid: u64,
    ignored: u64,
    pattern_errors: u64,
    seq_errors: u64,
}

impl Canvas {
    fn new(id: u32) -> Canvas {
        Canvas {
            id,
            name: CANVAS_NAMES[id as usize],
            msg: None,
            fds: Vec::new(),
            views: Vec::new(),
            images: Vec::new(),
            generation: 0,
            canvases: 0,
            held: VecDeque::new(),
            frames: 0,
            frames_at_report: 0,
            first_frame: None,
            last_seq: None,
            fence_timeouts: 0,
            max_fence_wait: Duration::ZERO,
            last_sample: None,
            imports_ok: 0,
            import_errors: 0,
            invalid: 0,
            ignored: 0,
            pattern_errors: 0,
            seq_errors: 0,
        }
    }

    /// RGBA at the center of an shm buffer.
    fn shm_center(&self, buffer: u32) -> Option<[u8; 4]> {
        let msg = self.msg.as_ref()?;
        let view = self.views.get(buffer as usize)?;
        let at = msg.offsets[0] as usize + (msg.height / 2) as usize * msg.strides[0] as usize + (msg.width / 2) as usize * 4;
        view.as_slice().get(at..at + 4).map(|p| [p[0], p[1], p[2], p[3]])
    }
}

struct App {
    args: Args,
    client: FramesClient,
    gpu: Option<vk::Gpu>,
    canvases: Vec<Canvas>,
    protocol_errors: u64,
    start: Instant,
    shutdown: bool,
}

fn validate_fd(fd: &OwnedFd, need: u64, shm: bool) -> Result<u64, String> {
    let st = sys::fstat(fd.as_fd()).map_err(|e| format!("fstat: {e}"))?;
    if shm && st.st_mode & libc::S_IFMT != libc::S_IFREG {
        return Err("shm buffer fd is not a memfd".into());
    }
    let size = sys::fd_size(fd.as_fd()).map_err(|e| format!("lseek(SEEK_END): {e}"))?;
    if size < need {
        return Err(format!("{size} bytes < offset + stride * height = {need}"));
    }
    Ok(size)
}

fn fourcc(code: u32) -> String {
    if code == 0 {
        return "shm".into();
    }
    code.to_le_bytes().iter().map(|&b| if b.is_ascii_graphic() { b as char } else { '?' }).collect()
}

impl App {
    fn t(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn wanted(&self, canvas: u32) -> bool {
        (canvas as usize) < MAX_CANVASES && self.args.want & (1 << canvas) != 0
    }

    fn release(&self, canvas: u32, buffer: u32, seq: u64) {
        if let Err(e) = self.client.release(canvas, buffer, seq) {
            eprintln!("release canvas={canvas} buffer={buffer} seq={seq} failed: {e}");
        }
    }

    /// Forgets the current canvas buffers (after goodbye or before a new generation).
    fn drop_canvas(&mut self, ci: usize, release_held: bool) {
        let held: Vec<Held> = self.canvases[ci].held.drain(..).collect();
        if release_held {
            // The server ignores releases of an older generation; sending them is correct.
            for h in held {
                self.release(ci as u32, h.buffer, h.seq);
            }
        }
        let st = &mut self.canvases[ci];
        st.msg = None;
        st.views.clear();
        st.fds.clear();
        for img in st.images.drain(..) {
            if let Some(gpu) = &self.gpu {
                gpu.destroy(img);
            }
        }
    }

    fn on_canvas(&mut self, msg: CanvasMsg, fds: Vec<OwnedFd>) {
        if !self.wanted(msg.canvas) {
            self.protocol_errors += 1;
            eprintln!("se_canvas for unwanted canvas {}", msg.canvas);
            return;
        }
        let ci = msg.canvas as usize;
        self.drop_canvas(ci, true);
        let shm = self.args.shm;
        let t = self.t();
        let st = &mut self.canvases[ci];
        st.canvases += 1;
        println!(
            "[{t:6.2}s] {} se_canvas gen={} {}x{} format={} modifier={:#x} offset={} stride={} planes={} buffers={}",
            st.name,
            msg.generation,
            msg.width,
            msg.height,
            fourcc(msg.drm_fourcc),
            msg.modifier,
            msg.offsets[0],
            msg.strides[0],
            msg.planes,
            msg.buffer_count
        );
        let mut problems = Vec::new();
        if msg.generation <= st.generation {
            problems.push(format!("generation {} does not increase (had {})", msg.generation, st.generation));
        }
        if shm && msg.drm_fourcc != 0 {
            problems.push("dmabuf canvas sent to a client without the dmabuf flag".into());
        }
        if !shm && msg.drm_fourcc == 0 {
            problems.push("shm canvas sent to a client with the dmabuf flag".into());
        }
        if !shm && msg.drm_fourcc != 0 && msg.drm_fourcc != DRM_FORMAT_ABGR8888 {
            problems.push(format!("unexpected fourcc {}", fourcc(msg.drm_fourcc)));
        }
        if msg.planes != 1 {
            problems.push(format!("planes = {}, expected 1", msg.planes));
        }
        if !(MIN_BUFFERS..=MAX_BUFFERS).contains(&(msg.buffer_count as usize)) {
            problems.push(format!("buffer_count {} outside {MIN_BUFFERS}..={MAX_BUFFERS}", msg.buffer_count));
        }
        if msg.width == 0 || msg.height == 0 {
            problems.push("empty canvas".into());
        }
        if msg.drm_fourcc == 0 && u64::from(msg.strides[0]) < u64::from(msg.width) * 4 {
            problems.push(format!("stride {} < width * 4", msg.strides[0]));
        }
        let need = msg.min_buffer_len();
        for (i, fd) in fds.iter().enumerate() {
            if let Err(e) = validate_fd(fd, need, msg.drm_fourcc == 0) {
                problems.push(format!("buffer {i}: {e}"));
            }
        }
        st.generation = st.generation.max(msg.generation);
        if !problems.is_empty() {
            for p in &problems {
                eprintln!("[{t:6.2}s] {} INVALID se_canvas: {p}", st.name);
            }
            st.invalid += problems.len() as u64;
            return;
        }

        if msg.drm_fourcc == 0 {
            for (i, fd) in fds.iter().enumerate() {
                match ShmView::map(fd.as_fd(), need as usize) {
                    Ok(v) => st.views.push(v),
                    Err(e) => {
                        eprintln!("[{t:6.2}s] {} INVALID buffer {i}: mmap: {e}", st.name);
                        st.invalid += 1;
                        st.views.clear();
                        return;
                    }
                }
            }
        } else if let Some(gpu) = &self.gpu {
            for (i, fd) in fds.iter().enumerate() {
                match gpu.import(fd.as_fd(), &msg) {
                    Ok(img) => {
                        st.images.push(img);
                        st.imports_ok += 1;
                    }
                    Err(e) => {
                        eprintln!("[{t:6.2}s] {} IMPORT FAILED buffer {i}: {e:#}", st.name);
                        st.import_errors += 1;
                    }
                }
            }
            if st.images.len() == fds.len() {
                println!("[{t:6.2}s] {} imported {} dmabufs on {}", st.name, fds.len(), gpu.name);
            } else {
                for img in st.images.drain(..) {
                    gpu.destroy(img);
                }
            }
        }
        st.fds = fds;
        st.msg = Some(msg);
    }

    fn on_frame(&mut self, msg: FrameMsg, fence: Option<OwnedFd>, now: Instant) {
        if !self.wanted(msg.canvas) {
            self.protocol_errors += 1;
            eprintln!("se_frame for unwanted canvas {}", msg.canvas);
            self.release(msg.canvas, msg.buffer, msg.seq);
            return;
        }
        let ci = msg.canvas as usize;
        let t = self.t();
        let st = &mut self.canvases[ci];
        let Some(canvas) = st.msg.filter(|c| c.generation == msg.generation) else {
            // Older generation (or a canvas we could not use): not ours to sample.
            st.ignored += 1;
            self.release(msg.canvas, msg.buffer, msg.seq);
            return;
        };
        if msg.buffer >= canvas.buffer_count {
            self.protocol_errors += 1;
            eprintln!("[{t:6.2}s] {} se_frame buffer {} >= buffer_count {}", st.name, msg.buffer, canvas.buffer_count);
            self.release(msg.canvas, msg.buffer, msg.seq);
            return;
        }

        let mut ready = true;
        if let Some(fence) = &fence {
            let t0 = Instant::now();
            match wait_fence(fence, FENCE_TIMEOUT) {
                Ok(true) => {}
                Ok(false) => {
                    st.fence_timeouts += 1;
                    ready = false;
                }
                Err(e) => {
                    eprintln!("[{t:6.2}s] {} fence poll failed: {e}", st.name);
                    st.fence_timeouts += 1;
                    ready = false;
                }
            }
            st.max_fence_wait = st.max_fence_wait.max(t0.elapsed());
        }
        drop(fence);

        st.frames += 1;
        st.first_frame.get_or_insert(now);
        if let Some(prev) = st.last_seq
            && msg.seq <= prev
        {
            st.seq_errors += 1;
            eprintln!("[{t:6.2}s] {} seq {} after {prev}", st.name, msg.seq);
        }
        st.last_seq = Some(msg.seq);

        if ready {
            if self.args.check_pattern && !st.views.is_empty() {
                let got = st.shm_center(msg.buffer);
                if got != Some(pattern(msg.seq)) {
                    st.pattern_errors += 1;
                    if st.pattern_errors <= 5 {
                        eprintln!(
                            "[{t:6.2}s] {} PATTERN MISMATCH seq={} buffer={} center={:?} expected={:?}",
                            st.name,
                            msg.seq,
                            msg.buffer,
                            got,
                            pattern(msg.seq)
                        );
                    }
                }
            }
            let due = st.last_sample.is_none_or(|s| now.duration_since(s) >= REPORT_EVERY);
            if due {
                st.last_sample = Some(now);
                if !st.views.is_empty() {
                    if let Some(c) = st.shm_center(msg.buffer) {
                        println!("[{t:6.2}s] {} seq={} buffer={} center=RGBA({},{},{},{})", st.name, msg.seq, msg.buffer, c[0], c[1], c[2], c[3]);
                    }
                } else if let (Some(gpu), Some(img)) = (&self.gpu, st.images.get(msg.buffer as usize)) {
                    match gpu.sample(img) {
                        Ok(s) => {
                            let c = s.center;
                            println!(
                                "[{t:6.2}s] {} seq={} buffer={} center=RGBA({},{},{},{}) checksum={:#010x}",
                                st.name, msg.seq, msg.buffer, c[0], c[1], c[2], c[3], s.checksum
                            );
                            if self.args.check_pattern && c != pattern(msg.seq) {
                                st.pattern_errors += 1;
                                eprintln!("[{t:6.2}s] {} PATTERN MISMATCH seq={} expected={:?}", st.name, msg.seq, pattern(msg.seq));
                            }
                        }
                        Err(e) => {
                            st.import_errors += 1;
                            eprintln!("[{t:6.2}s] {} readback failed: {e:#}", st.name);
                        }
                    }
                }
            }
        }

        st.held.push_back(Held { buffer: msg.buffer, seq: msg.seq, at: now });
        self.release_due(ci, now);
    }

    /// Releases every held frame except the newest once it was held `--hold-ms`.
    fn release_due(&mut self, ci: usize, now: Instant) {
        let hold = Duration::from_millis(self.args.hold_ms);
        loop {
            let st = &mut self.canvases[ci];
            if st.held.len() < 2 || st.held[0].at + hold > now {
                return;
            }
            let Some(h) = st.held.pop_front() else {
                return;
            };
            if self.args.check_pattern && !st.views.is_empty() && st.shm_center(h.buffer) != Some(pattern(h.seq)) {
                st.pattern_errors += 1;
                eprintln!("{} buffer {} (seq {}) was overwritten while held", st.name, h.buffer, h.seq);
            }
            self.release(ci as u32, h.buffer, h.seq);
        }
    }

    fn next_release(&self) -> Option<Instant> {
        let hold = Duration::from_millis(self.args.hold_ms);
        self.canvases.iter().filter(|c| c.held.len() >= 2).map(|c| c.held[0].at + hold).min()
    }

    fn on_goodbye(&mut self, g: Goodbye) {
        let t = self.t();
        match g.reason {
            GOODBYE_SHUTDOWN => {
                println!("[{t:6.2}s] se_goodbye: engine shutting down");
                self.shutdown = true;
            }
            GOODBYE_DEVICE_LOST | GOODBYE_CANVAS_REMOVED => {
                let what = if g.reason == GOODBYE_DEVICE_LOST { "device lost" } else { "canvas removed" };
                println!(
                    "[{t:6.2}s] se_goodbye: {what} (canvas {}); waiting for a new se_canvas",
                    if g.canvas == ALL_CANVASES { "all".to_string() } else { g.canvas.to_string() }
                );
                for ci in 0..MAX_CANVASES {
                    if g.canvas == ALL_CANVASES || g.canvas == ci as u32 {
                        self.drop_canvas(ci, false);
                    }
                }
            }
            other => {
                self.protocol_errors += 1;
                eprintln!("[{t:6.2}s] se_goodbye with unknown reason {other}");
            }
        }
    }

    fn report(&mut self, since: Duration) {
        let t = self.t();
        for st in self.canvases.iter_mut() {
            if self.args.want & (1 << st.id) == 0 {
                continue;
            }
            let window = st.frames - st.frames_at_report;
            st.frames_at_report = st.frames;
            println!(
                "[{t:6.2}s] {} gen={} frames={} fps={:.1} held={} fence_timeouts={} max_fence_wait_ms={:.2}",
                st.name,
                st.generation,
                st.frames,
                window as f64 / since.as_secs_f64(),
                st.held.len(),
                st.fence_timeouts,
                st.max_fence_wait.as_secs_f64() * 1e3
            );
        }
    }

    fn run(&mut self) -> Result<(), io::Error> {
        let end = self.start + Duration::from_secs_f64(self.args.seconds);
        let mut last_report = self.start;
        loop {
            let now = Instant::now();
            for ci in 0..MAX_CANVASES {
                self.release_due(ci, now);
            }
            if now >= last_report + REPORT_EVERY {
                self.report(now - last_report);
                last_report = now;
            }
            if now >= end || self.shutdown {
                return Ok(());
            }
            let mut wake = end.min(last_report + REPORT_EVERY);
            if let Some(r) = self.next_release() {
                wake = wake.min(r);
            }
            match self.client.recv(wake.saturating_duration_since(now)) {
                Ok(Some(ClientMsg::Canvas { msg, fds })) => self.on_canvas(msg, fds),
                Ok(Some(ClientMsg::Frame { msg, fence })) => self.on_frame(msg, fence, Instant::now()),
                Ok(Some(ClientMsg::Goodbye(g))) => self.on_goodbye(g),
                Ok(None) => {}
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {
                    self.protocol_errors += 1;
                    eprintln!("[{:6.2}s] protocol error: {e}", self.t());
                }
                Err(e) => return Err(e),
            }
        }
    }

    /// Prints the summary lines; returns whether every check passed.
    fn summary(&mut self) -> bool {
        let end = Instant::now();
        let mut ok = true;
        if self.protocol_errors > 0 {
            eprintln!("FAIL: {} protocol errors", self.protocol_errors);
            ok = false;
        }
        for ci in 0..MAX_CANVASES {
            if !self.wanted(ci as u32) {
                continue;
            }
            let st = &self.canvases[ci];
            let fps = match st.first_frame {
                Some(first) if st.frames > 0 => st.frames as f64 / end.duration_since(first).as_secs_f64().max(1e-3),
                _ => 0.0,
            };
            let import = if !self.args.import {
                "off"
            } else if st.imports_ok > 0 && st.import_errors == 0 {
                "ok"
            } else {
                "fail"
            };
            println!(
                "canvas={} frames={} fps={:.2} gen={} fence_timeouts={} max_fence_wait_ms={:.2} import={}",
                st.name,
                st.frames,
                fps,
                st.generation,
                st.fence_timeouts,
                st.max_fence_wait.as_secs_f64() * 1e3,
                import
            );
            if st.ignored > 0 {
                println!("canvas={} ignored_frames={} (older generation)", st.name, st.ignored);
            }
            let mut fail = |msg: String| {
                eprintln!("FAIL canvas={}: {msg}", st.name);
                ok = false;
            };
            if st.canvases == 0 {
                fail(format!("no se_canvas arrived within {:.1}s", self.args.seconds));
            }
            if self.args.import && import == "fail" {
                if st.imports_ok == 0 && st.import_errors == 0 {
                    fail("--import: no dmabuf se_canvas arrived, nothing was imported".into());
                } else {
                    fail(format!("--import: {} import/readback errors, {} buffers imported", st.import_errors, st.imports_ok));
                }
            }
            if let Some(min) = self.args.expect_fps
                && fps < min
            {
                fail(format!("averaged {fps:.2} fps, expected at least {min}"));
            }
            if st.invalid > 0 {
                fail(format!("{} invalid se_canvas fields/fds", st.invalid));
            }
            if st.pattern_errors > 0 {
                fail(format!("{} test pattern mismatches", st.pattern_errors));
            }
            if st.seq_errors > 0 {
                fail(format!("{} non-increasing seqs", st.seq_errors));
            }
        }
        ok
    }
}

fn main() -> ExitCode {
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Some(a)) => a,
        Ok(None) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(e) => {
            eprintln!("se-frames-client: {e}\n{USAGE}");
            return ExitCode::from(2);
        }
    };
    let gpu = if args.import {
        match vk::Gpu::new() {
            Ok(g) => {
                println!("vulkan: importing on {}", g.name);
                Some(g)
            }
            Err(e) => {
                eprintln!("se-frames-client: Vulkan setup for --import failed: {e:#}");
                None
            }
        }
    } else {
        None
    };
    let client = match FramesClient::connect(&args.socket) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("se-frames-client: {e} (is the engine running?)");
            return ExitCode::from(2);
        }
    };
    if let Err(e) = client.hello(CLIENT_OTHER, args.want, !args.shm) {
        eprintln!("se-frames-client: sending hello: {e}");
        return ExitCode::from(2);
    }
    let names: Vec<&str> = (0..MAX_CANVASES).filter(|&i| args.want & (1 << i) != 0).map(|i| CANVAS_NAMES[i]).collect();
    println!("connected to {} want={} transport={} seconds={}", args.socket.display(), names.join(","), if args.shm { "shm" } else { "dmabuf" }, args.seconds);
    let mut app =
        App { args, client, gpu, canvases: (0..MAX_CANVASES as u32).map(Canvas::new).collect(), protocol_errors: 0, start: Instant::now(), shutdown: false };
    if let Err(e) = app.run() {
        eprintln!("[{:6.2}s] connection ended: {e}", app.t());
    }
    let ok = app.summary();
    for ci in 0..MAX_CANVASES {
        app.drop_canvas(ci, false);
    }
    if ok { ExitCode::SUCCESS } else { ExitCode::from(1) }
}
