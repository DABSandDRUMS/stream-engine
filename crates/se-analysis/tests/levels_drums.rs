//! Loudness (BS.1770), third-octave bands, drop/hype detection, drum triggers, and the
//! no-allocation guarantee of the real-time paths.

#[global_allocator]
static ALLOC: se_alloc::Counting = se_alloc::Counting;

mod common;

use common::*;
use se_analysis::{AnalysisEvent, DrumHit, DrumPadConfig, DrumTriggers, Features, LiveAnalyzer, LiveConfig, N_BANDS, band_center};

fn last(frames: &[(Features, Vec<AnalysisEvent>)]) -> &Features {
    &frames.last().unwrap().0
}

#[test]
fn lufs_of_a_997_hz_sine_matches_bs1770() {
    // BS.1770-4 / EBU Tech 3341 case 1: a stereo 1 kHz sine at −23 dBFS reads −23.0 LUFS (the
    // −0.691 dB constant cancels the K-filter gain at 1 kHz, and two channels add 3 dB to the
    // −3 dB of a sine's mean square).
    for (dbfs, want) in [(-23.0f64, -23.0f32), (-20.0, -20.0), (-6.0, -6.0)] {
        let x = sine(997.0, 10f64.powf(dbfs / 20.0), 5.0, SR);
        let mut an = LiveAnalyzer::new(LiveConfig { beat: false, ..LiveConfig::default() });
        let fr = run_live(&mut an, &x, &x, 512, 0);
        let f = last(&fr);
        assert!((f.lufs_s - want).abs() < 0.3, "{dbfs} dBFS: short-term {} vs {want}", f.lufs_s);
        assert!((f.lufs_m - want).abs() < 0.3, "{dbfs} dBFS: momentary {} vs {want}", f.lufs_m);
        let rms_lin = 10f32.powf(dbfs as f32 / 20.0) / 2f32.sqrt();
        assert!((f.level / rms_lin - 1.0).abs() < 0.02, "level {} vs {rms_lin}", f.level);
    }
}

#[test]
fn pink_noise_loudness_is_stable_and_scales_by_the_gain() {
    let mut rng = Rng::new(7);
    let p = pink(SR as usize * 6, &mut rng);
    let run = |g: f32| {
        let x: Vec<f32> = p.iter().map(|v| v * g).collect();
        let mut an = LiveAnalyzer::new(LiveConfig { beat: false, ..LiveConfig::default() });
        let fr = run_live(&mut an, &x, &x, 256, 0);
        let s: Vec<f32> = fr[fr.len() / 2..].iter().map(|(f, _)| f.lufs_s).collect();
        (s.iter().cloned().fold(f32::MAX, f32::min), s.iter().cloned().fold(f32::MIN, f32::max))
    };
    let (lo_a, hi_a) = run(0.5);
    let (lo_b, hi_b) = run(0.05);
    assert!(hi_a - lo_a < 0.5, "short-term LUFS of stationary pink noise wanders {lo_a}..{hi_a}");
    assert!((hi_a - hi_b - 20.0).abs() < 0.05, "max: {hi_a} vs {hi_b}");
    assert!(((lo_a - lo_b) - 20.0).abs() < 0.05, "−20 dB gain → −20 LU: {lo_a} vs {lo_b}");
}

#[test]
fn a_band_center_sine_peaks_in_its_third_octave_band() {
    for i in [3usize, 10, 17, 24, 29] {
        let fc = band_center(i) as f64;
        let x = sine(fc, 0.3, 1.0, SR);
        let mut an = LiveAnalyzer::new(LiveConfig { beat: false, ..LiveConfig::default() });
        let fr = run_live(&mut an, &x, &x, 256, 0);
        let b = last(&fr).bands;
        let best = (0..N_BANDS).max_by(|a, c| b[*a].total_cmp(&b[*c])).unwrap();
        assert_eq!(best, i, "{fc:.0} Hz peaks in band {best}");
        // neighbours at least 10 dB down
        for n in [i.saturating_sub(1), (i + 1).min(N_BANDS - 1)] {
            if n != i {
                assert!(b[n] < b[i] * 0.32, "{fc:.0} Hz leaks into band {n}: {} vs {}", b[n], b[i]);
            }
        }
    }
}

