//! MIDI Time Code (MMA RP-004): quarter-frame messages `F1 0nnn dddd` and full-frame SysEx
//! `F0 7F <dev> 01 01 hh mm ss ff F7`.
//!
//! Eight quarter frames (pieces 0–7) spread over two frames carry one timecode: the frame
//! at which piece 0 was sent. A receiver therefore knows the position with quarter-frame
//! resolution once a group is complete: at piece `k` of the group describing frame `N` the
//! source is at `N + k/4` frames (forward). In reverse the pieces arrive 7→0.

use super::{FrameRate, ObsKind, TcObs, Timecode};
use se_proto::Ts;

/// Quarter-frame status byte.
pub const QF: u8 = 0xF1;

/// One quarter-frame message (piece 0–7) for `tc`.
pub fn quarter_frame(tc: &Timecode, piece: u8) -> [u8; 2] {
    let d = match piece & 7 {
        0 => tc.frames & 0x0F,
        1 => (tc.frames >> 4) & 0x01,
        2 => tc.seconds & 0x0F,
        3 => (tc.seconds >> 4) & 0x03,
        4 => tc.minutes & 0x0F,
        5 => (tc.minutes >> 4) & 0x03,
        6 => tc.hours & 0x0F,
        _ => ((tc.hours >> 4) & 0x01) | (tc.rate.mtc_code() << 1),
    };
    [QF, ((piece & 7) << 4) | d]
}

/// The eight quarter frames describing `tc`, in forward send order.
pub fn quarter_frames(tc: &Timecode) -> [[u8; 2]; 8] {
    std::array::from_fn(|i| quarter_frame(tc, i as u8))
}

/// Full-frame (locate) SysEx for `tc`; `device` 0x7F = all devices.
pub fn full_frame(tc: &Timecode, device: u8) -> [u8; 10] {
    [0xF0, 0x7F, device & 0x7F, 0x01, 0x01, (tc.rate.mtc_code() << 5) | (tc.hours & 0x1F), tc.minutes & 0x3F, tc.seconds & 0x3F, tc.frames & 0x1F, 0xF7]
}

/// Parse a full-frame SysEx message (any device id).
pub fn parse_full_frame(msg: &[u8]) -> Option<Timecode> {
    match msg {
        [0xF0, 0x7F, _, 0x01, 0x01, hh, mm, ss, ff, 0xF7] => {
            let tc = Timecode { hours: hh & 0x1F, minutes: *mm, seconds: *ss, frames: *ff, rate: FrameRate::from_mtc_code(hh >> 5) };
            tc.is_valid().then_some(tc)
        }
        _ => None,
    }
}

/// What the decoder learned from a message.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum MtcUpdate {
    /// Running: at `ts` the source was at `frames` (fractional absolute frame count, drop-frame
    /// aware) — `tc` is the timecode of the current quarter-frame group.
    Running { tc: Timecode, frames: f64, ts: Ts, reverse: bool },
    /// Full-frame message: the source located to `tc` (parked).
    Locate { tc: Timecode, ts: Ts },
}

impl MtcUpdate {
    pub fn obs(&self) -> TcObs {
        match *self {
            MtcUpdate::Running { tc, frames, ts, .. } => TcObs { seconds: frames / tc.rate.fps(), ts, kind: ObsKind::Run, rate: Some(tc.rate) },
            MtcUpdate::Locate { tc, ts } => TcObs { seconds: tc.to_seconds(), ts, kind: ObsKind::Locate, rate: Some(tc.rate) },
        }
    }
}

/// Streaming MTC decoder. Feed it complete messages or an arbitrary byte stream (running
/// status, interleaved realtime bytes, and split SysEx are handled).
#[derive(Clone, Debug, Default)]
pub struct MtcDecoder {
    pieces: [u8; 8],
    have: u8,
    last_piece: Option<u8>,
    /// Absolute frame count of the current group (forward: frame at piece 0).
    group: Option<(Timecode, u64)>,
    reverse: bool,
    sysex: Vec<u8>,
    in_sysex: bool,
    pending_qf: bool,
}

