//! SHM-only frames.sock consumer feeding a bounded, low-latency H264 publisher.

use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, ChildStdin, Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use se_frames::proto::{self, CLIENT_OTHER, MAX_BUFFERS, MIN_BUFFERS};
use se_frames::{CanvasMsg, ClientMsg, FrameMsg, FramesClient, ShmView, sys, wait_fence};

const WRITE_TIMEOUT: Duration = Duration::from_millis(100);
const MAX_FRAME_AGE_NS: u64 = 100_000_000;
const IDLE_TIMEOUT: Duration = Duration::from_secs(2);
const DRAIN_LIMIT: usize = 32;
const USAGE: &str = "usage: se-camera-stream [options]
  --socket <path>        frames.sock (default: standard engine runtime socket)
  --canvas <name>        wide, tall, preview, atlas (default: preview)
  --fps <integer>        output rate, 1..=60 (default: 30)
  --rtsp-url <url>       output (default: rtsp://127.0.0.1:18554/camera)
  --ffmpeg <path>        FFmpeg executable (default: ffmpeg)
  --vaapi-device <path>  use H264 VAAPI on this absolute DRM render-node path (default: NVENC)
  --audio <source>       also publish this PulseAudio/PipeWire source as Opus (e.g. se-program)
  -h, --help            show this help
Exits on disconnect, canvas replacement, encoder failure or stalled input/output.
Use systemd Restart=always for unattended reconnects.";

struct Args {
    socket: PathBuf,
    canvas: u32,
    fps: u32,
    rtsp_url: String,
    ffmpeg: PathBuf,
    audio: Option<String>,
    vaapi_device: Option<PathBuf>,
}

fn parse_args() -> Result<Option<Args>> {
    parse_args_from(std::env::args().skip(1))
}

fn parse_args_from(input: impl IntoIterator<Item = String>) -> Result<Option<Args>> {
    let mut args = Args {
        socket: se_frames::default_socket_path(),
        canvas: proto::CANVAS_PREVIEW,
        fps: 30,
        rtsp_url: "rtsp://127.0.0.1:18554/camera".into(),
        ffmpeg: "ffmpeg".into(),
        audio: None,
        vaapi_device: None,
    };
    let mut input = input.into_iter();
    while let Some(flag) = input.next() {
        if flag == "-h" || flag == "--help" {
            println!("{USAGE}");
            return Ok(None);
        }
        ensure!(matches!(flag.as_str(), "--socket" | "--canvas" | "--fps" | "--rtsp-url" | "--ffmpeg" | "--audio" | "--vaapi-device"), "unknown option {flag}\n{USAGE}");
        let value = input.next().with_context(|| format!("missing value for {flag}"))?;
        match flag.as_str() {
            "--socket" => args.socket = value.into(),
            "--canvas" => args.canvas = proto::canvas_by_name(&value).with_context(|| format!("unknown canvas {value}"))?,
            "--fps" => {
                args.fps = value.parse().context("fps must be an integer")?;
                ensure!((1..=60).contains(&args.fps), "fps must be between 1 and 60");
            }
            "--rtsp-url" => {
                ensure!(value.starts_with("rtsp://"), "output must be an rtsp:// URL");
                args.rtsp_url = value;
            }
            "--ffmpeg" => args.ffmpeg = value.into(),
            "--vaapi-device" => {
                let device = PathBuf::from(value);
                ensure!(device.is_absolute(), "vaapi-device must be an absolute DRM render-node path");
                args.vaapi_device = Some(device);
            }
            "--audio" => {
                ensure!(!value.is_empty(), "audio source must not be empty");
                args.audio = Some(value);
            }
            _ => unreachable!(),
        }
    }
    Ok(Some(args))
}

#[derive(Clone, Copy)]
struct Layout {
    offset: usize,
    stride: usize,
    row_bytes: usize,
    height: usize,
    len: usize,
}

impl Layout {
    fn new(msg: &CanvasMsg) -> Result<Self> {
        ensure!(msg.drm_fourcc == 0 && msg.planes == 1 && msg.modifier == 0, "expected single-plane linear RGBA SHM");
        ensure!(msg.width > 0 && msg.height > 0 && msg.width % 2 == 0 && msg.height % 2 == 0, "H264 baseline requires nonempty, even canvas dimensions");
        ensure!(msg.offsets[1..] == [0; 3] && msg.strides[1..] == [0; 3], "unexpected unused-plane metadata");
        ensure!((MIN_BUFFERS..=MAX_BUFFERS).contains(&(msg.buffer_count as usize)), "invalid buffer count {}", msg.buffer_count);
        let row_bytes = (msg.width as usize).checked_mul(4).context("RGBA row size overflow")?;
        let stride = msg.strides[0] as usize;
        ensure!(stride >= row_bytes, "SHM stride {stride} is shorter than RGBA row {row_bytes}");
        let offset = msg.offsets[0] as usize;
        let height = msg.height as usize;
        let len = stride.checked_mul(height).and_then(|size| offset.checked_add(size)).filter(|&size| size <= isize::MAX as usize).context("SHM mapping size overflow")?;
        Ok(Self { offset, stride, row_bytes, height, len })
    }

    /// The next tight rawvideo span; padding never reaches FFmpeg. Tight canvases
    /// use one contiguous slice instead of a row copy or one syscall per row.
    fn span<'a>(&self, bytes: &'a [u8], written: usize) -> &'a [u8] {
        if self.stride == self.row_bytes {
            return &bytes[self.offset + written..self.offset + self.row_bytes * self.height];
        }
        let row = written / self.row_bytes;
        let column = written % self.row_bytes;
        let start = self.offset + row * self.stride + column;
        &bytes[start..self.offset + row * self.stride + self.row_bytes]
    }
}

