//! Beat tracking on synthesised grooves: tempo accuracy, beat alignment, octave traps, tap tempo.

mod common;

use common::*;
use se_analysis::{AnalysisEvent, BeatEvent, LiveAnalyzer, LiveConfig};

fn beats_of(frames: &[(se_analysis::Features, Vec<AnalysisEvent>)]) -> Vec<BeatEvent> {
    frames
        .iter()
        .flat_map(|(_, ev)| ev.iter())
        .filter_map(|e| match e {
            AnalysisEvent::Beat(b) => Some(*b),
            _ => None,
        })
        .collect()
}

fn nearest(grid: &[f64], t: f64) -> f64 {
    grid.iter().map(|g| t - g).min_by(|a, b| a.abs().total_cmp(&b.abs())).unwrap()
}

struct Tracked {
    max_bpm_err: f64,
    max_beat_err_ms: f64,
    mean_beat_err_ms: f64,
    beats_after: usize,
    expected_beats: usize,
    settle_s: f64,
}

/// Run a groove through the analyzer; measure after `from_s`.
fn track(song: &Song, range: Option<(f32, f32)>, want_bpm: f64, from_s: f64) -> Tracked {
    let mut an = LiveAnalyzer::new(LiveConfig::default());
    if let Some((lo, hi)) = range {
        an.beat_mut().unwrap().set_range(lo, hi);
    }
    let frames = run_live(&mut an, &song.left, &song.right, 480, 0);
    let dur = song.left.len() as f64 / SR as f64;
    let max_bpm_err = frames.iter().filter(|(f, _)| secs(f.ts) >= from_s).map(|(f, _)| (f.bpm as f64 - want_bpm).abs()).fold(0.0, f64::max);
    // First time after which the bpm stays within ±1.
    let mut settle_s = 0.0;
    for (f, _) in &frames {
        if (f.bpm as f64 - want_bpm).abs() > 1.0 {
            settle_s = secs(f.ts);
        }
    }
    let period = 60.0 / want_bpm;
    let grid: Vec<f64> = (0..).map(|k| k as f64 * period).take_while(|&t| t < dur + period).collect();
    let beats: Vec<BeatEvent> = beats_of(&frames).into_iter().filter(|b| secs(b.ts) >= from_s && secs(b.ts) < dur - 0.3).collect();
    let errs: Vec<f64> = beats.iter().map(|b| nearest(&grid, secs(b.ts)) * 1000.0).collect();
    let max_beat_err_ms = errs.iter().map(|e| e.abs()).fold(0.0, f64::max);
    let mean_beat_err_ms = errs.iter().sum::<f64>() / errs.len().max(1) as f64;
    let expected_beats = ((dur - 0.3 - from_s) / period).floor() as usize;
    Tracked { max_bpm_err, max_beat_err_ms, mean_beat_err_ms, beats_after: beats.len(), expected_beats, settle_s }
}

#[test]
fn tracks_tempo_and_beats_across_styles() {
    let cases = [
        (90.0, Pattern::Rock, 11),
        (100.0, Pattern::Rock, 12),
        (120.0, Pattern::Rock, 13),
        (128.0, Pattern::Four, 14),
        (140.0, Pattern::Rock, 15),
        (174.0, Pattern::TwoStep, 16),
    ];
    for (bpm, pattern, seed) in cases {
        let song = groove(bpm, 14.0, pattern, seed);
        let r = track(&song, None, bpm, 8.0);
        eprintln!(
            "{bpm:>5} BPM {pattern:?}: settled ≤ {:.2} s, max |bpm err| after 8 s {:.3}, beats {} / {} expected, beat err mean {:+.1} ms max {:.1} ms",
            r.settle_s, r.max_bpm_err, r.beats_after, r.expected_beats, r.mean_beat_err_ms, r.max_beat_err_ms
        );
        assert!(r.max_bpm_err <= 1.0, "{bpm}: bpm error {}", r.max_bpm_err);
        assert!(r.max_beat_err_ms <= 25.0, "{bpm}: beat error {} ms", r.max_beat_err_ms);
        assert!(
            r.beats_after + 1 >= r.expected_beats && r.beats_after <= r.expected_beats + 1,
            "{bpm}: {} beats, expected {}",
            r.beats_after,
            r.expected_beats
        );
    }
}

