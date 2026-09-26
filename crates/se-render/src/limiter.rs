//! Video flash limiter (§22). The GPU reduces each output canvas to an 8×8 grid of mean
//! relative luminance; this detector counts general flashes per cell (a flash = a pair of
//! opposing luminance changes of ≥ 10 % where the darker state is below 0.8, per WCAG 2.x) over
//! a sliding one-second window. When enough of the screen flashes faster than the configured
//! rate, the limiter engages: flashy effects are scaled down and the output pass caps the
//! per-frame luminance step, so the result stays under the threshold.

pub const CELLS: usize = 64;
const WINDOW_NS: u64 = 1_000_000_000;
const MAX_EVENTS: usize = 64;
/// Relative luminance change that counts as a transition.
const DELTA: f32 = 0.1;
/// The darker state must be below this for a transition to count.
const DARK: f32 = 0.8;

#[derive(Clone, Copy)]
struct Cell {
    extreme: f32,
    /// +1 rising, -1 falling, 0 unknown.
    dir: i8,
    initialized: bool,
    events: [u64; MAX_EVENTS],
    head: usize,
    len: usize,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { extreme: 0.0, dir: 0, initialized: false, events: [0; MAX_EVENTS], head: 0, len: 0 }
    }
}

impl Cell {
    fn push(&mut self, t: u64) {
        self.events[(self.head + self.len) % MAX_EVENTS] = t;
        if self.len < MAX_EVENTS {
            self.len += 1;
        } else {
            self.head = (self.head + 1) % MAX_EVENTS;
        }
    }

    fn expire(&mut self, now: u64) {
        while self.len > 0 && now.saturating_sub(self.events[self.head]) > WINDOW_NS {
            self.head = (self.head + 1) % MAX_EVENTS;
            self.len -= 1;
        }
    }

    fn observe(&mut self, l: f32, t: u64) {
        if !self.initialized {
            self.initialized = true;
            self.extreme = l;
            return;
        }
        let d = l - self.extreme;
        let dir: i8 = if d > 0.0 { 1 } else { -1 };
        if dir == self.dir || self.dir == 0 && d.abs() < DELTA {
            // continuing in the same direction extends the extreme
            if (dir > 0 && l > self.extreme) || (dir < 0 && l < self.extreme) {
                self.extreme = l;
            }
            return;
        }
        if d.abs() >= DELTA && l.min(self.extreme) < DARK {
            self.push(t);
            self.dir = dir;
            self.extreme = l;
        }
    }

