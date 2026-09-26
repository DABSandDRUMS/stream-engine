//! §21: after construction, no effect and no sampler allocates or frees on the audio path.

#[global_allocator]
static ALLOC: se_alloc::Counting = se_alloc::Counting;

use se_dsp::sampler::{LayerDef, Pick, SampleBank, SampleData, Sampler, SoundDef};
use se_dsp::{Ctx, FxSlot, MAX_BLOCK, Transport, create, kinds, params_of};

const SR: f32 = 48000.0;

#[test]
fn effects_and_slots_do_not_allocate() {
    assert!(se_alloc::installed());
    for kind in kinds() {
        let mut slot = FxSlot::new(create(kind, SR).unwrap(), SR, true);
        let specs = params_of(kind).unwrap();
        let mut l = vec![0.0f32; MAX_BLOCK];
        let mut r = vec![0.0f32; MAX_BLOCK];
        let key = vec![0.1f32; MAX_BLOCK];
        let spare = create(kind, SR).unwrap();
        let mut spare = Some(spare);
        let scope = se_alloc::Scope::begin();
        let mut beat = 0.0;
        for b in 0..200usize {
            for (i, s) in specs.iter().enumerate() {
                slot.set_param(i, s.min + (s.max - s.min) * ((b * 7 + i * 3) % 11) as f32 / 10.0);
            }
            slot.set_trigger(b % 40 < 25);
            slot.set_env(if b % 40 < 25 { 1.0 } else { 0.0 });
            slot.set_wet(0.8);
            slot.set_enabled(b % 90 != 0);
            if b == 100
                && let Some(fx) = spare.take()
            {
                assert!(slot.swap_effect(fx).is_none());
            }
            let n = [64usize, 256, 1024][b % 3];
            for i in 0..n {
                l[i] = ((b * n + i) as f32 * 0.03).sin() * 0.5;
                r[i] = l[i];
            }
            slot.process(&Ctx { sr: SR, transport: Transport { bpm: 128.0, beat, beats_per_bar: 4 }, key: &key[..n] }, &mut l[..n], &mut r[..n]);
            beat += n as f64 * 128.0 / 60.0 / SR as f64;
        }
        let counts = (scope.allocs(), scope.frees());
        drop(scope);
        assert_eq!(counts, (0, 0), "{kind} allocated on the audio path");
        drop(slot.take_retired());
    }
}

#[test]
fn sampler_does_not_allocate() {
    assert!(se_alloc::installed());
    let mut bank = SampleBank {
        samples: (0..3).map(|k| SampleData { name: format!("s{k}"), l: vec![0.3; 2000 + k * 500], r: vec![0.2; 2000 + k * 500] }).collect(),
        sounds: vec![SoundDef {
            name: "hit".into(),
            layers: vec![LayerDef { vel: [0.0, 0.5], samples: vec![0] }, LayerDef { vel: [0.5, 1.0], samples: vec![1, 2] }],
            pick: Pick::Random,
            gain: 1.0,
            choke: Some(1),
            max_voices: 3,
            next: 0,
        }],
    };
    let mut s = Sampler::new(16, SR);
    let mut l = vec![0.0f32; 256];
    let mut r = vec![0.0f32; 256];
    let scope = se_alloc::Scope::begin();
    for b in 0..400 {
        if b % 3 == 0 {
            s.play(&mut bank, 0, (b % 10) as f32 / 10.0, 0.8, 0.3, (b % 5) as f32 - 2.0);
        }
        if b % 97 == 0 {
            s.stop_all(64);
        }
        l.fill(0.0);
        r.fill(0.0);
        s.process(&bank, &mut l, &mut r);
    }
    let counts = (scope.allocs(), scope.frees());
    drop(scope);
    assert_eq!(counts, (0, 0));
}
