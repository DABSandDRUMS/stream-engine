//! One recording file: one ffmpeg child for one video input. The master also carries every
//! audio track; ISOs are video-only. A child owns/reaps its feeder threads; finalization asks
//! ffmpeg to write its trailer (SIGINT), then escalates.

use super::camera::{self, Camera};
use super::canvas;
use super::{Process, Recording};
use crate::show::{Codec, RecordingInput, Role, SourceKind};
use parking_lot::Mutex;
use se_hub::Hub;
use std::io::{BufRead, BufReader};
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::LazyLock;
use std::sync::{Arc, atomic::{AtomicBool, AtomicU64, Ordering}};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Encoder {
    Nvenc,
    Software,
}

/// Everything needed to record one file.
#[derive(Clone)]
pub(crate) struct Job {
    pub index: usize,
    pub input: RecordingInput,
    pub role: Role,
    /// Audio inputs muxed into this file (the master's; empty for ISOs).
    pub audio: Vec<RecordingInput>,
    pub fps: u32,
    pub encoder: Encoder,
    pub dir: PathBuf,
    pub frames_socket: PathBuf,
    pub hub: Option<Arc<Hub>>,
}

/// Live counters the supervisor reads while the child runs.
#[derive(Default)]
pub(crate) struct Live {
    /// Pictures lost before the encoder (busy encoder / full tap).
    pub dropped: AtomicU64,
    /// Pictures encoded so far. The supervisor turns this into a windowed speed; ffmpeg's own
    /// `speed=` is an average since process start, so it reads slow for the first seconds.
    pub frames: AtomicU64,
    pub started: AtomicBool,
    /// The file being written, once created.
    pub path: Mutex<Option<PathBuf>>,
}

pub(crate) struct Outcome {
    /// The finalized, probed file.
    pub recording: Option<Recording>,
    /// The file path, once created (also when unusable).
    pub path: Option<PathBuf>,
    pub error: Option<String>,
    pub started: bool,
}

/// How piped pictures are laid out, for ffmpeg's color conversion.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Pipe {
    pub fd: i32,
    /// `(matrix, range)` of YUV sources (`bt601`/`bt709`, `limited`/`full`); None for RGB.
    pub yuv: Option<(String, String)>,
}

