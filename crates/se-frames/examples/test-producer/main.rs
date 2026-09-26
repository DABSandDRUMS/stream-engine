//! Engine stand-in for testing `frames.sock` clients (OBS plugin, UI, se-frames-client)
//! without the compositor: serves the canvases at a fixed rate, filled with the
//! `--check-pattern` test pattern (every pixel of frame `seq` is
//! RGBA(seq & 255, seq >> 8 & 255, seq >> 16 & 255, 255)).
//!
//! The shm ring is always served. With `--dmabuf` the dmabuf ring is served too: Vulkan
//! images with a DRM format modifier exported as dmabufs, cleared on the GPU each frame,
//! with a sync_file fence per frame.
//!
//! ```text
//! cargo run -p se-frames --example test-producer -- --socket /tmp/f.sock --fps 60 [--dmabuf]
//! ```

mod gpu;

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use se_frames::proto::{CANVAS_NAMES, canvas_by_name};
use se_frames::{FramesServer, RingDesc, RingKind, ShmBuffer};

struct Args {
    socket: PathBuf,
    canvases: Vec<u32>,
    width: u32,
    height: u32,
    fps: f64,
    seconds: f64,
    buffers: usize,
    dmabuf: bool,
    recreate_every: Option<f64>,
}

const USAGE: &str = "usage: test-producer [--socket <path>] [--canvases wide,tall,...] \
[--size WxH] [--fps F] [--seconds N (0 = forever)] [--buffers 3|4] [--dmabuf] \
[--recreate-every S (alternately resize to half size and simulate device loss)]";

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        socket: se_frames::default_socket_path(),
        canvases: vec![0, 1],
        width: 640,
        height: 360,
        fps: 60.0,
        seconds: 0.0,
        buffers: 4,
        dmabuf: false,
        recreate_every: None,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        let mut value = || it.next().ok_or(format!("{flag} needs a value"));
        match flag.as_str() {
            "--socket" => a.socket = value()?.into(),
            "--canvases" => {
                a.canvases = value()?.split(',').map(|n| canvas_by_name(n.trim()).ok_or(format!("unknown canvas {n:?}"))).collect::<Result<_, _>>()?;
            }
            "--size" => {
                let v = value()?;
                let (w, h) = v.split_once('x').ok_or("--size needs WxH")?;
                a.width = w.parse().map_err(|_| "bad width")?;
                a.height = h.parse().map_err(|_| "bad height")?;
            }
            "--fps" => a.fps = value()?.parse().map_err(|_| "bad --fps")?,
            "--seconds" => a.seconds = value()?.parse().map_err(|_| "bad --seconds")?,
            "--buffers" => a.buffers = value()?.parse().map_err(|_| "bad --buffers")?,
            "--dmabuf" => a.dmabuf = true,
            "--recreate-every" => a.recreate_every = Some(value()?.parse().map_err(|_| "bad --recreate-every")?),
            "-h" | "--help" => return Err(USAGE.into()),
            other => return Err(format!("unknown argument {other:?}\n{USAGE}")),
        }
    }
    if !a.fps.is_finite() || a.fps <= 0.0 || a.width == 0 || a.height == 0 {
        return Err("fps and size must be positive".into());
    }
    Ok(a)
}

fn pattern(seq: u64) -> [u8; 4] {
    [seq as u8, (seq >> 8) as u8, (seq >> 16) as u8, 0xff]
}

fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: ts is writable.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Fills a buffer of `stride`-byte rows with `color` (one row by hand, then memcpy).
fn fill(buf: &mut [u8], stride: usize, color: [u8; 4]) {
    let (first, rest) = buf.split_at_mut(stride);
    for px in first.as_chunks_mut::<4>().0 {
        *px = color;
    }
    for row in rest.chunks_exact_mut(stride) {
        row.copy_from_slice(first);
    }
}

struct Canvas {
    id: u32,
    stride: u32,
    shm: Vec<ShmBuffer>,
    dmabuf: Option<gpu::Ring>,
    shm_frames: u64,
    dmabuf_frames: u64,
    skipped: u64,
}

