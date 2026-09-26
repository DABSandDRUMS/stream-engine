//! Sampler (§8.5, §8.6): one-shot / multi-sample playback for sound effects and drum
//! layering. Sounds have velocity layers, each with one or more samples picked round-robin
//! or at random; per-sound polyphony steals the oldest voice with a short fade; choke groups
//! fade the other voices of the group. Voices live in a fixed pool: `play`, `process`, and
//! `stop_all` never allocate.

use crate::util::Rng;

/// One decoded sample at the graph rate (mono sources are duplicated to both channels).
pub struct SampleData {
    pub name: String,
    pub l: Vec<f32>,
    pub r: Vec<f32>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Pick {
    #[default]
    RoundRobin,
    Random,
}

/// Samples for a velocity range (inclusive, 0–1).
pub struct LayerDef {
    pub vel: [f32; 2],
    /// Indices into [`SampleBank::samples`].
    pub samples: Vec<usize>,
}

pub struct SoundDef {
    pub name: String,
    pub layers: Vec<LayerDef>,
    pub pick: Pick,
    /// Linear gain.
    pub gain: f32,
    pub choke: Option<u32>,
    pub max_voices: usize,
    /// Round-robin cursor (advanced by [`Sampler::play`]).
    pub next: usize,
}

pub struct SampleBank {
    pub samples: Vec<SampleData>,
    pub sounds: Vec<SoundDef>,
}

impl SampleBank {
    pub fn find(&self, name: &str) -> Option<usize> {
        self.sounds.iter().position(|s| s.name == name)
    }
}

/// Fade used when a voice is stolen or choked (ms).
pub const STEAL_MS: f32 = 5.0;

#[derive(Clone, Copy, Default)]
struct Voice {
    active: bool,
    sound: usize,
    sample: usize,
    pos: f64,
    rate: f64,
    gl: f32,
    gr: f32,
    /// Fade-out: remaining samples and per-sample decrement (1 → 0).
    fade: f32,
    fade_step: f32,
    fading: bool,
    choke: Option<u32>,
    age: u64,
}

pub struct Sampler {
    voices: Vec<Voice>,
    sr: f32,
    clock: u64,
    rng: Rng,
}

impl Sampler {
    pub fn new(max_voices: usize, sr: f32) -> Sampler {
        Sampler { voices: vec![Voice::default(); max_voices.max(1)], sr, clock: 0, rng: Rng::new(0x5a3f_19c7) }
    }

    fn fade_out(v: &mut Voice, samples: u32) {
        if samples == 0 {
            v.active = false;
            return;
        }
        if !v.fading || v.fade_step < v.fade / samples as f32 {
            v.fading = true;
            v.fade_step = v.fade / samples as f32;
        }
    }

