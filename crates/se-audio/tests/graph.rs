//! Offline tests of the real-time graph: routing, gains, limiter, ducking, slots, taps,
//! alignment, graph swaps, the tempo-synced stutter on the music bus, and drum triggers.

mod common;

use common::*;
use se_audio::builder::LinkTarget;
use se_audio::dsp::SlotReader;
use se_audio::graph::{ChainRef, FxWhat, RtMsg, RtOut, SlotEntry, TapEntry, TapSource, Target};
use se_audio::taps::TapProducer;

const MUSIC_STUTTER: &str = r#"
quantum = 256
[buses.music]
fx = [{ name = "stutter", kind = "stutter", trigger = true, division = "1/8" }]
"#;

fn bus(r: &Rig, name: &str) -> u8 {
    r.cfg.bus_index(name).unwrap() as u8
}

fn attach_slot(r: &mut Rig, name: &str, index: u16, channels: u16) -> rtrb::Producer<f32> {
    let (p, c) = rtrb::RingBuffer::new(48000 * channels as usize);
    let reader = SlotReader::new(c, channels, 48000, 48000, 10.0);
    let entry = SlotEntry::new(name.into(), reader, SR, 48000, 64 + index as usize);
    r.send(RtMsg::AddSlot { index, entry: Box::new(entry) });
    let bus = r.cfg.route_slot(name).map(|b| b as u8);
    r.send(RtMsg::SlotRoute { generation: r.generation, index, bus, direct: None });
    p
}

#[test]
fn band_input_reaches_band_and_program_nodes_and_follows_the_fader() {
    let mut r = Rig::new("quantum = 256");
    let (il, ir) = (r.in_port("in_band_1"), r.in_port("in_band_2"));
    let band = r.out_port("out_band_L");
    let out = r.run(60, &[il, ir], sine(1000.0, 0.25), band);
    let settled = &out[out.len() - 4800..];
    assert!((rms(settled) - 0.25 / 2f32.sqrt()).abs() < 1e-3, "band rms {}", rms(settled));
    let b = bus(&r, "band");
    r.param(Target::BusGain(b), -6.0206);
    let out = r.run(40, &[il, ir], sine(1000.0, 0.25), band);
    let settled = &out[out.len() - 4800..];
    assert!((rms(settled) - 0.125 / 2f32.sqrt()).abs() < 1e-3, "after -6 dB: {}", rms(settled));
    assert!(max_d2(&out) < 0.25 * (std::f32::consts::TAU * 1000.0 / SR).powi(2) * 1.2, "fader move must be smooth");
    // program carries the band bus (other buses are silent)
    let prog = r.out_port("out_program_L");
    let out = r.run(20, &[il, ir], sine(1000.0, 0.25), prog);
    assert!((rms(&out[out.len() - 4800..]) - 0.125 / 2f32.sqrt()).abs() < 2e-3);
}

#[test]
fn motherboard_playback_keeps_the_24c_capture_out_and_follows_the_music_fader() {
    let mut r = Rig::new("quantum = 256\n[playback]\ntarget = \"Motherboard Audio Speakers\"\nbuses = [\"music\", \"sfx\", \"tts\", \"game\"]");
    for (port, channel) in [("play_1", 1), ("play_2", 2)] {
        assert!(
            r.built.links.iter().any(|link| { link.port == port && link.to == LinkTarget::Sink { target: "Motherboard Audio Speakers".into(), channel } }),
            "{port} must link to motherboard channel {channel}, not to the 24c"
        );
    }
    let (il, ir) = (r.in_port("in_band_1"), r.in_port("in_band_2"));
    let play = r.out_port("play_1");
    let band = r.out_port("out_band_L");
    let capture = r.run(60, &[il, ir], sine(220.0, 0.3), play);
    assert!(max_abs(&capture) < 1e-6, "24c capture must never return to the 16R");
    assert!(rms(&r.io.outs[band]) > 0.15, "the 24c input was present for the test");

    let mut youtube = attach_slot(&mut r, "youtube", 0, 2);
    let quantum = r.quantum;
    let mut fill = |cycles: usize| {
        for i in 0..cycles * quantum {
            let sample = 0.2 * (std::f32::consts::TAU * 440.0 * i as f32 / SR).sin();
            youtube.push(sample).unwrap();
            youtube.push(sample).unwrap();
        }
    };
    fill(60);
    let before = r.run(60, &[il, ir], sine(220.0, 0.3), play);
    let before = rms(&before[before.len() - 4800..]);
    assert!(before > 0.1, "YouTube audio must reach the motherboard");
    let music = bus(&r, "music");
    r.param(Target::BusGain(music), -6.0206);
    fill(60);
    let after = r.run(60, &[il, ir], sine(220.0, 0.3), play);
    let after = rms(&after[after.len() - 4800..]);
    assert!((after / before - 0.5).abs() < 0.03, "music fader must affect the physical output: {after}/{before}");
}

