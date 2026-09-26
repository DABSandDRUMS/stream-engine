//! Voice activity on the mic (PLAN §8.3): a small neural voice detector (Earshot, 40 KiB
//! network, 16 kHz / 16 ms frames) fed from the analysis stream at any sample rate.
//!
//! The input is mixed to mono, low-passed below 8 kHz (4th-order Butterworth), and resampled to
//! 16 kHz by linear interpolation; every full 256-sample frame updates the score. Speech from any
//! language scores high; silence and broadband noise score low. Steady pitched tones can score
//! near the threshold, so callers also gate on level.

use crate::filters::Biquad;

/// Detector input rate and frame size (fixed by the model).
pub const VAD_RATE: f32 = 16_000.0;
const FRAME: usize = 256;

/// Default score above which the frame counts as speech.
pub const VAD_THRESHOLD: f32 = 0.5;

pub struct Vad {
    det: Box<earshot::Detector>,
    lp: [Biquad; 2],
    /// Input samples per output sample (`rate / 16 kHz`).
    step: f64,
    /// Position of the next output sample, in input samples after `prev`.
    pos: f64,
    prev: f32,
    frame: Vec<f32>,
    score: f32,
    frames: u64,
}

impl Vad {
    pub fn new(sample_rate: f32) -> Vad {
        let sr = sample_rate as f64;
        let cutoff = 7_200.0f64.min(sr * 0.45);
        Vad {
            det: earshot::Detector::default_boxed(),
            // Butterworth 4th order = two biquads with Q 0.541 and 1.307
            lp: [Biquad::lowpass(cutoff, sr, 0.541_196), Biquad::lowpass(cutoff, sr, 1.306_563)],
            step: sample_rate as f64 / VAD_RATE as f64,
            pos: 0.0,
            prev: 0.0,
            frame: Vec::with_capacity(FRAME),
            score: 0.0,
            frames: 0,
        }
    }

    /// Feed a stereo chunk; returns true when at least one frame was scored.
    pub fn push(&mut self, l: &[f32], r: &[f32]) -> bool {
        let mut scored = false;
        for (&a, &b) in l.iter().zip(r) {
            let [lp0, lp1] = &mut self.lp;
            let x = lp1.process(lp0.process(0.5 * (a + b) as f64)) as f32;
            // output samples that fall between `prev` (t = 0) and `x` (t = 1)
            while self.pos <= 1.0 {
                let t = self.pos as f32;
                self.frame.push((self.prev + (x - self.prev) * t).clamp(-1.0, 1.0));
                self.pos += self.step;
                if self.frame.len() == FRAME {
                    self.score = self.det.predict_f32(&self.frame);
                    self.frame.clear();
                    self.frames += 1;
                    scored = true;
                }
            }
            self.pos -= 1.0;
            self.prev = x;
        }
        scored
    }

    /// Latest speech score, 0 (no voice) … 1 (voice).
    pub fn score(&self) -> f32 {
        self.score
    }

    /// Frames scored so far.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    pub fn reset(&mut self) {
        self.det.reset();
        self.lp.iter_mut().for_each(Biquad::reset);
        self.pos = 0.0;
        self.prev = 0.0;
        self.frame.clear();
        self.score = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(v: &mut Vad, rate: f32, secs: f32, mut f: impl FnMut(usize) -> f32) -> Vec<f32> {
        let n = (rate * secs) as usize;
        let mut scores = Vec::new();
        for chunk in (0..n).collect::<Vec<_>>().chunks(480) {
            let x: Vec<f32> = chunk.iter().map(|&i| f(i)).collect();
            if v.push(&x, &x) {
                scores.push(v.score());
            }
        }
        scores
    }

    #[test]
    fn frames_arrive_every_16_ms_at_any_input_rate() {
        for rate in [48_000.0, 44_100.0, 16_000.0] {
            let mut v = Vad::new(rate);
            run(&mut v, rate, 2.0, |_| 0.0);
            // 2 s at 16 kHz = 32000 samples = 125 frames (±1 for the interpolation edge)
            assert!((124..=125).contains(&v.frames()), "{rate} Hz: {} frames", v.frames());
        }
    }

    #[test]
    fn silence_and_noise_are_not_speech() {
        let rate = 48_000.0;
        let mut v = Vad::new(rate);
        let s = run(&mut v, rate, 1.0, |_| 0.0);
        assert!(s.iter().all(|&x| x < VAD_THRESHOLD), "silence: {s:?}");
        v.reset();
        let mut seed = 0x1234_5678u32;
        let s = run(&mut v, rate, 2.0, |_| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            (seed as f32 / u32::MAX as f32 - 0.5) * 0.2
        });
        let late = &s[s.len() / 2..];
        assert!(late.iter().all(|&x| x < VAD_THRESHOLD), "noise: {late:?}");
    }

    #[test]
    fn content_above_8_khz_is_removed_before_resampling() {
        // a 12 kHz tone would alias to 4 kHz at 16 kHz; the low-pass must kill it first
        let rate = 48_000.0;
        let mut v = Vad::new(rate);
        let mut peak = 0.0f32;
        let n = 48_000;
        for i in 0..n {
            let x = (std::f32::consts::TAU * 12_000.0 * i as f32 / rate).sin();
            let [lp0, lp1] = &mut v.lp;
            let y = lp1.process(lp0.process(x as f64)) as f32;
            if i > n / 2 {
                peak = peak.max(y.abs());
            }
        }
        assert!(peak < 0.1, "12 kHz after the low-pass: {peak}");
    }

    /// Speech from espeak-ng scores as voice (needs the `espeak-ng` binary).
    #[test]
    #[ignore]
    fn spoken_words_are_speech() {
        let out = std::process::Command::new("espeak-ng")
            .args(["--stdout", "-s", "150", "hello everyone, thanks for the follow and welcome to the stream"])
            .output()
            .expect("espeak-ng");
        // 22050 Hz mono 16-bit WAV with a 44-byte header
        let pcm: Vec<f32> = out.stdout[44..].as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as f32 / 32768.0).collect();
        let mut v = Vad::new(22_050.0);
        let mut scores = Vec::new();
        for c in pcm.chunks(441) {
            if v.push(c, c) {
                scores.push(v.score());
            }
        }
        let voiced = scores.iter().filter(|&&s| s >= VAD_THRESHOLD).count();
        assert!(voiced * 3 > scores.len(), "{voiced} of {} frames voiced", scores.len());
    }
}