struct Canvas {
    msg: CanvasMsg,
    layout: Layout,
    views: Vec<ShmView>,
}

impl Canvas {
    fn map(msg: CanvasMsg, fds: Vec<OwnedFd>, wanted: u32) -> Result<Self> {
        ensure!(msg.canvas == wanted, "unexpected canvas {}", msg.canvas);
        let layout = Layout::new(&msg)?;
        ensure!(fds.len() == msg.buffer_count as usize, "canvas fd count mismatch");
        let mut views = Vec::with_capacity(fds.len());
        for (index, fd) in fds.iter().enumerate() {
            let stat = sys::fstat(fd.as_fd())?;
            ensure!(stat.st_mode & libc::S_IFMT == libc::S_IFREG, "SHM buffer {index} is not a regular memfd");
            views.push(ShmView::map(fd.as_fd(), layout.len).with_context(|| format!("mapping SHM buffer {index}"))?);
        }
        Ok(Self { msg, layout, views })
    }

    fn validate_frame(&self, frame: &FrameMsg) -> Result<()> {
        ensure!(frame.canvas == self.msg.canvas, "unexpected frame canvas {}", frame.canvas);
        ensure!(frame.generation <= self.msg.generation, "frame arrived before its canvas generation");
        if frame.generation == self.msg.generation {
            ensure!((frame.buffer as usize) < self.views.len(), "frame buffer {} is outside the ring", frame.buffer);
        }
        Ok(())
    }
}

struct Encoder {
    child: Child,
    input: Option<ChildStdin>,
    warmup_until: Instant,
}