#[test]
fn mute_ramps_without_clicks() {
    let mut r = Rig::new("quantum = 128");
    let (il, ir) = (r.in_port("in_band_1"), r.in_port("in_band_2"));
    let band = r.out_port("out_band_L");
    r.run(20, &[il, ir], |_| 0.5, band);
    let b = bus(&r, "band");
    r.param(Target::BusMute(b), 1.0);
    let out = r.run(20, &[il, ir], |_| 0.5, band);
    let step = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
    assert!(step < 0.5 / 400.0, "mute step {step}");
    assert!(out[out.len() - 1].abs() < 1e-6);
}

#[test]
fn program_limiter_never_exceeds_the_ceiling() {
    let mut r = Rig::new("quantum = 256\n[buses.program]\nceiling = -1.0");
    let (il, ir) = (r.in_port("in_band_1"), r.in_port("in_band_2"));
    let prog = r.out_port("out_program_L");
    let out = r.run(200, &[il, ir], sine(220.0, 2.0), prog);
    let ceiling = se_dsp::db_to_gain(-1.0);
    let peak = max_abs(&out);
    assert!(peak <= ceiling * 1.0001, "peak {peak} > ceiling {ceiling}");
    assert!(peak > ceiling * 0.9, "limiter should hold the level near the ceiling, got {peak}");
}

#[test]
fn slots_route_by_name_and_music_ducks_under_manual_and_tts() {
    let mut r = Rig::new("quantum = 256\n[duck]\ndepth = -12.0\nattack = \"20ms\"\nrelease = \"100ms\"\nhold = \"0ms\"\nthreshold = -40");
    let mut yt = attach_slot(&mut r, "youtube", 0, 2);
    let mut tts = attach_slot(&mut r, "tts", 1, 1);
    let music = r.out_port("out_music_L");
    let tts_out = r.out_port("out_tts_L");
    let mut phase = 0u64;
    fn feed(p: &mut rtrb::Producer<f32>, phase: u64, ch: usize, n: usize, amp: f32) {
        for i in 0..n {
            let v = amp * (std::f32::consts::TAU * 500.0 * (phase + i as u64) as f32 / SR).sin();
            for _ in 0..ch {
                let _ = p.push(v);
            }
        }
    }
    let mut run = |r: &mut Rig, cycles: usize, tts_amp: f32, yt: &mut rtrb::Producer<f32>, tts: &mut rtrb::Producer<f32>| -> (Vec<f32>, Vec<f32>) {
        let mut m = Vec::new();
        let mut t = Vec::new();
        for _ in 0..cycles {
            feed(yt, phase, 2, 256, 0.5);
            feed(tts, phase, 1, 256, tts_amp);
            phase += 256;
            r.cycle(|_, _| 0.0);
            m.extend_from_slice(&r.io.outs[music]);
            t.extend_from_slice(&r.io.outs[tts_out]);
        }
        (m, t)
    };
    let (m, _) = run(&mut r, 60, 0.0, &mut yt, &mut tts);
    let base = rms(&m[m.len() - 4800..]);
    assert!((base - 0.5 / 2f32.sqrt()).abs() < 0.01, "music rms {base}");
    // manual duck (preset / audio.duck)
    r.param(Target::DuckActive, 1.0);
    let (m, _) = run(&mut r, 40, 0.0, &mut yt, &mut tts);
    let ducked = rms(&m[m.len() - 4800..]);
    assert!((se_dsp::gain_to_db(ducked / base) + 12.0).abs() < 0.3, "ducked by {} dB", se_dsp::gain_to_db(ducked / base));
    assert!(max_d2(&m) < 0.5 * (std::f32::consts::TAU * 500.0 / SR).powi(2) * 1.5, "duck ramp must not click");
    r.param(Target::DuckActive, 0.0);
    let (m, _) = run(&mut r, 60, 0.0, &mut yt, &mut tts);
    assert!((rms(&m[m.len() - 4800..]) - base).abs() < 0.01, "released");
    // tts above threshold ducks music automatically; tts itself is not ducked
    let (m, t) = run(&mut r, 40, 0.3, &mut yt, &mut tts);
    let d = se_dsp::gain_to_db(rms(&m[m.len() - 4800..]) / base);
    assert!((d + 12.0).abs() < 0.5, "tts duck {d} dB");
    assert!(rms(&t[t.len() - 4800..]) > 0.2);
    // mic.talking signal ducks too
    let (_, _) = run(&mut r, 40, 0.0, &mut yt, &mut tts);
    r.param(Target::DuckSignal, 1.0);
    let (m, _) = run(&mut r, 40, 0.0, &mut yt, &mut tts);
    assert!((se_dsp::gain_to_db(rms(&m[m.len() - 4800..]) / base) + 12.0).abs() < 0.5);
}

