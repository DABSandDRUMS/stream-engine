//! Onset detection/classification, kick latency and scale invariance on synthesised grooves.

mod common;

use common::*;
use se_analysis::{AnalysisEvent, Features, LiveAnalyzer, LiveConfig, OnsetClass};

#[derive(Clone, Copy, Debug)]
struct Detected {
    class: OnsetClass,
    ts: f64,
    strength: f32,
    /// End of the hop whose callback carried the event.
    emitted: f64,
}

fn onsets(frames: &[(Features, Vec<AnalysisEvent>)]) -> Vec<Detected> {
    frames
        .iter()
        .flat_map(|(f, ev)| {
            ev.iter().filter_map(move |e| match *e {
                AnalysisEvent::Onset { class, strength, ts } => Some(Detected { class, ts: secs(ts), strength, emitted: secs(f.ts) }),
                _ => None,
            })
        })
        .collect()
}

fn drum_of(c: OnsetClass) -> Drum {
    match c {
        OnsetClass::Kick => Drum::Kick,
        OnsetClass::Snare => Drum::Snare,
        OnsetClass::Hat => Drum::Hat,
    }
}

struct Score {
    precision: f64,
    recall: f64,
    /// (true onset, detection) pairs.
    matched: Vec<(f64, Detected)>,
}

fn score(song: &Song, det: &[Detected], class: OnsetClass, from: f64, to: f64) -> Score {
    let truth: Vec<f64> = song.hits.iter().filter(|h| h.drum == drum_of(class) && h.t >= from && h.t <= to).map(|h| h.t).collect();
    let found: Vec<Detected> = det.iter().copied().filter(|d| d.class == class && d.ts >= from - 0.02 && d.ts <= to + 0.02).collect();
    let mut used = vec![false; found.len()];
    let mut matched = Vec::new();
    for &t in &truth {
        let best =
            found.iter().enumerate().filter(|(i, d)| !used[*i] && (d.ts - t).abs() <= 0.02).min_by(|a, b| (a.1.ts - t).abs().total_cmp(&(b.1.ts - t).abs()));
        if let Some((i, d)) = best {
            used[i] = true;
            matched.push((t, *d));
        }
    }
    Score { precision: matched.len() as f64 / found.len().max(1) as f64, recall: matched.len() as f64 / truth.len().max(1) as f64, matched }
}

#[test]
fn classifies_kick_snare_hat_with_low_latency() {
    for (bpm, pattern, seed) in [(100.0, Pattern::Rock, 41), (128.0, Pattern::Four, 42), (174.0, Pattern::TwoStep, 43)] {
        let song = groove(bpm, 12.0, pattern, seed);
        let mut an = LiveAnalyzer::new(LiveConfig::default());
        let det = onsets(&run_live(&mut an, &song.left, &song.right, 256, 0));
        for class in OnsetClass::ALL {
            let s = score(&song, &det, class, 0.5, 11.5);
            eprintln!("{bpm} {pattern:?} {:>5}: precision {:.3} recall {:.3} ({} matched)", class.name(), s.precision, s.recall, s.matched.len());
            assert!(s.precision >= 0.9 && s.recall >= 0.9, "{bpm} {}: P {:.3} R {:.3}", class.name(), s.precision, s.recall);
            if class == OnsetClass::Kick {
                let lat: Vec<f64> = s.matched.iter().map(|(t, d)| (d.emitted - t) * 1000.0).collect();
                let err: Vec<f64> = s.matched.iter().map(|(t, d)| (d.ts - t) * 1000.0).collect();
                let max_lat = lat.iter().copied().fold(f64::MIN, f64::max);
                let mean_lat = lat.iter().sum::<f64>() / lat.len() as f64;
                let max_err = err.iter().map(|e| e.abs()).fold(0.0, f64::max);
                let mean_err = err.iter().sum::<f64>() / err.len() as f64;
                eprintln!("    kick latency mean {mean_lat:.2} ms max {max_lat:.2} ms; ts error mean {mean_err:+.2} ms max |{max_err:.2}| ms");
                assert!(max_lat <= 12.0, "kick emitted {max_lat:.2} ms after the onset");
                assert!(max_err <= 6.0, "kick ts off by {max_err:.2} ms");
            }
        }
    }
}

#[test]
fn quiet_and_loud_material_behave_identically() {
    let song = groove(120.0, 10.0, Pattern::Rock, 51);
    let peak = song.left.iter().chain(&song.right).fold(0f32, |m, x| m.max(x.abs()));
    let run = |dbfs: f32| {
        let g = 10f32.powf(dbfs / 20.0) / peak;
        let (l, r) = song.scaled(g);
        let mut an = LiveAnalyzer::new(LiveConfig::default());
        run_live(&mut an, &l, &r, 256, 0)
    };
    let (loud, quiet) = (run(-6.0), run(-30.0));
    let gain = 10f64.powf(-24.0 / 20.0);
    let mut worst = 0f64;
    for ((a, _), (b, _)) in loud.iter().zip(&quiet).skip(100) {
        for (x, y) in [(a.bass, b.bass), (a.level, b.level), (a.mid, b.mid), (a.bands[5], b.bands[5])] {
            if x > 1e-4 {
                worst = worst.max((y as f64 / x as f64 / gain - 1.0).abs());
            }
        }
        assert!((a.lufs_m - b.lufs_m - 24.0).abs() < 0.01 || a.lufs_m < -45.0);
    }
    eprintln!("worst relative deviation of bass/level/mid/band ratio from the gain: {:.2e}", worst);
    assert!(worst < 1e-3, "ratio deviation {worst}");
    let (dl, dq) = (onsets(&loud), onsets(&quiet));
    eprintln!("onsets: {} at −6 dBFS, {} at −30 dBFS", dl.len(), dq.len());
    assert_eq!(dl.len(), dq.len());
    for (a, b) in dl.iter().zip(&dq) {
        assert_eq!(a.class, b.class);
        assert!((a.ts - b.ts).abs() < 1e-4, "{a:?} vs {b:?}");
        assert!((a.strength - b.strength).abs() < 0.02, "{a:?} vs {b:?}");
    }
}

#[test]
fn strength_follows_relative_velocity() {
    // Kicks alternating full and −12 dB: the loud ones get 1, the soft ones clearly less. The
    // level is read 2 ms after the onset (the 12 ms latency budget leaves no more), which
    // compresses the −12 dB step to about −7…−9 dB.
    let n = (8.0 * SR) as usize;
    let mut buf = vec![0f32; n];
    let mut truth = Vec::new();
    for k in 0..16 {
        let t = 0.25 + k as f64 * 0.45;
        let amp = if k % 2 == 0 { 0.8 } else { 0.8 * 10f32.powf(-12.0 / 20.0) };
        kick(&mut buf, (t * SR as f64) as usize, amp);
        truth.push((t, k % 2 == 0));
    }
    let mut an = LiveAnalyzer::new(LiveConfig::default());
    let det: Vec<Detected> = onsets(&run_live(&mut an, &buf, &buf, 256, 0)).into_iter().filter(|d| d.class == OnsetClass::Kick).collect();
    assert_eq!(det.len(), truth.len(), "{det:?}");
    for (d, (t, loud)) in det.iter().zip(&truth).skip(2) {
        assert!((d.ts - t).abs() < 0.006);
        if *loud {
            assert!(d.strength > 0.85, "loud kick strength {}", d.strength);
        } else {
            assert!((0.15..0.55).contains(&d.strength), "soft kick strength {}", d.strength);
        }
    }
}
