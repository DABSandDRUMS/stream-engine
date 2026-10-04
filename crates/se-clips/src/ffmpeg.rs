//! FFmpeg command construction and execution: probing recordings, PCM for Whisper, clip cuts
//! (NVENC `h264_nvenc`, libx264 when NVENC can't open a session), burned-in ASS captions,
//! the non-music audio mix, and thumbnails. Children run niced; outputs are written to a
//! temporary name and renamed, so a clip file is always complete.

use crate::config::VideoConfig;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Probe {
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub audio_streams: usize,
}

fn secs(t: f64) -> String {
    format!("{:.3}", t.max(0.0))
}

/// A child at CPU niceness `nice`, idle IO class, that dies with the engine.
/// Exec-only wrappers preserve its PID for the live guard's signals without a `pre_exec`
/// hook, which would fork and write-protect the engine's entire address space.
pub(crate) fn background_cmd(program: &str, nice: i32, cpus: &[usize]) -> Command {
    // SAFETY: getpriority only reads this thread's niceness. `nice` adjusts the inherited
    // value, so compensate for workers already niced by `transcribe::niced`.
    let inherited = unsafe { libc::getpriority(libc::PRIO_PROCESS, 0) };
    let adjustment = nice.clamp(-20, 19) - inherited;
    let mut c = Command::new("setpriv");
    c.args(["--pdeathsig", "KILL", "--", "nice", "--adjustment"])
        .arg(adjustment.to_string())
        .args(["--", "ionice", "--class", "3", "--"]);
    if !cpus.is_empty() {
        let list = cpus.iter().map(usize::to_string).collect::<Vec<_>>().join(",");
        c.args(["taskset", "--cpu-list"]).arg(list);
    }
    c.arg(program);
    c
}

/// `ffprobe` a recording.
pub fn probe(path: &Path) -> Result<Probe, String> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_format", "-show_streams"])
        .arg(path)
        .output()
        .map_err(|e| format!("ffprobe: {e} (is ffmpeg installed?)"))?;
    if !out.status.success() {
        return Err(format!("ffprobe {}: {}", path.display(), String::from_utf8_lossy(&out.stderr).trim()));
    }
    parse_probe(&out.stdout)
}

pub fn parse_probe(json: &[u8]) -> Result<Probe, String> {
    let v: serde_json::Value = serde_json::from_slice(json).map_err(|e| format!("ffprobe output: {e}"))?;
    let num = |x: Option<&serde_json::Value>| x.and_then(|x| x.as_str().and_then(|s| s.parse::<f64>().ok()).or_else(|| x.as_f64()));
    let mut p = Probe { duration: num(v.pointer("/format/duration")).unwrap_or(0.0), ..Default::default() };
    for s in v.get("streams").and_then(|s| s.as_array()).map(Vec::as_slice).unwrap_or(&[]) {
        match s.get("codec_type").and_then(|c| c.as_str()) {
            Some("video") if p.width == 0 => {
                // cover art / attached pictures are not the program video
                if s.pointer("/disposition/attached_pic").and_then(|d| d.as_i64()) == Some(1) {
                    continue;
                }
                p.width = s.get("width").and_then(|w| w.as_u64()).unwrap_or(0) as u32;
                p.height = s.get("height").and_then(|h| h.as_u64()).unwrap_or(0) as u32;
                let rate = s.get("avg_frame_rate").or_else(|| s.get("r_frame_rate")).and_then(|r| r.as_str()).unwrap_or("0/1");
                p.fps = match rate.split_once('/') {
                    Some((a, b)) => a.parse::<f64>().unwrap_or(0.0) / b.parse::<f64>().unwrap_or(1.0).max(1e-9),
                    None => rate.parse().unwrap_or(0.0),
                };
                if p.duration == 0.0 {
                    p.duration = num(s.get("duration")).unwrap_or(0.0);
                }
            }
            Some("audio") => p.audio_streams += 1,
            _ => {}
        }
    }
    if p.width == 0 {
        return Err("recording has no video stream".into());
    }
    Ok(p)
}

/// Args producing mono 16 kHz f32le PCM of one audio stream on stdout.
pub fn pcm_args(input: &Path, stream: usize, from: f64, dur: f64) -> Vec<String> {
    vec![
        "-nostdin".into(),
        "-v".into(),
        "error".into(),
        "-ss".into(),
        secs(from),
        "-t".into(),
        secs(dur),
        "-i".into(),
        input.to_string_lossy().into_owned(),
        "-map".into(),
        format!("0:a:{stream}"),
        "-ac".into(),
        "1".into(),
        "-ar".into(),
        crate::transcribe::SAMPLE_RATE.to_string(),
        "-f".into(),
        "f32le".into(),
        "pipe:1".into(),
    ]
}