#[test]
fn drop_after_a_build_is_detected_once() {
    // 16 s quiet groove (hats + pad, no kick/bass), then the full groove at +12 dB.
    let build = groove_with(128.0, 16.0, Pattern::HalfTime, 3, false);
    let full = groove(128.0, 12.0, Pattern::Four, 4);
    let mut l: Vec<f32> = build.left.iter().map(|v| v * 0.12).collect();
    let mut r: Vec<f32> = build.right.iter().map(|v| v * 0.12).collect();
    l.extend(full.left.iter().map(|v| v * 0.5));
    r.extend(full.right.iter().map(|v| v * 0.5));
    let mut an = LiveAnalyzer::new(LiveConfig::default());
    let fr = run_live(&mut an, &l, &r, 256, 0);
    let drops: Vec<f64> =
        fr.iter().flat_map(|(_, ev)| ev.iter()).filter_map(|e| if let AnalysisEvent::Drop { ts, .. } = e { Some(secs(*ts)) } else { None }).collect();
    assert_eq!(drops.len(), 1, "drops at {drops:?}");
    assert!((drops[0] - 16.0).abs() < 1.0, "drop at {:.2} s", drops[0]);
}

#[test]
fn hype_spikes_on_a_crowd_burst_over_the_baseline() {
    let mut rng = Rng::new(9);
    let n = SR as usize * 40;
    let base = pink(n, &mut rng);
    let mut x: Vec<f32> = base.iter().map(|v| v * 0.03).collect();
    // 3 s cheer at +18 dB from 30 s
    for (i, v) in x.iter_mut().enumerate().skip(SR as usize * 30).take(SR as usize * 3) {
        *v = base[i] * 0.24;
    }
    let mut an = LiveAnalyzer::new(LiveConfig { beat: false, hype: true, ..LiveConfig::default() });
    let fr = run_live(&mut an, &x, &x, 256, 0);
    let spikes: Vec<f64> =
        fr.iter().flat_map(|(_, ev)| ev.iter()).filter_map(|e| if let AnalysisEvent::HypeSpike { ts, .. } = e { Some(secs(*ts)) } else { None }).collect();
    assert_eq!(spikes.len(), 1, "{spikes:?}");
    assert!(spikes[0] >= 30.0 && spikes[0] < 32.0, "spike at {}", spikes[0]);
    let at = |t: f64| fr.iter().find(|(f, _)| secs(f.ts) >= t).unwrap().0.hype;
    assert!(at(25.0) < 0.3 && at(31.5) > 0.7, "hype {} → {}", at(25.0), at(31.5));
}

fn burst(buf: &mut [f32], start: usize, f: f64, amp: f32, decay: f64) {
    for (i, v) in buf.iter_mut().enumerate().skip(start).take(SR as usize / 4) {
        let t = (i - start) as f64 / SR as f64;
        *v += (amp as f64 * (std::f64::consts::TAU * f * t).sin() * (-t / decay).exp() * (t / 0.0005).min(1.0)) as f32;
    }
}

fn run_pads(trig: &mut DrumTriggers, chans: &[Vec<f32>], block: usize) -> Vec<DrumHit> {
    let mut hits = Vec::new();
    let n = chans[0].len();
    let mut i = 0;
    while i < n {
        let j = (i + block).min(n);
        let refs: Vec<&[f32]> = chans.iter().map(|c| &c[i..j]).collect();
        trig.process(&refs, i as u64, &mut |h| hits.push(h));
        i = j;
    }
    hits
}