impl Canvas {
    /// (Re)creates both rings at `width × height` and hands them to the server.
    fn install(&mut self, server: &FramesServer, gpu: Option<&mut gpu::Gpu>, width: u32, height: u32, buffers: usize) -> anyhow::Result<()> {
        self.stride = width * 4;
        self.shm = (0..buffers).map(|_| ShmBuffer::new(self.stride, height)).collect::<std::io::Result<Vec<_>>>()?;
        let fds = self.shm.iter().map(ShmBuffer::try_clone_fd).collect::<std::io::Result<Vec<_>>>()?;
        server.set_ring(self.id, RingKind::Shm, Some(RingDesc::shm(width, height, self.stride, fds)))?;
        if let Some(g) = gpu {
            let ring = g.create_ring(width, height, buffers)?;
            server.set_ring(self.id, RingKind::Dmabuf, Some(ring.desc()?))?;
            if let Some(old) = self.dmabuf.replace(ring) {
                g.destroy_ring(old);
            }
        }
        Ok(())
    }
}

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    match run(args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("test-producer: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> anyhow::Result<()> {
    let server = FramesServer::start(&args.socket)?;
    let mut gpu = if args.dmabuf { Some(gpu::Gpu::new()?) } else { None };
    let mut canvases = Vec::new();
    for &id in &args.canvases {
        let mut c = Canvas { id, stride: 0, shm: Vec::new(), dmabuf: None, shm_frames: 0, dmabuf_frames: 0, skipped: 0 };
        c.install(&server, gpu.as_mut(), args.width, args.height, args.buffers)?;
        canvases.push(c);
    }
    let names: Vec<&str> = args.canvases.iter().map(|&i| CANVAS_NAMES[i as usize]).collect();
    println!(
        "serving {} {}x{} at {} fps on {} (shm{})",
        names.join(","),
        args.width,
        args.height,
        args.fps,
        args.socket.display(),
        if gpu.is_some() { " + dmabuf" } else { "" }
    );

    let period = Duration::from_secs_f64(1.0 / args.fps);
    let start = Instant::now();
    let end = (args.seconds > 0.0).then(|| start + Duration::from_secs_f64(args.seconds));
    let mut next = start;
    let mut next_report = start + Duration::from_secs(1);
    let recreate_every = args.recreate_every.map(Duration::from_secs_f64);
    let mut next_recreate = recreate_every.map(|d| start + d);
    let mut recreations = 0u32;
    let mut seq = 0u64;
    loop {
        seq += 1;
        let mono = monotonic_ns();
        let color = pattern(seq);
        for c in &mut canvases {
            let demand = server.demand(c.id);
            if demand.shm {
                match server.acquire(c.id, RingKind::Shm) {
                    Some(b) => {
                        fill(c.shm[b as usize].as_mut_slice(), c.stride as usize, color);
                        server.present(c.id, RingKind::Shm, b, seq, mono, None);
                        c.shm_frames += 1;
                    }
                    None => c.skipped += 1,
                }
            }
            if let (true, Some(ring), Some(g)) = (demand.dmabuf, c.dmabuf.as_mut(), gpu.as_mut()) {
                match server.acquire(c.id, RingKind::Dmabuf) {
                    Some(b) => match g.render(ring, b as usize, color) {
                        Ok(fence) => {
                            server.present(c.id, RingKind::Dmabuf, b, seq, mono, fence);
                            c.dmabuf_frames += 1;
                        }
                        Err(e) => {
                            server.abandon(c.id, RingKind::Dmabuf, b);
                            return Err(e);
                        }
                    },
                    None => c.skipped += 1,
                }
            }
        }
        let now = Instant::now();
        if now >= next_report {
            let s = server.stats();
            let per: Vec<String> = canvases
                .iter()
                .map(|c| format!("{}: shm={} dmabuf={} skipped={}", CANVAS_NAMES[c.id as usize], c.shm_frames, c.dmabuf_frames, c.skipped))
                .collect();
            println!(
                "[{:6.2}s] clients={} (dmabuf {}, shm {}) sent={} dropped={} disconnects={} | {}",
                now.duration_since(start).as_secs_f64(),
                s.clients,
                s.dmabuf_clients,
                s.shm_clients,
                s.frames_sent,
                s.frames_dropped,
                s.disconnects,
                per.join(" | ")
            );
            next_report += Duration::from_secs(1);
        }
        if end.is_some_and(|e| now >= e) {
            break;
        }
        if let (Some(at), Some(every)) = (next_recreate, recreate_every)
            && now >= at
        {
            recreations += 1;
            let (w, h) = if recreations % 2 == 1 {
                println!("recreating the rings at half size");
                (args.width / 2, args.height / 2)
            } else {
                println!("simulating device loss, then recreating at full size");
                server.device_lost();
                (args.width, args.height)
            };
            for c in &mut canvases {
                c.install(&server, gpu.as_mut(), w, h, args.buffers)?;
            }
            next_recreate = Some(at + every);
        }
        next += period;
        match next.checked_duration_since(Instant::now()) {
            Some(d) => std::thread::sleep(d),
            None => next = Instant::now(), // fell behind: do not burst
        }
    }
    for c in &mut canvases {
        if let (Some(ring), Some(g)) = (c.dmabuf.take(), gpu.as_mut()) {
            g.destroy_ring(ring);
        }
    }
    server.shutdown();
    Ok(())
}
