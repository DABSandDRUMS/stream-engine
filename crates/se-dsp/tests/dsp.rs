//! Offline DSP tests (PLAN §23): magnitude responses against analytic biquads, null tests,
//! latency reporting, tempo sync, click tests for the performance effects, and a fuzz pass
//! over every registered effect.

use se_dsp::util::{DIVISIONS, division_beats};
use se_dsp::{Ctx, Effect, FxSlot, MAX_BLOCK, Transport, create, kinds, params_of};

const SR: f32 = 48000.0;

fn idx(fx: &dyn Effect, name: &str) -> usize {
    fx.params().iter().position(|p| p.name == name).unwrap_or_else(|| panic!("{} has no param {name}", fx.kind()))
}

fn set(fx: &mut dyn Effect, name: &str, v: f32) {
    let i = idx(fx, name);
    let spec = fx.params()[i];
    fx.set_param(i, spec.clamp(v));
}

fn choice(fx: &dyn Effect, name: &str, opt: &str) -> f32 {
    match fx.params()[idx(fx, name)].kind {
        se_dsp::ParamKind::Choice(o) => o.iter().position(|x| *x == opt).unwrap_or_else(|| panic!("{name} has no option {opt}")) as f32,
        _ => panic!("{name} is not a choice"),
    }
}

/// Run `input` (mono duplicated to stereo) through `fx` in blocks with an advancing transport.
fn run(fx: &mut dyn Effect, input: &[f32], block: usize, bpm: f32) -> (Vec<f32>, Vec<f32>) {
    let mut ol = Vec::with_capacity(input.len());
    let mut or = Vec::with_capacity(input.len());
    let mut beat = 0.0f64;
    for chunk in input.chunks(block) {
        let mut l = chunk.to_vec();
        let mut r = chunk.to_vec();
        let t = Transport { bpm, beat, beats_per_bar: 4 };
        fx.process(&Ctx { sr: SR, transport: t, key: &[] }, &mut l, &mut r);
        beat += chunk.len() as f64 * bpm as f64 / 60.0 / SR as f64;
        ol.extend_from_slice(&l);
        or.extend_from_slice(&r);
    }
    (ol, or)
}

fn sine(f: f32, amp: f32, n: usize) -> Vec<f32> {
    (0..n).map(|i| amp * (std::f32::consts::TAU * f * i as f32 / SR).sin()).collect()
}

fn rms(x: &[f32]) -> f32 {
    (x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32).sqrt()
}

fn db(x: f32) -> f32 {
    20.0 * x.max(1e-9).log10()
}

fn max_d2(x: &[f32]) -> f32 {
    x.windows(3).map(|w| (w[2] - 2.0 * w[1] + w[0]).abs()).fold(0.0, f32::max)
}

/// Magnitude (dB) of `fx` at `f`, measured with a steady sine after settling.
fn gain_at(fx: &mut dyn Effect, f: f32) -> f32 {
    fx.reset();
    let x = sine(f, 0.25, 48000);
    let (y, _) = run(fx, &x, 256, 120.0);
    db(rms(&y[24000..]) / rms(&x[24000..]))
}

/// RBJ biquad magnitude (dB) of normalised coefficients at `f`.
fn biquad_db(b: [f64; 3], a: [f64; 3], f: f32) -> f32 {
    let w = std::f64::consts::TAU * f as f64 / SR as f64;
    let (c1, s1, c2, s2) = (w.cos(), w.sin(), (2.0 * w).cos(), (2.0 * w).sin());
    let num = ((b[0] + b[1] * c1 + b[2] * c2).powi(2) + (b[1] * s1 + b[2] * s2).powi(2)).sqrt();
    let den = ((a[0] + a[1] * c1 + a[2] * c2).powi(2) + (a[1] * s1 + a[2] * s2).powi(2)).sqrt();
    (20.0 * (num / den).log10()) as f32
}