#[test]
fn buses_are_time_aligned_into_program() {
    // the band bus has a latency-reporting effect (pitch shifter); music doesn't: an impulse on
    // both inputs must reach program together (one peak, not two)
    let toml = r#"
quantum = 256
[inputs.band]
target = "Studio 24c"
channels = [1]
fx = [{ kind = "pitch" }]
[inputs.aux]
target = "Studio 24c"
channels = [2]
bus = "music"
"#;
    let mut r = Rig::new(toml);
    let (ib, ia) = (r.in_port("in_band_1"), r.in_port("in_aux_1"));
    let prog = r.out_port("out_program_L");
    assert!(r.built.graph.is_none());
    for _ in 0..10 {
        r.cycle(|_, _| 0.0);
    }
    let mut out = Vec::new();
    for c in 0..20 {
        r.cycle(|p, i| if (p == ib || p == ia) && c == 0 && i == 10 { 0.25 } else { 0.0 });
        out.extend_from_slice(&r.io.outs[prog]);
    }
    let peaks: Vec<usize> = out.iter().enumerate().filter(|(_, v)| v.abs() > 0.1).map(|(i, _)| i).collect();
    assert_eq!(peaks.len(), 1, "one aligned impulse expected, got {peaks:?}");
    assert!((out[peaks[0]] - 0.5).abs() < 0.05, "both contributions sum: {}", out[peaks[0]]);
    let pitch = se_dsp::create("pitch", SR).unwrap().latency();
    let lim = se_dsp::create("limiter", SR).unwrap().latency();
    assert_eq!(peaks[0], 10 + pitch + 2 * lim, "pitch + bus limiter + program limiter");
}

#[test]
fn input_av_delay_shifts_audio() {
    let mut r = Rig::new("quantum = 256");
    let (il, ir) = (r.in_port("in_band_1"), r.in_port("in_band_2"));
    let band = r.out_port("out_band_L");
    r.param(Target::InputDelay(0), 10.0);
    r.run(10, &[il, ir], |_| 0.0, band);
    let mut out = Vec::new();
    for c in 0..10 {
        r.cycle(|p, i| if (p == il || p == ir) && c == 0 && i == 0 { 1.0 } else { 0.0 });
        out.extend_from_slice(&r.io.outs[band]);
    }
    let at = out.iter().position(|v| v.abs() > 0.5).expect("impulse");
    // every bus carries its limiter slot (bypassed or not) so the latency never jumps
    let lim = se_dsp::create("limiter", SR).unwrap().latency();
    assert_eq!(at, 480 + lim, "10 ms = 480 samples + limiter lookahead");
}

