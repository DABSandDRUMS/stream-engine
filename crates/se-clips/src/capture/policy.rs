//! Pure recorder policies, kept free of processes and clocks so their transitions are testable:
//! restart backoff, disk-space holds, and load shedding of ISO encoders.

use std::time::{Duration, Instant};

/// Bounded exponential restart delay. Never gives up; a run that recorded for a while resets it.
#[derive(Clone, Debug, Default)]
pub(crate) struct Backoff {
    failures: u32,
}

impl Backoff {
    pub(crate) const MAX: Duration = Duration::from_secs(60);
    /// A run that recorded at least this long counts as healthy and resets the delay.
    pub(crate) const STABLE: Duration = Duration::from_secs(60);

    /// Delay before the next attempt after a failed run that recorded for `ran`.
    pub(crate) fn fail(&mut self, ran: Duration) -> Duration {
        if ran >= Self::STABLE {
            self.failures = 0;
        }
        let delay = Duration::from_secs(1u64 << self.failures.min(6)).min(Self::MAX);
        self.failures = self.failures.saturating_add(1);
        delay
    }
}

/// A free-space floor with hysteresis: held below `stop_below`, released at `resume_at`.
#[derive(Clone, Debug)]
pub(crate) struct Floor {
    stop_below: f64,
    resume_at: f64,
    held: bool,
}

impl Floor {
    pub(crate) fn new(stop_below_gb: f64) -> Self {
        // Resume only with a margin so a file being finalized cannot flap the hold.
        Floor { stop_below: stop_below_gb, resume_at: stop_below_gb * 1.2 + 1.0, held: false }
    }

    /// Feed the current free space; unknown readings keep the previous state.
    pub(crate) fn update(&mut self, free_gb: Option<f64>) -> bool {
        if self.stop_below <= 0.0 {
            self.held = false;
        } else if let Some(free) = free_gb {
            if self.held { self.held = free < self.resume_at } else { self.held = free < self.stop_below }
        }
        self.held
    }
}

/// Disk holds for one recording: ISOs stop under 1.5 × `min_free_gb`, the master under 1 ×.
#[derive(Clone, Debug)]
pub(crate) struct DiskGuard {
    pub(crate) iso: Floor,
    pub(crate) master: Floor,
}

impl DiskGuard {
    pub(crate) fn new(min_free_gb: f64) -> Self {
        DiskGuard { iso: Floor::new(min_free_gb * 1.5), master: Floor::new(min_free_gb) }
    }

    /// `master_free`: the master's folder; `iso_free`: the ISOs' folder (often the same disk).
    /// ISOs always stop when the master does.
    pub(crate) fn update(&mut self, master_free: Option<f64>, iso_free: Option<f64>) -> (bool, bool) {
        let master = self.master.update(master_free);
        let iso = self.iso.update(iso_free) || master;
        (master, iso)
    }
}

/// Load shedding of ISO encoders. Sustained pressure (render or master encoder falling behind)
/// stops one more ISO, lowest priority first; sustained calm restores them one at a time,
/// most recently shed first. Never touches the master.
#[derive(Clone, Debug)]
pub(crate) struct Shedder {
    /// Shed ISO video indices, in the order they were shed.
    shed: Vec<usize>,
    pressure_since: Option<Instant>,
    calm_since: Option<Instant>,
    last_change: Option<Instant>,
}

impl Shedder {
    /// Pressure must persist this long before an ISO is shed.
    pub(crate) const SHED_AFTER: Duration = Duration::from_secs(3);
    /// Minimum time between two changes.
    pub(crate) const SETTLE: Duration = Duration::from_secs(10);
    /// Calm must persist this long before a shed ISO is restored.
    pub(crate) const RESTORE_AFTER: Duration = Duration::from_secs(60);

    pub(crate) fn new() -> Self {
        Shedder { shed: Vec::new(), pressure_since: None, calm_since: None, last_change: None }
    }

    pub(crate) fn is_shed(&self, index: usize) -> bool {
        self.shed.contains(&index)
    }

