//! Output-stage flash limiter (§9.1, §22). Runs after every other layer, just before encoding.
//!
//! A *flash onset* is a rise of the light level by at least `threshold` (fraction of full
//! scale) above the running low, while that low is below 0.8 (the photosensitivity guideline's
//! "darker state"). Each head keeps the timestamps of its last onsets; when `max_hz` onsets
//! already happened within the last second, a new rise is clamped to just under the threshold
//! above the low, so a head's output never contains more than `floor(max_hz)` onsets in any
//! 1 s window. Rig-wide, an [`OnsetWindow`] gates every head: at most `floor(max_hz)` frames per
//! second may start a flash anywhere (heads flashing in the same frame count once), so a chase
//! across many heads cannot add up to a strobe. The per-head limiter is the last stage and
//! works on what the fixture actually shows, which is what makes the guarantee hold for any
//! input.

/// Most onsets tracked per window (caps `max_hz` at 16).
const RING: usize = 16;
const DARK_LIMIT: f32 = 0.8;
/// The cap counts onsets over 1 s plus a 50 ms guard band, so it still holds at the fixtures
/// when frames reach them with transport/scheduling jitter.
const WINDOW_NS: u64 = 1_050_000_000;

#[derive(Clone, Copy, Debug)]
pub struct Limiter {
    low: f32,
    peak: f32,
    high: bool,
    window: OnsetWindow,
    /// Clamped this frame.
    pub limited: bool,
    /// Total clamped frames.
    pub suppressed: u64,
}

impl Default for Limiter {
    fn default() -> Self {
        Limiter { low: 0.0, peak: 0.0, high: false, window: OnsetWindow::default(), limited: false, suppressed: 0 }
    }
}

/// Timestamps of recent onsets (fixed ring, no allocation).
#[derive(Clone, Copy, Debug)]
pub struct OnsetWindow {
    onsets: [u64; RING],
    head: usize,
    count: usize,
}

impl Default for OnsetWindow {
    fn default() -> Self {
        OnsetWindow { onsets: [0; RING], head: 0, count: 0 }
    }
}

impl OnsetWindow {
    /// Onsets within the last second.
    pub fn recent(&self, now: u64) -> usize {
        (0..self.count).filter(|i| now.saturating_sub(self.onsets[(self.head + RING - 1 - i) % RING]) < WINDOW_NS).count()
    }

    pub fn record(&mut self, now: u64) {
        self.onsets[self.head] = now;
        self.head = (self.head + 1) % RING;
        self.count = (self.count + 1).min(RING);
    }

    /// Room for another onset under `max_hz`.
    pub fn room(&self, now: u64, max_hz: f32) -> bool {
        self.recent(now) < max_onsets(max_hz)
    }
}

fn max_onsets(max_hz: f32) -> usize {
    (max_hz.max(0.0).floor() as usize).min(RING)
}

impl Limiter {
    /// Feed the level this frame wants to show (0..1); returns the level allowed.
    pub fn process(&mut self, level: f32, now: u64, max_hz: f32, threshold: f32) -> f32 {
        self.process_gated(level, now, max_hz, threshold, true).0
    }