    /// Start `sound`. Picks the layer by velocity, then a sample (round robin / random).
    /// `gain` linear, `pan` −1..1 (equal power), `pitch` in semitones. Returns false if the
    /// sound has nothing to play.
    pub fn play(&mut self, bank: &mut SampleBank, sound: usize, velocity: f32, gain: f32, pan: f32, pitch: f32) -> bool {
        let steal = (STEAL_MS * 0.001 * self.sr) as u32;
        let Some(def) = bank.sounds.get_mut(sound) else { return false };
        let vel = if velocity.is_finite() { velocity.clamp(0.0, 1.0) } else { 1.0 };
        // layer: first whose range contains the velocity, else the nearest
        let layer = def.layers.iter().position(|l| vel >= l.vel[0] && vel <= l.vel[1]).or_else(|| {
            def.layers
                .iter()
                .enumerate()
                .filter(|(_, l)| !l.samples.is_empty())
                .min_by(|(_, a), (_, b)| {
                    let da = (a.vel[0] - vel).abs().min((a.vel[1] - vel).abs());
                    let db = (b.vel[0] - vel).abs().min((b.vel[1] - vel).abs());
                    da.total_cmp(&db)
                })
                .map(|(i, _)| i)
        });
        let Some(layer) = layer.map(|i| &def.layers[i]) else { return false };
        if layer.samples.is_empty() {
            return false;
        }
        let pick = match def.pick {
            Pick::RoundRobin => {
                let i = def.next % layer.samples.len();
                def.next = def.next.wrapping_add(1);
                i
            }
            Pick::Random => self.rng.below(layer.samples.len()),
        };
        let sample = layer.samples[pick];
        if bank.samples.get(sample).is_none_or(|s| s.l.is_empty()) {
            return false;
        }
        let choke = def.choke;
        let max = def.max_voices.max(1);
        // choke group: fade every other voice of the group
        if let Some(g) = choke {
            for v in self.voices.iter_mut().filter(|v| v.active && v.choke == Some(g)) {
                Self::fade_out(v, steal);
            }
        }
        // per-sound polyphony: fade the oldest non-fading voices beyond the limit
        let mut playing = self.voices.iter().filter(|v| v.active && v.sound == sound && !v.fading).count();
        while playing >= max {
            let Some(old) = self.voices.iter_mut().filter(|v| v.active && v.sound == sound && !v.fading).min_by_key(|v| v.age) else { break };
            Self::fade_out(old, steal);
            playing -= 1;
        }
        // a free slot, else steal the oldest voice overall (fading ones first)
        let slot = match self.voices.iter().position(|v| !v.active) {
            Some(i) => i,
            None => self.voices.iter().enumerate().min_by_key(|(_, v)| (!v.fading, v.age)).map(|(i, _)| i).unwrap_or(0),
        };
        // velocity curve: −30 dB at velocity 0 up to 0 dB at 1 (square law)
        let vg = vel * vel;
        let g = def.gain * gain * vg;
        let p = (pan.clamp(-1.0, 1.0) + 1.0) * std::f32::consts::FRAC_PI_4;
        self.clock += 1;
        self.voices[slot] = Voice {
            active: true,
            sound,
            sample,
            pos: 0.0,
            rate: 2f64.powf(pitch.clamp(-48.0, 48.0) as f64 / 12.0),
            gl: g * p.cos(),
            gr: g * p.sin(),
            fade: 1.0,
            fade_step: 0.0,
            fading: false,
            choke,
            age: self.clock,
        };
        true
    }

    /// Add all active voices into `l`/`r`.
    pub fn process(&mut self, bank: &SampleBank, l: &mut [f32], r: &mut [f32]) {
        let n = l.len().min(r.len());
        for v in self.voices.iter_mut().filter(|v| v.active) {
            let Some(s) = bank.samples.get(v.sample) else {
                v.active = false;
                continue;
            };
            let len = s.l.len().min(s.r.len());
            for i in 0..n {
                let idx = v.pos as usize;
                if idx + 1 >= len {
                    // last sample: play it, then stop
                    if idx < len {
                        l[i] += s.l[idx] * v.gl * v.fade;
                        r[i] += s.r[idx] * v.gr * v.fade;
                    }
                    v.active = false;
                    break;
                }
                let t = (v.pos - idx as f64) as f32;
                let a = s.l[idx] + (s.l[idx + 1] - s.l[idx]) * t;
                let b = s.r[idx] + (s.r[idx + 1] - s.r[idx]) * t;
                l[i] += a * v.gl * v.fade;
                r[i] += b * v.gr * v.fade;
                v.pos += v.rate;
                if v.fading {
                    v.fade -= v.fade_step;
                    if v.fade <= 0.0 {
                        v.active = false;
                        break;
                    }
                }
            }
        }
    }

    /// Fade out every voice over `fade_samples` (0 = stop now).
    pub fn stop_all(&mut self, fade_samples: u32) {
        for v in self.voices.iter_mut().filter(|v| v.active) {
            Self::fade_out(v, fade_samples);
        }
    }

