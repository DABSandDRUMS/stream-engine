//! LTC out end to end in real time: the output thread follows a timeline through the state
//! snapshot and writes the `timecode.ltc` audio slot; the captured audio decodes with our
//! decoder and with libltc's `ltcdump` (when installed in ~/.local/share/stream-engine/tools).

use se_clock::timecode::ltc::LtcDecoder;
use se_clock::timecode::{FrameRate, Timecode, wav};
use se_timeline::follow::snapshot_for;
use se_timeline::{Health, ltc};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

#[test]
fn ltc_out_thread_writes_decodable_timecode() {
    let (hub, _rx) = se_hub::Hub::new(Arc::new(se_clock::Clock::new()));
    let health = Arc::new(Health::new(hub.clone()));
    let stop = Arc::new(AtomicBool::new(false));
    let start_tc = 3600.0 + 59.0 * 60.0 + 58.0; // 01:59:58:00 → crosses the hour
    let t0 = se_clock::now();
    hub.snapshot.store(Arc::new(snapshot_for("show", 0, t0, start_tc, true)));
    let h = ltc::spawn_output(hub.clone(), health, "show".into(), FrameRate::Fps2997Df, -12.0, 0.0, stop.clone()).unwrap();
    // wait for the slot, then drain it like the audio graph would
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut stream = loop {
        if let Some((_, s)) = hub.audio.take_new().into_iter().find(|(n, _)| n == ltc::SLOT) {
            break s;
        }
        assert!(Instant::now() < deadline, "slot registered");
        std::thread::sleep(Duration::from_millis(2));
    };
    assert_eq!((stream.channels, stream.rate), (1, 48_000));
    let mut audio = Vec::new();
    let begin = Instant::now();
    let mut tick = 1;
    while begin.elapsed() < Duration::from_secs(3) {
        // the core publishes the timeline every ~8 ms
        let now = se_clock::now();
        hub.snapshot.store(Arc::new(snapshot_for("show", tick, now, start_tc + (now - t0) as f64 / 1e9, true)));
        tick += 1;
        // play out in real time (48 samples per ms)
        let due = (begin.elapsed().as_secs_f64() * 48_000.0) as usize;
        while audio.len() < due {
            match stream.consumer.pop() {
                Ok(s) => audio.push(s),
                Err(_) => break,
            }
        }
        std::thread::sleep(Duration::from_millis(8));
    }
    stop.store(true, Ordering::Relaxed);
    h.join().unwrap();

    let peak = audio.iter().fold(0.0f32, |m, s| m.max(s.abs()));
    assert!((peak - 0.251).abs() < 0.01, "-12 dBFS: {peak}");
    let mut dec = LtcDecoder::new(48_000, FrameRate::Fps30);
    let mut got = Vec::new();
    dec.feed(&audio, |f| got.push(f));
    assert!(got.len() > 80, "decoded {} frames", got.len());
    for w in got.windows(2) {
        assert_eq!(w[1].frame.tc.to_frames(), w[0].frame.tc.to_frames() + 1, "{} → {}", w[0].frame.tc, w[1].frame.tc);
    }
    assert_eq!(got[0].frame.tc.rate, FrameRate::Fps2997Df);
    assert!(got.iter().any(|g| g.frame.tc.to_string() == "02:00:00;00"), "crossed the hour");
    // every frame is on the timeline's clock: frame start sample ↔ timeline time (±1 frame for
    // the real-time drain jitter of this test)
    let first = &got[5];
    let offset = first.frame.tc.to_seconds() - first.start / 48_000.0;
    for g in got.iter().skip(5) {
        let d = g.frame.tc.to_seconds() - g.start / 48_000.0 - offset;
        assert!(d.abs() < 0.004, "{} drifts {d}", g.frame.tc);
    }

    // external decoder
    let path = std::env::temp_dir().join(format!("se-ltc-out-{}.wav", std::process::id()));
    std::fs::write(&path, wav::encode_pcm16(&audio, 48_000, 1)).unwrap();
    let tool = std::path::PathBuf::from(std::env::var("HOME").unwrap()).join(".local/share/stream-engine/tools/ltc/bin/ltcdump");
    if tool.exists() {
        let out = std::process::Command::new(&tool).args(["-f", "30000/1001"]).arg(&path).output().unwrap();
        let text = String::from_utf8_lossy(&out.stdout);
        let labels: Vec<String> =
            text.lines().filter(|l| !l.starts_with('#')).filter_map(|l| l.split_whitespace().nth(1)).map(|s| s.replace('.', ";")).collect();
        assert!(labels.len() + 2 >= got.len(), "ltcdump decoded {} of {}:\n{text}", labels.len(), got.len());
        let ours: Vec<String> = got.iter().map(|g| g.frame.tc.to_string()).collect();
        for l in &labels {
            assert!(ours.contains(l), "ltcdump saw {l}");
        }
        assert!(labels.contains(&"02:00:00;00".to_string()));
        eprintln!("ltcdump decoded {} frames: {} … {}", labels.len(), labels[0], labels[labels.len() - 1]);
    } else {
        eprintln!("ltcdump not installed; external decode skipped");
    }
    let _ = std::fs::remove_file(&path);
    let _ = Timecode::default();
}