    /// One observation. `order`: running-eligible ISO indices, lowest priority first.
    /// Returns true when the shed set changed.
    pub(crate) fn update(&mut self, now: Instant, pressure: bool, order: &[usize]) -> bool {
        self.shed.retain(|i| order.contains(i));
        if pressure {
            self.calm_since = None;
            let since = *self.pressure_since.get_or_insert(now);
            let settled = self.last_change.is_none_or(|at| now.duration_since(at) >= Self::SETTLE);
            if now.duration_since(since) >= Self::SHED_AFTER
                && settled
                && let Some(&next) = order.iter().find(|i| !self.shed.contains(i))
            {
                self.shed.push(next);
                self.last_change = Some(now);
                self.pressure_since = None;
                return true;
            }
        } else {
            self.pressure_since = None;
            let since = *self.calm_since.get_or_insert(now);
            let settled = self.last_change.is_none_or(|at| now.duration_since(at) >= Self::RESTORE_AFTER);
            if !self.shed.is_empty() && now.duration_since(since) >= Self::RESTORE_AFTER && settled {
                self.shed.pop();
                self.last_change = Some(now);
                self.calm_since = Some(now);
                return true;
            }
        }
        false
    }
}

/// ISO shedding order, lowest priority first: ISOs scaled below 1080 lines, then the rest;
/// within each group the later-configured first.
pub(crate) fn shed_order(isos: &[(usize, Option<u32>)]) -> Vec<usize> {
    let mut order: Vec<_> = isos.to_vec();
    order.sort_by_key(|(index, height)| (height.is_none_or(|h| h >= 1080), std::cmp::Reverse(*index)));
    order.into_iter().map(|(index, _)| index).collect()
}

/// Rising counters per second between two observations.
#[derive(Clone, Debug, Default)]
pub(crate) struct Rate {
    last: Option<(u64, Instant)>,
}

impl Rate {
    pub(crate) fn per_sec(&mut self, value: u64, now: Instant) -> f64 {
        let rate = match self.last {
            Some((prev, at)) if value >= prev && now > at => (value - prev) as f64 / now.duration_since(at).as_secs_f64(),
            _ => 0.0,
        };
        self.last = Some((value, now));
        rate
    }
}

/// Render/master load signals sampled once a second.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Load {
    pub render_dropped_per_s: f64,
    pub render_late_per_s: f64,
    pub render_fps: f64,
    pub master_dropped_per_s: f64,
    /// Encoder speed (1.0 = real time); 0 when unknown.
    pub master_speed: f64,
}

impl Load {
    pub(crate) fn pressure(&self, fps: u32) -> bool {
        self.render_dropped_per_s >= 2.0
            || self.render_late_per_s >= fps as f64 / 2.0
            || (self.render_fps > 0.0 && self.render_fps < 57.0 && fps >= 60)
            || self.master_dropped_per_s >= 2.0
            || (self.master_speed > 0.0 && self.master_speed < 0.95)
    }
}

/// An ISO encoder that cannot keep up: losing a quarter of its pictures or running under
/// 90 % of real time for `BUSY_AFTER` is stopped and retried later.
#[derive(Clone, Debug, Default)]
pub(crate) struct Busy {
    since: Option<Instant>,
}

impl Busy {
    pub(crate) const AFTER: Duration = Duration::from_secs(5);

    pub(crate) fn update(&mut self, now: Instant, dropped_per_s: f64, speed: f64, fps: u32) -> bool {
        let behind = dropped_per_s >= fps as f64 / 4.0 || (speed > 0.0 && speed < 0.9);
        if !behind {
            self.since = None;
            return false;
        }
        now.duration_since(*self.since.get_or_insert(now)) >= Self::AFTER
    }

