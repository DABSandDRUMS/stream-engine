//! UDP meter datagrams (`MS` `levl`). Port of `MeterServer.ts`.
//!
//! Layout after the 12-byte header: 4-byte kind (`levl`/`redu`/`rtan`), 2 bytes, u16 LE value
//! count, the values (u16 LE, linear 0–65535 = 0–1 full scale), a u8 descriptor count, then
//! descriptors of (u16 **BE** group id, u16 LE offset, u16 LE count). Consoles trim trailing
//! zero channels from a group and omit all-zero groups, so anything absent is silence.

use super::packet::{self, Code};
use thiserror::Error;

pub mod group {
    /// Input signal of every line channel (pre-gate/dynamics/fader).
    pub const INPUT: u16 = 0x0000;
    pub const INPUT_GATE_IN: u16 = 0x0001;
    pub const INPUT_GATE_OUT: u16 = 0x0002;
    pub const INPUT_STRIP1_OUT: u16 = 0x0003;
    pub const INPUT_STRIP2_OUT: u16 = 0x0004;
    pub const INPUT_LIMITER_OUT: u16 = 0x0005;
    pub const RETURNS: u16 = 0x0200;
    pub const AUX_SENDS: u16 = 0x0400;
    pub const AUX_LIMITER_OUT: u16 = 0x0405;
    pub const FX_SENDS: u16 = 0x0500;
    pub const MAIN_SENDS: u16 = 0x0700;
    pub const MAIN_LIMITER_OUT: u16 = 0x0705;
}

#[derive(Debug, Error, PartialEq)]
pub enum MeterError {
    #[error(transparent)]
    Packet(#[from] packet::PacketError),
    #[error("meter datagram truncated")]
    Truncated,
    #[error("meter datagram length {actual} does not match its descriptors ({expected})")]
    Length { expected: usize, actual: usize },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    pub group: u16,
    pub offset: u16,
    pub count: u16,
}

/// One level frame, reused across datagrams to avoid per-packet allocation.
#[derive(Debug, Default, Clone)]
pub struct LevelFrame {
    pub values: Vec<u16>,
    pub groups: Vec<Descriptor>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum MeterKind {
    Level,
    /// Gain reduction (`redu`), not decoded.
    Reduction,
    /// Real-time analyser (`rtan`), not decoded.
    Analyzer,
    Other([u8; 4]),
}

impl LevelFrame {
    /// Parse a datagram into `self`. Returns the kind; `self` holds data only for `Level`.
    pub fn parse(&mut self, datagram: &[u8]) -> Result<MeterKind, MeterError> {
        let f = packet::parse_lenient(datagram)?;
        if f.code != Code::METER16 {
            return Ok(MeterKind::Other([f.code.0[0], f.code.0[1], 0, 0]));
        }
        let b = f.body;
        let kind: [u8; 4] = b.get(..4).and_then(|s| s.try_into().ok()).ok_or(MeterError::Truncated)?;
        match &kind {
            b"levl" => {}
            b"redu" => return Ok(MeterKind::Reduction),
            b"rtan" => return Ok(MeterKind::Analyzer),
            _ => return Ok(MeterKind::Other(kind)),
        }
        let n = b.get(6..8).map(|s| u16::from_le_bytes([s[0], s[1]])).ok_or(MeterError::Truncated)? as usize;
        let dc_at = 8 + n * 2;
        let dcount = *b.get(dc_at).ok_or(MeterError::Truncated)? as usize;
        let expected = dc_at + 1 + dcount * 6;
        if expected != b.len() {
            return Err(MeterError::Length { expected: expected + packet::HEADER_LEN, actual: datagram.len() });
        }
        self.values.clear();
        self.values.extend(b[8..dc_at].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)));
        self.groups.clear();
        for d in b[dc_at + 1..].as_chunks::<6>().0 {
            let desc =
                Descriptor { group: u16::from_be_bytes([d[0], d[1]]), offset: u16::from_le_bytes([d[2], d[3]]), count: u16::from_le_bytes([d[4], d[5]]) };
            if desc.offset as usize + desc.count as usize > n {
                return Err(MeterError::Truncated);
            }
            self.groups.push(desc);
        }
        Ok(MeterKind::Level)
    }

    /// Raw values of a group (empty when the console omitted it = all silent).
    pub fn group(&self, id: u16) -> &[u16] {
        self.groups.iter().find(|d| d.group == id).map(|d| &self.values[d.offset as usize..d.offset as usize + d.count as usize]).unwrap_or(&[])
    }

    /// Linear level 0–1 of channel index `i` (0-based) in `group`; absent = 0.
    pub fn level(&self, group: u16, i: usize) -> f32 {
        self.group(group).get(i).map(|v| *v as f32 / 65535.0).unwrap_or(0.0)
    }
}

/// Linear amplitude → dBFS (floor −120).
pub fn to_db(level: f32) -> f32 {
    if level <= 1e-6 { -120.0 } else { 20.0 * level.log10() }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn datagram(values: &[u16], descs: &[(u16, u16, u16)]) -> Vec<u8> {
        let mut b = b"levl".to_vec();
        b.extend([0, 0]);
        b.extend((values.len() as u16).to_le_bytes());
        for v in values {
            b.extend(v.to_le_bytes());
        }
        b.push(descs.len() as u8);
        for (g, o, c) in descs {
            b.extend(g.to_be_bytes());
            b.extend(o.to_le_bytes());
            b.extend(c.to_le_bytes());
        }
        let mut p = packet::encode(Code::METER16, &b).unwrap();
        // consoles put the port (53000) in the length field of meter datagrams
        p[4..6].copy_from_slice(&53000u16.to_le_bytes());
        p
    }

    #[test]
    fn trimmed_and_absent_groups_read_as_silence() {
        let d = datagram(&[0, 0, 65535, 3, 3], &[(group::INPUT, 0, 3), (group::MAIN_LIMITER_OUT, 3, 2)]);
        let mut f = LevelFrame::default();
        assert_eq!(f.parse(&d).unwrap(), MeterKind::Level);
        assert_eq!(f.level(group::INPUT, 2), 1.0);
        assert_eq!(f.level(group::INPUT, 15), 0.0);
        assert_eq!(f.group(group::AUX_LIMITER_OUT), &[] as &[u16]);
        assert_eq!(f.group(group::MAIN_LIMITER_OUT), &[3, 3]);
    }

    #[test]
    fn inconsistent_datagrams_are_rejected() {
        let mut d = datagram(&[1, 2], &[(group::INPUT, 0, 2)]);
        d.push(0);
        let mut f = LevelFrame::default();
        assert!(matches!(f.parse(&d), Err(MeterError::Length { .. })));
        let d = datagram(&[1, 2], &[(group::INPUT, 1, 2)]);
        assert_eq!(f.parse(&d), Err(MeterError::Truncated));
    }

    #[test]
    fn db_conversion() {
        assert_eq!(to_db(0.0), -120.0);
        assert!((to_db(1.0)).abs() < 1e-6);
        assert!((to_db(0.1) + 20.0).abs() < 1e-4);
    }
}