impl Encoder {
    fn spawn(args: &Args, canvas: &Canvas) -> Result<Self> {
        let mut command = Command::new(&args.ffmpeg);
        if let Some(device) = &args.vaapi_device {
            // Initialize the selected device before any input; never fall back to NVENC.
            command.arg("-vaapi_device").arg(device);
        }
        command.args(["-hide_banner", "-loglevel", "warning", "-nostdin", "-fflags", "nobuffer", "-probesize", "32", "-analyzeduration", "0", "-f", "rawvideo", "-pixel_format", "rgba", "-video_size"])
            .arg(format!("{}x{}", canvas.msg.width, canvas.msg.height))
            .arg("-framerate").arg(args.fps.to_string())
            .args(["-i", "pipe:0"]);
        if let Some(source) = &args.audio {
            // Live mix for monitoring: Opus is what the WebRTC viewer can play.
            command.args(["-thread_queue_size", "64", "-f", "pulse", "-fragment_size", "1920", "-i"]).arg(source)
                .args(["-map", "0:v:0", "-map", "1:a:0", "-c:a", "libopus", "-b:a", "128k", "-ar", "48000", "-ac", "2", "-application", "lowdelay", "-frame_duration", "20"]);
        } else {
            command.arg("-an");
        }
        if args.vaapi_device.is_some() {
            command.args(["-vf", "format=nv12,hwupload", "-c:v", "h264_vaapi", "-profile:v", "constrained_baseline", "-rc_mode", "CBR", "-b:v", "4M", "-maxrate", "4M", "-bufsize", "133k", "-g"])
                .arg(args.fps.to_string())
                .args(["-bf", "0", "-async_depth", "1"]);
        } else {
            command.args(["-vf", "format=yuv420p", "-c:v", "h264_nvenc", "-profile:v", "baseline", "-preset", "p1", "-tune", "ull", "-rc", "cbr", "-b:v", "4M", "-maxrate", "4M", "-bufsize", "133k", "-g"])
                .arg(args.fps.to_string())
                .args(["-bf", "0", "-rc-lookahead", "0", "-zerolatency", "1", "-delay", "0", "-forced-idr", "1"]);
        }
        command.args(["-flush_packets", "1", "-f", "rtsp", "-rtsp_transport", "tcp"])
            .arg(&args.rtsp_url)
            .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::inherit());
        let parent_pid = std::process::id() as libc::pid_t;
        // SAFETY: only async-signal-safe Linux syscalls run between fork and exec.
        // A terminated helper must not leave a detached publisher holding the RTSP path.
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::getppid() != parent_pid {
                    return Err(io::Error::other("publisher parent exited during spawn"));
                }
                Ok(())
            });
        }
        let child = command.spawn().with_context(|| format!("starting {}", args.ffmpeg.display()))?;
        let mut encoder = Self { child, input: None, warmup_until: Instant::now() + Duration::from_secs(2) };
        encoder.input = encoder.child.stdin.take();
        let fd = encoder.input.as_ref().context("FFmpeg stdin was not piped")?.as_raw_fd();
        // SAFETY: fcntl operates on our live pipe descriptor, preserving existing flags.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
        ensure!(flags >= 0, "reading FFmpeg pipe flags: {}", io::Error::last_os_error());
        if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
            return Err(io::Error::last_os_error()).context("making FFmpeg pipe nonblocking");
        }
        Ok(encoder)
    }

    fn check(&mut self) -> Result<()> {
        if let Some(status) = self.child.try_wait()? {
            bail!("FFmpeg exited: {status}");
        }
        Ok(())
    }

    fn write_frame(&mut self, client: &mut FramesClient, canvas: &Canvas, frame: FrameMsg) -> Result<()> {
        let input = self.input.as_mut().context("FFmpeg input is closed")?;
        let bytes = canvas.views[frame.buffer as usize].as_slice();
        // The encoder and RTSP initialize after raw input begins; steady-state writes
        // keep the short deadline, without treating cold encoder startup as a stall.
        let timeout = self.warmup_until.checked_duration_since(Instant::now()).unwrap_or(WRITE_TIMEOUT).max(WRITE_TIMEOUT);
        let deadline = Instant::now() + timeout;
        let total = canvas.layout.row_bytes * canvas.layout.height;
        let mut written = 0;
        while written < total {
            ensure!(Instant::now() < deadline, "FFmpeg raw-frame pipe stalled for {}ms; discarding encoder to avoid partial-frame corruption", timeout.as_millis());
            match input.write(canvas.layout.span(bytes, written)) {
                Ok(0) => bail!("FFmpeg closed its input pipe"),
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    // Keep releasing new frames while a single active frame is being
                    // read. Never queue frame-sized copies or hold the whole SHM ring.
                    drain_skipped(client, canvas)?;
                    let mut poll = [
                        libc::pollfd { fd: input.as_raw_fd(), events: libc::POLLOUT, revents: 0 },
                        libc::pollfd { fd: client.as_fd().as_raw_fd(), events: libc::POLLIN, revents: 0 },
                    ];
                    sys::poll(&mut poll, Some(deadline.saturating_duration_since(Instant::now())))?;
                }
                Err(error) => return Err(error).context("writing raw RGBA frame to FFmpeg"),
            }
        }
        Ok(())
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        drop(self.input.take());
        // Never wait for a blocked RTSP muxer to flush. The next process starts a
        // fresh stream rather than appending to an incomplete rawvideo frame.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn drain_skipped(client: &mut FramesClient, canvas: &Canvas) -> Result<()> {
    for _ in 0..DRAIN_LIMIT {
        match client.recv(Duration::ZERO)? {
            Some(ClientMsg::Frame { msg, .. }) => {
                client.release(msg.canvas, msg.buffer, msg.seq)?;
                canvas.validate_frame(&msg)?;
            }
            Some(ClientMsg::Canvas { .. }) => bail!("canvas replaced; reconnecting with fresh SHM mappings"),
            Some(ClientMsg::Goodbye(msg)) => bail!("frames server goodbye: reason {} canvas {}", msg.reason, msg.canvas),
            None => break,
        }
    }
    Ok(())
}