    pub(crate) fn reset(&mut self) {
        self.since = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_to_a_cap_never_gives_up_and_resets_after_a_stable_run() {
        let mut b = Backoff::default();
        let delays: Vec<u64> = (0..9).map(|_| b.fail(Duration::ZERO).as_secs()).collect();
        assert_eq!(delays, [1, 2, 4, 8, 16, 32, 60, 60, 60]);
        assert_eq!(b.fail(Duration::from_secs(59)).as_secs(), 60);
        assert_eq!(b.fail(Backoff::STABLE).as_secs(), 1, "a long healthy run restarts quickly");
        assert_eq!(b.fail(Duration::ZERO).as_secs(), 2);
    }

    #[test]
    fn disk_guard_stops_isos_first_then_master_and_releases_with_margin() {
        let mut d = DiskGuard::new(20.0);
        assert_eq!(d.update(Some(100.0), Some(100.0)), (false, false));
        assert_eq!(d.update(Some(29.0), Some(29.0)), (false, true), "under 1.5 × min: ISOs stop");
        assert_eq!(d.update(Some(31.0), Some(31.0)), (false, true), "no flapping just above the floor");
        assert_eq!(d.update(Some(19.9), Some(19.9)), (true, true), "under min: master stops too");
        assert_eq!(d.update(None, None), (true, true), "unknown keeps the hold");
        assert_eq!(d.update(Some(24.0), Some(24.0)), (true, true));
        assert_eq!(d.update(Some(25.5), Some(25.5)), (false, true), "master resumes at 1.2 × min + 1 GB");
        assert_eq!(d.update(Some(37.0), Some(37.0)), (false, false), "ISOs resume at 1.2 × 1.5 × min + 1 GB");
        // Master in a fallback folder on another disk: its floor is independent.
        assert_eq!(d.update(Some(500.0), Some(10.0)), (false, true));
        let mut off = DiskGuard::new(0.0);
        assert_eq!(off.update(Some(0.0), Some(0.0)), (false, false), "min_free_gb = 0 disables the guard");
    }

    #[test]
    fn shedding_order_drops_small_isos_first_and_later_configured_first() {
        assert_eq!(shed_order(&[(1, Some(1080)), (2, Some(720)), (3, None), (4, Some(480))]), [4, 2, 3, 1]);
    }

    #[test]
    fn shedder_needs_sustained_pressure_settles_between_steps_and_restores_lifo_after_calm() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let order = [4, 2, 1];
        let mut s = Shedder::new();
        assert!(!s.update(at(0), true, &order));
        assert!(!s.update(at(2), true, &order), "a 2 s blip sheds nothing");
        assert!(!s.update(at(3), false, &order));
        assert!(!s.update(at(4), true, &order));
        assert!(s.update(at(7), true, &order));
        assert!(s.is_shed(4) && !s.is_shed(2));
        assert!(!s.update(at(11), true, &order), "settle between steps");
        assert!(s.update(at(17), true, &order));
        assert!(s.is_shed(2));
        assert!(!s.update(at(20), false, &order));
        assert!(!s.update(at(79), false, &order), "restore needs a full minute of calm");
        assert!(s.update(at(80), false, &order));
        assert!(!s.is_shed(2) && s.is_shed(4), "most recently shed returns first");
        assert!(!s.update(at(100), false, &order));
        assert!(s.update(at(140), false, &order));
        assert!(!s.is_shed(4));
        // Pressure with every ISO already shed changes nothing (the master is never shed).
        let mut all = Shedder::new();
        for t in 0..100 {
            all.update(at(t), true, &[7]);
        }
        assert!(all.is_shed(7));
        assert!(!all.update(at(200), true, &[7]));
    }

    #[test]
    fn load_pressure_and_iso_busy_thresholds() {
        assert!(!Load { render_fps: 60.0, master_speed: 1.0, ..Default::default() }.pressure(60));
        assert!(Load { render_dropped_per_s: 3.0, ..Default::default() }.pressure(60));
        assert!(Load { render_late_per_s: 30.0, ..Default::default() }.pressure(60));
        assert!(Load { render_fps: 50.0, ..Default::default() }.pressure(60));
        assert!(!Load { render_fps: 29.0, ..Default::default() }.pressure(30), "a 30 fps recording does not judge render fps");
        assert!(Load { master_speed: 0.9, ..Default::default() }.pressure(60));
        let t0 = Instant::now();
        let mut b = Busy::default();
        assert!(!b.update(t0, 20.0, 1.0, 60));
        assert!(!b.update(t0 + Duration::from_secs(4), 20.0, 1.0, 60));
        assert!(b.update(t0 + Duration::from_secs(5), 20.0, 1.0, 60));
        assert!(!b.update(t0 + Duration::from_secs(6), 0.0, 1.0, 60), "recovers when it keeps up");
        assert!(!b.update(t0 + Duration::from_secs(7), 0.0, 0.85, 60));
        assert!(b.update(t0 + Duration::from_secs(12), 0.0, 0.85, 60));
    }

    #[test]
    fn rate_is_per_second_and_ignores_counter_resets() {
        let t0 = Instant::now();
        let mut r = Rate::default();
        assert_eq!(r.per_sec(10, t0), 0.0);
        assert_eq!(r.per_sec(20, t0 + Duration::from_secs(2)), 5.0);
        assert_eq!(r.per_sec(3, t0 + Duration::from_secs(3)), 0.0);
    }
}