    /// Flashes (pairs of opposing transitions) per second.
    fn rate(&self) -> f32 {
        self.len as f32 / 2.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LimiterState {
    /// 0..1: how hard the limiter is engaged.
    pub limit: f32,
    /// Highest per-cell flash rate (flashes/s) in the window.
    pub max_rate: f32,
    /// Fraction of the screen flashing above the threshold.
    pub area: f32,
    pub violating: bool,
}

pub struct FlashDetector {
    cells: Box<[Cell; CELLS]>,
    pub max_flashes: f32,
    /// Screen fraction that must flash above the threshold to engage.
    pub area_threshold: f32,
    limit: f32,
    last: u64,
    state: LimiterState,
}

impl FlashDetector {
    pub fn new(max_flashes: f32) -> FlashDetector {
        FlashDetector {
            cells: Box::new([Cell::default(); CELLS]),
            max_flashes,
            area_threshold: 0.1,
            limit: 0.0,
            last: 0,
            state: LimiterState { limit: 0.0, max_rate: 0.0, area: 0.0, violating: false },
        }
    }

    /// Feed one frame of cell luminances measured at `t` (master clock ns).
    pub fn observe(&mut self, lum: &[f32; CELLS], t: u64) -> LimiterState {
        let mut max_rate = 0.0f32;
        let mut over = 0usize;
        let mut near = 0usize;
        for (c, &l) in self.cells.iter_mut().zip(lum.iter()) {
            c.expire(t);
            c.observe(if l.is_finite() { l.clamp(0.0, 1.0) } else { 0.0 }, t);
            let r = c.rate();
            max_rate = max_rate.max(r);
            if r > self.max_flashes {
                over += 1;
            }
            if r > self.max_flashes - 1.0 {
                near += 1;
            }
        }
        let area = over as f32 / CELLS as f32;
        let violating = area >= self.area_threshold;
        let approaching = near as f32 / CELLS as f32 >= self.area_threshold;
        let target = if violating {
            1.0
        } else if approaching {
            0.6
        } else {
            0.0
        };
        let dt = if self.last == 0 { 0.0 } else { (t.saturating_sub(self.last)) as f32 / 1e9 };
        self.last = t;
        // fast attack (~80 ms), slow release (~2 s)
        let rate = if target > self.limit { dt / 0.08 } else { dt / 2.0 };
        self.limit += (target - self.limit) * rate.clamp(0.0, 1.0);
        if target > 0.0 && self.limit < target * 0.5 {
            // never let a detected violation through at low strength
            self.limit = target * 0.5;
        }
        self.state = LimiterState { limit: self.limit, max_rate, area, violating };
        self.state
    }

    pub fn state(&self) -> LimiterState {
        self.state
    }

    /// Scale for flashy effect strengths.
    pub fn effect_scale(&self) -> f32 {
        1.0 - self.limit
    }

    /// Max per-frame luminance step for the output pass (1 = unlimited).
    pub fn max_step(&self) -> f32 {
        if self.limit <= 0.001 { 1.0 } else { 1.0 + (0.04 - 1.0) * self.limit }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: u64 = 16_666_667;

    fn run(det: &mut FlashDetector, seconds: f32, mut lum: impl FnMut(u64) -> [f32; CELLS]) -> LimiterState {
        let frames = (seconds * 60.0) as u64;
        let start = det.last;
        let mut s = det.state();
        for f in 1..=frames {
            s = det.observe(&lum(f), start + f * FRAME);
        }
        s
    }

    #[test]
    fn steady_and_slow_changes_do_not_engage() {
        let mut d = FlashDetector::new(3.0);
        let s = run(&mut d, 3.0, |_| [0.5; CELLS]);
        assert!(!s.violating && s.limit == 0.0);
        // a slow fade from black to white and a single hard cut
        let s = run(&mut d, 2.0, |f| [(f as f32 / 120.0).min(1.0); CELLS]);
        assert!(!s.violating, "{s:?}");
        let s = run(&mut d, 1.0, |_| [1.0; CELLS]);
        assert!(!s.violating, "{s:?}");
        let s = run(&mut d, 1.0, |f| if f < 30 { [0.0; CELLS] } else { [1.0; CELLS] });
        assert!(!s.violating && s.max_rate <= 1.0, "{s:?}");
    }

    #[test]
    fn full_screen_strobe_engages_and_releases() {
        let mut d = FlashDetector::new(3.0);
        // 10 Hz black/white strobe (6 frames per half period... 3 frames on, 3 off)
        let s = run(&mut d, 1.5, |f| if (f / 3) % 2 == 0 { [0.0; CELLS] } else { [1.0; CELLS] });
        assert!(s.violating, "{s:?}");
        assert!(s.limit > 0.9);
        assert!(d.max_step() < 0.1);
        assert!(d.effect_scale() < 0.1);
        let s = run(&mut d, 6.0, |_| [0.3; CELLS]);
        assert!(!s.violating && s.limit < 0.1, "{s:?}");
    }

    #[test]
    fn small_area_flicker_is_tolerated() {
        let mut d = FlashDetector::new(3.0);
        let s = run(&mut d, 2.0, |f| {
            let mut l = [0.2; CELLS];
            let v = if (f / 3) % 2 == 0 { 0.0 } else { 1.0 };
            l[0] = v;
            l[1] = v;
            l
        });
        assert!(!s.violating, "2 of 64 cells: {s:?}");
        assert!(s.max_rate > 3.0);
    }

    #[test]
    fn bright_flicker_above_dark_threshold_is_not_a_flash() {
        let mut d = FlashDetector::new(3.0);
        let s = run(&mut d, 2.0, |f| if (f / 3) % 2 == 0 { [0.85; CELLS] } else { [1.0; CELLS] });
        assert!(!s.violating, "{s:?}");
    }

    #[test]
    fn detector_does_not_allocate() {
        let mut d = FlashDetector::new(3.0);
        let s = se_alloc::Scope::begin();
        run(&mut d, 1.0, |f| if f % 2 == 0 { [0.0; CELLS] } else { [1.0; CELLS] });
        assert_eq!(s.allocs(), 0);
    }
}
