//! Follow a core timeline from the lock-free state snapshot, for timecode generators that
//! run on their own threads (MTC/LTC out). The snapshot carries `timeline.<n>.time` at core
//! time `snap.now`; a tight chase turns those samples into a smooth position at any instant.

use se_clock::timecode::chase::{Chase, ChaseConfig, ChaseEvent};
use se_clock::timecode::{ObsKind, TcObs};
use se_hub::Snapshot;
use se_proto::{Ts, Value};

pub struct Follower {
    time: String,
    playing: String,
    active: String,
    ids: Option<(u64, usize, usize, usize)>,
    chase: Chase,
    last_tick: u64,
    active_now: bool,
}

impl Follower {
    pub fn new(timeline: &str) -> Self {
        let cfg = ChaseConfig { jitter: 0.01, jump: 0.2, freewheel: 0.5, dropout: 0.05, window: 0.25, lock_count: 2, slew: 0.05 };
        Follower {
            time: format!("timeline.{timeline}.time"),
            playing: format!("timeline.{timeline}.playing"),
            active: format!("timeline.{timeline}.active"),
            ids: None,
            chase: Chase::new(cfg),
            last_tick: u64::MAX,
            active_now: false,
        }
    }

    /// Take a new snapshot into account; returns the chase event (jumps re-locate outputs).
    pub fn update(&mut self, snap: &Snapshot) -> ChaseEvent {
        if snap.tick == self.last_tick {
            return ChaseEvent::None;
        }
        self.last_tick = snap.tick;
        if self.ids.is_none_or(|(g, ..)| g != snap.generation) {
            self.ids = match (snap.id(&self.time), snap.id(&self.playing), snap.id(&self.active)) {
                (Some(t), Some(p), Some(a)) => Some((snap.generation, t, p, a)),
                _ => None,
            };
        }
        let Some((_, t, p, a)) = self.ids else {
            self.active_now = false;
            return ChaseEvent::None;
        };
        let pos = snap.value(t).as_f64().unwrap_or(0.0);
        let playing = snap.value(p).truthy();
        self.active_now = snap.value(a).truthy();
        let kind = if playing && self.active_now { ObsKind::Run } else { ObsKind::Stop };
        self.chase.observe(&TcObs { seconds: pos, ts: snap.now, kind, rate: None })
    }

    /// Advance freewheel bookkeeping (core stalled).
    pub fn tick(&mut self, now: Ts) {
        self.chase.tick(now);
    }

    pub fn position(&self, t: Ts) -> f64 {
        self.chase.position(t)
    }
    pub fn running(&self) -> bool {
        self.active_now && self.chase.running()
    }
    pub fn active(&self) -> bool {
        self.active_now
    }
    pub fn speed(&self) -> f64 {
        self.chase.speed()
    }
}

/// Build a snapshot holding one timeline's transport (tests and tools).
pub fn snapshot_for(timeline: &str, tick: u64, now: Ts, time: f64, playing: bool) -> Snapshot {
    let addrs = [format!("timeline.{timeline}.time"), format!("timeline.{timeline}.playing"), format!("timeline.{timeline}.active")];
    Snapshot {
        tick,
        now,
        generation: 1,
        index: std::sync::Arc::new(addrs.iter().enumerate().map(|(i, a)| (a.clone(), i)).collect()),
        values: vec![Value::Float(time), Value::Bool(playing), Value::Bool(true)],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn follows_core_samples_smoothly() {
        let mut f = Follower::new("show");
        let ms = 1_000_000;
        for k in 0..100u64 {
            // core snapshots every ~8 ms, time rounded to 1 ms like the published state
            let now = k * 8 * ms + (k % 3) * ms / 2;
            let time = ((10.0 + now as f64 / 1e9) * 1000.0).round() / 1000.0;
            f.update(&snapshot_for("show", k, now, time, true));
        }
        assert!(f.running());
        let t = 800 * ms;
        assert!((f.position(t) - 10.8).abs() < 0.002, "{}", f.position(t));
        f.update(&snapshot_for("show", 200, 810 * ms, 10.81, false));
        assert!(!f.running());
        assert!((f.position(900 * ms) - 10.81).abs() < 1e-9);
    }
}