    /// Like [`Limiter::process`], but a new onset also needs `gate` (rig-wide room). Returns
    /// the allowed level and whether an onset started this frame.
    pub fn process_gated(&mut self, level: f32, now: u64, max_hz: f32, threshold: f32, gate: bool) -> (f32, bool) {
        let level = level.clamp(0.0, 1.0);
        self.limited = false;
        let mut out = level;
        if self.high {
            self.peak = self.peak.max(level);
            if self.peak - level >= threshold {
                // fell back: a new dark phase starts here
                self.high = false;
                self.low = level;
            }
            return (out, false);
        }
        self.low = self.low.min(level);
        if level - self.low >= threshold && self.low < DARK_LIMIT {
            if gate && self.window.room(now, max_hz) {
                self.window.record(now);
                self.high = true;
                self.peak = level;
                return (out, true);
            }
            // over the cap: stay under the flash threshold
            out = self.low + threshold * 0.95;
            self.limited = true;
            self.suppressed += 1;
        }
        (out, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME_NS: u64 = 22_727_273; // 44 Hz

    /// Independent onset counter over the *output* with the same definition, used to check
    /// the guarantee: max onsets in any 1 s window.
    fn max_onsets_per_second(levels: &[(u64, f32)], threshold: f32) -> usize {
        let mut onsets = Vec::new();
        // start dark at level 0, like the limiter (fixtures power up dark)
        let (mut low, mut peak, mut high) = (0.0f32, 0.0f32, false);
        for &(t, l) in levels {
            if high {
                peak = peak.max(l);
                if peak - l >= threshold {
                    high = false;
                    low = l;
                }
            } else {
                low = low.min(l);
                if l - low >= threshold && low < DARK_LIMIT {
                    onsets.push(t);
                    high = true;
                    peak = l;
                }
            }
        }
        onsets.iter().map(|t0| onsets.iter().filter(|t| **t >= *t0 && **t - *t0 < 1_000_000_000).count()).max().unwrap_or(0)
    }

    fn run(input: impl Fn(u64, usize) -> f32, frames: usize, max_hz: f32) -> (Vec<(u64, f32)>, Vec<(u64, f32)>) {
        let mut l = Limiter::default();
        let mut inp = Vec::new();
        let mut out = Vec::new();
        for i in 0..frames {
            let t = i as u64 * FRAME_NS;
            let v = input(t, i);
            inp.push((t, v));
            out.push((t, l.process(v, t, max_hz, 0.2)));
        }
        (inp, out)
    }

    #[test]
    fn caps_a_10hz_strobe_to_3_flashes_per_second() {
        // 10 Hz full-scale square wave for 10 s
        let (inp, out) = run(|t, _| if (t / 50_000_000) % 2 == 0 { 1.0 } else { 0.0 }, 440, 3.0);
        assert!(max_onsets_per_second(&inp, 0.2) >= 9, "input really strobes");
        assert_eq!(max_onsets_per_second(&out, 0.2), 3, "output capped at 3 onsets in any 1 s window");
        // and it still flashes (not just blacked out): ~3/s over 10 s
        let total = {
            let (mut n, mut high, mut low, mut peak) = (0, false, 0.0f32, 0.0f32);
            for &(_, l) in &out {
                if high {
                    peak = peak.max(l);
                    if peak - l >= 0.2 {
                        high = false;
                        low = l;
                    }
                } else {
                    low = low.min(l);
                    if l - low >= 0.2 && low < 0.8 {
                        n += 1;
                        high = true;
                        peak = l;
                    }
                }
            }
            n
        };
        assert!((27..=31).contains(&total), "{total} flashes in 10 s");
    }

    #[test]
    fn slow_changes_and_fades_pass_untouched() {
        // 1 Hz blink and a 2 s fade: below the cap, output == input
        let (inp, out) = run(|t, _| if (t / 500_000_000) % 2 == 0 { 1.0 } else { 0.0 }, 300, 3.0);
        assert_eq!(inp, out);
        let (inp, out) = run(|t, _| (t as f32 / 2e9).min(1.0), 100, 3.0);
        assert_eq!(inp, out);
    }

    #[test]
    fn property_any_input_respects_the_cap() {
        // deterministic pseudo-random inputs: noise, bursts, and mixed rates
        for seed in 1..200u64 {
            let mut s = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
            let mut rnd = move || {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 40) as f32 / (1u64 << 24) as f32
            };
            let levels: Vec<f32> = (0..600).map(|_| rnd()).collect();
            for max_hz in [1.0f32, 2.0, 3.0, 5.0] {
                let (_, out) = run(|_, i| levels[i], levels.len(), max_hz);
                let n = max_onsets_per_second(&out, 0.2);
                assert!(n <= max_hz as usize, "seed {seed} cap {max_hz}: {n} onsets in 1 s");
            }
        }
    }

    #[test]
    fn bright_base_flicker_is_not_a_flash() {
        // flicker between 0.85 and 1.0 (darker state above 0.8) is not counted
        let (inp, out) = run(|_, i| if i % 2 == 0 { 1.0 } else { 0.85 }, 200, 3.0);
        assert_eq!(inp, out);
    }
}
