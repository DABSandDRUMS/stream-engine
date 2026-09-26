//! Offline song analysis (PLAN §8.4): beat grid, sections, chorus candidates, decoding.

mod common;

use common::*;
use se_analysis::offline::{analyze_file, analyze_samples, decode_file, media_id};

const BPM: f64 = 120.0;
const BAR: f64 = 2.0; // 4 beats at 120 BPM
const SECTION_BARS: f64 = 8.0;

/// One section: its own drum pattern, chords, and level.
fn section(pattern: Pattern, chords: &[[i32; 3]], level: f32, bass: bool, seed: u64) -> (Vec<f32>, Vec<f32>) {
    let secs = SECTION_BARS * BAR;
    let song = groove_with(BPM, secs, pattern, seed, false);
    let n = song.left.len();
    let p = pad(BPM, n, chords);
    let b = if bass { bass_line(BPM, n) } else { vec![0.0; n] };
    let l = (0..n).map(|i| level * (song.left[i] + p[i] + b[i])).collect();
    let r = (0..n).map(|i| level * (song.right[i] + 0.8 * p[i] + b[i])).collect();
    (l, r)
}

/// A-B-A-B-C-B, 8 bars each: verse (rock, minor chords), chorus (four-on-the-floor, bass,
/// major chords, louder), bridge (half-time, different chords).
fn song() -> (Vec<f32>, Vec<f32>) {
    let verse = [[57, 60, 64], [53, 57, 60], [55, 59, 62], [52, 55, 59]];
    let chorus = [[60, 64, 67], [65, 69, 72], [67, 71, 74], [60, 64, 67]];
    let bridge = [[62, 65, 69], [58, 62, 65], [63, 67, 70], [61, 65, 68]];
    let mut l = Vec::new();
    let mut r = Vec::new();
    let parts = [
        section(Pattern::Rock, &verse, 0.35, false, 1),
        section(Pattern::Four, &chorus, 0.6, true, 2),
        section(Pattern::Rock, &verse, 0.35, false, 3),
        section(Pattern::Four, &chorus, 0.6, true, 4),
        section(Pattern::HalfTime, &bridge, 0.3, false, 5),
        section(Pattern::Four, &chorus, 0.6, true, 6),
    ];
    for (a, b) in parts {
        l.extend(a);
        r.extend(b);
    }
    (l, r)
}

#[test]
fn song_structure_beat_grid_and_chorus() {
    let (l, r) = song();
    let mono: Vec<f32> = l.iter().zip(&r).map(|(a, b)| 0.5 * (a + b)).collect();
    let a = analyze_samples(&mono, SR);
    let dur = mono.len() as f64 / SR as f64;
    assert!((a.duration - dur).abs() < 0.01);
    assert!((a.bpm - BPM).abs() < 1.0, "bpm {}", a.bpm);
    // beats every 0.5 s, on the grid within 25 ms (humanisation is ±5 ms)
    let off: Vec<f64> = a.beats.iter().map(|b| ((b / 0.5).round() * 0.5 - b).abs()).collect();
    let bad = off.iter().filter(|o| **o > 0.025).count();
    assert!(bad <= a.beats.len() / 50, "{bad} of {} beats off the grid", a.beats.len());
    assert!((a.beats.len() as f64 - dur / 0.5).abs() <= 4.0, "{} beats", a.beats.len());
    // downbeats on bar lines
    let bar_hits = a.downbeats.iter().filter(|d| ((*d / BAR).round() * BAR - *d).abs() < 0.05).count();
    assert!(bar_hits as f64 >= 0.9 * a.downbeats.len() as f64, "downbeats {:?}", &a.downbeats[..a.downbeats.len().min(8)]);
    // sections: boundaries at every 16 s within one bar
    let bounds: Vec<f64> = a.sections.iter().skip(1).map(|s| s.start).collect();
    for want in [16.0, 32.0, 48.0, 64.0, 80.0] {
        assert!(bounds.iter().any(|b| (b - want).abs() <= BAR + 0.01), "no boundary near {want}: {bounds:?}");
    }
    assert!(bounds.len() <= 7, "too many boundaries: {bounds:?}");
    let label_at = |t: f64| a.sections.iter().find(|s| s.start <= t && t < s.end).map(|s| s.label.clone()).unwrap();
    let (a1, b1, a2, b2, c, b3) = (label_at(8.0), label_at(24.0), label_at(40.0), label_at(56.0), label_at(72.0), label_at(88.0));
    assert_eq!(a1, a2, "verses share a label");
    assert_eq!(b1, b2, "choruses share a label");
    assert_eq!(b1, b3);
    assert_ne!(a1, b1);
    assert_ne!(c, b1);
    // chorus candidates: the chorus starts
    assert!(!a.chorus.is_empty());
    for c in &a.chorus {
        assert!([16.0, 48.0, 80.0].iter().any(|b| (c - b).abs() <= BAR + 0.01), "chorus candidate {c} is not a chorus start");
    }
    assert!(a.chorus.len() >= 2, "{:?}", a.chorus);
    // waveform peaks per 10 ms
    assert_eq!(a.peaks.len(), (dur / 0.01).ceil() as usize);
    let m = mono.iter().fold(0f32, |x, v| x.max(v.abs()));
    assert!((a.peaks.iter().cloned().fold(0.0, f32::max) - m.min(1.0)).abs() < 1e-6, "peak {m}");
}

