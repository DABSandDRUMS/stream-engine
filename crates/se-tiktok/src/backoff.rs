//! Capped exponential backoff with "equal jitter" (each delay lies in `[d/2, d]` where `d`
//! doubles from `base` up to `cap`), plus the tiny PRNG it uses.

use std::time::Duration;

/// xorshift64* — jitter only, not for anything security-relevant.
#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }

    /// Seeded from the wall clock and the process id.
    pub fn from_time() -> Self {
        let t = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
        Rng::new(t ^ (u64::from(std::process::id()) << 32))
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `[0, 1)`.
    pub fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// A random positive `i64` (≥ 1).
    pub fn positive_i64(&mut self) -> i64 {
        ((self.next_u64() >> 1) as i64).max(1)
    }
}

#[derive(Clone, Debug)]
pub struct Backoff {
    base: Duration,
    cap: Duration,
    attempt: u32,
    rng: Rng,
}

impl Backoff {
    pub fn new(base: Duration, cap: Duration, rng: Rng) -> Self {
        Backoff { base, cap: cap.max(base), attempt: 0, rng }
    }

    /// The upper bound of the next delay (before jitter).
    pub fn ceiling(&self) -> Duration {
        let factor = 1u32.checked_shl(self.attempt.min(30)).unwrap_or(u32::MAX);
        self.base.saturating_mul(factor).min(self.cap)
    }

    pub fn next_delay(&mut self) -> Duration {
        let d = self.ceiling();
        self.attempt = self.attempt.saturating_add(1);
        d.mul_f64(0.5 + 0.5 * self.rng.unit())
    }

    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_within_jitter_bounds_and_stay_capped() {
        let base = Duration::from_secs(2);
        let cap = Duration::from_secs(300);
        let mut b = Backoff::new(base, cap, Rng::new(7));
        let mut ceilings = Vec::new();
        for _ in 0..40 {
            let ceil = b.ceiling();
            let d = b.next_delay();
            assert!(d >= ceil / 2 && d <= ceil, "{d:?} outside [{:?}, {ceil:?}]", ceil / 2);
            assert!(d <= cap);
            ceilings.push(ceil.as_secs());
        }
        assert_eq!(&ceilings[..9], &[2, 4, 8, 16, 32, 64, 128, 256, 300]);
        assert!(ceilings[9..].iter().all(|c| *c == 300));
    }

    #[test]
    fn jitter_spreads_delays() {
        let mut b = Backoff::new(Duration::from_secs(10), Duration::from_secs(10), Rng::new(99));
        let ds: std::collections::HashSet<u128> = (0..50).map(|_| b.next_delay().as_millis()).collect();
        assert!(ds.len() > 40, "jitter should vary delays, got {} distinct", ds.len());
    }

    #[test]
    fn reset_starts_over() {
        let mut b = Backoff::new(Duration::from_secs(1), Duration::from_secs(60), Rng::new(3));
        for _ in 0..5 {
            b.next_delay();
        }
        assert_eq!(b.ceiling(), Duration::from_secs(32));
        b.reset();
        assert_eq!(b.ceiling(), Duration::from_secs(1));
    }
}
