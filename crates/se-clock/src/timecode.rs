//! SMPTE timecode values and frame-rate arithmetic (§2.7).

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
        Some(match s {
            "24" => FrameRate::Fps24,
            "25" => FrameRate::Fps25,
            "29.97" | "29.97df" | "2997df" => FrameRate::Fps2997Df,
            "30" => FrameRate::Fps30,
            _ => return None,
        })
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
        let fps = rate.nominal() as u64;
        if rate.drop_frame() {
            // Standard drop-frame conversion: 17982 frames per 10 minutes, 1798 per minute (after the first).
            let d = frames / 17982;
            let m = frames % 17982;
            let add = if m < 2 { 0 } else { 2 * ((m - 2) / 1798) };
            frames += 18 * d + add;
        }
        let day = fps * 86400;
        frames %= day;
        Timecode {
            frames: (frames % fps) as u8,
            seconds: ((frames / fps) % 60) as u8,
            minutes: ((frames / (fps * 60)) % 60) as u8,
            hours: (frames / (fps * 3600)) as u8,
            rate,
        }
    }

    /// Seconds since 00:00:00:00 in real time.
    pub fn to_seconds(&self) -> f64 {
        self.to_frames() as f64 / self.rate.fps()
    }

    pub fn from_seconds(secs: f64, rate: FrameRate) -> Timecode {
        Timecode::from_frames((secs.max(0.0) * rate.fps()).floor() as u64, rate)
    }

    pub fn parse(s: &str, rate: FrameRate) -> Option<Timecode> {
        let parts: Vec<&str> = s.split([':', ';', '.']).collect();
        if parts.len() != 4 {
            return None;
        }
        let n: Vec<u8> = parts.iter().map(|p| p.parse().ok()).collect::<Option<_>>()?;
        let tc = Timecode { hours: n[0], minutes: n[1], seconds: n[2], frames: n[3], rate };
        (tc.minutes < 60 && tc.seconds < 60 && (tc.frames as u32) < rate.nominal() && tc.hours < 24).then_some(tc)
    }
}

impl fmt::Display for Timecode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let sep = if self.rate.drop_frame() { ';' } else { ':' };
        write!(f, "{:02}:{:02}:{:02}{sep}{:02}", self.hours, self.minutes, self.seconds, self.frames)
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
    }
}
