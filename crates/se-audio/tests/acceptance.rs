//! M4 acceptance, offline and deterministic, with the shipped project files: generated band
//! audio → live analysis → the core with `bindings/bass_zoom.toml` and `rules/audio.toml`.
//!
//! * the band's bass drives `fx.zoom_pulse.amount` with the same feel on a quiet (−20 dB) and
//!   a loud rendition of the same song (binding `auto_normalize`);
//! * band kicks fire the `kick punch` rule (→ `fx.zoom_pulse.trigger`) equally on both.

use se_analysis::{AnalysisEvent, LiveAnalyzer, LiveConfig, OnsetClass, OnsetConfig};
use se_core::config::{Config, SourceFile};
use se_core::{Core, Input, Output};
use se_proto::{Event, Meta, Origin, Value};
use std::path::Path;

const SR: f32 = 48000.0;
const BPM: f32 = 120.0;
const SECONDS: f32 = 40.0;

/// A band groove: kick on every beat, a bass line whose loudness changes per bar (so the
/// binding has something to follow), off-beat hats, snare on 2 and 4.
fn band(gain: f32) -> Vec<f32> {
    let n = (SECONDS * SR) as usize;
    let spb = 60.0 / BPM * SR;
    let mut rng = 0x1234_5678u32;
    let mut noise = move || {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        (rng as f32 / u32::MAX as f32) * 2.0 - 1.0
    };
    let mut out = vec![0.0f32; n];
    let mut bass_phase = 0.0f32;
    for (i, o) in out.iter_mut().enumerate() {
        let beat = i as f32 / spb;
        let bi = beat.floor();
        let t = (beat - bi) * spb / SR;
        // kick: pitch-dropping sine
        let f = 50.0 + 90.0 * (-t * 35.0).exp();
        let kick = (std::f32::consts::TAU * f * t).sin() * (-t * 9.0).exp() * 0.9;
        // bass: 55 Hz, level follows a 4-bar pattern (loud, soft, medium, off)
        let bar = (bi / 4.0).floor() as i64;
        let lvl = [0.8, 0.25, 0.5, 0.0][(bar % 4) as usize];
        bass_phase = (bass_phase + 55.0 / SR).fract();
        let bass = (std::f32::consts::TAU * bass_phase).sin() * lvl * 0.5;
        // hats on the off-beat, snare on 2 and 4
        let off = ((beat + 0.5).fract()) * spb / SR;
        let hat = noise() * (-off * 60.0).exp() * 0.15;
        let snare = if (bi as i64) % 2 == 1 { noise() * (-t * 20.0).exp() * 0.35 } else { 0.0 };
        *o = gain * (kick + bass + hat + snare);
    }
    out
}

struct Run {
    amount: Vec<f64>,
    kick_triggers: usize,
    onsets: usize,
}

fn file(kind: &str, name: &str) -> SourceFile {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../project-example");
    let path = format!("{kind}/{name}.toml");
    let src = std::fs::read_to_string(root.join(&path)).unwrap();
    SourceFile { kind: kind.into(), name: name.into(), path, table: toml::from_str(&src).unwrap() }
}