#[test]
fn stutter_on_the_music_bus_is_tempo_synced_and_click_free() {
    let mut r = Rig::new(MUSIC_STUTTER);
    let mut yt = attach_slot(&mut r, "youtube", 0, 2);
    let music = r.out_port("out_music_L");
    let m = bus(&r, "music");
    // transport: 120 BPM → 1/8 = 0.5 beat = 12000 samples
    r.send(RtMsg::Transport { bpm: 120.0, beat: 0.0, ts: r.t });
    let tone = |i: u64| 0.4 * (std::f32::consts::TAU * 330.0 * i as f32 / SR).sin() * (1.0 + 0.5 * (std::f32::consts::TAU * 0.7 * i as f32 / SR).sin()) / 1.5;
    let mut n = 0u64;
    let mut out = Vec::new();
    let mut run = |r: &mut Rig, cycles: usize, yt: &mut rtrb::Producer<f32>, out: &mut Vec<f32>| {
        for _ in 0..cycles {
            for _ in 0..256 {
                let v = tone(n);
                n += 1;
                yt.push(v).unwrap();
                yt.push(v).unwrap();
            }
            r.cycle(|_, _| 0.0);
            out.extend_from_slice(&r.io.outs[music]);
        }
    };
    run(&mut r, 100, &mut yt, &mut out);
    let before = out.len();
    let fx = |what| Target::Fx { chain: ChainRef::Bus(m), slot: 0, what };
    r.param(fx(FxWhat::Active), 1.0);
    r.param(fx(FxWhat::Env), 1.0);
    run(&mut r, 400, &mut yt, &mut out);
    r.param(fx(FxWhat::Active), 0.0);
    r.param(fx(FxWhat::Env), 0.0);
    run(&mut r, 150, &mut yt, &mut out);
    // clicks: the stutter output's 2nd difference stays within the dry signal's own range
    let dry_d2 = {
        let d: Vec<f32> = (0..48000).map(tone).collect();
        max_d2(&d)
    };
    let d2 = max_d2(&out);
    assert!(d2 < dry_d2 * 4.0, "click: max d2 {d2} vs dry {dry_d2}");
    // periodicity at the 1/8 note while held (after the capture settles)
    let seg = &out[before + 48000..before + 48000 + 36000];
    let per = 12000;
    let mut err = 0.0f32;
    let mut cnt = 0;
    for i in 0..seg.len() - per {
        err += (seg[i] - seg[i + per]).abs();
        cnt += 1;
    }
    let mean = err / cnt as f32;
    assert!(mean < 0.02, "not repeating every 1/8 note: mean diff {mean}");
}

#[test]
fn graph_swap_crossfades_without_clicks() {
    let mut r = Rig::new("quantum = 256");
    let (il, ir) = (r.in_port("in_band_1"), r.in_port("in_band_2"));
    let band = r.out_port("out_band_L");
    let mut out = r.run(20, &[il, ir], sine(200.0, 0.5), band);
    // generation 2 with the band bus at -12 dB
    let cfg2 = cfg("quantum = 256\n[buses.band]\ngain = -12.0");
    let (bank, _) = se_audio::sounds::build_bank(&cfg2, std::path::Path::new("/nonexistent"), &mut se_audio::sounds::SoundCache::default());
    let mut built = se_audio::builder::build(&cfg2, 2, &mut r.ports, bank, &se_audio::wasmfx::DspPatches::new(1000));
    r.send(RtMsg::Swap(built.graph.take().unwrap()));
    r.generation = 2;
    let tail = r.run(40, &[il, ir], sine(200.0, 0.5), band);
    out.extend_from_slice(&tail);
    let lim = 0.5 * (std::f32::consts::TAU * 200.0 / SR).powi(2) * 3.0;
    // (skip the start of the test tone itself)
    let d2 = max_d2(&out[1000..]);
    assert!(d2 < lim, "swap click {d2}");
    let settled = rms(&tail[tail.len() - 4800..]);
    assert!((se_dsp::gain_to_db(settled / (0.5 / 2f32.sqrt())) + 12.0).abs() < 0.2, "new gain applied");
    let outs = r.drain();
    assert!(outs.iter().any(|o| matches!(o, RtOut::Live(2))));
    assert!(outs.iter().any(|o| matches!(o, RtOut::Garbage(_))), "old graph handed back for dropping");
}

