//! Real-time stereo taps must retain fresh song tempo/phase instead of resetting the tracker
//! through frame/sample timestamp drift. Auto source selection gives an audible song priority.

use se_audio::ana::{self, Msg, Setup, Source};
use se_audio::taps::TapProducer;
use se_proto::Value;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const SR: u32 = 48000;
const BLOCK: usize = 256;

/// Mono kick/snare/hat loop: kick on beats 1 and 3, snare on 2 and 4, hats on eighths.
fn drum_loop(bpm: f64, secs: f64) -> Vec<f32> {
    let sr = SR as f64;
    let n = (secs * sr) as usize;
    let mut out = vec![0.0f32; n];
    let beat = 60.0 / bpm;
    let mut seed = 0x1234_5678u32;
    let mut noise = move || {
        seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        (seed >> 8) as f32 / (1u32 << 23) as f32 - 1.0
    };
    let mut add = |start: usize, len_s: f64, f: &mut dyn FnMut(f64) -> f32| {
        for k in 0..(len_s * sr) as usize {
            if let Some(s) = out.get_mut(start + k) {
                *s += f(k as f64 / sr);
            }
        }
    };
    let mut index = 0usize;
    while index as f64 * beat < secs {
        let start = (index as f64 * beat * sr) as usize;
        for eighth in 0..2 {
            let at = start + (eighth as f64 * beat / 2.0 * sr) as usize;
            add(at, 0.04, &mut |t| 0.15 * noise() * (-t / 0.01).exp() as f32);
        }
        if index % 2 == 0 {
            let mut phase = 0.0f64;
            add(start, 0.35, &mut |t| {
                phase += 2.0 * std::f64::consts::PI * (45.0 + 110.0 * (-t / 0.03).exp()) / sr;
                (0.8 * phase.sin() * (-t / 0.12).exp()) as f32
            });
        } else {
            add(start, 0.2, &mut |t| {
                ((0.5 * noise() as f64 + 0.3 * (2.0 * std::f64::consts::PI * 190.0 * t).sin()) * (-t / 0.06).exp()) as f32
            });
        }
        index += 1;
    }
    out
}

/// What feeds a bus tap.
enum Feed {
    /// No audio arrives at all (stale).
    Nothing,
    /// Digital silence, in real time.
    Silence,
    /// This mono signal on both channels, in real time.
    Audio(Vec<f32>),
}

fn source(prefix: &str) -> (Source, TapProducer) {
    let (sp, sc) = rtrb::RingBuffer::new(SR as usize * 2);
    let (cp, cc) = rtrb::RingBuffer::new(4096);
    let producer = TapProducer { name: format!("analysis.{prefix}"), samples: sp, clock: cp, written: 0, dropped: 0 };
    (Source { prefix: prefix.into(), samples: sc, clock: cc, is_mic: false }, producer)
}

/// Push stereo blocks at the audio rate, stamped with the master clock like the RT graph.
fn feed(mut producers: Vec<(TapProducer, Feed)>, stop: Arc<AtomicBool>, master_start: u64) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let start = Instant::now();
        let mut block = vec![0.0f32; 2 * BLOCK];
        let mut pos = 0usize;
        while !stop.load(Ordering::Relaxed) {
            let due = start + Duration::from_secs_f64(pos as f64 / SR as f64);
            if let Some(wait) = due.checked_duration_since(Instant::now()) {
                std::thread::sleep(wait);
            }
            let ts = master_start + (pos as f64 * 1e9 / SR as f64) as u64;
            for (p, f) in &mut producers {
                let mono: &[f32] = match f {
                    Feed::Nothing => continue,
                    Feed::Silence => &[],
                    Feed::Audio(s) => s.get(pos..pos + BLOCK).unwrap_or(&[]),
                };
                for i in 0..BLOCK {
                    let v = mono.get(i).copied().unwrap_or(0.0);
                    block[2 * i] = v;
                    block[2 * i + 1] = v;
                }
                p.push(&block, ts);
            }
            pos += BLOCK;
        }
    })
}

struct Run {
    /// Seconds from the first audio to the first locked clock reading.
    first_lock_s: Option<f64>,
    /// Clock state at the end of the run.
    locked: bool,
    bpm: f64,
    bus: String,
    confidence: f64,
    age_ms: f64,
    phase_error: f64,
    /// Confidence of the selected song's real tracker, not the clock override.
    song_confidence: f64,
    band_locked_before_song: bool,
}