#[test]
fn drum_triggers_threshold_retrigger_bleed_and_velocity() {
    let n = SR as usize * 4;
    let mut kick_mic = vec![0f32; n];
    let mut snare_mic = vec![0f32; n];
    let s = |t: f64| (t * SR as f64) as usize;
    // kicks at 0.1 (loud), 0.6 (soft), 1.1 (below threshold)
    burst(&mut kick_mic, s(0.1), 60.0, 0.8, 0.08);
    burst(&mut kick_mic, s(0.6), 60.0, 0.1, 0.08);
    burst(&mut kick_mic, s(1.1), 60.0, 0.003, 0.08);
    // snare at 1.6 bleeding into the kick mic at −12 dB (no kick)
    burst(&mut snare_mic, s(1.6), 220.0, 0.6, 0.05);
    burst(&mut kick_mic, s(1.6), 220.0, 0.15, 0.05);
    // flam on the snare: 10 ms apart (one hit), then 60 ms later (two)
    burst(&mut snare_mic, s(2.1), 220.0, 0.5, 0.03);
    burst(&mut snare_mic, s(2.11), 220.0, 0.5, 0.03);
    burst(&mut snare_mic, s(2.6), 220.0, 0.5, 0.03);
    burst(&mut snare_mic, s(2.66), 220.0, 0.5, 0.03);
    // kick + snare together: both fire
    burst(&mut kick_mic, s(3.2), 60.0, 0.7, 0.08);
    burst(&mut snare_mic, s(3.2), 220.0, 0.6, 0.05);
    burst(&mut snare_mic, s(3.2), 60.0, 0.1, 0.08); // kick bleed into the snare mic
    let mut kick = DrumPadConfig::kick();
    kick.threshold_db = -30.0;
    let mut snare = DrumPadConfig::snare();
    snare.threshold_db = -30.0;
    let mut trig = DrumTriggers::new(SR, vec![kick, snare]);
    let hits = run_pads(&mut trig, &[kick_mic, snare_mic], 64);
    let at = |pad: usize| hits.iter().filter(|h| h.pad == pad).map(|h| (h.frame as f64 / SR as f64, h.velocity)).collect::<Vec<_>>();
    let k = at(0);
    let sn = at(1);
    let near = |v: &[(f64, f32)], t: f64| v.iter().filter(|(x, _)| (x - t).abs() < 0.01).count();
    assert_eq!(near(&k, 0.1), 1);
    assert_eq!(near(&k, 0.6), 1);
    assert_eq!(near(&k, 1.1), 0, "below threshold");
    assert_eq!(near(&k, 1.6), 0, "snare bleed in the kick mic must not fire the kick");
    assert_eq!(near(&k, 3.2), 1);
    assert_eq!(k.len(), 3, "kick hits {k:?}");
    assert_eq!(near(&sn, 1.6), 1);
    assert_eq!(sn.iter().filter(|(x, _)| (2.09..2.13).contains(x)).count(), 1, "flam inside the retrigger window is one hit");
    assert_eq!(near(&sn, 2.6) + near(&sn, 2.66), 2, "hits 60 ms apart are two");
    assert_eq!(near(&sn, 3.2), 1);
    assert_eq!(sn.len(), 5, "snare hits {sn:?}");
    // velocity monotonic in amplitude, onsets located within 1 ms
    let v_loud = k.iter().find(|(x, _)| (x - 0.1).abs() < 0.01).unwrap();
    let v_soft = k.iter().find(|(x, _)| (x - 0.6).abs() < 0.01).unwrap();
    assert!(v_loud.1 > v_soft.1 + 0.2, "velocity {v_loud:?} vs {v_soft:?}");
    assert!((v_loud.0 - 0.1).abs() < 0.001);
    // zero allocations on the real-time path
    let n = SR as usize;
    let chans = [vec![0.01f32; n], vec![0.01f32; n]];
    let mut out = [DrumHit { pad: 0, velocity: 0.0, frame: 0 }; 64];
    let mut cnt = 0usize;
    let scope = se_alloc::Scope::begin();
    let mut i = 0;
    while i < n {
        let refs = [&chans[0][i..i + 64], &chans[1][i..i + 64]];
        trig.process(&refs, i as u64, &mut |h| {
            out[cnt % 64] = h;
            cnt += 1;
        });
        i += 64;
    }
    assert_eq!((scope.allocs(), scope.frees()), (0, 0), "DrumTriggers::process allocated");
}

#[test]
fn live_analyzer_does_not_allocate_in_steady_state() {
    assert!(se_alloc::installed());
    let song = groove(120.0, 20.0, Pattern::Rock, 77);
    let mut an = LiveAnalyzer::new(LiveConfig { hype: true, ..LiveConfig::default() });
    // warm-up (first hops fill lazily-sized nothing, but keep the scope to steady state)
    let warm = SR as usize * 2;
    let mut sink = 0usize;
    an.process(&song.left[..warm], &song.right[..warm], 0, &mut |_, ev| sink += ev.len());
    let scope = se_alloc::Scope::begin();
    let mut i = warm;
    while i + 256 <= song.left.len() {
        an.process(&song.left[i..i + 256], &song.right[i..i + 256], ns(i as f64 / SR as f64), &mut |f, ev| sink += ev.len() + (f.bpm > 0.0) as usize);
        i += 256;
    }
    let counts = (scope.allocs(), scope.frees());
    drop(scope);
    assert_eq!(counts, (0, 0), "LiveAnalyzer::process allocated");
    assert!(sink > 0);
}
