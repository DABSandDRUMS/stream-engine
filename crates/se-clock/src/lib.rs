//! Master clock (§3.2): one monotonic nanosecond clock that timestamps everything, plus
//! mappings to wall time, OBS stream/record time, Twitch stream delay, and the audio clock.
//! Timecode sources and generators live in [`timecode`].

pub mod timecode;

use parking_lot::RwLock;
use se_proto::Ts;
use serde::{Deserialize, Serialize};

/// Current master-clock time: CLOCK_MONOTONIC in nanoseconds.
#[inline]
pub fn now() -> Ts {
    let mut ts = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: valid pointer to a timespec; CLOCK_MONOTONIC is always available on Linux.
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Wall clock in nanoseconds since the Unix epoch.
pub fn wall_now_ns() -> i128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as i128).unwrap_or(0)
}

pub const NS_PER_MS: u64 = 1_000_000;
pub const NS_PER_SEC: u64 = 1_000_000_000;

pub fn ms(n: u64) -> Ts {
    n * NS_PER_MS
}

/// A linear mapping `other = master + offset` (with an optional rate for drifting clocks).
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Mapping {
    /// Master time of the reference point.
    pub master_ref: Ts,
    /// Other-clock time at the reference point (ns).
    pub other_ref: i128,
    /// Other-clock ns per master ns (1.0 for no drift).
    pub rate: f64,
    pub valid: bool,
}

impl Mapping {
    pub fn at(master_ref: Ts, other_ref: i128) -> Self {
        Mapping { master_ref, other_ref, rate: 1.0, valid: true }
    }
    pub fn to_other(&self, master: Ts) -> Option<i128> {
        self.valid.then(|| self.other_ref + ((master as i128 - self.master_ref as i128) as f64 * self.rate) as i128)
    }
    pub fn to_master(&self, other: i128) -> Option<Ts> {
        if !self.valid || self.rate == 0.0 {
            return None;
        }
        let m = self.master_ref as i128 + ((other - self.other_ref) as f64 / self.rate) as i128;
        u64::try_from(m).ok()
    }
    /// Refine rate/offset from a new observation (exponential smoothing of the rate).
    pub fn observe(&mut self, master: Ts, other: i128) {
        if !self.valid {
            *self = Mapping::at(master, other);
            return;
        }
        let dm = master as f64 - self.master_ref as f64;
        if dm > 1e9 {
            let measured = (other - self.other_ref) as f64 / dm;
            if (0.9..1.1).contains(&measured) {
                self.rate = self.rate * 0.9 + measured * 0.1;
            }
        }
        self.master_ref = master;
        self.other_ref = other;
    }
}

/// Clock mappings maintained by the engine (§3.2).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Mappings {
    pub wall: Mapping,
    pub obs_stream: Mapping,
    pub obs_record: Mapping,
    pub audio: Mapping,
    /// Measured Twitch stream delay (chat is shifted back by this much).
    pub twitch_delay_ms: u32,
}

/// Shared clock state.
#[derive(Default)]
pub struct Clock {
    maps: RwLock<Mappings>,
}

impl Clock {
    pub fn new() -> Self {
        let c = Clock::default();
        c.resync_wall();
        c
    }
    pub fn now(&self) -> Ts {
        now()
    }
    pub fn resync_wall(&self) {
        let m = now();
        let w = wall_now_ns();
        self.maps.write().wall = Mapping::at(m, w);
    }
    pub fn mappings(&self) -> Mappings {
        self.maps.read().clone()
    }
    pub fn update(&self, f: impl FnOnce(&mut Mappings)) {
        f(&mut self.maps.write());
    }
    /// Master time → wall-clock ns.
    pub fn to_wall(&self, t: Ts) -> i128 {
        self.maps.read().wall.to_other(t).unwrap_or_else(wall_now_ns)
    }
    /// Shift a chat timestamp back by the measured stream delay (moment on screen).
    pub fn chat_to_screen(&self, t: Ts) -> Ts {
        t.saturating_sub(self.maps.read().twitch_delay_ms as u64 * NS_PER_MS)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monotonic() {
        let a = now();
        let b = now();
        assert!(b >= a);
    }

    #[test]
    fn mapping_roundtrip() {
        let m = Mapping::at(1_000, 5_000_000);
        assert_eq!(m.to_other(2_000), Some(5_001_000));
        assert_eq!(m.to_master(5_001_000), Some(2_000));
        assert_eq!(Mapping::default().to_other(5), None);
    }

    #[test]
    fn mapping_rate_tracks_drift() {
        let mut m = Mapping::at(0, 0);
        for i in 1..50u64 {
            let master = i * 2 * NS_PER_SEC;
            m.observe(master, (master as f64 * 1.001) as i128);
        }
        assert!((m.rate - 1.001).abs() < 0.0002, "rate {}", m.rate);
    }
}
