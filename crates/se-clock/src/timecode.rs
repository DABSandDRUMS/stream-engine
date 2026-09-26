//! SMPTE timecode values and frame-rate arithmetic (§2.7), plus the codecs and the chase
//! logic every timecode source shares:
//!
//! * [`mtc`] — MIDI Time Code quarter-frame / full-frame decode and encode, and a generator
//!   that schedules quarter frames from any position source;
//! * [`ltc`] — SMPTE linear timecode: 80-bit frames, biphase-mark audio encoder and a
//!   speed-tolerant streaming decoder;
//! * [`chase`] — lock/freewheel/jump logic that turns noisy position observations into a
//!   smooth, deterministic position;
//! * [`wav`] — minimal RIFF/WAVE encode/decode for rendering and checking LTC audio.
//!
//! Everything here is pure (no I/O, no wall clock): positions are seconds, times are
//! master-clock nanoseconds supplied by the caller, so the same inputs always give the same
//! outputs (replay-safe).

pub mod chase;
pub mod ltc;
pub mod mtc;
pub mod wav;

use se_proto::Ts;
use serde::{Deserialize, Serialize};
use std::fmt;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub enum FrameRate {
    #[serde(rename = "24")]
    Fps24,
    #[serde(rename = "25")]
    Fps25,
    /// 29.97 drop-frame.
    #[serde(rename = "29.97df")]
    Fps2997Df,
    #[default]
    #[serde(rename = "30")]
    Fps30,
}

impl FrameRate {
    pub const ALL: [FrameRate; 4] = [FrameRate::Fps24, FrameRate::Fps25, FrameRate::Fps2997Df, FrameRate::Fps30];

    /// Nominal integer frames per second (frame-count base).
    pub fn nominal(self) -> u32 {
        match self {
            FrameRate::Fps24 => 24,
            FrameRate::Fps25 => 25,
            FrameRate::Fps2997Df | FrameRate::Fps30 => 30,
        }
    }
    /// Real frames per second.
    pub fn fps(self) -> f64 {
        match self {
            FrameRate::Fps2997Df => 30000.0 / 1001.0,
            r => r.nominal() as f64,
        }
    }
    pub fn drop_frame(self) -> bool {
        self == FrameRate::Fps2997Df
    }
    pub fn parse(s: &str) -> Option<FrameRate> {
        Some(match s.trim().to_ascii_lowercase().as_str() {
            "24" => FrameRate::Fps24,
            "25" => FrameRate::Fps25,
            "29.97" | "29.97df" | "2997df" | "29.97_df" => FrameRate::Fps2997Df,
            "30" => FrameRate::Fps30,
            _ => return None,
        })
    }
    /// The label used in files and state (`"24"`, `"25"`, `"29.97df"`, `"30"`).
    pub fn as_str(self) -> &'static str {
        match self {
            FrameRate::Fps24 => "24",
            FrameRate::Fps25 => "25",
            FrameRate::Fps2997Df => "29.97df",
            FrameRate::Fps30 => "30",
        }
    }
    /// MTC rate code (quarter-frame piece 7 / full-frame hour byte bits 5–6).
    pub fn mtc_code(self) -> u8 {
        match self {
            FrameRate::Fps24 => 0,
            FrameRate::Fps25 => 1,
            FrameRate::Fps2997Df => 2,
            FrameRate::Fps30 => 3,
        }
    }
    pub fn from_mtc_code(c: u8) -> FrameRate {
        match c & 3 {
            0 => FrameRate::Fps24,
            1 => FrameRate::Fps25,
            2 => FrameRate::Fps2997Df,
            _ => FrameRate::Fps30,
        }
    }
    /// Seconds of one frame.
    pub fn frame_seconds(self) -> f64 {
        1.0 / self.fps()
    }
}

impl fmt::Display for FrameRate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// hh:mm:ss:ff at a frame rate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
pub struct Timecode {
    pub hours: u8,
    pub minutes: u8,
    pub seconds: u8,
    pub frames: u8,
    pub rate: FrameRate,
}

/// Frames in 24 hours of timecode at `rate` (drop-frame aware).
pub fn frames_per_day(rate: FrameRate) -> u64 {
    if rate.drop_frame() { 24 * 107_892 } else { rate.nominal() as u64 * 86_400 }
}