#[test]
fn octave_ambiguity_resolves_inside_the_range() {
    // A 70 BPM backbeat: its own tempo by default, doubled when the range excludes 70.
    let slow = groove(70.0, 16.0, Pattern::Rock, 21);
    let r = track(&slow, None, 70.0, 10.0);
    eprintln!("70 Rock default range: bpm err {:.3}, beat err max {:.1} ms", r.max_bpm_err, r.max_beat_err_ms);
    assert!(r.max_bpm_err <= 1.0 && r.max_beat_err_ms <= 25.0);
    let r = track(&slow, Some((90.0, 180.0)), 140.0, 10.0);
    eprintln!("70 Rock in 90–180: bpm err vs 140 {:.3}, beat err max {:.1} ms", r.max_bpm_err, r.max_beat_err_ms);
    assert!(r.max_bpm_err <= 1.0 && r.max_beat_err_ms <= 25.0);

    // A 140 BPM half-time groove: 140 when the range starts at 100, 70 when it ends at 100.
    let half = groove(140.0, 16.0, Pattern::HalfTime, 22);
    let r = track(&half, Some((100.0, 180.0)), 140.0, 10.0);
    eprintln!("140 half-time in 100–180: bpm err {:.3}, beat err max {:.1} ms", r.max_bpm_err, r.max_beat_err_ms);
    assert!(r.max_bpm_err <= 1.0 && r.max_beat_err_ms <= 25.0);
    let r = track(&half, Some((50.0, 100.0)), 70.0, 10.0);
    eprintln!("140 half-time in 50–100: bpm err vs 70 {:.3}, beat err max {:.1} ms", r.max_bpm_err, r.max_beat_err_ms);
    assert!(r.max_bpm_err <= 1.0 && r.max_beat_err_ms <= 25.0);
}

#[test]
fn tap_tempo_overrides_and_clear_returns() {
    let song = groove(120.0, 24.0, Pattern::Rock, 31);
    let mut an = LiveAnalyzer::new(LiveConfig::default());
    let taps = [8.0, 8.6, 9.2, 9.8];
    let clear_at = 16.0;
    let block = 480;
    let mut frames = Vec::new();
    let (mut tapped, mut cleared) = (0, false);
    let mut i = 0;
    while i < song.left.len() {
        let t = i as f64 / SR as f64;
        while tapped < taps.len() && taps[tapped] <= t {
            an.tap(ns(taps[tapped]));
            tapped += 1;
        }
        if !cleared && t >= clear_at {
            an.clear_tap();
            cleared = true;
        }
        let j = (i + block).min(song.left.len());
        an.process(&song.left[i..j], &song.right[i..j], ns(t), &mut |f, ev| frames.push((f.clone(), ev.to_vec())));
        i = j;
    }
    let bpm_at = |t: f64| frames.iter().find(|(f, _)| secs(f.ts) >= t).map(|(f, _)| f.bpm as f64).unwrap();
    assert!((bpm_at(7.5) - 120.0).abs() <= 1.0, "groove tempo before taps: {}", bpm_at(7.5));
    // While tapped: 100 BPM and beats on the tapped grid (last tap + k·0.6 s).
    for t in [9.9, 12.0, 15.9] {
        assert!((bpm_at(t) - 100.0).abs() < 0.05, "tapped bpm at {t}: {}", bpm_at(t));
    }
    let beats = beats_of(&frames);
    let tapped_grid: Vec<f64> = (0..12).map(|k| 9.8 + k as f64 * 0.6).collect();
    let during: Vec<f64> = beats.iter().map(|b| secs(b.ts)).filter(|&t| t > 9.9 && t < 15.9).collect();
    let errs: Vec<f64> = during.iter().map(|&t| nearest(&tapped_grid, t) * 1000.0).collect();
    eprintln!("tapped beats {:?}\n  errors ms {:?}", during, errs);
    assert!(during.len() >= 9 && during.len() <= 10, "{} beats while tapped", during.len());
    assert!(errs.iter().all(|e| e.abs() <= 25.0), "tapped beat alignment {errs:?}");
    // After clear_tap the groove tempo returns and beats realign with the 120 grid.
    assert!((bpm_at(16.2) - 120.0).abs() <= 1.0, "bpm right after clear: {}", bpm_at(16.2));
    let grid: Vec<f64> = (0..60).map(|k| k as f64 * 0.5).collect();
    let after: Vec<f64> = beats.iter().map(|b| secs(b.ts)).filter(|&t| t > 19.0 && t < 23.7).collect();
    let errs: Vec<f64> = after.iter().map(|&t| nearest(&grid, t) * 1000.0).collect();
    eprintln!("beats after clear {:?}\n  errors ms {:?}", after, errs);
    assert!(after.len() >= 8, "{} beats after clear", after.len());
    assert!(errs.iter().all(|e| e.abs() <= 25.0), "post-clear alignment {errs:?}");
}

#[test]
fn silence_and_noise_do_not_emit_beats() {
    let mut an = LiveAnalyzer::new(LiveConfig::default());
    let silence = vec![0f32; (6.0 * SR) as usize];
    let frames = run_live(&mut an, &silence, &silence, 512, 0);
    assert!(beats_of(&frames).is_empty());
    let mut rng = Rng::new(5);
    let noise: Vec<f32> = pink((10.0 * SR) as usize, &mut rng).iter().map(|x| x * 0.3).collect();
    let frames = run_live(&mut an, &noise, &noise, 512, ns(6.0));
    let n = beats_of(&frames).len();
    let conf = frames.last().unwrap().0.beat_confidence;
    eprintln!("stationary pink noise: {n} beats, confidence {conf:.2}");
    assert!(n <= 2, "{n} beats on stationary noise");
}