/// Run ffmpeg and return stdout. Under [`live::with_halt`](crate::live::with_halt) the child
/// stops when the show goes live.
pub fn run_capture(args: &[String], nice: i32) -> Result<Vec<u8>, String> {
    let mut child = background_cmd("ffmpeg", nice, &[])
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("background ffmpeg launch: {e} (requires setpriv, nice, ionice and ffmpeg)"))?;
    let err_thread = drain(child.stderr.take().expect("piped"));
    let mut stdout = child.stdout.take().expect("piped");
    let out_thread = std::thread::spawn(move || {
        let mut out = Vec::new();
        stdout.read_to_end(&mut out).map(|_| out)
    });
    let status = crate::live::wait_child(&mut child, crate::live::current().as_ref());
    let out = out_thread.join().map_err(|_| "ffmpeg reader panicked".to_string())?.map_err(|e| e.to_string());
    let err = String::from_utf8_lossy(&err_thread.join().unwrap_or_default()).into_owned();
    if !status?.success() {
        return Err(format!("ffmpeg failed: {}", tail(&err)));
    }
    out
}

/// Run ffmpeg with `cwd`, returning stderr's tail on failure. Live-guarded like [`run_capture`].
pub fn run(args: &[String], cwd: &Path, nice: i32) -> Result<(), String> {
    let mut child = background_cmd("ffmpeg", nice, &[])
        .args(args)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("background ffmpeg launch: {e} (requires setpriv, nice, ionice and ffmpeg)"))?;
    let err_thread = drain(child.stderr.take().expect("piped"));
    let status = crate::live::wait_child(&mut child, crate::live::current().as_ref());
    let err = err_thread.join().unwrap_or_default();
    if status?.success() { Ok(()) } else { Err(tail(&String::from_utf8_lossy(&err))) }
}

/// Read a pipe to the end on its own thread (keeps the child from blocking on a full pipe).
pub fn drain(mut r: impl Read + Send + 'static) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = r.read_to_end(&mut buf);
        buf
    })
}

pub fn tail(s: &str) -> String {
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(6)..].join(" | ")
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Codec {
    Nvenc,
    X264,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Nvenc => "h264_nvenc",
            Codec::X264 => "libx264",
        }
    }
}

/// How the output frame is made from the source video.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Frame {
    /// Fit into `w×h` (letterbox if the aspect differs).
    Fit { w: u32, h: u32 },
    /// Crop a `w:h`-aspect window at horizontal `center` (0…1) from a wider source, then scale.
    Crop { w: u32, h: u32, center: f64 },
}

/// One clip render.
#[derive(Clone, Debug, PartialEq)]
pub struct Cut {
    pub video: PathBuf,
    /// Seek into `video` (seconds).
    pub video_at: f64,
    /// Audio source (another recording, e.g. the multitrack wide one) and its seek; `None`
    /// takes audio from `video`.
    pub audio: Option<(PathBuf, f64)>,
    /// Audio-stream indices to mix (empty = no audio).
    pub tracks: Vec<usize>,
    pub duration: f64,
    pub frame: Frame,
    pub fps: f64,
    /// ASS file name relative to the working directory (captions are burned in).
    pub subtitles: Option<String>,
    pub out: PathBuf,
}

