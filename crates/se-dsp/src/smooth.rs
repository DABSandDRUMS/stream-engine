//! Parameter smoothing: linear ramps with a fixed duration, so every change (gain, wet,
//! bypass, envelopes) lands exactly after `ramp` samples without zipper noise or clicks.

#[derive(Clone, Copy, Debug)]
pub struct Smoother {
    cur: f32,
    target: f32,
    step: f32,
    left: u32,
    ramp: u32,
}

impl Smoother {
    /// Starts settled at `value`; changes ramp over `ramp_samples`.
    pub fn new(value: f32, ramp_samples: u32) -> Smoother {
        Smoother { cur: value, target: value, step: 0.0, left: 0, ramp: ramp_samples.max(1) }
    }

    /// Ramp length from milliseconds at `sr`.
    pub fn with_ms(value: f32, ms: f32, sr: f32) -> Smoother {
        Smoother::new(value, (ms * 0.001 * sr).round().max(1.0) as u32)
    }

    pub fn set_ramp(&mut self, ramp_samples: u32) {
        self.ramp = ramp_samples.max(1);
    }

    /// Move towards `v` over the ramp length (restarts the ramp from the current value).
    #[inline]
    pub fn set(&mut self, v: f32) {
        if v == self.target {
            return;
        }
        self.target = v;
        self.left = self.ramp;
        self.step = (v - self.cur) / self.ramp as f32;
    }

    /// Jump to `v` immediately.
    #[inline]
    pub fn reset(&mut self, v: f32) {
        self.cur = v;
        self.target = v;
        self.left = 0;
        self.step = 0.0;
    }

    /// Next per-sample value.
    #[allow(clippy::should_implement_trait)]
    #[inline]
    pub fn next(&mut self) -> f32 {
        if self.left > 0 {
            self.left -= 1;
            if self.left == 0 {
                self.cur = self.target;
            } else {
                self.cur += self.step;
            }
        }
        self.cur
    }

    /// Advance `n` samples at once (for block-rate consumers).
    #[inline]
    pub fn skip(&mut self, n: u32) -> f32 {
        if self.left > 0 {
            if n >= self.left {
                self.left = 0;
                self.cur = self.target;
            } else {
                self.left -= n;
                self.cur += self.step * n as f32;
            }
        }
        self.cur
    }

    #[inline]
    pub fn value(&self) -> f32 {
        self.cur
    }

    #[inline]
    pub fn target(&self) -> f32 {
        self.target
    }

    #[inline]
    pub fn settled(&self) -> bool {
        self.left == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ramps_linearly_and_lands_exactly() {
        let mut s = Smoother::new(0.0, 4);
        s.set(1.0);
        let v: Vec<f32> = (0..6).map(|_| s.next()).collect();
        assert_eq!(v, vec![0.25, 0.5, 0.75, 1.0, 1.0, 1.0]);
        assert!(s.settled());
    }

    #[test]
    fn retarget_mid_ramp_continues_from_current() {
        let mut s = Smoother::new(0.0, 4);
        s.set(1.0);
        s.next();
        s.next();
        s.set(0.0);
        let first = s.next();
        assert!((first - 0.375).abs() < 1e-6, "{first}");
        assert_eq!(s.skip(10), 0.0);
    }
}