#[test]
fn analyze_file_decodes_wav_and_ids_by_content() {
    let dir = scratch_dir("offline");
    let (l, r) = song();
    let secs = 40.0;
    let n = (secs * SR as f64) as usize;
    let wav = dir.join("song.wav");
    write_wav(&wav, SR as u32, &[&l[..n], &r[..n]]).unwrap();
    let d = decode_file(&wav).unwrap();
    assert_eq!(d.sample_rate, SR);
    assert_eq!(d.channels.len(), 2);
    assert_eq!(d.channels[0].len(), n);
    assert!((d.channels[1][1000] - r[1000]).abs() < 1e-4, "16-bit round trip");
    let a = analyze_file(&wav).unwrap();
    let bytes = std::fs::read(&wav).unwrap();
    assert_eq!(a.media, media_id(&bytes));
    assert!(a.media.starts_with("file:") && a.media.len() == 5 + 16);
    assert!((a.bpm - BPM).abs() < 1.0, "bpm {}", a.bpm);
    assert!((a.duration - secs).abs() < 0.01);
    // compressed formats through ffmpeg (present on this machine)
    let ffmpeg = std::process::Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success());
    if !ffmpeg {
        eprintln!("ffmpeg not found: skipping the MP3/FLAC decode checks");
        return;
    }
    for (ext, args) in [("flac", vec!["-c:a", "flac"]), ("mp3", vec!["-c:a", "libmp3lame", "-b:a", "192k"]), ("ogg", vec!["-c:a", "libvorbis", "-q:a", "5"])] {
        let out = dir.join(format!("song.{ext}"));
        let st = std::process::Command::new("ffmpeg").args(["-loglevel", "error", "-y", "-i"]).arg(&wav).args(&args).arg(&out).status().unwrap();
        if !st.success() {
            eprintln!("ffmpeg cannot encode {ext} here; skipped");
            continue;
        }
        let d = decode_file(&out).unwrap_or_else(|e| panic!("{ext}: {e:#}"));
        assert_eq!(d.channels.len(), 2, "{ext}");
        assert!((d.duration() - secs).abs() < 0.1, "{ext}: {} s", d.duration());
        let a = analyze_file(&out).unwrap();
        assert!((a.bpm - BPM).abs() < 1.0, "{ext}: bpm {}", a.bpm);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn decode_errors_are_reported() {
    let dir = scratch_dir("bad");
    let p = dir.join("x.wav");
    std::fs::write(&p, b"not audio at all").unwrap();
    assert!(decode_file(&p).is_err());
    assert!(analyze_file(&dir.join("missing.flac")).is_err());
    let _ = std::fs::remove_dir_all(&dir);
}