/// The ffmpeg argument list for a cut.
pub fn cut_args(c: &Cut, codec: Codec, v: &VideoConfig) -> Vec<String> {
    let mut a: Vec<String> = vec!["-nostdin".into(), "-hide_banner".into(), "-v".into(), "error".into(), "-y".into()];
    a.extend(["-ss".into(), secs(c.video_at), "-t".into(), secs(c.duration), "-i".into(), c.video.to_string_lossy().into_owned()]);
    let audio_input = match &c.audio {
        Some((p, at)) => {
            a.extend(["-ss".into(), secs(*at), "-t".into(), secs(c.duration), "-i".into(), p.to_string_lossy().into_owned()]);
            1
        }
        None => 0,
    };
    let mut fg = String::new();
    match c.frame {
        Frame::Fit { w, h } => fg.push_str(&format!(
            "[0:v:0]scale={w}:{h}:force_original_aspect_ratio=decrease:flags=lanczos,pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1"
        )),
        Frame::Crop { w, h, center } => fg.push_str(&format!(
            "[0:v:0]crop=w='trunc(min(iw,ih*{w}/{h})/2)*2':h='trunc(min(ih,iw*{h}/{w})/2)*2':x='(iw-ow)*{center:.4}':y='(ih-oh)/2',scale={w}:{h}:flags=lanczos,setsar=1"
        )),
    }
    if let Some(s) = &c.subtitles {
        fg.push_str(&format!(",subtitles=f={s}"));
    }
    fg.push_str(",format=yuv420p[v]");
    if !c.tracks.is_empty() {
        fg.push(';');
        for t in &c.tracks {
            fg.push_str(&format!("[{audio_input}:a:{t}]"));
        }
        if c.tracks.len() > 1 {
            fg.push_str(&format!("amix=inputs={}:duration=longest:normalize=0,", c.tracks.len()));
        }
        if v.loudnorm {
            fg.push_str(&format!("loudnorm=I={}:TP=-1.5:LRA=11,", v.loudness_lufs));
        }
        fg.push_str("aresample=48000[a]");
    }
    a.extend(["-filter_complex".into(), fg, "-map".into(), "[v]".into()]);
    if !c.tracks.is_empty() {
        a.extend(["-map".into(), "[a]".into(), "-c:a".into(), "aac".into(), "-b:a".into(), v.audio_bitrate.clone(), "-ac".into(), "2".into()]);
    } else {
        a.push("-an".into());
    }
    let gop = ((if c.fps > 0.0 { c.fps } else { 60.0 }) * 2.0).round().max(1.0) as u32;
    match codec {
        Codec::Nvenc => a.extend(
            [
                "-c:v",
                "h264_nvenc",
                "-preset",
                &v.nvenc_preset,
                "-tune",
                "hq",
                "-rc",
                "vbr",
                "-cq",
                &v.nvenc_cq.to_string(),
                "-b:v",
                "0",
                "-maxrate",
                &v.max_bitrate,
                "-bufsize",
                &v.max_bitrate,
                "-profile:v",
                "high",
                "-bf",
                "2",
                "-g",
                &gop.to_string(),
            ]
            .map(String::from),
        ),
        Codec::X264 => a.extend(
            [
                "-c:v",
                "libx264",
                "-preset",
                &v.x264_preset,
                "-crf",
                &v.x264_crf.to_string(),
                "-maxrate",
                &v.max_bitrate,
                "-bufsize",
                &v.max_bitrate,
                "-profile:v",
                "high",
                "-g",
                &gop.to_string(),
            ]
            .map(String::from),
        ),
    }
    a.extend(["-movflags".into(), "+faststart".into(), "-f".into(), "mp4".into(), c.out.to_string_lossy().into_owned()]);
    a
}

/// NVENC failures that mean "no encoder session right now" (OBS holds them, driver limit,
/// no device) rather than a bad input — the cut is retried with libx264.
pub fn nvenc_unavailable(err: &str) -> bool {
    let e = err.to_lowercase();
    [
        "openencodesessionex",
        "no capable devices",
        "cannot load libnvidia-encode",
        "nvenc",
        "cuda",
        "out of memory",
        "incompatible client key",
        "unsupported device",
    ]
    .iter()
    .any(|k| e.contains(k))
}

/// Cut with the preferred codec; with `fallback`, an NVENC session failure retries on x264.
/// Writes `<out>.part` and renames on success. Returns the codec used.
pub fn cut(c: &Cut, prefer: Codec, fallback: bool, v: &VideoConfig, cwd: &Path, nice: i32) -> Result<Codec, String> {
    let part = c.out.with_extension("part.mp4");
    let tmp = Cut { out: part.clone(), ..c.clone() };
    let res = match run(&cut_args(&tmp, prefer, v), cwd, nice) {
        Ok(()) => Ok(prefer),
        Err(e) if prefer == Codec::Nvenc && fallback && nvenc_unavailable(&e) => {
            tracing::warn!("NVENC unavailable ({e}); encoding with libx264");
            run(&cut_args(&tmp, Codec::X264, v), cwd, nice).map(|_| Codec::X264)
        }
        Err(e) => Err(e),
    };
    match res {
        Ok(codec) => {
            let out = cwd.join(&c.out);
            std::fs::rename(cwd.join(&part), &out).map_err(|e| format!("{}: {e}", out.display()))?;
            Ok(codec)
        }
        Err(e) => {
            let _ = std::fs::remove_file(cwd.join(&part));
            Err(e)
        }
    }
}

