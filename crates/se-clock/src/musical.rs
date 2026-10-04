//! Allocation-free musical timing servo shared by live audio and lighting fallback.
//! Observations are measurements, not clock resets: tempo and phase converge without
//! discontinuities, while silence and expired measurements keep the last trusted tempo.

const FRESH_NS: u64 = 250_000_000;
const LOCK: f32 = 0.6;
const UNLOCK: f32 = 0.35;
const MAX_TAPS: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    Fallback,
    Live,
    Manual,
    Tap,
}

impl Source {
    pub fn name(self) -> &'static str {
        match self {
            Self::Fallback => "fallback",
            Self::Live => "live",
            Self::Manual => "manual",
            Self::Tap => "tap",
        }
    }
    pub fn code(self) -> f32 {
        match self {
            Self::Fallback => 0.0,
            Self::Live => 1.0,
            Self::Manual => 2.0,
            Self::Tap => 3.0,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Observation {
    pub ts: u64,
    pub bpm: f32,
    pub phase: f32,
    pub confidence: f32,
}

#[derive(Clone, Copy, Debug)]
pub struct Snapshot {
    pub ts: u64,
    pub bpm: f32,
    pub position: f64,
    pub phase: f32,
    pub confidence: f32,
    pub source: Source,
    pub locked: bool,
}

#[derive(Clone, Debug)]
pub struct MusicalClock {
    ts: u64,
    position: f64,
    bpm: f64,
    target_bpm: f64,
    correction: f64,
    observation: Option<Observation>,
    locked: bool,
    override_source: Option<Source>,
    taps: [u64; MAX_TAPS],
    n_taps: usize,
}

impl MusicalClock {
    pub fn new(ts: u64) -> Self {
        Self {
            ts, position: 0.0, bpm: 120.0, target_bpm: 120.0,
            correction: 0.0, observation: None, locked: false,
            override_source: None, taps: [0; MAX_TAPS], n_taps: 0,
        }
    }

    pub fn valid_bpm(bpm: f32) -> bool {
        bpm.is_finite() && (20.0..=400.0).contains(&bpm)
    }

    fn trusted(&self, ts: u64) -> bool {
        self.locked && self.observation.is_some_and(|o| ts.saturating_sub(o.ts) <= FRESH_NS)
    }

    /// Advances only forward in master-clock time. Repeated queries do not re-anchor phase.
    pub fn advance(&mut self, ts: u64) -> Snapshot {
        if ts > self.ts {
            let dt = (ts - self.ts) as f64 * 1e-9;
            let active_dt = self.observation.filter(|_| self.locked)
                .map_or(0.0, |o| (o.ts.saturating_add(FRESH_NS).min(ts).saturating_sub(self.ts)) as f64 * 1e-9);
            if self.override_source.is_none() && active_dt > 0.0 {
                // Two-second tempo servo; phase correction is at most 20% of beat rate.
                let delta = self.target_bpm - self.bpm;
                let integral = if delta == 0.0 {
                    self.bpm * active_dt
                } else {
                    let decay = (-active_dt / 2.0).exp();
                    let integral = self.target_bpm * active_dt + delta * 2.0 * (decay - 1.0);
                    self.bpm += delta * (1.0 - decay);
                    integral
                };
                let correction = self.correction.clamp(-self.bpm / 300.0, self.bpm / 300.0);
                self.position += integral / 60.0 + active_dt * correction;
                self.position += (dt - active_dt) * self.bpm / 60.0;
            } else {
                self.position += dt * self.bpm / 60.0;
                if self.override_source.is_some() && self.correction != 0.0 {
                    // Manual/tap phase alignment also slews, never jumps backwards.
                    let correction_time = dt.min(0.25);
                    self.position += correction_time * self.correction;
                    self.correction *= (-dt / 0.5).exp();
                }
            }
            self.ts = ts;
        }
        self.snapshot()
    }

    pub fn snapshot(&self) -> Snapshot {
        let trusted = self.trusted(self.ts);
        let source = self.override_source.unwrap_or(if trusted { Source::Live } else { Source::Fallback });
        Snapshot {
            ts: self.ts, bpm: self.bpm as f32, position: self.position,
            phase: (self.position.rem_euclid(1.0) as f32).min(1.0 - f32::EPSILON),
            confidence: if self.override_source.is_some() { 1.0 } else if trusted { self.observation.unwrap().confidence } else { 0.0 },
            source, locked: self.override_source.is_some() || trusted,
        }
    }

    /// Invalid, out-of-order and uncertain observations cannot steer the clock.
    pub fn observe(&mut self, o: Observation) {
        if o.ts < self.ts || !Self::valid_bpm(o.bpm) || !o.phase.is_finite() || !o.confidence.is_finite() {
            return;
        }
        self.advance(o.ts);
        self.locked = o.confidence >= if self.trusted(o.ts) { UNLOCK } else { LOCK };
        self.observation = Some(Observation { confidence: o.confidence.clamp(0.0, 1.0), ..o });
        if self.override_source.is_none() && self.locked {
            self.target_bpm = o.bpm as f64;
            let error = (o.phase as f64 - self.position.rem_euclid(1.0) + 0.5).rem_euclid(1.0) - 0.5;
            self.correction = (error * 0.6).clamp(-self.bpm / 300.0, self.bpm / 300.0);
        } else if self.override_source.is_none() {
            self.correction = 0.0;
        }
    }

    /// Adopt an authoritative continuous clock publication, not a raw audio measurement.
    /// The publisher already smoothed its tempo/phase. Delayed or reacquired samples may
    /// not move this follower backwards; expired publications freewheel in `advance`.
    pub fn follow(&mut self, ts: u64, bpm: f32, position: f64, confidence: f32) {
        if ts < self.ts || !Self::valid_bpm(bpm) || !position.is_finite() || position < 0.0 || !confidence.is_finite() {
            return;
        }
        self.advance(ts);
        self.position = self.position.max(position);
        self.bpm = bpm as f64;
        self.target_bpm = self.bpm;
        self.correction = 0.0;
        self.override_source = None;
        self.locked = confidence >= if self.trusted(ts) { UNLOCK } else { LOCK };
        self.observation = Some(Observation { ts, bpm, phase: position.rem_euclid(1.0) as f32, confidence: confidence.clamp(0.0, 1.0) });
    }

    /// A stopped, replaced, silent or disconnected source relinquishes lock immediately.
    pub fn invalidate_source(&mut self, ts: u64) {
        self.advance(ts);
        self.observation = None;
        self.locked = false;
        if self.override_source.is_none() {
            self.correction = 0.0;
        }
    }

    /// Explicit tempo is immediate, but preserves continuous beat position.
    pub fn set_bpm(&mut self, ts: u64, bpm: f32) -> bool {
        if !Self::valid_bpm(bpm) { return false; }
        self.advance(ts);
        self.bpm = bpm as f64;
        self.target_bpm = self.bpm;
        self.override_source = Some(Source::Manual);
        self.correction = 0.0;
        self.n_taps = 0;
        true
    }

    /// Two or more taps within two seconds override both automatic and explicit tempo.
    pub fn tap(&mut self, ts: u64) {
        if self.n_taps > 0 && ts <= self.taps[self.n_taps - 1] { return; }
        self.advance(ts);
        if self.n_taps > 0 && ts - self.taps[self.n_taps - 1] > 2_000_000_000 {
            self.n_taps = 0;
        }
        if self.n_taps == MAX_TAPS {
            self.taps.copy_within(1.., 0);
            self.n_taps -= 1;
        }
        self.taps[self.n_taps] = ts;
        self.n_taps += 1;
        if self.n_taps < 2 { return; }
        let n = self.n_taps as f64;
        let t0 = self.taps[0];
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
        for (i, &t) in self.taps[..self.n_taps].iter().enumerate() {
            let (x, y) = (i as f64, (t - t0) as f64);
            sx += x; sy += y; sxx += x * x; sxy += x * y;
        }
        let period = (n * sxy - sx * sy) / (n * sxx - sx * sx);
        let bpm = (60e9 / period) as f32;
        if !Self::valid_bpm(bpm) { return; }
        self.bpm = bpm as f64;
        self.target_bpm = self.bpm;
        self.override_source = Some(Source::Tap);
        let desired_phase = (self.ts.saturating_sub(ts) as f64 * 1e-9 * self.bpm / 60.0).rem_euclid(1.0);
        let error = (desired_phase - self.position.rem_euclid(1.0) + 0.5).rem_euclid(1.0) - 0.5;
        self.correction = (error * 0.6).clamp(-self.bpm / 300.0, self.bpm / 300.0);
    }

    /// Clear every tempo override. Fresh audio must reacquire confidence; tempo freewheels meanwhile.
    pub fn clear_override(&mut self, ts: u64) {
        self.advance(ts);
        self.override_source = None;
        self.n_taps = 0;
        self.invalidate_source(ts);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn obs(ts: u64, bpm: f32, phase: f32, confidence: f32) -> Observation {
        Observation { ts, bpm, phase, confidence }
    }

    #[test]
    fn silence_and_reacquisition_preserve_forward_motion_and_trusted_tempo() {
        let mut c = MusicalClock::new(0);
        for i in 0..500 {
            let ts = i * 20_000_000;
            c.observe(obs(ts, 100.0, (ts as f64 / 600_000_000.0).fract() as f32, 0.9));
        }
        let before = c.snapshot();
        assert!((before.bpm - 100.0).abs() < 0.2);
        c.observe(obs(before.ts + 20_000_000, 200.0, 0.8, 0.1));
        let dropped = c.advance(before.ts + 3_000_000_000);
        assert_eq!(dropped.source, Source::Fallback);
        assert_eq!(dropped.confidence, 0.0);
        assert!((dropped.bpm - before.bpm).abs() < 0.01);
        assert!(dropped.position > before.position + 4.8);
        let mut previous = dropped;
        for i in 1..100 {
            let ts = dropped.ts + i * 20_000_000;
            c.observe(obs(ts, 140.0, 0.1, 0.9));
            let next = c.snapshot();
            assert!(next.position >= previous.position);
            assert!((next.bpm - previous.bpm).abs() < 0.5);
            previous = next;
        }
        assert_eq!(previous.source, Source::Live);
    }

    #[test]
    fn manual_tap_clear_and_source_replacement_have_explicit_precedence() {
        let mut c = MusicalClock::new(0);
        c.observe(obs(0, 90.0, 0.0, 0.9));
        assert!(c.set_bpm(10_000_000, 150.0));
        c.observe(obs(20_000_000, 90.0, 0.5, 1.0));
        assert_eq!(c.snapshot().source, Source::Manual);
        assert_eq!(c.snapshot().bpm, 150.0);
        c.tap(100_000_000);
        c.tap(600_000_000);
        assert_eq!(c.snapshot().source, Source::Tap);
        assert_eq!(c.snapshot().bpm, 120.0);
        c.invalidate_source(700_000_000);
        assert_eq!(c.snapshot().source, Source::Tap);
        c.clear_override(800_000_000);
        assert_eq!(c.snapshot().source, Source::Fallback);
        c.observe(obs(810_000_000, 90.0, 0.5, 0.9));
        assert_eq!(c.snapshot().source, Source::Live);
        c.invalidate_source(820_000_000);
        assert_eq!(c.snapshot().confidence, 0.0);
        assert!(!c.snapshot().locked);
    }

    #[test]
    fn invalid_measurements_staleness_and_repeated_queries_do_not_reset_phase() {
        let mut c = MusicalClock::new(0);
        c.observe(obs(0, 120.0, 0.0, 0.9));
        let a = c.advance(100_000_000);
        assert_eq!(a.position, c.advance(100_000_000).position);
        assert_eq!(a.position, c.advance(0).position);
        assert!(!c.set_bpm(100_000_000, f32::NAN));
        c.observe(obs(110_000_000, f32::NAN, 0.0, 1.0));
        c.observe(obs(110_000_000, 120.0, f32::NAN, 1.0));
        let stale = c.advance(1_000_000_000);
        assert_eq!(stale.source, Source::Fallback);
        assert_eq!(stale.confidence, 0.0);
        assert!((stale.position - 2.0).abs() < 1e-9);
        assert_eq!(stale.bpm, 120.0);
        c.observe(obs(1_010_000_000, 120.0, 0.8, 0.4));
        assert_eq!(c.snapshot().source, Source::Fallback, "reacquisition needs lock threshold");
    }

    #[test]
    fn confidence_hysteresis_and_freshness_boundary_are_distinct() {
        let mut c = MusicalClock::new(0);
        c.observe(obs(0, 120.0, 0.0, 0.59));
        assert!(!c.snapshot().locked);
        c.observe(obs(10_000_000, 120.0, 0.02, 0.6));
        assert!(c.snapshot().locked);
        c.observe(obs(20_000_000, 120.0, 0.04, 0.35));
        assert!(c.snapshot().locked, "confidence can dip below acquisition threshold");
        assert!(c.advance(270_000_000).locked);
        assert!(!c.advance(270_000_001).locked);
        c.observe(obs(280_000_000, 120.0, 0.56, 0.4));
        assert!(!c.snapshot().locked, "expired input must reacquire, not preserve old hysteresis");
    }

    #[test]
    fn delayed_tap_commands_use_event_timestamps_without_rewinding() {
        let mut c = MusicalClock::new(0);
        let before = c.advance(900_000_000);
        c.tap(100_000_000);
        assert_eq!(c.snapshot().source, Source::Fallback, "one tap leaves current tempo");
        c.tap(600_000_000);
        assert_eq!(c.snapshot().source, Source::Tap);
        assert_eq!(c.snapshot().bpm, 120.0, "tap spacing uses event time, not delivery time");
        assert_eq!(c.snapshot().position, before.position);
        let after = c.advance(1_000_000_000);
        assert!(after.position > before.position);
        c.tap(500_000_000);
        assert_eq!(c.snapshot().bpm, 120.0, "out-of-order tap cannot corrupt the series");
    }
}
