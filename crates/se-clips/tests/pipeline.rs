//! M10 acceptance on a synthetic session: a 3-minute recording (video + mic speech from
//! espeak-ng + music + band tracks) with hype and manual markers → the post-stream job
//! produces ranked wide + tall clips with burned captions and without the music track.
//! Needs ffmpeg, espeak-ng, and the Whisper model (downloaded on first run, ~490 MB), so it
//! runs on demand: `cargo test -p se-clips --test pipeline -- --ignored --nocapture`.

use se_clips::config::ClipsConfig;
use se_clips::job::{self, JobEnv};
use se_clips::store;
use se_store::Db;
use std::path::{Path, PathBuf};
use std::process::Command;

const START_NS: i64 = 5_000_000_000_000;
const SEC: i64 = 1_000_000_000;

const SPEECH: &[(f64, &str)] = &[
    (15.0, "Welcome back everyone. Tonight we are learning the new song."),
    (52.0, "Oh my god, did you see that? That was the best drum fill I have ever played. Let's go!"),
    (95.0, "Thank you so much for the raid. Welcome raiders, grab a seat and enjoy the show."),
    (138.0, "Alright, one more time from the top. Here we go, count it in."),
];

fn run(cmd: &mut Command) {
    let out = cmd.output().unwrap();
    assert!(out.status.success(), "{cmd:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Write a synthetic multitrack recording; returns its path.
fn make_recording(dir: &Path) -> PathBuf {
    let mut ff = Command::new("ffmpeg");
    ff.args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=60:duration=180"]);
    let mut fg = String::new();
    for (i, (at, text)) in SPEECH.iter().enumerate() {
        let wav = dir.join(format!("s{i}.wav"));
        run(Command::new("espeak-ng").args(["-v", "en-us", "-s", "150", "-w"]).arg(&wav).arg(text));
        ff.arg("-i").arg(&wav);
        fg.push_str(&format!("[{}:a]aresample=48000,adelay={}:all=1[s{i}];", i + 1, (at * 1000.0) as u64));
    }
    let n = SPEECH.len();
    for i in 0..n {
        fg.push_str(&format!("[s{i}]"));
    }
    fg.push_str(&format!("amix=inputs={n}:normalize=0,apad=whole_dur=180[mic];"));
    // music: a 440 Hz lead with a 660 Hz beep; band: brown noise + a 2 Hz kick
    fg.push_str("sine=frequency=440:sample_rate=48000:duration=180,volume=0.5[m1];sine=frequency=660:beep_factor=2:sample_rate=48000:duration=180,volume=0.2[m2];[m1][m2]amix=inputs=2:normalize=0[music];");
    fg.push_str("anoisesrc=color=brown:amplitude=0.03:sample_rate=48000:duration=180[noise];aevalsrc='0.6*sin(2*PI*55*t)*exp(-25*mod(t\\,0.5))':s=48000:d=180[kick];[noise][kick]amix=inputs=2:normalize=0[band]");
    let out = dir.join("2026-09-25 20-00-00.mkv");
    let nvenc =
        Command::new("ffmpeg").args(["-hide_banner", "-encoders"]).output().map(|o| String::from_utf8_lossy(&o.stdout).contains("h264_nvenc")).unwrap_or(false);
    ff.args(["-filter_complex", &fg, "-map", "0:v", "-map", "[mic]", "-map", "[music]", "-map", "[band]"]);
    if nvenc {
        ff.args(["-c:v", "h264_nvenc", "-preset", "p1", "-cq", "28"]);
    } else {
        ff.args(["-c:v", "libx264", "-preset", "ultrafast", "-crf", "30"]);
    }
    ff.args(["-c:a", "aac", "-b:a", "128k", "-t", "180"]).arg(&out);
    run(&mut ff);
    out
}

fn write_session(session_dir: &Path, rec: &Path) {
    std::fs::create_dir_all(session_dir).unwrap();
    let hype = |ts: i64, s: i64, p: i64, e: i64, score: f64, reasons: &[&str]| {
        serde_json::json!({"ts": START_NS + ts * SEC, "wall_ms": 1_790_000_000_000i64 + ts * 1000, "label": "hype", "origin": "patch",
            "args": {"kind": "hype", "label": "hype", "start": START_NS + s * SEC, "peak": START_NS + p * SEC, "end": START_NS + e * SEC,
                     "score": score, "reasons": reasons}})
    };
    let markers = serde_json::json!([
        hype(75, 45, 60, 68, 1.6, &["chat", "emotes"]),
        {"ts": START_NS + 112 * SEC, "wall_ms": 1_790_000_112_000i64, "label": "raid", "origin": "deck", "args": {"label": "raid"}},
        hype(160, 130, 145, 152, 1.1, &["bits"]),
    ]);
    std::fs::write(session_dir.join("markers.json"), serde_json::to_string_pretty(&markers).unwrap()).unwrap();
    let meta = format!(
        r#"recordings = [{{ canvas = "wide", path = "{}", start_ns = {START_NS}, end_ns = {}, tracks = [
  {{ index = 0, mixer = 2, name = "Mic", sources = ["Mic"], devices = ["se-mic"] }},
  {{ index = 1, mixer = 3, name = "Music", sources = ["se-music"], devices = ["se-music"] }},
  {{ index = 2, mixer = 4, name = "Band", sources = ["se-band"], devices = ["se-band"] }} ] }}]
"#,
        rec.display(),
        START_NS + 180 * SEC
    );
    std::fs::write(session_dir.join("meta.toml"), meta).unwrap();
}

