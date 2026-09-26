//! Fixed-capacity running statistics (no allocation after construction).

/// Median over the last `capacity` pushed values. `push` is O(capacity) (binary search + shift in
/// a sorted copy); nothing allocates after `new`.
#[derive(Clone, Debug)]
pub(crate) struct SlidingMedian {
    ring: Vec<f32>,
    sorted: Vec<f32>,
    pos: usize,
    cap: usize,
}

impl SlidingMedian {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        SlidingMedian { ring: Vec::with_capacity(cap), sorted: Vec::with_capacity(cap), pos: 0, cap }
    }

    pub fn push(&mut self, v: f32) {
        let v = if v.is_finite() { v } else { 0.0 };
        if self.ring.len() < self.cap {
            self.ring.push(v);
        } else {
            let old = std::mem::replace(&mut self.ring[self.pos], v);
            let i = self.sorted.partition_point(|&x| x < old);
            // `old` is present: it was inserted when pushed and never removed since.
            self.sorted.remove(i.min(self.sorted.len() - 1));
            self.pos = (self.pos + 1) % self.cap;
        }
        let j = self.sorted.partition_point(|&x| x < v);
        self.sorted.insert(j, v);
    }

    /// Median of the window (0 when empty).
    pub fn median(&self) -> f32 {
        if self.sorted.is_empty() { 0.0 } else { self.sorted[self.sorted.len() / 2] }
    }

    pub fn clear(&mut self) {
        self.ring.clear();
        self.sorted.clear();
        self.pos = 0;
    }
}

/// Fixed-length ring of samples with an O(n) percentile query through a preallocated scratch.
#[derive(Clone, Debug)]
pub(crate) struct PercentileRing {
    ring: Vec<f32>,
    scratch: Vec<f32>,
    pos: usize,
    cap: usize,
}

impl PercentileRing {
    pub fn new(capacity: usize) -> Self {
        let cap = capacity.max(1);
        PercentileRing { ring: Vec::with_capacity(cap), scratch: Vec::with_capacity(cap), pos: 0, cap }
    }

    pub fn push(&mut self, v: f32) {
        if self.ring.len() < self.cap {
            self.ring.push(v);
        } else {
            self.ring[self.pos] = v;
            self.pos = (self.pos + 1) % self.cap;
        }
    }

    pub fn len(&self) -> usize {
        self.ring.len()
    }

    /// `q`-quantile (0–1, nearest rank) of the stored values; `None` when empty.
    pub fn percentile(&mut self, q: f32) -> Option<f32> {
        if self.ring.is_empty() {
            return None;
        }
        self.scratch.clear();
        self.scratch.extend_from_slice(&self.ring);
        let k = ((self.scratch.len() - 1) as f32 * q.clamp(0.0, 1.0)).round() as usize;
        let (_, v, _) = self.scratch.select_nth_unstable_by(k, f32::total_cmp);
        Some(*v)
    }

    pub fn clear(&mut self) {
        self.ring.clear();
        self.pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sliding_median_tracks_window_exactly() {
        let mut m = SlidingMedian::new(5);
        let data = [5.0, 1.0, 9.0, 3.0, 7.0, 2.0, 2.0, 8.0, -1.0, 4.0, 4.0, 4.0];
        for (i, &v) in data.iter().enumerate() {
            m.push(v);
            let lo = (i + 1).saturating_sub(5);
            let mut w: Vec<f32> = data[lo..=i].to_vec();
            w.sort_by(f32::total_cmp);
            assert_eq!(m.median(), w[w.len() / 2], "step {i}");
        }
    }

    #[test]
    fn percentile_ring_nearest_rank() {
        let mut p = PercentileRing::new(4);
        assert_eq!(p.percentile(0.5), None);
        for v in [10.0, 0.0, 30.0, 20.0, 40.0] {
            p.push(v);
        }
        // Window is [40, 0, 30, 20] after wrap.
        assert_eq!(p.percentile(0.0), Some(0.0));
        assert_eq!(p.percentile(1.0), Some(40.0));
        assert_eq!(p.percentile(0.5), Some(30.0));
    }
}