fn peaking(f0: f32, q: f32, gain_db: f32) -> ([f64; 3], [f64; 3]) {
    let a = 10f64.powf(gain_db as f64 / 40.0);
    let w = std::f64::consts::TAU * f0 as f64 / SR as f64;
    let al = w.sin() / (2.0 * q as f64);
    ([1.0 + al * a, -2.0 * w.cos(), 1.0 - al * a], [1.0 + al / a, -2.0 * w.cos(), 1.0 - al / a])
}

#[test]
fn svf_lowpass_and_highpass_match_the_butterworth_response() {
    let mut fx = create("svf", SR).unwrap();
    // resonance → Q = 0.5·40^r; Butterworth Q = 1/√2
    let r = (2f32.sqrt()).ln() / 40f32.ln();
    set(fx.as_mut(), "resonance", r);
    set(fx.as_mut(), "cutoff", 1000.0);
    for (mode, sign) in [("lp", 1.0f32), ("hp", -1.0)] {
        let m = choice(fx.as_ref(), "mode", mode);
        set(fx.as_mut(), "mode", m);
        for f in [100.0f32, 500.0, 1000.0, 2000.0, 8000.0, 16000.0] {
            // the TPT SVF is the bilinear transform of the analog prototype with a prewarped
            // cutoff: compare against the analog response at the warped frequency
            let w = |x: f32| (std::f32::consts::PI * x / SR).tan();
            let ratio = (w(f) / w(1000.0)).powf(sign * 4.0);
            let expect = -10.0 * (1.0 + ratio).log10();
            let got = gain_at(fx.as_mut(), f);
            // deep in the stop band the measurement itself limits the precision
            let tol = if expect < -40.0 { 1.5 } else { 0.3 };
            assert!((got - expect).abs() < tol, "{mode} at {f} Hz: {got:.2} dB vs {expect:.2} dB");
        }
    }
}

#[test]
fn eq_nulls_when_flat_and_peaking_band_matches_rbj() {
    let mut fx = create("eq", SR).unwrap();
    let noise: Vec<f32> = {
        let mut s = 1u32;
        (0..48000)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 17;
                s ^= s << 5;
                (s as f32 / u32::MAX as f32 - 0.5) * 0.5
            })
            .collect()
    };
    let (y, _) = run(fx.as_mut(), &noise, 256, 120.0);
    let err = y.iter().zip(&noise).map(|(a, b)| (a - b).abs()).fold(0.0, f32::max);
    assert!(err < 1e-5, "flat EQ must null: {err}");
    set(fx.as_mut(), "mid2_freq", 1000.0);
    set(fx.as_mut(), "mid2_q", 2.0);
    set(fx.as_mut(), "mid2_gain", 9.0);
    let (b, a) = peaking(1000.0, 2.0, 9.0);
    for f in [200.0f32, 700.0, 1000.0, 1400.0, 5000.0] {
        let got = gain_at(fx.as_mut(), f);
        let exp = biquad_db(b, a, f);
        assert!((got - exp).abs() < 0.3, "peaking at {f}: {got:.2} vs {exp:.2}");
    }
}

#[test]
fn djfilter_is_flat_in_the_middle_and_sweeps_both_ways() {
    let mut fx = create("djfilter", SR).unwrap();
    assert!(gain_at(fx.as_mut(), 1000.0).abs() < 0.1, "centre = flat");
    set(fx.as_mut(), "filter", -0.8);
    assert!(gain_at(fx.as_mut(), 5000.0) < -12.0, "left = low-pass");
    assert!(gain_at(fx.as_mut(), 60.0).abs() < 1.5);
    set(fx.as_mut(), "filter", 0.8);
    assert!(gain_at(fx.as_mut(), 60.0) < -12.0, "right = high-pass");
}

