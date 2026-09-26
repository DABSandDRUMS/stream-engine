//! Deterministic PRNG (SplitMix64) so replays reproduce random choices.

#[derive(Clone, Debug)]
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Rng {
        Rng(seed ^ 0x2545F4914F6CDD1D)
    }
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }
    pub fn f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 { 0 } else { self.next_u64() % n }
    }
    pub fn range(&mut self, lo: u32, hi: u32) -> u32 {
        if hi <= lo { lo } else { lo + self.below((hi - lo + 1) as u64) as u32 }
    }
    pub fn state(&self) -> u64 {
        self.0
    }
    pub fn set_state(&mut self, s: u64) {
        self.0 = s;
    }
}