impl MtcDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    /// Forget the current group (e.g. the port went away).
    pub fn reset(&mut self) {
        *self = MtcDecoder::default();
    }

    /// Feed raw MIDI bytes received at `ts`; `out` gets every update.
    pub fn feed(&mut self, bytes: &[u8], ts: Ts, mut out: impl FnMut(MtcUpdate)) {
        for &b in bytes {
            if b >= 0xF8 {
                continue; // realtime bytes may appear anywhere, even inside SysEx
            }
            if self.in_sysex {
                if b == 0xF7 {
                    self.sysex.push(b);
                    self.in_sysex = false;
                    if let Some(tc) = parse_full_frame(&self.sysex) {
                        self.locate();
                        out(MtcUpdate::Locate { tc, ts });
                    }
                    self.sysex.clear();
                    continue;
                }
                if b & 0x80 == 0 {
                    if self.sysex.len() < 64 {
                        self.sysex.push(b);
                    }
                    continue;
                }
                // a status byte aborts an unterminated SysEx
                self.in_sysex = false;
                self.sysex.clear();
            }
            if self.pending_qf {
                self.pending_qf = false;
                if b & 0x80 == 0 {
                    if let Some(u) = self.quarter(b, ts) {
                        out(u);
                    }
                    continue;
                }
            }
            match b {
                0xF0 => {
                    self.in_sysex = true;
                    self.sysex.clear();
                    self.sysex.push(b);
                }
                QF => self.pending_qf = true,
                _ => {}
            }
        }
    }

    fn locate(&mut self) {
        self.have = 0;
        self.last_piece = None;
        self.group = None;
    }

    fn quarter(&mut self, data: u8, ts: Ts) -> Option<MtcUpdate> {
        let piece = (data >> 4) & 7;
        let val = data & 0x0F;
        // direction from piece order
        match self.last_piece {
            Some(p) if piece == (p + 1) & 7 => {
                if self.reverse {
                    self.reverse = false;
                    self.have = 0;
                    self.group = None;
                }
            }
            Some(p) if piece == (p + 7) & 7 => {
                if !self.reverse {
                    self.reverse = true;
                    self.have = 0;
                    self.group = None;
                }
            }
            Some(_) => {
                // lost messages: start over
                self.have = 0;
                self.group = None;
            }
            None => {}
        }
        self.last_piece = Some(piece);
        self.pieces[piece as usize] = val;
        self.have |= 1 << piece;
        let complete_at = if self.reverse { 0 } else { 7 };
        let start_at = if self.reverse { 7 } else { 0 };
        if piece == start_at {
            // a new group begins: keep only this piece
            self.have = 1 << piece;
            if let Some((tc, f)) = self.group {
                // predicted group frame (two frames per group)
                let next = if self.reverse { f.saturating_sub(2) } else { f + 2 };
                self.group = Some((Timecode::from_frames(next, tc.rate), next));
            }
        }
        if piece == complete_at && self.have == 0xFF {
            let p = self.pieces;
            let rate = FrameRate::from_mtc_code(p[7] >> 1);
            let tc = Timecode {
                frames: p[0] | ((p[1] & 1) << 4),
                seconds: p[2] | ((p[3] & 3) << 4),
                minutes: p[4] | ((p[5] & 3) << 4),
                hours: p[6] | ((p[7] & 1) << 4),
                rate,
            };
            if !tc.is_valid() {
                self.group = None;
                return None;
            }
            self.group = Some((tc, tc.to_frames()));
        }
        let (tc, f) = self.group?;
        // Position at this piece: forward piece k of group N = N + k/4; reverse mirrors it
        // (piece 7 arrives first at N + 2 frames, piece 0 last at N + 1/4).
        let frames = if self.reverse { f as f64 + (piece as f64 + 1.0) / 4.0 } else { f as f64 + piece as f64 / 4.0 };
        Some(MtcUpdate::Running { tc, frames, ts, reverse: self.reverse })
    }
}

