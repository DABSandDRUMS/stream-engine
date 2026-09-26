//! New projects have no timeline programming; explicitly authored media cues still fire on time.

use se_core::config::SourceFile;
use se_core::{Config, Core, Input, Output};
use se_proto::Value;
use se_store::Project;

fn starter() -> Config {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../project-example");
    let p = Project::open(&root).unwrap();
    let loaded = p.load();
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    Config::build(&loaded.files)
}

#[test]
fn starter_has_no_timelines() {
    let cfg = starter();
    let (defs, errs) = se_core::timeline::parse_all(&cfg);
    assert!(errs.is_empty(), "{errs:?}");
    assert!(defs.is_empty(), "timeline programming must be created explicitly");
}

#[test]
fn nightly_song_chorus_fires_on_time() {
    let files = [
        SourceFile {
            kind: "timelines".into(),
            name: "song".into(),
            path: "timelines/song.toml".into(),
            table: "media = \"yt:VIDEO_ID\"\ncues = [{ at = \"1:12.400\", do = [\"preset.fire chorus_blast\"], label = \"chorus\" }]".parse().unwrap(),
        },
        SourceFile {
            kind: "presets".into(),
            name: "chorus_blast".into(),
            path: "presets/chorus_blast.toml".into(),
            table: "hold = \"4s\"\nset = { \"fx.zoom_pulse.amount\" = 0.6 }".parse().unwrap(),
        },
    ];
    let mut c = Core::new(Config::build(&files), 1_000_000_000);
    assert!(c.config.errors.iter().all(|e| !e.file.starts_with("timelines/")), "{:?}", c.config.errors);
    c.submit(Input::Publish { address: "song.media".into(), value: "yt:VIDEO_ID".into() });
    c.submit(Input::Publish { address: "song.state".into(), value: "playing".into() });
    c.step();
    let start = c.now();
    let mut fired = Vec::new();
    let mut presets = Vec::new();
    let period = c.period();
    // player reports every 33 ms from 1:05, like the Songs player page
    for k in 0..(12_000_000_000 / period) {
        if k % 8 == 0 {
            c.submit(Input::Signal { name: "song.position".into(), value: (65.0 + (k * period) as f64 / 1e9) as f32 });
        }
        c.step();
        for o in c.drain_outputs() {
            if let Output::Event(e) = o {
                let song_t = 65.0 + (c.now() - start) as f64 / 1e9;
                if e.ty == "timeline.cue" {
                    fired.push((e.payload.get_path("label").cloned().unwrap_or_default(), song_t));
                }
                if e.ty == "preset.chorus_blast.fired" {
                    presets.push(song_t);
                }
            }
        }
    }
    assert_eq!(fired.len(), 1, "{fired:?}");
    assert_eq!(fired[0].0, Value::Str("chorus".into()));
    assert!((fired[0].1 - 72.4).abs() < 0.02, "chorus at {}", fired[0].1);
    assert_eq!(presets.len(), 1);
    assert!((presets[0] - 72.4).abs() < 0.02);
}