/// The ffmpeg argv (without the program) for one file.
pub(crate) fn argv(job: &Job, pipe: Option<&Pipe>, wall_ns: i128, path: &Path) -> Vec<String> {
    let mut a: Vec<String> = Vec::new();
    let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
    push(&["-hide_banner", "-nostdin", "-loglevel", "warning", "-nostats", "-stats_period", "0.25", "-progress", "pipe:1", "-copyts", "-n"]);
    match pipe {
        Some(pipe) => push(&["-thread_queue_size", "4", "-f", "matroska", "-probesize", "32", "-analyzeduration", "0", "-i", &format!("pipe:{}", pipe.fd)]),
        None => {
            push(&["-thread_queue_size", "4"]);
            input_args(&mut a, &job.input, wall_ns);
        }
    }
    for input in &job.audio {
        a.extend(["-thread_queue_size".into(), "64".into()]);
        input_args(&mut a, input, wall_ns);
    }
    let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
    push(&["-map", "0:v:0"]);
    for track in 0..job.audio.len() {
        push(&["-map", &format!("{}:a:0", track + 1)]);
    }
    // Scale down only (never up), keep the aspect ratio with an even width, and convert to
    // BT.709 limited 4:2:0 with explicit input colorimetry for camera YUV.
    let mut scale = String::from("scale=");
    if let Some(h) = job.input.height {
        scale.push_str(&format!("w=-2:h='min(ih,{h})':"));
    }
    scale.push_str(if job.role == Role::Master { "flags=lanczos" } else { "flags=bicubic" });
    if let Some((matrix, range)) = pipe.and_then(|p| p.yuv.as_ref()) {
        scale.push_str(&format!(":in_color_matrix={matrix}:in_range={range}"));
    }
    scale.push_str(":out_color_matrix=bt709:out_range=limited");
    push(&["-vf", &format!("fps={}:start_time=0,{scale},format=yuv420p", job.fps), "-fps_mode", "cfr", "-g", &(job.fps * 2).to_string()]);
    let hevc = job.input.codec() == Codec::Hevc;
    match (job.role, job.encoder) {
        (Role::Master, Encoder::Nvenc) => {
            let max = (job.input.max_mbps() * 1000.0).round() as u64;
            push(&["-c:v", if hevc { "hevc_nvenc" } else { "h264_nvenc" }, "-preset", "p5", "-tune", "hq", "-rc", "vbr"]);
            push(&["-cq", &job.input.cq().to_string(), "-b:v", "0", "-maxrate", &format!("{max}k"), "-bufsize", &format!("{}k", max * 2)]);
            push(&["-profile:v", if hevc { "main" } else { "high" }]);
        }
        (Role::Master, Encoder::Software) => {
            let max = (job.input.max_mbps() * 1000.0).round() as u64;
            let crf = job.input.cq().to_string();
            if hevc {
                push(&["-c:v", "libx265", "-preset", "veryfast", "-crf", &crf]);
                push(&["-x265-params", &format!("log-level=error:pools=6:vbv-maxrate={max}:vbv-bufsize={}", max * 2)]);
            } else {
                push(&["-c:v", "libx264", "-preset", "veryfast", "-crf", &crf, "-maxrate", &format!("{max}k"), "-bufsize", &format!("{}k", max * 2), "-threads", "6"]);
            }
        }
        (Role::Iso, Encoder::Nvenc) => {
            let rate = (job.input.mbps() * 1000.0).round() as u64;
            push(&["-c:v", if hevc { "hevc_nvenc" } else { "h264_nvenc" }, "-preset", "p4", "-tune", "hq", "-rc", "vbr"]);
            push(&["-b:v", &format!("{rate}k"), "-maxrate", &format!("{}k", rate * 3 / 2), "-bufsize", &format!("{}k", rate * 2)]);
            push(&["-profile:v", if hevc { "main" } else { "high" }]);
        }
        (Role::Iso, Encoder::Software) => {
            let rate = (job.input.mbps() * 1000.0).round() as u64;
            if hevc {
                push(&["-c:v", "libx265", "-preset", "ultrafast", "-b:v", &format!("{rate}k")]);
                push(&["-x265-params", &format!("log-level=error:pools=2:vbv-maxrate={}:vbv-bufsize={}", rate * 3 / 2, rate * 2)]);
            } else {
                push(&["-c:v", "libx264", "-preset", "superfast", "-b:v", &format!("{rate}k"), "-maxrate", &format!("{}k", rate * 3 / 2)]);
                push(&["-bufsize", &format!("{}k", rate * 2), "-threads", "2"]);
            }
        }
    }
    push(&["-colorspace", "bt709", "-color_primaries", "bt709", "-color_trc", "bt709", "-color_range", "tv"]);
    push(&["-metadata:s:v:0", &format!("title={}", job.input.name)]);
    push(&["-stats_enc_pre:v:0", "pipe:1", "-stats_enc_pre_fmt:v:0", "se_video={pts}"]);
    if !job.audio.is_empty() {
        // Lossless audio avoids AAC encoder priming shifting the master origin.
        // first_pts pads delayed device opens; async follows capture timestamps
        // rather than accumulating device/sample-clock drift over a long show.
        push(&["-c:a", "flac", "-ar", "48000", "-af", "aresample=async=1000:first_pts=0"]);
        for (track, input) in job.audio.iter().enumerate() {
            push(&[&format!("-metadata:s:a:{track}"), &format!("title={}", input.name)]);
            push(&[&format!("-stats_enc_pre:a:{track}"), "pipe:1", &format!("-stats_enc_pre_fmt:a:{track}"), &format!("se_audio_{track}={{pts}}")]);
        }
    }
    // Crash safety: packets are flushed as written and clusters close every second, so a
    // killed engine leaves a playable file. -shortest is avoided: live sync queues can deadlock.
    push(&["-max_interleave_delta", "1000000", "-flush_packets", "1", "-cluster_time_limit", "1000", "-avoid_negative_ts", "disabled", "-f", "matroska"]);
    a.push(path.to_string_lossy().into_owned());
    a
}