/// Schedules MTC output for a position source. Call [`MtcGenerator::poll`] often (≤ 2 ms
/// apart); it emits every quarter frame that is due and a full-frame message when the
/// source stops or locates.
#[derive(Clone, Debug)]
pub struct MtcGenerator {
    pub rate: FrameRate,
    /// SysEx device id for full-frame messages.
    pub device: u8,
    /// Next quarter frame to send (absolute quarter index = frames × 4 + piece).
    next_q: Option<u64>,
    /// Last position a full frame was sent for while parked.
    parked: Option<u64>,
}

impl MtcGenerator {
    pub fn new(rate: FrameRate) -> Self {
        MtcGenerator { rate, device: 0x7F, next_q: None, parked: None }
    }

    /// `pos` = source position in seconds now; `running` = moving forward at play speed.
    pub fn poll(&mut self, pos: f64, running: bool, mut send: impl FnMut(&[u8])) {
        let fps = self.rate.fps();
        let frames = (pos.max(0.0) * fps + 1e-9).floor() as u64;
        if !running {
            self.next_q = None;
            if self.parked != Some(frames) {
                self.parked = Some(frames);
                send(&full_frame(&Timecode::from_frames(frames, self.rate), self.device));
            }
            return;
        }
        self.parked = None;
        let q_now = (pos.max(0.0) * fps * 4.0 + 1e-9).floor() as u64;
        let resync = match self.next_q {
            None => true,
            Some(n) => q_now + 8 < n || q_now > n + 8,
        };
        if resync {
            // locate the receiver, then start quarter frames on the next group boundary
            // (groups start on even frames)
            send(&full_frame(&Timecode::from_frames(frames, self.rate), self.device));
            self.next_q = Some(q_now.div_ceil(8) * 8);
        }
        let mut n = self.next_q.unwrap_or(0);
        while n <= q_now {
            let group_frame = (n / 8) * 2;
            let tc = Timecode::from_frames(group_frame, self.rate);
            send(&quarter_frame(&tc, (n % 8) as u8));
            n += 1;
        }
        self.next_q = Some(n);
    }