fn monotonic_ns() -> Result<u64> {
    let mut time = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: CLOCK_MONOTONIC is valid and time is writable.
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut time) } < 0 {
        return Err(io::Error::last_os_error()).context("reading monotonic clock");
    }
    Ok(time.tv_sec as u64 * 1_000_000_000 + time.tv_nsec as u64)
}

fn run(args: Args) -> Result<()> {
    let mut client = FramesClient::connect(&args.socket)?;
    client.hello(CLIENT_OTHER, 1 << args.canvas, false)?;
    let canvas = match client.recv(IDLE_TIMEOUT)?.context("timed out waiting for SHM canvas")? {
        ClientMsg::Canvas { msg, fds } => Canvas::map(msg, fds, args.canvas)?,
        _ => bail!("expected initial SHM canvas announcement"),
    };
    let mut encoder = Encoder::spawn(&args, &canvas)?;
    if let Some(device) = &args.vaapi_device {
        eprintln!("publishing {} {}x{} at {}fps to {} using H264 VAAPI constrained_baseline on {}", proto::CANVAS_NAMES[args.canvas as usize], canvas.msg.width, canvas.msg.height, args.fps, args.rtsp_url, device.display());
    } else {
        eprintln!("publishing {} {}x{} at {}fps to {} using H264 NVENC baseline", proto::CANVAS_NAMES[args.canvas as usize], canvas.msg.width, canvas.msg.height, args.fps, args.rtsp_url);
    }
    let period = Duration::from_secs_f64(1.0 / f64::from(args.fps));
    let mut next_frame = Instant::now();
    let mut last_received = Instant::now();
    let mut last_seq = None;
    loop {
        encoder.check()?;
        let Some(message) = client.recv(Duration::from_millis(250))? else {
            ensure!(last_received.elapsed() < IDLE_TIMEOUT, "no camera frames for {} seconds", IDLE_TIMEOUT.as_secs());
            continue;
        };
        let ClientMsg::Frame { msg, fence } = message else {
            match message {
                ClientMsg::Canvas { .. } => bail!("canvas replaced; reconnecting with fresh SHM mappings"),
                ClientMsg::Goodbye(msg) => bail!("frames server goodbye: reason {} canvas {}", msg.reason, msg.canvas),
                _ => unreachable!(),
            }
        };
        last_received = Instant::now();
        // Release even when validation, fence polling or writing fails. Disconnect
        // returns all queued holds to the server on every terminal error path.
        let result = (|| -> Result<()> {
            canvas.validate_frame(&msg)?;
            if msg.generation != canvas.msg.generation {
                return Ok(());
            }
            ensure!(last_seq.is_none_or(|previous| msg.seq > previous), "frame sequence did not increase");
            last_seq = Some(msg.seq);
            if Instant::now() < next_frame || monotonic_ns()?.saturating_sub(msg.monotonic_ns) > MAX_FRAME_AGE_NS {
                return Ok(());
            }
            if let Some(fence) = fence.as_ref() {
                if !wait_fence(fence, Duration::ZERO)? {
                    return Ok(());
                }
            }
            encoder.write_frame(&mut client, &canvas, msg)?;
            // Stay on the requested clock, without catch-up bursts after a stall.
            let now = Instant::now();
            while next_frame <= now {
                next_frame += period;
            }
            Ok(())
        })();
        let release = client.release(msg.canvas, msg.buffer, msg.seq);
        result?;
        release.context("releasing camera frame")?;
    }
}