impl Timecode {
    /// Total frame count from 00:00:00:00 (drop-frame aware).
    pub fn to_frames(&self) -> u64 {
        let fps = self.rate.nominal() as u64;
        let total_minutes = self.hours as u64 * 60 + self.minutes as u64;
        let mut f = (total_minutes * 60 + self.seconds as u64) * fps + self.frames as u64;
        if self.rate.drop_frame() {
            f -= 2 * (total_minutes - total_minutes / 10);
        }
        f
    }

    pub fn from_frames(mut frames: u64, rate: FrameRate) -> Timecode {
        frames %= frames_per_day(rate);
        let fps = rate.nominal() as u64;
        if rate.drop_frame() {
            // Standard drop-frame conversion: 17982 frames per 10 minutes, 1798 per minute (after the first).
            let d = frames / 17982;
            let m = frames % 17982;
            let add = if m < 2 { 0 } else { 2 * ((m - 2) / 1798) };
            frames += 18 * d + add;
        }
        Timecode {
            frames: (frames % fps) as u8,
            seconds: ((frames / fps) % 60) as u8,
            minutes: ((frames / (fps * 60)) % 60) as u8,
            hours: ((frames / (fps * 3600)) % 24) as u8,
            rate,
        }
    }

    /// Seconds since 00:00:00:00 in real time.
    pub fn to_seconds(&self) -> f64 {
        self.to_frames() as f64 / self.rate.fps()
    }

    /// The frame containing `secs` (real time).
    pub fn from_seconds(secs: f64, rate: FrameRate) -> Timecode {
        // A hair of tolerance so `to_seconds()` round-trips despite float rounding.
        Timecode::from_frames((secs.max(0.0) * rate.fps() + 1e-6).floor() as u64, rate)
    }

    /// The frame after this one (wraps at 24 h).
    pub fn next(&self) -> Timecode {
        Timecode::from_frames(self.to_frames() + 1, self.rate)
    }

    /// Whether the fields form a valid label at this rate (drop-frame labels ;00 and ;01 do
    /// not exist in minutes not divisible by ten).
    pub fn is_valid(&self) -> bool {
        let base = self.hours < 24 && self.minutes < 60 && self.seconds < 60 && (self.frames as u32) < self.rate.nominal();
        base && !(self.rate.drop_frame() && self.seconds == 0 && self.frames < 2 && !self.minutes.is_multiple_of(10))
    }

    pub fn parse(s: &str, rate: FrameRate) -> Option<Timecode> {
        let parts: Vec<&str> = s.trim().split([':', ';', '.']).collect();
        if parts.len() != 4 {
            return None;
        }
        let n: Vec<u8> = parts.iter().map(|p| p.parse().ok()).collect::<Option<_>>()?;
        let tc = Timecode { hours: n[0], minutes: n[1], seconds: n[2], frames: n[3], rate };
        tc.is_valid().then_some(tc)
    }
}

impl fmt::Display for Timecode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sep = if self.rate.drop_frame() { ';' } else { ':' };
        write!(f, "{:02}:{:02}:{:02}{sep}{:02}", self.hours, self.minutes, self.seconds, self.frames)
    }
}

/// What a timecode observation says about the source.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ObsKind {
    /// The source is running and was at `seconds` at `ts`.
    #[default]
    Run,
    /// The source jumped to `seconds` and is parked there (MTC full-frame message, a seek
    /// while paused).
    Locate,
    /// The source stopped at `seconds`.
    Stop,
}

/// One position observation from a timecode source: the source was at `seconds` at
/// master-clock time `ts`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct TcObs {
    pub seconds: f64,
    pub ts: Ts,
    #[serde(default)]
    pub kind: ObsKind,
    /// Frame rate carried by the source (MTC/LTC), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate: Option<FrameRate>,
}

impl TcObs {
    pub fn run(seconds: f64, ts: Ts) -> TcObs {
        TcObs { seconds, ts, kind: ObsKind::Run, rate: None }
    }
    pub fn with_rate(mut self, r: FrameRate) -> TcObs {
        self.rate = Some(r);
        self
    }
}