fn input_args(a: &mut Vec<String>, input: &RecordingInput, wall_ns: i128) {
    let mut push = |items: &[&str]| a.extend(items.iter().map(|s| s.to_string()));
    // Generated and file media already carry a precise sample timeline. Replacing
    // it with packet-arrival wall time makes resampling change pitch with scheduling
    // jitter. Pace their native timestamps; devices instead use the capture clock.
    if input.format == "lavfi" || Path::new(&input.source).is_file() {
        // ffmpeg treats a zero initial burst as its half-second default. A 1ms
        // burst keeps native source playback on the engine timeline from t=0.
        push(&["-re", "-readrate_initial_burst", "0.001"]);
    } else {
        if !matches!(input.format.as_str(), "pulse" | "alsa") {
            push(&["-use_wallclock_as_timestamps", "1"]);
        }
        push(&["-itsoffset", &format!("-{:.6}", wall_ns as f64 / 1e9)]);
    }
    if !input.format.is_empty() {
        push(&["-f", &input.format]);
    }
    push(&["-i", &input.source]);
}

/// ffmpeg errors meaning the hardware encoder could not start (driver, GPU, session limit).
pub(crate) fn encoder_unavailable(error: &str) -> bool {
    let e = error.to_ascii_lowercase();
    ["nvenc", "libcuda", "cuda", "openencodesessionex", "no capable devices", "cannot load", "opening encoder", "unknown encoder"].iter().any(|s| e.contains(s))
}