    /// Master-clock-free helper: seconds until the next quarter frame is due at `pos`.
    pub fn seconds_to_next(&self, pos: f64) -> f64 {
        let qd = 1.0 / (self.rate.fps() * 4.0);
        match self.next_q {
            Some(n) => (n as f64 * qd - pos).max(0.0),
            None => 0.0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_all(dec: &mut MtcDecoder, bytes: &[u8], ts: Ts) -> Vec<MtcUpdate> {
        let mut v = Vec::new();
        dec.feed(bytes, ts, |u| v.push(u));
        v
    }

    #[test]
    fn full_frame_roundtrip_all_rates() {
        for r in FrameRate::ALL {
            let tc = Timecode { hours: 23, minutes: 59, seconds: 58, frames: (r.nominal() - 1) as u8, rate: r };
            let m = full_frame(&tc, 0x7F);
            assert_eq!(parse_full_frame(&m), Some(tc));
            let mut d = MtcDecoder::new();
            let ups = decode_all(&mut d, &m, 5);
            assert_eq!(ups, vec![MtcUpdate::Locate { tc, ts: 5 }]);
        }
    }

    #[test]
    fn quarter_frames_roundtrip_including_drop_frame() {
        for r in FrameRate::ALL {
            let mut g = MtcGenerator::new(r);
            let mut dec = MtcDecoder::new();
            // start just before a drop-frame minute boundary
            let start = Timecode::parse(if r.drop_frame() { "00:00:59;20" } else { "10:00:59:20" }, r).unwrap();
            let t0 = start.to_seconds();
            let mut got: Vec<(f64, Timecode)> = Vec::new();
            let mut msgs: Vec<(f64, Vec<u8>)> = Vec::new();
            let step = 0.0005;
            let mut t = 0.0;
            while t < 1.0 {
                g.poll(t0 + t, true, |m| msgs.push((t, m.to_vec())));
                t += step;
            }
            for (t, m) in &msgs {
                dec.feed(m, (*t * 1e9) as Ts, |u| {
                    if let MtcUpdate::Running { tc, frames, .. } = u {
                        got.push((frames, tc));
                        // position within a quarter frame of the true time
                        let truth = (t0 + t) * r.fps();
                        assert!((frames - truth).abs() <= 0.26, "{r}: {frames} vs {truth}");
                    }
                });
            }
            assert!(got.len() > 80, "{r}: {} updates", got.len());
            // every group timecode is two frames after the previous one, across the DF skip
            let mut groups: Vec<Timecode> = got.iter().map(|g| g.1).collect();
            groups.dedup();
            for w in groups.windows(2) {
                assert_eq!(w[1].to_frames(), w[0].to_frames() + 2, "{r}: {} → {}", w[0], w[1]);
                assert!(w[1].is_valid());
            }
            if r.drop_frame() {
                assert!(groups.iter().any(|g| g.to_string() == "00:01:00;02"), "{groups:?}");
            }
        }
    }

    #[test]
    fn decoder_handles_running_status_realtime_and_split_sysex() {
        let tc = Timecode { hours: 1, minutes: 2, seconds: 3, frames: 4, rate: FrameRate::Fps25 };
        let ff = full_frame(&tc, 0x10);
        let mut d = MtcDecoder::new();
        let mut stream = vec![0xF8, ff[0], ff[1], 0xFE, ff[2], ff[3]];
        assert!(decode_all(&mut d, &stream, 1).is_empty());
        stream = ff[4..].to_vec();
        assert_eq!(decode_all(&mut d, &stream, 2), vec![MtcUpdate::Locate { tc, ts: 2 }]);
        // quarter frames in one buffer with clock bytes interleaved
        let mut qf = Vec::new();
        for m in quarter_frames(&tc) {
            qf.extend_from_slice(&m);
            qf.push(0xF8);
        }
        let ups = decode_all(&mut d, &qf, 3);
        assert_eq!(ups.len(), 1, "complete at piece 7");
        let MtcUpdate::Running { tc: got, frames, .. } = ups[0] else { panic!() };
        assert_eq!(got, tc);
        assert!((frames - (tc.to_frames() as f64 + 1.75)).abs() < 1e-9);
    }

    #[test]
    fn reverse_direction_is_detected() {
        let r = FrameRate::Fps30;
        let tc = Timecode::parse("00:00:10:00", r).unwrap();
        let mut d = MtcDecoder::new();
        let mut ups = Vec::new();
        // two reverse groups: 7..0 of N, then 7..0 of N-2
        for tcx in [tc, Timecode::from_frames(tc.to_frames() - 2, r), Timecode::from_frames(tc.to_frames() - 4, r)] {
            for p in (0..8).rev() {
                d.feed(&quarter_frame(&tcx, p), 0, |u| ups.push(u));
            }
        }
        let last = ups.last().unwrap();
        let MtcUpdate::Running { tc: g, reverse, frames, .. } = *last else { panic!() };
        assert!(reverse);
        assert_eq!(g.to_frames(), tc.to_frames() - 4);
        assert!((frames - (g.to_frames() as f64 + 0.25)).abs() < 1e-9);
    }

    #[test]
    fn generator_sends_full_frame_when_parked_and_on_jump() {
        let mut g = MtcGenerator::new(FrameRate::Fps25);
        let mut out = Vec::new();
        g.poll(10.0, false, |m| out.push(m.to_vec()));
        g.poll(10.0, false, |m| out.push(m.to_vec()));
        assert_eq!(out.len(), 1, "one locate while parked");
        assert_eq!(parse_full_frame(&out[0]).unwrap().to_string(), "00:00:10:00");
        out.clear();
        g.poll(10.0, true, |m| out.push(m.to_vec()));
        assert_eq!(out.len(), 2, "resync full frame + piece 0 at an even frame");
        out.clear();
        g.poll(60.0, true, |m| out.push(m.to_vec()));
        assert_eq!(parse_full_frame(&out[0]).unwrap().to_string(), "00:01:00:00", "jump re-locates");
    }
}