#[test]
fn taps_deliver_bus_input_with_block_stamps() {
    let mut r = Rig::new("quantum = 256");
    let (sp, mut sc) = rtrb::RingBuffer::new(48000);
    let (cp, mut cc) = rtrb::RingBuffer::new(64);
    let producer = TapProducer { name: "bus.band.0".into(), samples: sp, clock: cp, written: 0, dropped: 0 };
    r.send(RtMsg::AddTap { index: 8, entry: Box::new(TapEntry { producer, source: None, stereo: false }) });
    let b = bus(&r, "band");
    r.send(RtMsg::TapRoute { generation: 1, index: 8, source: Some(TapSource::BusIn(b, 0)) });
    let (il, ir) = (r.in_port("in_band_1"), r.in_port("in_band_2"));
    let t0 = r.t;
    r.run(4, &[il, ir], |i| i as f32 * 1e-4, 0);
    assert_eq!(sc.slots(), 1024);
    let first = cc.pop().unwrap();
    assert_eq!((first.sample, first.ts), (0, t0));
    let second = cc.pop().unwrap();
    assert_eq!(second.sample, 256);
    assert!((second.ts - t0) as f64 - 256.0 / 48000.0 * 1e9 < 2.0);
    assert_eq!(sc.pop().unwrap(), 0.0);
    assert!((sc.pop().unwrap() - 1e-4).abs() < 1e-9);
}

#[test]
fn drum_pads_fire_hits_with_velocity_and_reject_silence() {
    let toml = r#"
quantum = 128
[inputs.band]
target = "Studio 24c"
channels = [1, 2]
[inputs.kick]
target = "16R"
channels = [1]
bus = "drums"
[inputs.snare]
target = "16R"
channels = [2]
bus = "drums"
[drums.pads.kick]
input = "kick"
threshold = -30
[drums.pads.snare]
input = "snare"
threshold = -30
"#;
    let mut r = Rig::new(toml);
    let k = r.in_port("in_kick_1");
    let s = r.in_port("in_snare_1");
    // kick hits every 0.5 s (60 Hz burst), snare in between (noise-ish 1 kHz burst)
    let hit = |i: u64, start: u64, f: f32, amp: f32| {
        if i < start {
            return 0.0;
        }
        let t = (i - start) as f32 / SR;
        if t > 0.15 { 0.0 } else { amp * (std::f32::consts::TAU * f * t).sin() * (-t * 30.0).exp() }
    };
    let mut kicks = 0;
    let mut snares = 0;
    let mut vel = Vec::new();
    for c in 0..(48000 * 3 / 128) {
        let base = c as u64 * 128;
        r.cycle(|p, i| {
            let n = base + i as u64;
            let beat = n / 24000;
            let start = beat * 24000;
            if p == k && beat.is_multiple_of(2) {
                hit(n, start, 60.0, 0.3 + 0.2 * (beat % 4) as f32 / 2.0)
            } else if p == s && beat % 2 == 1 {
                hit(n, start, 1000.0, 0.4)
            } else {
                0.0
            }
        });
        for o in r.drain() {
            if let RtOut::Hit { pad, velocity, .. } = o {
                if pad == 0 {
                    kicks += 1;
                    vel.push(velocity);
                } else {
                    snares += 1;
                }
            }
        }
    }
    assert_eq!(kicks, 3, "kick hits at 0, 1, 2 s");
    assert_eq!(snares, 3, "snare hits at 0.5, 1.5, 2.5 s");
    assert!(vel.iter().all(|v| (0.0..=1.0).contains(v)));
}

/// A dsp patch's trigger payload (`patch.<id>.payload.*`) is bound before its `.active` edge:
/// one parameter sync that sees a new trigger delivers the payload first, so the module never
/// counts the new trigger with the previous payload.
#[test]
fn dsp_patch_payload_is_delivered_before_the_trigger_edge() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../project-example");
    let r = Rig::with_root("[buses.band]\nfx = [{ name = \"ringmod\", patch = \"ringmod\" }]", &root);
    let pos = |addr: &str| r.built.params.iter().position(|p| p.addr == addr).unwrap_or_else(|| panic!("{addr} not bound"));
    let active = pos("patch.ringmod.active");
    for f in se_core::triggers::PAYLOAD_FIELDS.iter().map(|f| format!("patch.ringmod.payload.{f}")).chain(["patch.ringmod.payload.user_color".to_string()]) {
        assert!(pos(&f) < active, "{f} bound after the edge");
    }
}