/// Thumbnail (JPEG) of `input` at `at` seconds, `width` pixels wide.
pub fn thumb_args(input: &Path, at: f64, width: u32, out: &Path) -> Vec<String> {
    vec![
        "-nostdin".into(),
        "-v".into(),
        "error".into(),
        "-y".into(),
        "-ss".into(),
        secs(at),
        "-i".into(),
        input.to_string_lossy().into_owned(),
        "-frames:v".into(),
        "1".into(),
        "-vf".into(),
        format!("scale={width}:-2:flags=lanczos"),
        "-q:v".into(),
        "3".into(),
        "-f".into(),
        "image2".into(),
        out.to_string_lossy().into_owned(),
    ]
}

/// Does this ffmpeg have the encoder? (`ffmpeg -encoders`)
pub fn has_encoder(name: &str) -> bool {
    Command::new("ffmpeg")
        .args(["-hide_banner", "-encoders"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().any(|l| l.split_whitespace().nth(1) == Some(name)))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn background_child_probe() {
        if let Some(path) = std::env::var_os("SE_CLIPS_ORPHAN_PROBE") {
            let mut child = background_cmd("sleep", 19, &[]).arg("30").spawn().unwrap();
            std::thread::sleep(std::time::Duration::from_millis(100));
            std::fs::write(path, child.id().to_string()).unwrap();
            let _ = child.wait();
            return;
        }
        let Some(path) = std::env::var_os("SE_CLIPS_BACKGROUND_PROBE") else { return };
        let mut affinity: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: these calls only inspect this process and fill valid out-parameters.
        let (nice, io) = unsafe {
            assert_eq!(libc::sched_getaffinity(0, std::mem::size_of_val(&affinity), &mut affinity), 0);
            (libc::getpriority(libc::PRIO_PROCESS, 0), libc::syscall(libc::SYS_ioprio_get, 1, 0))
        };
        let cpus: Vec<usize> = (0..libc::CPU_SETSIZE as usize).filter(|&cpu| unsafe { libc::CPU_ISSET(cpu, &affinity) }).collect();
        let observed = (std::process::id(), nice, io, cpus);
        std::fs::write(path, serde_json::to_vec(&observed).unwrap()).unwrap();
        std::process::exit(37);
    }

    #[test]
    fn background_child_keeps_pid_exit_status_priorities_and_affinity() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("observed.json");
        let mut allowed: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: sched_getaffinity fills our properly sized CPU set.
        assert_eq!(unsafe { libc::sched_getaffinity(0, std::mem::size_of_val(&allowed), &mut allowed) }, 0);
        let cpu = (0..libc::CPU_SETSIZE as usize).find(|&cpu| unsafe { libc::CPU_ISSET(cpu, &allowed) }).unwrap();
        // Run from an already-niced worker: applying nice=19 must be absolute, not +19.
        let (pid, status) = crate::transcribe::niced(10, move || {
            let mut child = background_cmd(std::env::current_exe().unwrap().to_str().unwrap(), 19, &[cpu])
                .args(["--exact", "ffmpeg::tests::background_child_probe", "--test-threads=1"])
                .env("SE_CLIPS_BACKGROUND_PROBE", &path)
                .stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
            (child.id(), child.wait().unwrap())
        }).unwrap();
        assert_eq!(status.code(), Some(37), "wrapper must return the actual child's exit");
        let (observed_pid, nice, io, cpus): (u32, i32, i64, Vec<usize>) =
            serde_json::from_slice(&std::fs::read(tmp.path().join("observed.json")).unwrap()).unwrap();
        assert_eq!(observed_pid, pid, "signals must target the encoder, not a wrapper parent");
        assert_eq!(nice, 19);
        assert_eq!(io >> 13, 3, "background IO stays idle");
        assert_eq!(cpus, [cpu]);
    }

    #[test]
    fn background_child_dies_when_its_parent_process_exits() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("child.pid");
        let mut parent = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "ffmpeg::tests::background_child_probe", "--test-threads=1"])
            .env("SE_CLIPS_ORPHAN_PROBE", &path)
            .stdin(Stdio::null()).stdout(Stdio::null()).spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !path.exists() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        parent.kill().unwrap();
        parent.wait().unwrap();
        let pid: u32 = std::fs::read_to_string(&path).expect("child started").parse().unwrap();
        let exited = || std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .map_or(true, |s| s.split_once(") ").is_some_and(|(_, fields)| fields.starts_with('Z')));
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        while !exited() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let died = exited();
        if !died {
            // Clean up only this test's child if the parent-death protection regresses.
            unsafe { libc::kill(pid as i32, libc::SIGKILL) };
        }
        assert!(died, "background encoder survived its parent");
    }

    fn cut() -> Cut {
        Cut {
            video: "/rec/wide.mkv".into(),
            video_at: 83.5,
            audio: None,
            tracks: vec![0, 2],
            duration: 31.25,
            frame: Frame::Fit { w: 1920, h: 1080 },
            fps: 60.0,
            subtitles: Some("p83000_wide.ass".into()),
            out: "p83000_wide.mp4".into(),
        }
    }

    fn after<'a>(a: &'a [String], flag: &str) -> Vec<&'a str> {
        a.windows(2).filter(|w| w[0] == flag).map(|w| w[1].as_str()).collect()
    }

    #[test]
    fn wide_cut_mixes_non_music_tracks_burns_captions_and_uses_nvenc() {
        let a = cut_args(&cut(), Codec::Nvenc, &VideoConfig::default());
        assert_eq!(after(&a, "-ss"), vec!["83.500"]);
        assert_eq!(after(&a, "-t"), vec!["31.250"]);
        let fg = after(&a, "-filter_complex")[0];
        assert!(fg.starts_with("[0:v:0]scale=1920:1080:force_original_aspect_ratio=decrease"), "{fg}");
        assert!(fg.contains(",subtitles=f=p83000_wide.ass,format=yuv420p[v]"), "{fg}");
        // only streams 0 and 2 (music = 1 is not mapped anywhere)
        assert!(fg.contains(";[0:a:0][0:a:2]amix=inputs=2:duration=longest:normalize=0,loudnorm=I=-14:TP=-1.5:LRA=11,aresample=48000[a]"), "{fg}");
        assert!(!a.iter().any(|x| x.contains("0:a:1")));
        assert_eq!(after(&a, "-map"), vec!["[v]", "[a]"]);
        assert_eq!(after(&a, "-c:v"), vec!["h264_nvenc"]);
        assert_eq!(after(&a, "-g"), vec!["120"]);
        assert_eq!(a.last().unwrap(), "p83000_wide.mp4");
    }

    #[test]
    fn tall_crop_single_track_x264_and_separate_audio_input() {
        let mut c = cut();
        c.frame = Frame::Crop { w: 1080, h: 1920, center: 0.5 };
        c.tracks = vec![1];
        c.subtitles = None;
        let v = VideoConfig { loudnorm: false, ..VideoConfig::default() };
        let a = cut_args(&c, Codec::X264, &v);
        let fg = after(&a, "-filter_complex")[0];
        assert!(fg.starts_with("[0:v:0]crop=w='trunc(min(iw,ih*1080/1920)/2)*2'"), "{fg}");
        assert!(fg.contains("x='(iw-ow)*0.5000'") && fg.contains("scale=1080:1920"));
        assert!(fg.ends_with("[0:a:1]aresample=48000[a]"), "{fg}");
        assert_eq!(after(&a, "-c:v"), vec!["libx264"]);
        // tall video from its own recording, audio from the multitrack wide one
        c.audio = Some(("/rec/wide.mkv".into(), 90.0));
        c.video = "/rec/tall.mkv".into();
        let a = cut_args(&c, Codec::X264, &v);
        assert_eq!(after(&a, "-i"), vec!["/rec/tall.mkv", "/rec/wide.mkv"]);
        assert_eq!(after(&a, "-ss"), vec!["83.500", "90.000"]);
        assert!(after(&a, "-filter_complex")[0].contains("[1:a:1]"));
        // no tracks at all → no audio
        c.tracks.clear();
        let a = cut_args(&c, Codec::X264, &v);
        assert!(a.contains(&"-an".to_string()) && after(&a, "-map") == vec!["[v]"]);
    }

    #[test]
    fn probe_parsing_and_nvenc_error_classification() {
        let json = br#"{"streams":[
            {"index":0,"codec_type":"video","width":1920,"height":1080,"avg_frame_rate":"60000/1001"},
            {"index":1,"codec_type":"audio"},{"index":2,"codec_type":"audio"},{"index":3,"codec_type":"audio"}],
            "format":{"duration":"180.040000"}}"#;
        let p = parse_probe(json).unwrap();
        assert_eq!((p.width, p.height, p.audio_streams), (1920, 1080, 3));
        assert!((p.fps - 59.94).abs() < 0.01 && (p.duration - 180.04).abs() < 1e-9);
        assert!(parse_probe(br#"{"streams":[{"codec_type":"audio"}],"format":{}}"#).is_err());
        assert!(nvenc_unavailable("[h264_nvenc @ 0x5] OpenEncodeSessionEx failed: out of memory (10)"));
        assert!(!nvenc_unavailable("p1.mkv: No such file or directory"));
    }
}
