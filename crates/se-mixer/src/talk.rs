//! `mic.talking`: voice activity from a channel's input meter, for ducking.
//!
//! On when the level stays above the threshold for `attack`; off when it stays below
//! `threshold − hysteresis` for `hold` (speech has gaps between words).

use crate::config::TalkConfig;
use crate::ucnet::meters::to_db;
use std::time::{Duration, Instant};

#[derive(Debug, Clone)]
pub struct TalkDetector {
    on_db: f32,
    off_db: f32,
    attack: Duration,
    hold: Duration,
    talking: bool,
    since: Option<Instant>,
}

impl TalkDetector {
    pub fn new(c: &TalkConfig) -> TalkDetector {
        TalkDetector { on_db: c.threshold_db, off_db: c.threshold_db - c.hysteresis_db, attack: c.attack, hold: c.hold, talking: false, since: None }
    }

    pub fn talking(&self) -> bool {
        self.talking
    }

    /// Feed one linear level (0–1). Returns `Some(new state)` on a transition.
    pub fn update(&mut self, level: f32, now: Instant) -> Option<bool> {
        let db = to_db(level);
        let (wants_change, wait) = if self.talking { (db < self.off_db, self.hold) } else { (db >= self.on_db, self.attack) };
        if !wants_change {
            self.since = None;
            return None;
        }
        let since = *self.since.get_or_insert(now);
        if now.duration_since(since) >= wait {
            self.talking = !self.talking;
            self.since = None;
            return Some(self.talking);
        }
        None
    }

    /// Meters stopped (disconnect): talking ends immediately.
    pub fn reset(&mut self) -> Option<bool> {
        self.since = None;
        std::mem::take(&mut self.talking).then_some(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn det() -> TalkDetector {
        TalkDetector::new(&TalkConfig {
            channel: None,
            threshold_db: -30.0,
            attack: Duration::from_millis(40),
            hold: Duration::from_millis(300),
            hysteresis_db: 4.0,
        })
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    const LOUD: f32 = 0.1; // −20 dBFS
    const GAP: f32 = 0.025; // −32 dBFS: under the on threshold, above the off threshold
    const QUIET: f32 = 0.001; // −60 dBFS

    #[test]
    fn needs_attack_time_to_start() {
        let t0 = Instant::now();
        let mut d = det();
        assert_eq!(d.update(LOUD, t0), None);
        assert_eq!(d.update(QUIET, at(t0, 20)), None); // a click, not speech
        assert_eq!(d.update(LOUD, at(t0, 50)), None);
        assert_eq!(d.update(LOUD, at(t0, 90)), Some(true));
    }

    #[test]
    fn holds_through_word_gaps_and_releases_after_hold() {
        let t0 = Instant::now();
        let mut d = det();
        d.update(LOUD, t0);
        assert_eq!(d.update(LOUD, at(t0, 50)), Some(true));
        // between words: below the on threshold but inside the hysteresis band → stays on
        assert_eq!(d.update(GAP, at(t0, 100)), None);
        assert_eq!(d.update(GAP, at(t0, 800)), None);
        assert!(d.talking());
        // silence shorter than hold → still talking
        assert_eq!(d.update(QUIET, at(t0, 900)), None);
        assert_eq!(d.update(LOUD, at(t0, 1100)), None);
        assert_eq!(d.update(QUIET, at(t0, 1200)), None);
        assert_eq!(d.update(QUIET, at(t0, 1550)), Some(false));
    }

    #[test]
    fn reset_reports_the_end_of_talking_once() {
        let t0 = Instant::now();
        let mut d = det();
        d.update(LOUD, t0);
        d.update(LOUD, at(t0, 50));
        assert_eq!(d.reset(), Some(false));
        assert_eq!(d.reset(), None);
    }
}
