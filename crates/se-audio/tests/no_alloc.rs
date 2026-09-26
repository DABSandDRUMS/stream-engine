//! §21: the audio callback never allocates or frees — graph with inputs, slots, effect
//! chains (incl. a triggered stutter), limiter, ducking, sampler playback, taps, drum
//! triggers, parameter messages, and a graph swap, all inside an allocation scope.

#[global_allocator]
static ALLOC: se_alloc::Counting = se_alloc::Counting;

mod common;

use common::*;
use se_audio::dsp::SlotReader;
use se_audio::graph::{ChainRef, FxWhat, RtMsg, SlotEntry, TapEntry, TapSource, Target};
use se_audio::taps::TapProducer;

fn write_wav(path: &std::path::Path, samples: &[f32], rate: u32) {
    let mut b = Vec::new();
    let data_len = (samples.len() * 2) as u32;
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(36 + data_len).to_le_bytes());
    b.extend_from_slice(b"WAVEfmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&1u16.to_le_bytes());
    b.extend_from_slice(&rate.to_le_bytes());
    b.extend_from_slice(&(rate * 2).to_le_bytes());
    b.extend_from_slice(&2u16.to_le_bytes());
    b.extend_from_slice(&16u16.to_le_bytes());
    b.extend_from_slice(b"data");
    b.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        b.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0) as i16).to_le_bytes());
    }
    std::fs::write(path, b).unwrap();
}

#[test]
fn audio_callback_does_not_allocate() {
    assert!(se_alloc::installed());
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("assets/sounds")).unwrap();
    let blip: Vec<f32> = (0..4410).map(|i| (i as f32 * 0.2).sin() * (1.0 - i as f32 / 4410.0)).collect();
    write_wav(&dir.path().join("assets/sounds/blip.wav"), &blip, 44100);
    let toml = r#"
quantum = 256
[inputs.band]
target = "Studio 24c"
channels = [1, 2]
[inputs.kick]
target = "16R"
channels = [3]
bus = "drums"
[buses.music]
fx = [{ name = "stutter", kind = "stutter", trigger = true, division = "1/16" }, { name = "lp", kind = "svf" }]
[drums.pads.kick]
input = "kick"
threshold = -30
layer = { sound = "blip", gain = -6 }
"#;
    let mut r = Rig::with_root(toml, dir.path());
    // a youtube slot and a tap
    let (mut yt, c) = rtrb::RingBuffer::new(96000);
    let entry = SlotEntry::new("youtube".into(), SlotReader::new(c, 2, 48000, 48000, 10.0), SR, 48000, 64);
    r.send(RtMsg::AddSlot { index: 0, entry: Box::new(entry) });
    let music = r.cfg.bus_index("music").unwrap() as u8;
    r.send(RtMsg::SlotRoute { generation: 1, index: 0, bus: Some(music), direct: None });
    let (sp, _sc) = rtrb::RingBuffer::new(1 << 20);
    let (cp, _cc) = rtrb::RingBuffer::new(1 << 14);
    let producer = TapProducer { name: "bus.band.0".into(), samples: sp, clock: cp, written: 0, dropped: 0 };
    r.send(RtMsg::AddTap { index: 8, entry: Box::new(TapEntry { producer, source: None, stereo: true }) });
    r.send(RtMsg::TapRoute { generation: 1, index: 8, source: Some(TapSource::BusInStereo(0)) });
    let sound = r.built.sounds.iter().position(|s| s == "blip").expect("implicit sound from assets/sounds") as u16;
    let (il, ir, kick) = (r.in_port("in_band_1"), r.in_port("in_band_2"), r.in_port("in_kick_1"));
    // warm up outside the scope (first-touch of lazily initialised thread-locals etc.)
    for _ in 0..20 {
        for _ in 0..512 {
            let _ = yt.push(0.1);
        }
        r.cycle(|_, i| (i as f32 * 0.01).sin() * 0.2);
    }
    // a second generation to swap in under the scope
    let cfg2 = cfg(toml);
    let (bank2, _) = se_audio::sounds::build_bank(&cfg2, dir.path(), &mut se_audio::sounds::SoundCache::default());
    let mut built2 = se_audio::builder::build(&cfg2, 2, &mut r.ports, bank2, &se_audio::wasmfx::DspPatches::new(1000));
    let g2 = built2.graph.take().unwrap();
    let fx = |what| Target::Fx { chain: ChainRef::Bus(music), slot: 0, what };

    // everything the control thread sends, queued before the scope (queueing allocates nothing
    // on the RT side; the Boxes were built above)
    let mut msgs: Vec<RtMsg> = vec![
        RtMsg::Param { generation: 1, target: Target::BusGain(0), value: -3.0 },
        RtMsg::Param { generation: 1, target: Target::DuckActive, value: 1.0 },
        RtMsg::Param { generation: 1, target: fx(FxWhat::Active), value: 1.0 },
        RtMsg::Param { generation: 1, target: fx(FxWhat::Env), value: 1.0 },
        RtMsg::Param { generation: 1, target: Target::Fx { chain: ChainRef::Bus(music), slot: 1, what: FxWhat::Param(1) }, value: 800.0 },
        RtMsg::Play { generation: 1, sound, velocity: 0.8, gain_db: -3.0, pan: 0.2, pitch: 2.0 },
        RtMsg::Transport { bpm: 128.0, beat: 3.0, ts: r.t },
    ];
    for m in msgs.drain(..) {
        r.send(m);
    }
    let scope = se_alloc::Scope::begin();
    for c in 0..600u32 {
        if c == 300 {
            r.tx.push(RtMsg::Param { generation: 1, target: Target::DuckActive, value: 0.0 }).ok().unwrap();
            r.tx.push(RtMsg::Param { generation: 1, target: fx(FxWhat::Active), value: 0.0 }).ok().unwrap();
        }
        for _ in 0..512 {
            let _ = yt.push(0.1);
        }
        r.cycle(|p, i| {
            if p == kick {
                if c % 50 == 0 && i < 64 { 0.8 } else { 0.0 }
            } else if p == il || p == ir {
                (i as f32 * 0.01).sin() * 0.2
            } else {
                0.0
            }
        });
    }
    let allocs = (scope.allocs(), scope.frees());
    drop(scope);
    assert_eq!(allocs, (0, 0), "allocations/frees inside the audio callback");
    // the swap (the old graph must come back as garbage, not be freed on the RT thread)
    r.tx.push(RtMsg::Swap(g2)).ok().unwrap();
    let scope = se_alloc::Scope::begin();
    for _ in 0..40 {
        for _ in 0..512 {
            let _ = yt.push(0.1);
        }
        r.cycle(|_, i| (i as f32 * 0.01).sin() * 0.2);
    }
    let allocs = (scope.allocs(), scope.frees());
    drop(scope);
    assert_eq!(allocs, (0, 0), "graph swap on the audio thread");
    assert!(r.drain().iter().any(|o| matches!(o, se_audio::graph::RtOut::Garbage(_))));
}