fn run(gain: f32) -> Run {
    let audio = band(gain);
    let t0: u64 = 1_000_000_000;
    // analysis
    let mut an = LiveAnalyzer::new(LiveConfig { sample_rate: SR, hop: 256, onsets: OnsetConfig::default(), beat: true, hype: false });
    let mut hops: Vec<(u64, f32)> = Vec::new();
    let mut kicks: Vec<(u64, f32)> = Vec::new();
    for (k, block) in audio.chunks(256).enumerate() {
        let ts = t0 + (k as f64 * 256.0 * 1e9 / SR as f64) as u64;
        an.process(block, block, ts, &mut |f, ev| {
            hops.push((f.ts, f.bass));
            for e in ev {
                if let AnalysisEvent::Onset { class: OnsetClass::Kick, strength, ts } = *e {
                    kicks.push((ts, strength));
                }
            }
        });
    }
    // the core with the shipped binding and rule
    let cfg = Config::build(&[file("bindings", "bass_zoom"), file("rules", "audio")]);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let mut core = Core::new(cfg, t0);
    core.submit(Input::Declare { address: "fx.zoom_pulse.amount".into(), meta: Meta::float(0.0, [0.0, 1.0]) });
    let period = 1e9 / 240.0;
    let ticks = (SECONDS as f64 * 240.0) as usize;
    let (mut hi, mut ki) = (0, 0);
    let mut amount = Vec::with_capacity(ticks);
    let mut kick_triggers = 0;
    for tick in 1..=ticks {
        let now = t0 + (tick as f64 * period) as u64;
        while hi + 1 < hops.len() && hops[hi + 1].0 <= now {
            hi += 1;
        }
        if let Some((_, b)) = hops.get(hi) {
            core.submit(Input::Signal { name: "band.bass".into(), value: *b });
        }
        while ki < kicks.len() && kicks[ki].0 <= now {
            let mut e = Event::new("band.kick", Origin::System, Value::map().with("velocity", kicks[ki].1 as f64));
            e.ts = kicks[ki].0;
            core.submit(Input::Event { event: e });
            ki += 1;
        }
        core.step();
        for o in core.drain_outputs() {
            if let Output::Event(e) = o
                && e.ty == "fx.zoom_pulse.trigger"
            {
                kick_triggers += 1;
            }
        }
        amount.push(core.get("fx.zoom_pulse.amount").and_then(Value::as_f64).unwrap_or(0.0));
    }
    Run { amount, kick_triggers, onsets: kicks.len() }
}

fn stats(v: &[f64]) -> (f64, f64, f64) {
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    let mut s = v.to_vec();
    s.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let p90 = s[(s.len() as f64 * 0.9) as usize];
    let sd = (v.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / v.len() as f64).sqrt();
    (mean, p90, sd)
}

#[test]
fn bass_binding_feels_the_same_on_quiet_and_loud_songs_and_kicks_fire_the_rule() {
    let loud = run(0.8);
    let quiet = run(0.08);
    // compare after the 8 s normalisation window has adapted
    let skip = 12 * 240;
    let (lm, lp, lsd) = stats(&loud.amount[skip..]);
    let (qm, qp, qsd) = stats(&quiet.amount[skip..]);
    println!("loud: mean {lm:.3} p90 {lp:.3} sd {lsd:.3} | quiet: mean {qm:.3} p90 {qp:.3} sd {qsd:.3}");
    assert!(lsd > 0.05 && qsd > 0.05, "the binding must actually move the param");
    assert!((qm / lm - 1.0).abs() < 0.12, "mean: quiet {qm} vs loud {lm}");
    assert!((qp / lp - 1.0).abs() < 0.12, "p90: quiet {qp} vs loud {lp}");
    // sample-by-sample the two trajectories agree closely
    let diff = loud.amount[skip..].iter().zip(&quiet.amount[skip..]).map(|(a, b)| (a - b).abs()).sum::<f64>() / (loud.amount.len() - skip) as f64;
    assert!(diff < 0.05, "mean |loud − quiet| = {diff}");
    // kicks: 80 beats in 40 s; the rule fires on (nearly) every one, at both levels
    println!("kick onsets loud {} quiet {}; rule triggers loud {} quiet {}", loud.onsets, quiet.onsets, loud.kick_triggers, quiet.kick_triggers);
    assert!(loud.kick_triggers >= 72 && loud.kick_triggers <= 81, "loud rule triggers {}", loud.kick_triggers);
    assert!((loud.kick_triggers as i64 - quiet.kick_triggers as i64).abs() <= 3, "quiet {} vs loud {}", quiet.kick_triggers, loud.kick_triggers);
}