/// Feed `band` and `music` (auto beat source) for `secs` and watch the published clock.
fn run(band: Feed, music: Feed, secs: u64, expected_bpm: f64, song_offset_s: f64) -> Run {
    let (hub, _core) = se_hub::Hub::new(Arc::new(se_clock::Clock::new()));
    let handle = ana::spawn(hub).unwrap();
    let (band_src, band_tap) = source("band");
    let (music_src, music_tap) = source("music");
    handle
        .tx
        .send(Msg::Setup(Box::new(Setup {
            rate: SR,
            sources: vec![band_src, music_src],
            beat_source: "auto".into(),
            min_bpm: 70.0,
            max_bpm: 180.0,
            talk_threshold_db: -42.0,
            talk_hold_ms: 600.0,
        })))
        .unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let master_start = se_clock::now();
    let feeder = feed(vec![(band_tap, band), (music_tap, music)], stop.clone(), master_start);
    let start = Instant::now();
    let mut out = Run {
        first_lock_s: None, locked: false, bpm: 0.0, bus: String::new(),
        confidence: 0.0, age_ms: f64::INFINITY, phase_error: 1.0,
        song_confidence: 0.0, band_locked_before_song: false,
    };
    while start.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_millis(50));
        let Some(beat) = handle.latest.lock().get("beat").cloned() else { continue };
        let field = |k: &str| beat.get_path(k).cloned().unwrap_or(Value::Null);
        out.locked = field("locked").truthy();
        out.bpm = field("bpm").as_f64().unwrap_or(0.0);
        out.bus = field("bus").as_str().unwrap_or("").to_string();
        out.confidence = field("confidence").as_f64().unwrap_or(0.0);
        out.age_ms = field("age_ms").as_f64().unwrap_or(f64::INFINITY);
        out.song_confidence = handle.latest.lock().get("music")
            .and_then(|m| m.get_path("confidence")).and_then(Value::as_f64).unwrap_or(0.0);
        let (_, _, position, ts, _) = handle.beat.load();
        let expected = (ts.saturating_sub(master_start) as f64 * 1e-9 - song_offset_s) * expected_bpm / 60.0;
        out.phase_error = ((position - expected + 0.5).rem_euclid(1.0) - 0.5).abs();
        if out.locked && out.bus == "band" && start.elapsed().as_secs_f64() < song_offset_s {
            out.band_locked_before_song = true;
        }
        if out.locked && out.first_lock_s.is_none() {
            out.first_lock_s = Some(start.elapsed().as_secs_f64());
        }
    }
    stop.store(true, Ordering::Relaxed);
    feeder.join().unwrap();
    out
}

fn assert_locked(r: &Run, bpm: f64) {
    let first = r.first_lock_s.expect("the clock never locked");
    assert!(first <= 6.0, "first lock after {first:.1} s");
    assert!(r.locked, "lock lost by the end of the run");
    assert_eq!(r.bus, "music");
    // the clock's tempo servo has converged from its 120 BPM start by now
    assert!((r.bpm - bpm).abs() <= 1.5, "clock at {:.2} BPM, want {bpm}", r.bpm);
    assert!(r.confidence >= 0.35, "locked without measured confidence: {}", r.confidence);
    assert!(r.song_confidence >= 0.35, "song tracker did not acquire: {}", r.song_confidence);
    assert!(r.age_ms < 150.0, "stereo timestamp drift: audio age {} ms", r.age_ms);
    assert!(r.phase_error <= 0.15, "clock phase error {} beats", r.phase_error);
}

#[test]
fn locks_to_96_bpm_and_ignores_a_stale_bus() {
    assert_locked(&run(Feed::Nothing, Feed::Audio(drum_loop(96.0, 20.0)), 12, 96.0, 0.0), 96.0);
}

#[test]
fn locks_to_128_bpm_and_ignores_a_silent_bus() {
    assert_locked(&run(Feed::Silence, Feed::Audio(drum_loop(128.0, 20.0)), 12, 128.0, 0.0), 128.0);
}

#[test]
fn an_audible_song_takes_over_from_a_locked_kit() {
    let mut song = vec![0.0; 6 * SR as usize];
    song.extend(drum_loop(128.0, 20.0));
    let r = run(Feed::Audio(drum_loop(96.0, 30.0)), Feed::Audio(song), 20, 128.0, 6.0);
    assert!(r.band_locked_before_song, "kit never acquired before the song started");
    assert_locked(&r, 128.0);
}

#[test]
fn stopped_song_loses_confidence_and_freewheels() {
    let r = run(Feed::Nothing, Feed::Audio(drum_loop(128.0, 10.0)), 12, 128.0, 0.0);
    assert!(r.first_lock_s.is_some(), "song never acquired before stopping");
    assert!(!r.locked, "stopped song retained lock");
    assert_eq!(r.confidence, 0.0, "stopped song retained effective confidence");
    assert_eq!(r.bus, "", "auto retained a silent or stale source");
    assert!((r.bpm - 128.0).abs() <= 1.5, "freewheel lost the acquired tempo: {}", r.bpm);
}