    pub fn active(&self) -> usize {
        self.voices.iter().filter(|v| v.active).count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bank() -> SampleBank {
        let tone = |v: f32, n: usize| SampleData { name: format!("{v}"), l: vec![v; n], r: vec![v; n] };
        SampleBank {
            samples: vec![tone(0.1, 1000), tone(0.2, 1000), tone(0.3, 1000), tone(1.0, 4800)],
            sounds: vec![
                SoundDef {
                    name: "snare".into(),
                    layers: vec![LayerDef { vel: [0.0, 0.5], samples: vec![0] }, LayerDef { vel: [0.5, 1.0], samples: vec![1, 2] }],
                    pick: Pick::RoundRobin,
                    gain: 1.0,
                    choke: None,
                    max_voices: 2,
                    next: 0,
                },
                SoundDef {
                    name: "open".into(),
                    layers: vec![LayerDef { vel: [0.0, 1.0], samples: vec![3] }],
                    pick: Pick::RoundRobin,
                    gain: 1.0,
                    choke: Some(1),
                    max_voices: 4,
                    next: 0,
                },
                SoundDef {
                    name: "closed".into(),
                    layers: vec![LayerDef { vel: [0.0, 1.0], samples: vec![0] }],
                    pick: Pick::RoundRobin,
                    gain: 1.0,
                    choke: Some(1),
                    max_voices: 4,
                    next: 0,
                },
            ],
        }
    }

    fn render(s: &mut Sampler, b: &SampleBank, n: usize) -> Vec<f32> {
        let mut l = vec![0.0; n];
        let mut r = vec![0.0; n];
        s.process(b, &mut l, &mut r);
        l
    }

    #[test]
    fn velocity_layers_and_round_robin() {
        let mut b = bank();
        let mut s = Sampler::new(8, 48000.0);
        // soft hit → layer 0 (0.1), centre pan = cos(π/4)
        assert!(s.play(&mut b, 0, 0.4, 1.0, 0.0, 0.0));
        let out = render(&mut s, &b, 10);
        let k = 0.4f32 * 0.4 * std::f32::consts::FRAC_1_SQRT_2;
        assert!((out[0] - 0.1 * k).abs() < 1e-6);
        s.stop_all(0);
        // hard hits alternate between 0.2 and 0.3
        let mut firsts = Vec::new();
        for _ in 0..4 {
            s.stop_all(0);
            assert!(s.play(&mut b, 0, 1.0, 1.0, 0.0, 0.0));
            firsts.push((render(&mut s, &b, 1)[0] / std::f32::consts::FRAC_1_SQRT_2 * 10.0).round() as i32);
        }
        // the hard layer alternates between its two samples (the cursor is per sound)
        assert!(firsts.iter().all(|x| *x == 2 || *x == 3), "{firsts:?}");
        assert!(firsts.windows(2).all(|w| w[0] != w[1]), "round robin: {firsts:?}");
    }

    #[test]
    fn polyphony_steals_the_oldest_with_a_fade() {
        let mut b = bank();
        let mut s = Sampler::new(8, 48000.0);
        for _ in 0..3 {
            s.play(&mut b, 0, 1.0, 1.0, 0.0, 0.0);
        }
        assert_eq!(s.active(), 3, "stolen voice still fading");
        render(&mut s, &b, 300);
        assert_eq!(s.active(), 2, "after the 5 ms fade only max_voices remain");
    }

    #[test]
    fn choke_group_fades_other_voices() {
        let mut b = bank();
        let mut s = Sampler::new(8, 48000.0);
        s.play(&mut b, 1, 1.0, 1.0, 0.0, 0.0);
        render(&mut s, &b, 100);
        s.play(&mut b, 2, 1.0, 1.0, 0.0, 0.0);
        let out = render(&mut s, &b, 400);
        // the open hat fades smoothly (no step larger than the fade slope)
        let steps = out.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0, f32::max);
        assert!(steps < 0.02, "choke step {steps}");
        assert_eq!(s.active(), 1);
    }

    #[test]
    fn pitch_changes_playback_rate() {
        let mut b = bank();
        let mut s = Sampler::new(4, 48000.0);
        s.play(&mut b, 1, 1.0, 1.0, 0.0, 12.0);
        render(&mut s, &b, 2399);
        assert_eq!(s.active(), 1);
        render(&mut s, &b, 2);
        assert_eq!(s.active(), 0, "an octave up plays 4800 samples in 2400");
    }
}