/// Forward observations at most every `min_interval` ns while nothing notable changes
/// (discontinuities, kind or rate changes always pass). Used by the MTC/LTC inputs so the
/// session log records ~15 observations per second instead of every quarter frame.
#[derive(Clone, Debug)]
pub struct ObsThrottle {
    pub min_interval: Ts,
    /// Discontinuity threshold in seconds.
    pub jump: f64,
    last: Option<TcObs>,
}

impl ObsThrottle {
    pub fn new(min_interval: Ts, jump: f64) -> Self {
        ObsThrottle { min_interval, jump, last: None }
    }

    pub fn pass(&mut self, o: &TcObs) -> bool {
        let pass = match &self.last {
            None => true,
            Some(l) => {
                let dt = o.ts.saturating_sub(l.ts);
                let expect = l.seconds + if l.kind == ObsKind::Run { dt as f64 / 1e9 } else { 0.0 };
                o.kind != l.kind || o.rate != l.rate || (o.seconds - expect).abs() > self.jump || dt >= self.min_interval
            }
        };
        if pass {
            self.last = Some(*o);
        }
        pass
    }

    /// The source went quiet; the next observation always passes.
    pub fn reset(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nondrop_roundtrip() {
        for rate in [FrameRate::Fps24, FrameRate::Fps25, FrameRate::Fps30] {
            for f in [0u64, 1, 59, 1799, 86_399, 1_000_000] {
                assert_eq!(Timecode::from_frames(f, rate).to_frames(), f, "{rate:?} {f}");
            }
        }
    }

    #[test]
    fn drop_frame_skips_labels() {
        let r = FrameRate::Fps2997Df;
        // 00:00:59;29 → next frame is 00:01:00;02
        let tc = Timecode::parse("00:00:59;29", r).unwrap();
        let next = Timecode::from_frames(tc.to_frames() + 1, r);
        assert_eq!(next.to_string(), "00:01:00;02");
        // every 10th minute keeps ;00
        let tc = Timecode::parse("00:09:59;29", r).unwrap();
        assert_eq!(Timecode::from_frames(tc.to_frames() + 1, r).to_string(), "00:10:00;00");
        for f in [0u64, 1799, 1800, 17982, 107_892, 500_000] {
            assert_eq!(Timecode::from_frames(f, r).to_frames(), f);
        }
        // one hour of 29.97DF is 107892 frames
        assert_eq!(Timecode::parse("01:00:00;00", r).unwrap().to_frames(), 107_892);
        // labels that don't exist are rejected
        assert!(Timecode::parse("00:01:00;00", r).is_none());
        assert!(Timecode::parse("00:10:00;00", r).is_some());
    }

    #[test]
    fn every_drop_frame_label_roundtrips_for_an_hour() {
        let r = FrameRate::Fps2997Df;
        let mut prev = Timecode::from_frames(0, r);
        for f in 1..107_892u64 {
            let tc = Timecode::from_frames(f, r);
            assert!(tc.is_valid(), "{tc}");
            assert_eq!(tc.to_frames(), f);
            assert_eq!(prev.next(), tc);
            prev = tc;
        }
    }

    #[test]
    fn wraps_at_24h() {
        for r in FrameRate::ALL {
            let last = Timecode::from_frames(frames_per_day(r) - 1, r);
            assert_eq!(last.hours, 23);
            assert_eq!(last.next(), Timecode { rate: r, ..Default::default() });
        }
    }

    #[test]
    fn seconds_roundtrip_all_rates() {
        for r in FrameRate::ALL {
            for f in [0u64, 1, 29, 1798, 17_982, 100_000] {
                let tc = Timecode::from_frames(f, r);
                assert_eq!(Timecode::from_seconds(tc.to_seconds(), r), tc, "{r} {f}");
            }
        }
    }

    #[test]
    fn throttle_passes_discontinuities() {
        let mut t = ObsThrottle::new(60_000_000, 0.1);
        let ms = 1_000_000;
        assert!(t.pass(&TcObs::run(10.0, 0)));
        assert!(!t.pass(&TcObs::run(10.02, 20 * ms)));
        assert!(t.pass(&TcObs::run(10.07, 70 * ms)), "interval elapsed");
        assert!(t.pass(&TcObs::run(30.0, 80 * ms)), "jump");
        assert!(t.pass(&TcObs { kind: ObsKind::Stop, ..TcObs::run(30.0, 81 * ms) }), "kind change");
    }
}