struct Feeders {
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<Result<(), String>>>,
}
impl Drop for Feeders {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

enum Source {
    Canvas(Box<canvas::Canvas>),
    Camera(Camera<se_hub::media::VideoTap>),
    External,
}

fn open(job: &Job, stop: &AtomicBool) -> Result<(Source, Option<(String, String)>), String> {
    match job.input.kind() {
        SourceKind::Canvas(name) => {
            let id = se_frames::proto::canvas_by_name(name).ok_or_else(|| format!("unknown canvas {name}"))?;
            Ok((Source::Canvas(Box::new(canvas::Canvas::connect(&job.frames_socket, id, stop)?)), None))
        }
        SourceKind::Camera(id) => {
            let hub = job.hub.as_ref().ok_or("camera recording needs the running engine")?;
            let camera = Camera::open(&job.input.name, hub.tap_video(id, camera::TAP_CAPACITY), stop)?;
            let yuv = camera.layout.is_yuv().then(|| {
                let snap = hub.snapshot.load();
                let matrix = match snap.str(&format!("source.{id}.matrix")) {
                    Some("bt709") => "bt709",
                    _ => "bt601",
                };
                let range = match snap.str(&format!("source.{id}.range")) {
                    Some("full") => "full",
                    _ => "limited",
                };
                (matrix.to_string(), range.to_string())
            });
            Ok((Source::Camera(camera), yuv))
        }
        SourceKind::External => Ok((Source::External, None)),
    }
}

/// Record one file until `stop`, an input loss, or an encoder failure. Finished only after
/// ffmpeg has exited, IO threads have joined, and the file has a measured duration.
pub(crate) fn run(job: &Job, stop: &AtomicBool, live: &Live, started: &dyn Fn(Recording)) -> Outcome {
    let mut outcome = Outcome { recording: None, path: None, error: None, started: false };
    if let Err(error) = record(job, stop, live, started, &mut outcome) {
        outcome.error = Some(error);
    }
    outcome
}

/// Start ffmpeg WITHOUT forking the engine. A `pre_exec` hook would force `fork()`, which
/// write-protects the whole multi-GB engine and stalled the render thread (90 ms, 4 dropped
/// frames, at every recording start). `posix_spawn` (std's path without hooks) doesn't; the
/// parent-death kill comes from `setpriv` instead.
fn spawn_ffmpeg(args: &[String], stdin: Stdio) -> std::io::Result<Child> {
    static SETPRIV: LazyLock<bool> = LazyLock::new(|| {
        Command::new("setpriv").arg("--version").stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status().is_ok_and(|s| s.success())
    });
    let mut cmd = if *SETPRIV {
        let mut c = Command::new("setpriv");
        c.args(["--pdeathsig", "KILL", "--", "ffmpeg"]);
        c
    } else {
        Command::new("ffmpeg")
    };
    cmd.args(args).stdin(stdin).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn()
}

fn record(job: &Job, stop: &AtomicBool, live: &Live, on_started: &dyn Fn(Recording), outcome: &mut Outcome) -> Result<(), String> {
    std::fs::create_dir_all(&job.dir).map_err(|e| format!("cannot create recording folder {}: {e}", job.dir.display()))?;
    let (source, yuv) = open(job, stop)?;
    if stop.load(Ordering::Relaxed) {
        return Ok(());
    }
    let (read, write) = match source {
        Source::External => (None, None),
        _ => {
            let (read, write) = UnixStream::pair().map_err(|e| e.to_string())?;
            (Some(read), Some(write))
        }
    };
    let start_ns = se_clock::now();
    let wall_ns = se_clock::wall_now_ns();
    let path = job.dir.join(format!("{}-{start_ns}-{}.mkv", job.input.name, job.index));
    let tracks = super::tracks(&job.audio);
    let mut record = Recording {
        canvas: job.input.name.clone(),
        role: job.role,
        source: job.input.source.clone(),
        path: path.clone(),
        start_ns,
        end_ns: None,
        tracks,
    };
    // The pictures arrive on ffmpeg's stdin (`pipe:0`), so no other descriptor must be inherited.
    let pipe = read.as_ref().map(|_| Pipe { fd: 0, yuv });
    let args = argv(job, pipe.as_ref(), wall_ns, &path);
    let stdin = read.map_or_else(Stdio::null, |r| Stdio::from(OwnedFd::from(r)));
    let mut child = Process(spawn_ffmpeg(&args, stdin).map_err(|e| format!("cannot launch ffmpeg: {e}"))?);
    // ISOs yield CPU to the master encoder and the renderer under contention.
    if job.role == Role::Iso {
        // SAFETY: plain syscall on our own child's pid.
        unsafe { libc::setpriority(libc::PRIO_PROCESS, child.0.id(), 5) };
    }
    outcome.path = Some(path.clone());
    *live.path.lock() = Some(path.clone());
    let mut feeds = Feeders { stop: Arc::new(AtomicBool::new(false)), threads: Vec::new() };
    let dropped = Arc::new(AtomicU64::new(0));
    match (source, write) {
        (Source::Canvas(canvas), Some(write)) => feeds.threads.extend((*canvas).spawn(write, start_ns, job.fps, feeds.stop.clone(), dropped.clone())),
        (Source::Camera(camera), Some(write)) => feeds.threads.push(camera.spawn(write, start_ns, job.fps, feeds.stop.clone(), dropped.clone())),
        _ => {}
    }
    let stderr = Arc::new(Mutex::new(String::new()));
    let errors = stderr.clone();
    let error_pipe = child.0.stderr.take().expect("piped stderr");
    let error_thread = thread::spawn(move || {
        for line in BufReader::new(error_pipe).lines().map_while(Result::ok) {
            let mut text = errors.lock();
            text.push_str(&line);
            text.push('\n');
            if text.len() > 16_384 {
                let mut boundary = text.len() - 8192;
                while !text.is_char_boundary(boundary) {
                    boundary += 1;
                }
                text.drain(..boundary);
            }
        }
    });
    let frames = Arc::new(AtomicU64::new(0));
    let audio_seen: Arc<Vec<AtomicU64>> = Arc::new(job.audio.iter().map(|_| AtomicU64::new(start_ns)).collect());
    let video_seen = Arc::new(AtomicU64::new(start_ns));
    let progress_pipe = child.0.stdout.take().expect("piped stdout");
    let progress_thread = {
        let (frames, audio_seen, video_seen) = (frames.clone(), audio_seen.clone(), video_seen.clone());
        thread::spawn(move || {
            for line in BufReader::new(progress_pipe).lines().map_while(Result::ok) {
                if let Some(value) = line.strip_prefix("frame=").and_then(|s| s.trim().parse::<u64>().ok()) {
                    frames.store(value, Ordering::Relaxed);
                } else if line.starts_with("se_video=") {
                    video_seen.store(se_clock::now(), Ordering::Relaxed);
                } else if let Some((track, _pts)) = line.strip_prefix("se_audio_").and_then(|s| s.split_once('='))
                    && let Some(seen) = track.parse::<usize>().ok().and_then(|i| audio_seen.get(i))
                {
                    seen.store(se_clock::now(), Ordering::Relaxed);
                }
            }
        })
    };
    let (mut size, mut changed) = (0u64, Instant::now());
    let started_at = Instant::now();
    let mut stopping_at = None;
    let mut error = None;
    let status: Option<ExitStatus>;
    loop {
        match child.0.try_wait() {
            Ok(Some(exit)) => {
                status = Some(exit);
                break;
            }
            Err(e) => {
                error = Some(format!("cannot monitor ffmpeg: {e}"));
                let _ = child.0.kill();
                status = child.0.wait().ok();
                break;
            }
            Ok(None) => {}
        }
        live.dropped.store(dropped.load(Ordering::Relaxed), Ordering::Relaxed);
        live.frames.store(frames.load(Ordering::Relaxed), Ordering::Relaxed);
        if stopping_at.is_none() {
            if let Some(index) = feeds.threads.iter().position(JoinHandle::is_finished) {
                let result = feeds.threads.swap_remove(index).join().unwrap_or_else(|_| Err("picture input worker panicked".into()));
                error = Some(result.err().unwrap_or_else(|| "picture input ended unexpectedly".into()));
            }
            let current = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
            if current != size {
                size = current;
                changed = Instant::now();
            }
            let now = se_clock::now();
            if outcome.started {
                if changed.elapsed() > Duration::from_secs(10) {
                    error = Some(format!("recording {} stopped advancing for 10 seconds (input lost or encoder stalled)", job.input.name));
                }
                for (input, seen) in job.audio.iter().zip(audio_seen.iter()) {
                    if now.saturating_sub(seen.load(Ordering::Relaxed)) > 10_000_000_000 {
                        error = Some(format!("audio input {} stopped delivering samples for 10 seconds", input.name));
                    }
                }
                if now.saturating_sub(video_seen.load(Ordering::Relaxed)) > 10_000_000_000 {
                    error = Some(format!("video input {} stopped delivering pictures for 10 seconds", job.input.name));
                }
            }
            if !outcome.started
                && frames.load(Ordering::Relaxed) > 0
                && size > 0
                && video_seen.load(Ordering::Relaxed) > start_ns
                && audio_seen.iter().all(|seen| seen.load(Ordering::Relaxed) > start_ns)
            {
                outcome.started = true;
                live.started.store(true, Ordering::Relaxed);
                on_started(record.clone());
            }
            if !outcome.started && started_at.elapsed() > Duration::from_secs(20) {
                error = Some("ffmpeg did not produce video within 20 seconds".into());
            }
            if stop.load(Ordering::Relaxed) || error.is_some() {
                stopping_at = Some(Instant::now());
                // ffmpeg disables stdin commands for pipe:N inputs, even when N
                // isn't stdin. A single SIGINT requests trailer finalization for
                // every input kind; repeated signals would abort the trailer.
                unsafe {
                    libc::kill(child.0.id() as i32, libc::SIGINT);
                }
            }
        }
        if let Some(at) = stopping_at {
            // Keep complete timestamped pictures flowing until ffmpeg closes its
            // inputs. Cancelling a partial raw packet could truncate the final frame.
            if at.elapsed() > Duration::from_secs(12) {
                let _ = child.0.kill();
                error.get_or_insert_with(|| "ffmpeg did not finalize within 12 seconds; forced termination".into());
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    drop(feeds);
    let _ = progress_thread.join();
    let _ = error_thread.join();
    let diagnostic = stderr.lock().trim().to_owned();
    if !stop.load(Ordering::Relaxed) && error.is_none() {
        error = Some(if diagnostic.is_empty() { format!("ffmpeg input ended unexpectedly ({status:?})") } else { diagnostic.clone() });
    } else if error.is_some() && !diagnostic.is_empty() {
        error = error.map(|e| format!("{e}: {diagnostic}"));
    }
    // A killed/failed capture may still contain usable footage. Keep and index it,
    // but never advertise empty or unprobeable headers as completed recordings.
    match super::duration(&path) {
        Ok(seconds) => {
            record.end_ns = Some(record.start_ns + (seconds * 1e9) as u64);
            outcome.recording = Some(record);
        }
        Err(e) => {
            // A failed encoder initialization can leave a zero-byte output. Remove only
            // empty files we created; keep nonempty damaged media for recovery.
            if std::fs::metadata(&path).is_ok_and(|m| m.len() == 0) {
                let _ = std::fs::remove_file(&path);
            }
            error.get_or_insert(e);
        }
    }
    if let Some(error) = error { Err(error) } else { Ok(()) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn job(input: RecordingInput, role: Role, audio: Vec<RecordingInput>, encoder: Encoder) -> Job {
        Job { index: 0, input, role, audio, fps: 60, encoder, dir: PathBuf::from("/show"), frames_socket: PathBuf::new(), hub: None }
    }

    fn joined(a: &[String]) -> String {
        a.join(" ")
    }

    #[test]
    fn master_argv_is_hevc_nvenc_constant_quality_capped_with_every_audio_track() {
        let main = RecordingInput { role: Some(Role::Master), cq: Some(19), max_mbps: Some(35.0), ..RecordingInput::new("main", "canvas:wide", "") };
        let audio = vec![RecordingInput::new("Mix", "se-band", "pulse"), RecordingInput::new("Music", "se-music", "pulse")];
        let pipe = Pipe { fd: 7, yuv: None };
        let a = argv(&job(main, Role::Master, audio, Encoder::Nvenc), Some(&pipe), 0, Path::new("/show/main-1-0.mkv"));
        let s = joined(&a);
        assert!(s.contains("-f matroska -probesize 32 -analyzeduration 0 -i pipe:7"), "{s}");
        assert!(s.contains("-map 0:v:0 -map 1:a:0 -map 2:a:0"), "{s}");
        assert!(s.contains("-c:v hevc_nvenc -preset p5 -tune hq -rc vbr -cq 19 -b:v 0 -maxrate 35000k -bufsize 70000k -profile:v main"), "{s}");
        assert!(s.contains("-vf fps=60:start_time=0,scale=flags=lanczos:out_color_matrix=bt709:out_range=limited,format=yuv420p -fps_mode cfr -g 120"), "{s}");
        assert!(s.contains("-c:a flac -ar 48000"), "{s}");
        assert!(s.contains("-metadata:s:a:1 title=Music"), "{s}");
        assert!(s.contains("-flush_packets 1 -cluster_time_limit 1000 -avoid_negative_ts disabled -f matroska /show/main-1-0.mkv"), "{s}");
        assert_eq!(a.iter().filter(|x| *x == "-i").count(), 3);
    }

    #[test]
    fn master_software_fallback_is_x265_crf_with_vbv_cap() {
        let main = RecordingInput::new("main", "canvas:wide", "");
        let s = joined(&argv(&job(main, Role::Master, vec![], Encoder::Software), Some(&Pipe { fd: 3, yuv: None }), 0, Path::new("/o.mkv")));
        assert!(s.contains("-c:v libx265 -preset veryfast -crf 19 -x265-params log-level=error:pools=6:vbv-maxrate=35000:vbv-bufsize=70000"), "{s}");
        assert!(!s.contains("-c:a"), "{s}");
        let h264 = RecordingInput { codec: Some(Codec::H264), ..RecordingInput::new("main", "canvas:wide", "") };
        let s = joined(&argv(&job(h264, Role::Master, vec![], Encoder::Software), Some(&Pipe { fd: 3, yuv: None }), 0, Path::new("/o.mkv")));
        assert!(s.contains("-c:v libx264 -preset veryfast -crf 19 -maxrate 35000k"), "{s}");
    }

    #[test]
    fn iso_argv_is_video_only_target_bitrate_scaled_down_from_native_yuv() {
        let kick = RecordingInput { height: Some(720), mbps: Some(3.5), ..RecordingInput::new("kick", "camera:cam_kick", "") };
        let pipe = Pipe { fd: 9, yuv: Some(("bt601".into(), "limited".into())) };
        let a = argv(&job(kick, Role::Iso, vec![], Encoder::Nvenc), Some(&pipe), 0, Path::new("/show/kick-1-2.mkv"));
        let s = joined(&a);
        assert!(s.contains("-c:v hevc_nvenc -preset p4 -tune hq -rc vbr -b:v 3500k -maxrate 5250k -bufsize 7000k -profile:v main"), "{s}");
        assert!(
            s.contains("scale=w=-2:h='min(ih,720)':flags=bicubic:in_color_matrix=bt601:in_range=limited:out_color_matrix=bt709:out_range=limited,format=yuv420p"),
            "{s}"
        );
        assert!(!s.contains(":a:") && !s.contains("-c:a") && !s.contains("-map 1"), "ISOs carry no audio: {s}");
        assert!(s.contains("-flush_packets 1 -cluster_time_limit 1000"), "{s}");
    }

    #[test]
    fn hardware_encoder_failures_are_recognized() {
        assert!(encoder_unavailable("[hevc_nvenc @ 0x1] OpenEncodeSessionEx failed: incompatible client key (21)"));
        assert!(encoder_unavailable("Cannot load libcuda.so.1"));
        assert!(!encoder_unavailable("audio input Mix stopped delivering samples for 10 seconds"));
    }
}