#[test]
fn gain_utility_nulls_at_unity() {
    let mut fx = create("gain", SR).unwrap();
    let x = sine(440.0, 0.5, 9600);
    let (y, _) = run(fx.as_mut(), &x, 128, 120.0);
    assert!(y.iter().zip(&x).all(|(a, b)| (a - b).abs() < 1e-6));
}

#[test]
fn delay_without_feedback_is_a_delayed_copy_and_tempo_syncs() {
    let mut fx = create("delay", SR).unwrap();
    set(fx.as_mut(), "sync", 0.0);
    set(fx.as_mut(), "time", 10.0);
    set(fx.as_mut(), "feedback", 0.0);
    set(fx.as_mut(), "tone", 20000.0);
    let mut x = vec![0.0f32; 4800];
    x[100] = 1.0;
    let (y, _) = run(fx.as_mut(), &x, 256, 120.0);
    let peak = y.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap().0;
    assert_eq!(peak, 100 + 480, "10 ms echo");
    // synced: 1/8 note at 90, 120, 174 BPM
    for bpm in [90.0f32, 120.0, 174.0] {
        let mut fx = create("delay", SR).unwrap();
        set(fx.as_mut(), "sync", 1.0);
        let d = DIVISIONS.iter().position(|d| *d == "1/8").unwrap();
        set(fx.as_mut(), "division", d as f32);
        set(fx.as_mut(), "feedback", 0.0);
        let expect = (division_beats(d) * 60.0 / bpm as f64 * SR as f64).round() as usize;
        let mut x = vec![0.0f32; expect + 4800];
        x[10] = 1.0;
        let (y, _) = run(fx.as_mut(), &x, 256, bpm);
        let peak = y.iter().enumerate().max_by(|a, b| a.1.abs().total_cmp(&b.1.abs())).unwrap().0;
        assert!((peak as i64 - (10 + expect) as i64).abs() <= 1, "{bpm} BPM: echo at {} vs {}", peak - 10, expect);
    }
}

#[test]
fn limiter_is_transparent_below_threshold_and_never_exceeds_the_ceiling() {
    let mut fx = create("limiter", SR).unwrap();
    set(fx.as_mut(), "ceiling", -1.0);
    let lat = fx.latency();
    assert!(lat > 0, "lookahead limiter reports latency");
    let x = sine(300.0, 0.3, 24000);
    let (y, _) = run(fx.as_mut(), &x, 256, 120.0);
    for i in lat + 1000..x.len() {
        assert!((y[i] - x[i - lat]).abs() < 1e-5, "sample {i}: {} vs {}", y[i], x[i - lat]);
    }
    let ceiling = 10f32.powf(-1.0 / 20.0);
    let mut fx = create("limiter", SR).unwrap();
    set(fx.as_mut(), "ceiling", -1.0);
    let hot: Vec<f32> = (0..48000).map(|i| (std::f32::consts::TAU * 97.0 * i as f32 / SR).sin() * 4.0 + if i % 9000 == 0 { 8.0 } else { 0.0 }).collect();
    let (y, r) = run(fx.as_mut(), &hot, 64, 120.0);
    let pk = y.iter().chain(&r).fold(0.0f32, |m, v| m.max(v.abs()));
    assert!(pk <= ceiling * 1.0001, "peak {pk} over ceiling {ceiling}");
}

#[test]
fn pitch_at_zero_semitones_is_a_clean_delay() {
    let mut fx = create("pitch", SR).unwrap();
    let lat = fx.latency();
    let x = sine(523.0, 0.5, 24000);
    let (y, _) = run(fx.as_mut(), &x, 256, 120.0);
    for i in lat..x.len() {
        assert!((y[i] - x[i - lat]).abs() < 1e-4, "sample {i}");
    }
    // +12 semitones doubles the dominant frequency (zero-crossing count)
    set(fx.as_mut(), "semitones", 12.0);
    let x = sine(220.0, 0.5, 48000);
    let (y, _) = run(fx.as_mut(), &x, 256, 120.0);
    let zc = |s: &[f32]| s.windows(2).filter(|w| w[0] <= 0.0 && w[1] > 0.0).count();
    let (a, b) = (zc(&x[24000..]), zc(&y[24000..]));
    assert!((b as f32 / a as f32 - 2.0).abs() < 0.1, "octave up: {a} → {b} crossings");
}

