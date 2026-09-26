//! Chase: follow a timecode source from noisy, possibly sparse position observations.
//!
//! * **Lock:** observations are fitted with a short least-squares line (position and
//!   speed); deviations within `jitter` are absorbed smoothly (slewed at a bounded rate, so
//!   the output never steps), larger ones re-sync immediately. After `lock_count`
//!   consecutive in-tolerance observations the chase reports `Locked`.
//! * **Freewheel:** with no observation for `dropout`, the position keeps running at the
//!   last speed for up to `freewheel`, then stops (`Lost`).
//! * **Jump:** a discontinuity beyond `jump` is reported as [`ChaseEvent::Jump`] so the
//!   timeline applies the tracked state at the new time instead of firing skipped cues.
//!
//! Pure and deterministic: the caller supplies every timestamp.

use super::{FrameRate, ObsKind, TcObs};
use se_proto::Ts;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

const MAX_POINTS: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ChaseConfig {
    /// Lock tolerance in seconds.
    pub jitter: f64,
    /// Discontinuities beyond this (seconds) are jumps (tracked state is applied).
    pub jump: f64,
    /// Keep running this long (seconds) after observations stop.
    pub freewheel: f64,
    /// Silence (seconds) after which the chase reports `Freewheel`.
    pub dropout: f64,
    /// Regression window (seconds).
    pub window: f64,
    /// Consecutive in-tolerance observations needed to report `Locked`.
    pub lock_count: u32,
    /// In-tolerance corrections are absorbed at up to this fraction of play speed.
    pub slew: f64,
}