fn band_rms_db(file: &Path, stream: &str, hz: u32) -> f64 {
    let out = Command::new("ffmpeg")
        .args(["-v", "info", "-nostdin", "-i"])
        .arg(file)
        .args(["-map", stream, "-af", &format!("bandpass=f={hz}:width_type=h:w=20,astats=measure_perchannel=none"), "-f", "null", "-"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    err.lines().filter_map(|l| l.split("RMS level dB:").nth(1)).filter_map(|v| v.trim().parse::<f64>().ok()).next_back().unwrap_or(-200.0)
}

fn rms_db(file: &Path, stream: &str) -> f64 {
    let out = Command::new("ffmpeg")
        .args(["-v", "info", "-nostdin", "-i"])
        .arg(file)
        .args(["-map", stream, "-af", "astats=measure_perchannel=none", "-f", "null", "-"])
        .output()
        .unwrap();
    let err = String::from_utf8_lossy(&out.stderr);
    err.lines().filter_map(|l| l.split("RMS level dB:").nth(1)).filter_map(|v| v.trim().parse::<f64>().ok()).next_back().unwrap_or(-200.0)
}

#[test]
#[ignore = "needs ffmpeg, espeak-ng, and the Whisper model; ~1 min"]
fn synthetic_session_produces_ranked_wide_and_tall_clips() {
    let root = tempfile::tempdir().unwrap();
    let rec_dir = root.path().join("Videos");
    std::fs::create_dir_all(&rec_dir).unwrap();
    let t = std::time::Instant::now();
    let rec = make_recording(&rec_dir);
    eprintln!("fixture recording: {:.1} s", t.elapsed().as_secs_f64());
    let project = root.path().join("project");
    write_session(&project.join("sessions/20260925-200000"), &rec);

    let db = Db::open(&root.path().join("runtime.db")).unwrap();
    store::migrate(&db).unwrap();
    let mut cfg = ClipsConfig { auto_rank: false, ..ClipsConfig::default() };
    cfg.audio.drop = vec!["music".into(), "program".into()]; // explicit opt-in for this separate-track fixture
    let env = JobEnv { db: db.clone(), project_root: project.clone(), data_dir: se_store::data_dir(), cfg };
    let report = job::process(&env, "20260925-200000", &|p| eprintln!("  [{}] {}/{} {}", p.stage, p.done, p.total, p.detail)).unwrap();
    eprintln!("report: {report:?}");
    assert_eq!((report.clips, report.failed), (3, 0), "{report:?}");

    let clips = store::list(&db, Some("20260925-200000"), None, 10).unwrap();
    assert_eq!(clips.iter().map(|c| c.rank).collect::<Vec<_>>(), vec![1, 2, 3]);
    assert!(clips.windows(2).all(|w| w[0].score >= w[1].score));
    // the manual "clip that" (score 3) ranks first
    assert_eq!(clips[0].labels, vec!["raid"]);
    for c in &clips {
        eprintln!("#{} {} score {:.2} in {:.2} out {:.2} [{}] {}", c.rank, c.key, c.score, c.in_s, c.out_s, c.encoder, c.captions);
        let wide = PathBuf::from(c.wide_path.as_ref().unwrap());
        let tall = PathBuf::from(c.tall_path.as_ref().unwrap());
        let pw = se_clips::ffmpeg::probe(&wide).unwrap();
        let pt = se_clips::ffmpeg::probe(&tall).unwrap();
        assert_eq!((pw.width, pw.height, pw.audio_streams), (1920, 1080, 1));
        assert_eq!((pt.width, pt.height, pt.audio_streams), (1080, 1920, 1));
        assert!((pw.duration - (c.out_s - c.in_s)).abs() < 0.2);
        assert!(c.music_dropped);
        assert!(Path::new(c.wide_thumb.as_ref().unwrap()).exists() && Path::new(c.tall_thumb.as_ref().unwrap()).exists());
        let words: Vec<se_clips::transcribe::Word> = serde_json::from_str(&c.words).unwrap();
        eprintln!("   words: {}", words.iter().map(|w| format!("{:.2}:{}", w.t0, w.text)).collect::<Vec<_>>().join(" "));
        // music (a steady 440 Hz lead, louder than the band) is not in the clip: with it the
        // 440 Hz band would carry most of the clip's energy; without it only speech harmonics
        // and drum transients land there
        let clip_440 = band_rms_db(&wide, "0:a:0", 440);
        let clip_all = rms_db(&wide, "0:a:0");
        assert!(clip_all - clip_440 > 12.0, "{}: 440 Hz at {clip_440:.1} dB vs {clip_all:.1} dB overall", c.key);
        assert!(!c.captions.is_empty(), "{}: no captions", c.key);
    }
    // …while in the recording's music track that band carries most of the energy
    let (src_440, src_all) = (band_rms_db(&rec, "0:a:1", 440), rms_db(&rec, "0:a:1"));
    assert!(src_all - src_440 < 8.0, "music track: 440 Hz at {src_440:.1} dB vs {src_all:.1} dB overall");
    let fill = clips.iter().find(|c| c.key == format!("p{}", (START_NS + 60 * SEC) / 1_000_000)).unwrap();
    let text = fill.captions.to_lowercase();
    assert!(text.contains("fill") || text.contains("drum"), "{text}");
    // in/out on sentence boundaries: the fill clip starts before "Oh my god" (52 s) and after the marker start
    assert!(fill.in_s <= 52.0 && fill.in_s >= 44.0 - 4.0, "in {}", fill.in_s);
    eprintln!("timings: {}", report.timings_value());
}
