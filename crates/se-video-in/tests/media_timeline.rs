//! A media file drives the timelines following it (§2.7): the worker publishes the file's
//! identity (`file:<hash>` = the offline analysis key, ISRC from its tags) and exact frame
//! positions; fed into a core, a `media = "file:<hash>"` timeline fires its cue on time.

use se_core::config::SourceFile;
use se_core::{Config, Core, Input, Output};
use se_hub::{CoreMsg, Hub};
use se_proto::Value;
use se_video_in::{config, media, status::Status};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MS: u64 = 1_000_000;

#[test]
fn media_file_identity_and_frames_drive_its_timeline() {
    let dir = tempfile::tempdir().unwrap();
    let clip = dir.path().join("clip.mkv");
    let made = std::process::Command::new("ffmpeg")
        .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=64x48:rate=30", "-t", "3", "-c:v", "mpeg4", "-metadata", "ISRC=US-RC1-76-07839"])
        .arg(&clip)
        .status();
    if !made.is_ok_and(|s| s.success()) {
        eprintln!("ffmpeg CLI not available: skipped");
        return;
    }
    let id = se_analysis::offline::media_id(&std::fs::read(&clip).unwrap());

    let (hub, core_rx) = Hub::new(Arc::new(se_clock::Clock::new()));
    let src = format!("file = \"{}\"\nloop = false\nhwaccel = \"none\"", clip.display());
    let def = Arc::new(config::parse("clip", &toml::from_str(&src).unwrap(), dir.path()).unwrap());
    let writer = hub.video.register("clip");
    let (tx, rx) = crossbeam_channel::unbounded();
    let worker = media::spawn(hub.clone(), def, Arc::new(Status::default()), writer, rx).unwrap();
    // the file plays once (3 s); collect everything the worker hands the core
    let mut inputs = Vec::new();
    let deadline = Instant::now() + Duration::from_millis(3_600);
    while let Ok(m) = core_rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        if let CoreMsg::Input(i) = m {
            inputs.push(i);
        }
    }
    tx.send(media::Cmd::Stop).unwrap();
    worker.join().unwrap();

    let published = |a: &str| {
        inputs.iter().rev().find_map(|i| match i {
            Input::Publish { address, value } if address == a => Some(value.clone()),
            _ => None,
        })
    };
    assert_eq!(published("source.clip.media"), Some(Value::Str(id.clone())), "same key as the offline beat grid");
    assert_eq!(published("source.clip.isrc"), Some(Value::Str("USRC17607839".into())));
    let obs = |key: &str| -> Vec<se_clock::timecode::TcObs> {
        inputs
            .iter()
            .filter_map(|i| match i {
                Input::Timecode { source, obs } if source == key => Some(*obs),
                _ => None,
            })
            .collect()
    };
    let file_obs = obs(&format!("media:{id}"));
    assert_eq!(obs("media:isrc:USRC17607839").len(), file_obs.len(), "the ISRC key gets the same observations");
    use se_clock::timecode::ObsKind;
    let runs: Vec<_> = file_obs.iter().filter(|o| o.kind == ObsKind::Run).collect();
    assert!(runs.len() >= 30, "≈15 observations/s: {}", runs.len());
    // exact: position advances with the master-clock time of each shown frame
    for w in runs.windows(2) {
        let (dp, dt) = (w[1].seconds - w[0].seconds, (w[1].ts - w[0].ts) as f64 / 1e9);
        assert!((dp - dt).abs() < 0.002, "Δpos {dp} vs Δt {dt}");
    }
    assert_eq!(file_obs.last().map(|o| o.kind), Some(ObsKind::Stop), "the end of the file stops the timeline");

    // replay those observations into a core with a timeline on this file
    let tl = format!("media = \"{id}\"\ncues = [{{ at = \"0:01.500\", do = [\"emit clip.cue\"] }}]");
    let files = [
        SourceFile { kind: "project".into(), name: "project".into(), path: "project.toml".into(), table: toml::from_str("schema = 1").unwrap() },
        SourceFile { kind: "timelines".into(), name: "clip".into(), path: "timelines/clip.toml".into(), table: toml::from_str(&tl).unwrap() },
    ];
    let first = runs[0];
    let mut c = Core::new(Config::build(&files), first.ts - 10 * MS);
    let mut pending = file_obs.iter().peekable();
    let mut fired_at = None;
    let end = file_obs.last().unwrap().ts + 200 * MS;
    while c.now() < end {
        while let Some(o) = pending.next_if(|o| o.ts <= c.now()) {
            c.submit(Input::Timecode { source: format!("media:{id}"), obs: *o });
        }
        c.step();
        for o in c.drain_outputs() {
            if matches!(&o, Output::Event(e) if e.ty == "clip.cue") {
                fired_at = Some(c.now());
            }
        }
    }
    let expected = first.ts as f64 + (1.5 - first.seconds) * 1e9;
    let fired = fired_at.expect("cue fired") as f64;
    assert!((fired - expected).abs() < 20e6, "cue {:.1} ms off", (fired - expected) / 1e6);
    assert_eq!(c.get("timeline.clip.playing"), Some(&Value::Bool(false)), "stopped with the file");
}
