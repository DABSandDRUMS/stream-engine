//! Real cuts with ffmpeg on a generated 3-audio-track recording: the music track is left out
//! of the clip audio, captions are burned in, the tall crop has the right geometry, and an
//! NVENC failure falls back to libx264. Needs `ffmpeg`/`ffprobe` on PATH.

use se_clips::captions::{self, Layout};
use se_clips::config::{CaptionConfig, VideoConfig};
use se_clips::ffmpeg::{self, Codec, Cut, Frame};
use se_clips::transcribe::Word;
use std::path::Path;
use std::process::Command;

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

/// 12 s, 640×360@30, audio 0 = "mic" (1 kHz tone), 1 = "music" (440 Hz), 2 = "band" (quiet noise).
fn make_recording(dir: &Path) -> std::path::PathBuf {
    let out = dir.join("rec.mkv");
    let st = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30:duration=12"])
        .args(["-f", "lavfi", "-i", "sine=frequency=1000:sample_rate=48000:duration=12"])
        .args(["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=12"])
        .args(["-f", "lavfi", "-i", "anoisesrc=color=brown:amplitude=0.02:sample_rate=48000:duration=12"])
        .args(["-map", "0:v", "-map", "1:a", "-map", "2:a", "-map", "3:a", "-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"])
        .arg(&out)
        .status()
        .unwrap();
    assert!(st.success());
    out
}

/// RMS (dB) of the clip audio inside a narrow band around `hz`.
fn band_rms_db(file: &Path, hz: u32) -> f64 {
    let out = Command::new("ffmpeg")
        .args(["-v", "info", "-nostdin", "-i"])
        .arg(file)
        .args(["-map", "0:a:0", "-af", &format!("bandpass=f={hz}:width_type=h:w=30,astats=measure_perchannel=none"), "-f", "null", "-"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    err.lines()
        .filter_map(|l| l.split("RMS level dB:").nth(1))
        .filter_map(|v| v.trim().parse::<f64>().ok().or(if v.trim() == "-inf" { Some(-200.0) } else { None }))
        .next_back()
        .unwrap_or_else(|| panic!("no astats in {err}"))
}

fn words() -> Vec<Word> {
    ["Did", "you", "see", "that?"]
        .iter()
        .enumerate()
        .map(|(i, w)| Word { t0: 3.0 + i as f64 * 0.4, t1: 3.35 + i as f64 * 0.4, text: w.to_string(), p: 0.9, annotation: false })
        .collect()
}

#[test]
fn clips_drop_music_burn_captions_and_fall_back_to_x264() {
    if !have_ffmpeg() {
        eprintln!("ffmpeg not installed; skipping real cut test");
        return;
    }
    let d = tempfile::tempdir().unwrap();
    let rec = make_recording(d.path());
    let v = VideoConfig { loudnorm: false, ..VideoConfig::default() };
    let cc = CaptionConfig::default();
    let layout = Layout::for_canvas("wide", [640, 360], &cc);
    let cues = captions::cues(&words(), 2.0, 8.0, layout.max_chars, 3.5);
    std::fs::write(d.path().join("w.ass"), captions::ass(&cues, &layout, &cc)).unwrap();

    // an NVENC session that can't open (here: an invalid preset) must fall back to x264
    let bad = VideoConfig { nvenc_preset: "no-such-preset".into(), ..v.clone() };
    let wide = Cut {
        video: rec.clone(),
        video_at: 2.0,
        audio: None,
        tracks: vec![0, 2],
        duration: 6.0,
        frame: Frame::Fit { w: 640, h: 360 },
        fps: 30.0,
        subtitles: Some("w.ass".into()),
        out: "wide.mp4".into(),
    };
    let codec = ffmpeg::cut(&wide, Codec::Nvenc, true, &bad, d.path(), 10).unwrap();
    assert_eq!(codec, Codec::X264);
    assert!(!d.path().join("wide.part.mp4").exists());
    let p = ffmpeg::probe(&d.path().join("wide.mp4")).unwrap();
    assert_eq!((p.width, p.height, p.audio_streams), (640, 360, 1));
    assert!((p.duration - 6.0).abs() < 0.15, "duration {}", p.duration);
    // without fallback the same failure is an error
    assert!(ffmpeg::cut(&Cut { out: "x.mp4".into(), ..wide.clone() }, Codec::Nvenc, false, &bad, d.path(), 10).is_err());

    // reference cut with the music track mixed in
    let with_music = Cut { tracks: vec![0, 1, 2], subtitles: None, out: "music.mp4".into(), ..wide.clone() };
    ffmpeg::cut(&with_music, Codec::X264, false, &v, d.path(), 10).unwrap();
    let music_in = band_rms_db(&d.path().join("music.mp4"), 440);
    let music_out = band_rms_db(&d.path().join("wide.mp4"), 440);
    let mic_out = band_rms_db(&d.path().join("wide.mp4"), 1000);
    assert!(music_in - music_out > 20.0, "440 Hz: {music_in:.1} dB with music vs {music_out:.1} dB without");
    assert!(mic_out > -40.0, "mic tone missing: {mic_out:.1} dB");

    // captions are burned in: the frame under the caption differs from the uncaptioned cut
    let frame_at = |f: &str| {
        Command::new("ffmpeg")
            .args(["-v", "error", "-ss", "2.0", "-i"])
            .arg(d.path().join(f))
            .args(["-frames:v", "1", "-vf", "crop=640:90:0:270,format=gray", "-f", "rawvideo", "-"])
            .output()
            .unwrap()
            .stdout
    };
    let (a, b) = (frame_at("wide.mp4"), frame_at("music.mp4"));
    assert_eq!(a.len(), 640 * 90);
    let diff = a.iter().zip(&b).filter(|(x, y)| x.abs_diff(**y) > 60).count();
    assert!(diff > 500, "caption pixels differing: {diff}");

    // tall crop from the wide recording
    let tall = Cut { frame: Frame::Crop { w: 360, h: 640, center: 0.5 }, subtitles: None, out: "tall.mp4".into(), ..wide };
    ffmpeg::cut(&tall, Codec::X264, false, &v, d.path(), 10).unwrap();
    let p = ffmpeg::probe(&d.path().join("tall.mp4")).unwrap();
    assert_eq!((p.width, p.height), (360, 640));

    // thumbnail
    let thumb = d.path().join("t.jpg");
    ffmpeg::run(&ffmpeg::thumb_args(&d.path().join("tall.mp4"), 1.0, 180, &thumb), d.path(), 10).unwrap();
    assert!(std::fs::metadata(&thumb).unwrap().len() > 1000);
}

/// NVENC really encodes on this machine (RTX 3070). Run with `--ignored` on a box with an
/// NVIDIA GPU and no other process holding all encoder sessions.
#[test]
#[ignore = "needs an NVIDIA GPU with a free NVENC session"]
fn nvenc_encodes_here() {
    let d = tempfile::tempdir().unwrap();
    let rec = make_recording(d.path());
    let cut = Cut {
        video: rec,
        video_at: 1.0,
        audio: None,
        tracks: vec![0],
        duration: 4.0,
        frame: Frame::Fit { w: 1280, h: 720 },
        fps: 30.0,
        subtitles: None,
        out: "n.mp4".into(),
    };
    let codec = ffmpeg::cut(&cut, Codec::Nvenc, false, &VideoConfig::default(), d.path(), 0).unwrap();
    assert_eq!(codec, Codec::Nvenc);
    let p = ffmpeg::probe(&d.path().join("n.mp4")).unwrap();
    assert_eq!((p.width, p.height), (1280, 720));
}