fn main() -> ExitCode {
    match parse_args().and_then(|args| args.map(run).transpose()) {
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("se-camera-stream: {error:#}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vaapi_device_requires_a_value_and_an_absolute_path() {
        for input in [
            vec!["--vaapi-device"],
            vec!["--vaapi-device", ""],
            vec!["--vaapi-device", "renderD128"],
            vec!["--vaapi-device", "dev/dri/renderD128"],
        ] {
            let error = parse_args_from(input.into_iter().map(String::from)).err().unwrap();
            let detail = error.to_string();
            assert!(detail.contains("missing value for --vaapi-device") || detail.contains("absolute DRM render-node path"), "{detail}");
        }
    }

    #[test]
    fn vaapi_selection_preserves_fps_boundaries() {
        for fps in ["0", "61", "not-an-integer"] {
            assert!(parse_args_from([
                "--vaapi-device".into(), "/dev/dri/by-path/pci-0000:0d:00.0-render".into(),
                "--fps".into(), fps.into(),
            ]).is_err());
        }
        for fps in ["1", "30", "60"] {
            assert!(parse_args_from([
                "--vaapi-device".into(), "/dev/dri/by-path/pci-0000:0d:00.0-render".into(),
                "--fps".into(), fps.into(),
            ]).is_ok());
        }
    }

    #[test]
    fn padded_rows_and_partial_writes_exclude_offset_and_padding() {
        let msg = CanvasMsg {
            width: 2, height: 2, planes: 1, buffer_count: 3,
            offsets: [4, 0, 0, 0], strides: [12, 0, 0, 0],
            ..CanvasMsg::default()
        };
        let layout = Layout::new(&msg).unwrap();
        let bytes: Vec<u8> = (0..layout.len as u8).collect();
        let mut output = Vec::new();
        while output.len() < 16 {
            let span = layout.span(&bytes, output.len());
            output.extend_from_slice(&span[..span.len().min(3)]);
        }
        assert_eq!(output, [4, 5, 6, 7, 8, 9, 10, 11, 16, 17, 18, 19, 20, 21, 22, 23]);
    }

    #[test]
    fn short_stride_is_rejected_before_mapping_or_reading() {
        let msg = CanvasMsg {
            width: 1920, height: 1080, planes: 1, buffer_count: 3,
            strides: [7679, 0, 0, 0], ..CanvasMsg::default()
        };
        assert!(Layout::new(&msg).is_err());
    }
}