impl ChaseConfig {
    /// Defaults for MTC/LTC at `rate`: 1 frame of jitter, 4 frames (≥ 150 ms) is a locate.
    pub fn timecode(rate: FrameRate) -> Self {
        let f = rate.frame_seconds();
        ChaseConfig { jitter: f, jump: (4.0 * f).max(0.15), freewheel: 2.0, dropout: 0.25, window: 0.5, lock_count: 4, slew: 0.05 }
    }
    /// Defaults for media players (reported/extrapolated song position).
    pub fn media() -> Self {
        ChaseConfig { jitter: 0.08, jump: 0.5, freewheel: 1.0, dropout: 0.3, window: 1.0, lock_count: 4, slew: 0.1 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ChaseStatus {
    /// Nothing received yet.
    #[default]
    Idle,
    /// Running, not yet consistent.
    Locking,
    Locked,
    /// Observations stopped; extrapolating.
    Freewheel,
    /// The source reported stop/locate.
    Stopped,
    /// Freewheel time ran out.
    Lost,
}

impl ChaseStatus {
    pub fn running(self) -> bool {
        matches!(self, ChaseStatus::Locking | ChaseStatus::Locked | ChaseStatus::Freewheel)
    }
    pub fn as_str(self) -> &'static str {
        match self {
            ChaseStatus::Idle => "idle",
            ChaseStatus::Locking => "locking",
            ChaseStatus::Locked => "locked",
            ChaseStatus::Freewheel => "freewheel",
            ChaseStatus::Stopped => "stopped",
            ChaseStatus::Lost => "lost",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ChaseEvent {
    None,
    /// Started running from rest at about the parked position.
    Started,
    /// Discontinuity beyond `jump` (seconds from → to).
    Jump {
        from: f64,
        to: f64,
    },
    /// Correction beyond `jitter` but within `jump`: snapped, not a jump.
    Resync {
        from: f64,
        to: f64,
    },
    Stopped,
    /// Freewheel ran out.
    Lost,
}

#[derive(Clone, Debug)]
pub struct Chase {
    pub cfg: ChaseConfig,
    status: ChaseStatus,
    base_ts: Ts,
    base_pos: f64,
    speed: f64,
    offset: f64,
    offset_ts: Ts,
    points: VecDeque<(Ts, f64)>,
    last_obs: Ts,
    good: u32,
    frozen: f64,
    rate: Option<FrameRate>,
}

fn secs(a: Ts, b: Ts) -> f64 {
    (a as i128 - b as i128) as f64 / 1e9
}

impl Chase {
    pub fn new(cfg: ChaseConfig) -> Self {
        Chase {
            cfg,
            status: ChaseStatus::Idle,
            base_ts: 0,
            base_pos: 0.0,
            speed: 1.0,
            offset: 0.0,
            offset_ts: 0,
            points: VecDeque::with_capacity(MAX_POINTS),
            last_obs: 0,
            good: 0,
            frozen: 0.0,
            rate: None,
        }
    }

    pub fn status(&self) -> ChaseStatus {
        self.status
    }
    pub fn locked(&self) -> bool {
        matches!(self.status, ChaseStatus::Locked)
    }
    pub fn running(&self) -> bool {
        self.status.running()
    }
    /// Estimated play speed (1.0 = nominal).
    pub fn speed(&self) -> f64 {
        if self.running() { self.speed } else { 0.0 }
    }
    /// Frame rate reported by the source, if any.
    pub fn rate(&self) -> Option<FrameRate> {
        self.rate
    }
    pub fn last_observation(&self) -> Option<Ts> {
        (self.status != ChaseStatus::Idle).then_some(self.last_obs)
    }

    fn offset_at(&self, t: Ts) -> f64 {
        if self.offset == 0.0 {
            return 0.0;
        }
        let rate = self.cfg.slew * self.speed.abs().max(0.25);
        let left = self.offset.abs() - rate * secs(t, self.offset_ts).max(0.0);
        if left <= 0.0 { 0.0 } else { left.copysign(self.offset) }
    }

    fn model(&self, t: Ts) -> f64 {
        self.base_pos + self.speed * secs(t, self.base_ts) + self.offset_at(t)
    }

    /// Position (seconds) at master time `t`.
    pub fn position(&self, t: Ts) -> f64 {
        if self.running() { self.model(t) } else { self.frozen }
    }

    fn restart(&mut self, o: &TcObs) {
        self.points.clear();
        self.points.push_back((o.ts, o.seconds));
        self.base_ts = o.ts;
        self.base_pos = o.seconds;
        self.offset = 0.0;
        self.good = 1;
        self.status = ChaseStatus::Locking;
        self.last_obs = o.ts;
    }

    /// Feed one observation.
    pub fn observe(&mut self, o: &TcObs) -> ChaseEvent {
        if !o.seconds.is_finite() {
            return ChaseEvent::None;
        }
        if self.status != ChaseStatus::Idle && o.ts < self.last_obs {
            return ChaseEvent::None; // stale (out of order)
        }
        if o.rate.is_some() {
            self.rate = o.rate;
        }
        let was = self.status;
        let predicted = self.position(o.ts);
        let err = o.seconds - predicted;
        if matches!(o.kind, ObsKind::Stop | ObsKind::Locate) {
            let jumped = was != ChaseStatus::Idle && err.abs() > self.cfg.jump;
            self.status = ChaseStatus::Stopped;
            self.frozen = o.seconds;
            self.points.clear();
            self.good = 0;
            self.offset = 0.0;
            self.last_obs = o.ts;
            return if jumped {
                ChaseEvent::Jump { from: predicted, to: o.seconds }
            } else if was.running() {
                ChaseEvent::Stopped
            } else {
                ChaseEvent::None
            };
        }
        if !was.running() {
            self.speed = 1.0;
            self.restart(o);
            return if was != ChaseStatus::Idle && err.abs() > self.cfg.jump {
                ChaseEvent::Jump { from: predicted, to: o.seconds }
            } else {
                ChaseEvent::Started
            };
        }
        if err.abs() > self.cfg.jump {
            self.restart(o);
            return ChaseEvent::Jump { from: predicted, to: o.seconds };
        }
        self.last_obs = o.ts;
        if err.abs() > self.cfg.jitter {
            self.restart(o);
            self.good = 0;
            return ChaseEvent::Resync { from: predicted, to: o.seconds };
        }
        self.points.push_back((o.ts, o.seconds));
        let horizon = o.ts.saturating_sub((self.cfg.window * 1e9) as Ts);
        while self.points.len() > MAX_POINTS || (self.points.len() > 2 && self.points.front().is_some_and(|p| p.0 < horizon)) {
            self.points.pop_front();
        }
        let (speed, pos) = self.fit(o);
        // keep the output continuous: the difference is slewed out
        self.offset = predicted - pos;
        self.offset_ts = o.ts;
        self.base_ts = o.ts;
        self.base_pos = pos;
        self.speed = speed;
        self.good += 1;
        self.status = if self.good >= self.cfg.lock_count { ChaseStatus::Locked } else { ChaseStatus::Locking };
        ChaseEvent::None
    }

    /// Least-squares line through the window: (speed, position at `o.ts`).
    fn fit(&self, o: &TcObs) -> (f64, f64) {
        let n = self.points.len();
        let span = secs(o.ts, self.points.front().map(|p| p.0).unwrap_or(o.ts));
        if n < 3 || span < 0.05 {
            return (self.speed, o.seconds);
        }
        let (mut sx, mut sy, mut sxx, mut sxy) = (0.0, 0.0, 0.0, 0.0);
        for &(t, p) in &self.points {
            let x = secs(t, o.ts);
            sx += x;
            sy += p;
            sxx += x * x;
            sxy += x * p;
        }
        let nf = n as f64;
        let den = nf * sxx - sx * sx;
        if den.abs() < 1e-12 {
            return (self.speed, o.seconds);
        }
        let slope = ((nf * sxy - sx * sy) / den).clamp(-4.0, 4.0);
        let icpt = (sy - slope * sx) / nf;
        (slope, icpt)
    }

    /// Advance time without an observation (dropout → freewheel → lost).
    pub fn tick(&mut self, now: Ts) -> ChaseEvent {
        if !self.running() {
            return ChaseEvent::None;
        }
        let quiet = secs(now, self.last_obs);
        if quiet > self.cfg.freewheel {
            let end = self.last_obs + (self.cfg.freewheel * 1e9) as Ts;
            self.frozen = self.model(end);
            self.status = ChaseStatus::Lost;
            self.points.clear();
            return ChaseEvent::Lost;
        }
        if quiet > self.cfg.dropout && self.status != ChaseStatus::Freewheel {
            self.status = ChaseStatus::Freewheel;
        }
        ChaseEvent::None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: Ts = 1_000_000;

    fn cfg() -> ChaseConfig {
        ChaseConfig::timecode(FrameRate::Fps30)
    }

    /// Deterministic jitter in ±`amp` seconds.
    fn jitter(i: u64, amp: f64) -> f64 {
        let x = (i.wrapping_mul(2_654_435_761) % 1000) as f64 / 1000.0;
        (x - 0.5) * 2.0 * amp
    }

    #[test]
    fn locks_through_jitter_and_tracks_position() {
        let mut c = Chase::new(cfg());
        let mut max_err: f64 = 0.0;
        for i in 0..200u64 {
            let t = 1_000 * MS + i * 66 * MS;
            let truth = 100.0 + (i * 66) as f64 / 1000.0;
            let ev = c.observe(&TcObs::run(truth + jitter(i, 0.003), t));
            if i == 0 {
                assert_eq!(ev, ChaseEvent::Started);
            } else {
                assert_eq!(ev, ChaseEvent::None, "no resync/jump under jitter at {i}");
            }
            if i > 10 {
                max_err = max_err.max((c.position(t + 30 * MS) - (truth + 0.03)).abs());
            }
        }
        assert!(c.locked());
        assert!(max_err < 0.004, "{max_err}");
        assert!((c.speed() - 1.0).abs() < 0.01);
    }

    #[test]
    fn output_is_monotonic_under_jitter() {
        let mut c = Chase::new(ChaseConfig::media());
        let mut last = f64::MIN;
        let mut i = 0u64;
        for step in 0..3000u64 {
            let t = step * 4 * MS; // core ticks at 250 Hz
            if step % 8 == 0 {
                let truth = t as f64 / 1e9;
                c.observe(&TcObs::run(truth + jitter(i, 0.05), t));
                i += 1;
            }
            c.tick(t);
            let p = c.position(t);
            assert!(p >= last - 1e-9, "went backwards at {step}: {p} < {last}");
            last = p;
        }
    }

    #[test]
    fn freewheels_then_stops() {
        let mut c = Chase::new(cfg());
        for i in 0..20u64 {
            c.observe(&TcObs::run(10.0 + i as f64 * 0.066, i * 66 * MS));
        }
        let last = 19 * 66 * MS;
        c.tick(last + 100 * MS);
        assert!(c.locked());
        c.tick(last + 400 * MS);
        assert_eq!(c.status(), ChaseStatus::Freewheel);
        let p = c.position(last + 1000 * MS);
        assert!((p - (10.0 + 19.0 * 0.066 + 1.0)).abs() < 0.01, "keeps running: {p}");
        assert_eq!(c.tick(last + 2100 * MS), ChaseEvent::Lost);
        let frozen = c.position(last + 5000 * MS);
        assert!((frozen - (10.0 + 19.0 * 0.066 + 2.0)).abs() < 0.01, "held at freewheel end: {frozen}");
        // resuming near the held position is a start, far away is a jump
        let ev = c.observe(&TcObs::run(frozen + 0.05, last + 6000 * MS));
        assert_eq!(ev, ChaseEvent::Started);
    }

    #[test]
    fn jumps_and_resyncs_are_classified() {
        let mut c = Chase::new(cfg());
        for i in 0..10u64 {
            c.observe(&TcObs::run(i as f64 * 0.066, i * 66 * MS));
        }
        let t = 10 * 66 * MS;
        // 60 ms off: beyond 1 frame jitter, within the 150 ms jump threshold
        assert!(matches!(c.observe(&TcObs::run(0.66 + 0.06, t)), ChaseEvent::Resync { .. }));
        assert_eq!(c.status(), ChaseStatus::Locking);
        match c.observe(&TcObs::run(300.0, t + 66 * MS)) {
            ChaseEvent::Jump { from, to } => {
                assert!((from - 0.786).abs() < 0.01);
                assert_eq!(to, 300.0);
            }
            e => panic!("{e:?}"),
        }
        assert!((c.position(t + 66 * MS) - 300.0).abs() < 1e-9);
    }

    #[test]
    fn locate_and_stop() {
        let mut c = Chase::new(cfg());
        assert_eq!(c.observe(&TcObs { kind: ObsKind::Locate, ..TcObs::run(50.0, 0) }), ChaseEvent::None);
        assert_eq!(c.position(10 * MS), 50.0);
        assert_eq!(c.observe(&TcObs::run(50.0, 20 * MS)), ChaseEvent::Started);
        c.observe(&TcObs::run(50.5, 520 * MS));
        assert_eq!(c.observe(&TcObs { kind: ObsKind::Stop, ..TcObs::run(50.6, 620 * MS) }), ChaseEvent::Stopped);
        assert!(!c.running());
        assert_eq!(c.position(5_000 * MS), 50.6);
        assert!(matches!(c.observe(&TcObs { kind: ObsKind::Locate, ..TcObs::run(10.0, 700 * MS) }), ChaseEvent::Jump { .. }));
    }

    #[test]
    fn follows_varispeed() {
        let mut c = Chase::new(cfg());
        for i in 0..40u64 {
            c.observe(&TcObs::run(i as f64 * 0.066 * 1.1, i * 66 * MS));
        }
        assert!((c.speed() - 1.1).abs() < 0.01, "{}", c.speed());
        assert!(c.locked());
    }

    #[test]
    fn stale_observations_are_ignored() {
        let mut c = Chase::new(cfg());
        c.observe(&TcObs::run(1.0, 100 * MS));
        assert_eq!(c.observe(&TcObs::run(0.5, 50 * MS)), ChaseEvent::None);
        assert!((c.position(100 * MS) - 1.0).abs() < 1e-9);
    }
}
