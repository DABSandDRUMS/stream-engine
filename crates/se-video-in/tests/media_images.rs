//! Pictures as media sources: a still image (PNG/JPEG/WebP) shows once and holds (no restart
//! loop, no `source.ended`), an animated GIF loops, and RGB pictures keep full color (RGBA).

use se_hub::media::PixelFormat;
use se_hub::{CoreMsg, Hub};
use se_proto::Value;
use se_video_in::{config, media, status::Status};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Make `name` in `dir` with the ffmpeg CLI; false when ffmpeg isn't installed.
fn make(dir: &Path, name: &str, args: &[&str]) -> bool {
    let made = std::process::Command::new("ffmpeg").args(["-v", "error", "-f", "lavfi"]).args(args).arg(dir.join(name)).status();
    if !made.is_ok_and(|s| s.success()) {
        eprintln!("ffmpeg CLI not available: skipped");
        return false;
    }
    true
}

struct Run {
    /// Distinct frames published, in order: (format, width, height).
    frames: Vec<(PixelFormat, u32, u32)>,
    /// Events the worker emitted while playing (type names).
    events: Vec<String>,
    /// Last value published per address while playing (before it was stopped).
    published: Vec<(String, Value)>,
}

/// Play `file` (project-relative to `dir`) for `secs`, sampling the slot every few ms.
fn play(dir: &Path, file: &str, extra: &str, secs: f64) -> Run {
    let (hub, core_rx) = Hub::new(Arc::new(se_clock::Clock::new()));
    let src = format!("file = \"{file}\"\nhwaccel = \"none\"\n{extra}");
    let def = Arc::new(config::parse("pic", &toml::from_str(&src).unwrap(), dir).unwrap());
    let writer = hub.video.register("pic");
    let mut reader = hub.video.take_reader("pic").unwrap();
    let (tx, rx) = crossbeam_channel::unbounded();
    let worker = media::spawn(hub.clone(), def, Arc::new(Status::default()), writer, rx).unwrap();
    let mut frames = Vec::new();
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        if let Some(f) = reader.fresh() {
            frames.push((f.format, f.width, f.height));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    // what it said while playing (stopping publishes the idle state)
    let mut events = Vec::new();
    let mut published: Vec<(String, Value)> = Vec::new();
    while let Ok(m) = core_rx.try_recv() {
        match m {
            CoreMsg::Input(se_core::Input::Event { event }) => events.push(event.ty),
            CoreMsg::Input(se_core::Input::Publish { address, value }) => {
                published.retain(|(a, _)| *a != address);
                published.push((address, value));
            }
            _ => {}
        }
    }
    tx.send(media::Cmd::Stop).unwrap();
    worker.join().unwrap();
    Run { frames, events, published }
}

impl Run {
    fn get(&self, address: &str) -> Option<&Value> {
        self.published.iter().find(|(a, _)| a == address).map(|(_, v)| v)
    }
}

#[test]
fn a_still_image_shows_once_and_holds_its_picture() {
    let dir = tempfile::tempdir().unwrap();
    let src = ["-i", "testsrc=size=64x48:rate=1", "-frames:v", "1"];
    if !make(dir.path(), "still.png", &src) || !make(dir.path(), "still.jpg", &src) || !make(dir.path(), "still.webp", &src) {
        return;
    }
    for (file, format) in [("still.png", PixelFormat::Rgba8), ("still.jpg", PixelFormat::Nv12), ("still.webp", PixelFormat::Nv12)] {
        // loop is on by default: a picture must still not restart over and over
        let run = play(dir.path(), file, "", 0.6);
        assert_eq!(run.frames, vec![(format, 64, 48)], "{file}: one frame, then held");
        assert!(!run.events.iter().any(|e| e == "source.ended"), "{file}: a picture never ends");
        assert_eq!(run.get("source.pic.error"), Some(&Value::Str(String::new())), "{file}");
        assert_eq!(run.get("source.pic.signal"), Some(&Value::Bool(true)), "{file}: showing a picture");
    }
}

#[test]
fn an_animated_gif_loops_as_rgba() {
    let dir = tempfile::tempdir().unwrap();
    // 5 frames at 10 fps = 0.5 s per pass
    if !make(dir.path(), "anim.gif", &["-i", "testsrc=size=64x48:rate=10", "-t", "0.5", "-loop", "0"]) {
        return;
    }
    let run = play(dir.path(), "anim.gif", "", 1.4);
    assert!(run.frames.len() >= 11, "loops past its 5 frames: {}", run.frames.len());
    assert!(run.frames.iter().all(|f| *f == (PixelFormat::Rgba8, 64, 48)), "{:?}", run.frames.first());
    assert!(!run.events.iter().any(|e| e == "source.ended"));

    // loop = false: plays once, holds its last frame, and ends
    let run = play(dir.path(), "anim.gif", "loop = false", 0.9);
    assert!((4..=5).contains(&run.frames.len()), "one pass: {}", run.frames.len());
    assert_eq!(run.events.iter().filter(|e| *e == "source.ended").count(), 1);
}