/// Trigger a performance effect in a slot on `x` from `on` to `off` samples; return output.
fn perform(kind: &str, x: &[f32], bpm: f32, on: usize, off: usize, params: &[(&str, &str)]) -> Vec<f32> {
    let mut fx = create(kind, SR).unwrap();
    for (p, v) in params {
        let val = match fx.params()[idx(fx.as_ref(), p)].kind {
            se_dsp::ParamKind::Choice(_) => choice(fx.as_ref(), p, v),
            _ => v.parse().unwrap(),
        };
        set(fx.as_mut(), p, val);
    }
    let mut slot = FxSlot::new(fx, SR, true);
    let mut out = Vec::with_capacity(x.len());
    let mut beat = 0.0f64;
    let block = 256;
    for (k, chunk) in x.chunks(block).enumerate() {
        let s = k * block;
        if s <= on && on < s + block {
            slot.set_trigger(true);
            slot.set_env(1.0);
        }
        if s <= off && off < s + block {
            slot.set_trigger(false);
            slot.set_env(0.0);
        }
        let mut l = chunk.to_vec();
        let mut r = chunk.to_vec();
        slot.process(&Ctx { sr: SR, transport: Transport { bpm, beat, beats_per_bar: 4 }, key: &[] }, &mut l, &mut r);
        beat += chunk.len() as f64 * bpm as f64 / 60.0 / SR as f64;
        out.extend_from_slice(&l);
    }
    out
}

#[test]
fn stutter_repeats_the_division_at_any_tempo_without_clicks() {
    for bpm in [90.0f32, 120.0, 174.0] {
        let d = DIVISIONS.iter().position(|d| *d == "1/8").unwrap();
        let per = (division_beats(d) * 60.0 / bpm as f64 * SR as f64).round() as usize;
        // a sine with a slow amplitude wobble (not periodic in the slice)
        let x: Vec<f32> =
            (0..SR as usize * 6).map(|i| 0.4 * (std::f32::consts::TAU * 330.0 * i as f32 / SR).sin() * (0.75 + 0.25 * (i as f32 * 0.00013).sin())).collect();
        let on = 48000;
        let off = 48000 * 4;
        let y = perform("stutter", &x, bpm, on, off, &[("division", "1/8"), ("quantize", "1/16")]);
        // while held (after capture): y[i] == y[i + per]
        let a = on + 2 * per + 4800;
        let seg = &y[a..a + per * 2];
        let mean = (0..per).map(|i| (seg[i] - seg[i + per]).abs()).sum::<f32>() / per as f32;
        assert!(mean < 1e-3, "{bpm} BPM: slice of {per} samples does not repeat (mean diff {mean})");
        // clicks: 2nd difference within a small factor of the dry signal's
        let dry = max_d2(&x);
        let wet = max_d2(&y);
        assert!(wet < dry * 3.0, "{bpm} BPM: click {wet} vs dry {dry}");
        // after release the live input is back
        let tail = off + 9600;
        assert!(y[tail..tail + 4800].iter().zip(&x[tail..tail + 4800]).all(|(a, b)| (a - b).abs() < 1e-5), "{bpm} BPM: release returns to the input");
    }
}

#[test]
fn performance_effects_pass_input_untouched_when_idle_and_release_cleanly() {
    let x = sine(250.0, 0.5, 48000 * 4);
    for kind in ["stutter", "tapestop", "vinylbrake", "reverse"] {
        let mut fx = create(kind, SR).unwrap();
        let (y, _) = run(fx.as_mut(), &x[..48000], 256, 120.0);
        assert!(y.iter().zip(&x).all(|(a, b)| a == b), "{kind}: bit-exact pass-through when idle");
        let y = perform(kind, &x, 120.0, 30000, 100000, &[]);
        // chunk/loop edges crossfade between different phases of the sine, which raises the
        // curvature a little; a click (a step) would be orders of magnitude larger
        let wet = max_d2(&y);
        let dry = max_d2(&x);
        assert!(wet < dry * 6.0, "{kind}: click {wet} vs dry {dry}");
        assert!(y.iter().all(|v| v.is_finite() && v.abs() < 2.0));
    }
}

#[test]
fn chopper_gates_at_the_division_without_clicks() {
    for bpm in [90.0f32, 120.0, 174.0] {
        let mut fx = create("chopper", SR).unwrap();
        let d = DIVISIONS.iter().position(|d| *d == "1/16").unwrap();
        set(fx.as_mut(), "division", d as f32);
        let per = (division_beats(d) * 60.0 / bpm as f64 * SR as f64).round() as usize;
        let dc = vec![0.5f32; per * 20];
        let (y, _) = run(fx.as_mut(), &dc, 256, bpm);
        // period of the gate pattern = one division
        let seg = &y[per * 4..per * 12];
        let mean = (0..per * 4).map(|i| (seg[i] - seg[i + per]).abs()).sum::<f32>() / (per * 4) as f32;
        assert!(mean < 2e-3, "{bpm} BPM: gate period != {per} (diff {mean})");
        assert!(y.iter().any(|v| *v < 0.01) && y.iter().any(|v| *v > 0.49));
        let step = y.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(step < 0.5 / 20.0, "{bpm} BPM: gate edge step {step}");
    }
}

#[test]
fn every_effect_survives_random_params_triggers_and_extremes() {
    let mut rng = 0x2545_f491u32;
    let mut rand = move || {
        rng ^= rng << 13;
        rng ^= rng >> 17;
        rng ^= rng << 5;
        rng as f32 / u32::MAX as f32
    };
    for kind in kinds() {
        let mut fx = create(kind, SR).unwrap();
        let specs = params_of(kind).unwrap();
        let mut beat = 0.0f64;
        let mut l = vec![0.0f32; MAX_BLOCK];
        let mut r = vec![0.0f32; MAX_BLOCK];
        for block in 0..600 {
            if block % 20 == 0 {
                for (i, s) in specs.iter().enumerate() {
                    let v = match block / 20 % 3 {
                        0 => s.min,
                        1 => s.max,
                        _ => s.min + (s.max - s.min) * rand(),
                    };
                    fx.set_param(i, s.clamp(v));
                }
            }
            if block % 37 == 0 {
                fx.trigger(rand() > 0.5);
            }
            let n = [64usize, 256, 1024, 17][block % 4];
            let silent = block >= 400;
            for i in 0..n {
                let v = if silent { 0.0 } else { (rand() * 2.0 - 1.0) * if block % 50 < 25 { 1.0 } else { 0.01 } };
                l[i] = v;
                r[i] = -v;
            }
            let bpm = 60.0 + rand() * 140.0;
            fx.process(&Ctx { sr: SR, transport: Transport { bpm, beat, beats_per_bar: 4 }, key: &[] }, &mut l[..n], &mut r[..n]);
            beat += n as f64 * bpm as f64 / 60.0 / SR as f64;
            for v in l[..n].iter().chain(&r[..n]) {
                // bounded: extreme settings may legitimately stack gain (EQ bands all at +24 dB)
                assert!(v.is_finite() && v.abs() < 1e5, "{kind}: output {v}");
                if silent && block > 420 {
                    assert!(*v == 0.0 || v.abs() >= f32::MIN_POSITIVE, "{kind}: denormal {v:e} after silence");
                }
            }
        }
    }
}
