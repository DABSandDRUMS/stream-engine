//! MIDI 1.0 byte-stream parsing: running status, realtime bytes interleaved anywhere
//! (including inside SysEx), SysEx framing, and serialization back to bytes.

/// One complete MIDI message. Channels are 0-based (0 = MIDI channel 1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Msg {
    NoteOff {
        ch: u8,
        note: u8,
        vel: u8,
    },
    /// Kept as sent: velocity 0 is a note-off by convention and handled by the decoder.
    NoteOn {
        ch: u8,
        note: u8,
        vel: u8,
    },
    PolyPressure {
        ch: u8,
        note: u8,
        value: u8,
    },
    Cc {
        ch: u8,
        cc: u8,
        value: u8,
    },
    Program {
        ch: u8,
        program: u8,
    },
    ChannelPressure {
        ch: u8,
        value: u8,
    },
    /// 14-bit, 8192 = center.
    PitchBend {
        ch: u8,
        value: u16,
    },
    /// Complete SysEx including the leading `F0` and trailing `F7`.
    SysEx(Vec<u8>),
    /// MTC quarter frame data byte (`F1 xx`).
    QuarterFrame(u8),
    SongPosition(u16),
    SongSelect(u8),
    TuneRequest,
    /// Single-byte realtime message (`F8` clock, `FA` start, `FB` continue, `FC` stop, `FE`, `FF`).
    Realtime(u8),
}

impl Msg {
    pub fn channel(&self) -> Option<u8> {
        match *self {
            Msg::NoteOff { ch, .. }
            | Msg::NoteOn { ch, .. }
            | Msg::PolyPressure { ch, .. }
            | Msg::Cc { ch, .. }
            | Msg::Program { ch, .. }
            | Msg::ChannelPressure { ch, .. }
            | Msg::PitchBend { ch, .. } => Some(ch),
            _ => None,
        }
    }

    /// Append the canonical wire bytes (always with a status byte).
    pub fn write(&self, out: &mut Vec<u8>) {
        match self {
            Msg::NoteOff { ch, note, vel } => out.extend_from_slice(&[0x80 | ch, *note, *vel]),
            Msg::NoteOn { ch, note, vel } => out.extend_from_slice(&[0x90 | ch, *note, *vel]),
            Msg::PolyPressure { ch, note, value } => out.extend_from_slice(&[0xA0 | ch, *note, *value]),
            Msg::Cc { ch, cc, value } => out.extend_from_slice(&[0xB0 | ch, *cc, *value]),
            Msg::Program { ch, program } => out.extend_from_slice(&[0xC0 | ch, *program]),
            Msg::ChannelPressure { ch, value } => out.extend_from_slice(&[0xD0 | ch, *value]),
            Msg::PitchBend { ch, value } => out.extend_from_slice(&[0xE0 | ch, (value & 0x7F) as u8, ((value >> 7) & 0x7F) as u8]),
            Msg::SysEx(b) => out.extend_from_slice(b),
            Msg::QuarterFrame(d) => out.extend_from_slice(&[0xF1, *d]),
            Msg::SongPosition(p) => out.extend_from_slice(&[0xF2, (p & 0x7F) as u8, ((p >> 7) & 0x7F) as u8]),
            Msg::SongSelect(s) => out.extend_from_slice(&[0xF3, *s]),
            Msg::TuneRequest => out.push(0xF6),
            Msg::Realtime(b) => out.push(*b),
        }
    }

    pub fn to_bytes(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(3);
        self.write(&mut v);
        v
    }

    /// Parse exactly one message from `bytes` (must start with a status byte).
    pub fn from_bytes(bytes: &[u8]) -> Option<Msg> {
        let mut p = Parser::default();
        let mut found = None;
        p.feed(bytes, |m| {
            if found.is_none() {
                found = Some(m);
            }
        });
        found
    }
}

/// Data bytes that follow a channel/system-common status byte.
fn data_len(status: u8) -> usize {
    match status & 0xF0 {
        0x80 | 0x90 | 0xA0 | 0xB0 | 0xE0 => 2,
        0xC0 | 0xD0 => 1,
        _ => match status {
            0xF1 | 0xF3 => 1,
            0xF2 => 2,
            _ => 0,
        },
    }
}

/// Incremental parser. Feed any chunking of a byte stream; complete messages come out in order.
pub struct Parser {
    running: u8,
    data: [u8; 2],
    n: usize,
    sysex: Option<Vec<u8>>,
    /// SysEx longer than this is dropped (protects against a stuck stream).
    pub max_sysex: usize,
}

impl Default for Parser {
    fn default() -> Self {
        Parser { running: 0, data: [0; 2], n: 0, sysex: None, max_sysex: 64 * 1024 }
    }
}

impl Parser {
    pub fn reset(&mut self) {
        self.running = 0;
        self.n = 0;
        self.sysex = None;
    }

    pub fn feed(&mut self, bytes: &[u8], mut out: impl FnMut(Msg)) {
        for &b in bytes {
            self.push(b, &mut out);
        }
    }

    pub fn push(&mut self, b: u8, out: &mut impl FnMut(Msg)) {
        if b >= 0xF8 {
            // realtime: may appear anywhere, never affects running status or SysEx
            if b != 0xF9 && b != 0xFD {
                out(Msg::Realtime(b));
            }
            return;
        }
        if b & 0x80 != 0 {
            // any status byte terminates a SysEx
            if let Some(mut sx) = self.sysex.take() {
                sx.push(0xF7);
                out(Msg::SysEx(sx));
                if b == 0xF7 {
                    return;
                }
            }
            match b {
                0xF0 => {
                    self.sysex = Some(vec![0xF0]);
                    self.running = 0;
                }
                0xF7 => {} // stray EOX
                0xF6 => {
                    self.running = 0;
                    out(Msg::TuneRequest);
                }
                0xF4 | 0xF5 => self.running = 0,
                _ => {
                    self.running = b;
                    self.n = 0;
                }
            }
            return;
        }
        // data byte
        if let Some(sx) = &mut self.sysex {
            if sx.len() < self.max_sysex {
                sx.push(b);
            } else {
                self.sysex = None;
            }
            return;
        }
        if self.running == 0 {
            return; // data without status: ignore
        }
        let need = data_len(self.running);
        self.data[self.n] = b;
        self.n += 1;
        if self.n < need {
            return;
        }
        self.n = 0;
        let s = self.running;
        let ch = s & 0x0F;
        let (d0, d1) = (self.data[0], self.data[1]);
        let m = match s & 0xF0 {
            0x80 => Msg::NoteOff { ch, note: d0, vel: d1 },
            0x90 => Msg::NoteOn { ch, note: d0, vel: d1 },
            0xA0 => Msg::PolyPressure { ch, note: d0, value: d1 },
            0xB0 => Msg::Cc { ch, cc: d0, value: d1 },
            0xC0 => Msg::Program { ch, program: d0 },
            0xD0 => Msg::ChannelPressure { ch, value: d0 },
            0xE0 => Msg::PitchBend { ch, value: (d0 as u16) | ((d1 as u16) << 7) },
            _ => {
                // system common messages cancel running status
                self.running = 0;
                match s {
                    0xF1 => Msg::QuarterFrame(d0),
                    0xF2 => Msg::SongPosition((d0 as u16) | ((d1 as u16) << 7)),
                    0xF3 => Msg::SongSelect(d0),
                    _ => return,
                }
            }
        };
        out(m);
    }
}

/// Human-readable hex (`B0 10 41`).
pub fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 3);
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{b:02X}"));
    }
    s
}

/// Parse `"B0 10 41"` / `"b01041"` / `"0xB0,0x10"`.
pub fn parse_hex(s: &str) -> Option<Vec<u8>> {
    let cleaned: String = s.replace("0x", "").replace("0X", "").chars().filter(|c| c.is_ascii_hexdigit()).collect();
    if cleaned.is_empty() || !cleaned.len().is_multiple_of(2) {
        return None;
    }
    (0..cleaned.len()).step_by(2).map(|i| u8::from_str_radix(&cleaned[i..i + 2], 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(bytes: &[u8]) -> Vec<Msg> {
        let mut p = Parser::default();
        let mut v = Vec::new();
        p.feed(bytes, |m| v.push(m));
        v
    }

    #[test]
    fn running_status_and_realtime_interleave() {
        // CC 16 twice with running status, a clock byte in the middle of the second message
        let v = all(&[0xB0, 0x10, 0x41, 0x10, 0xF8, 0x01]);
        assert_eq!(v, vec![Msg::Cc { ch: 0, cc: 16, value: 0x41 }, Msg::Realtime(0xF8), Msg::Cc { ch: 0, cc: 16, value: 1 }]);
    }

    #[test]
    fn sysex_with_realtime_inside_and_chunked_feed() {
        let mut p = Parser::default();
        let mut v = Vec::new();
        p.feed(&[0xF0, 0x00, 0x00], |m| v.push(m));
        p.feed(&[0xFE, 0x66, 0x14], |m| v.push(m));
        p.feed(&[0x12, 0x00, 0x41, 0xF7, 0x90, 0x20, 0x7F], |m| v.push(m));
        assert_eq!(v[0], Msg::Realtime(0xFE));
        assert_eq!(v[1], Msg::SysEx(vec![0xF0, 0x00, 0x00, 0x66, 0x14, 0x12, 0x00, 0x41, 0xF7]));
        assert_eq!(v[2], Msg::NoteOn { ch: 0, note: 0x20, vel: 0x7F });
    }

    #[test]
    fn system_common_cancels_running_status() {
        let v = all(&[0x90, 0x24, 0x40, 0xF1, 0x21, 0x24, 0x00]);
        // the trailing data bytes have no running status after F1 → ignored
        assert_eq!(v, vec![Msg::NoteOn { ch: 0, note: 0x24, vel: 0x40 }, Msg::QuarterFrame(0x21)]);
    }

    #[test]
    fn pitch_bend_14_bit_round_trip() {
        let m = Msg::PitchBend { ch: 8, value: 0x3FFF };
        assert_eq!(m.to_bytes(), vec![0xE8, 0x7F, 0x7F]);
        assert_eq!(Msg::from_bytes(&[0xE8, 0x00, 0x40]), Some(Msg::PitchBend { ch: 8, value: 8192 }));
    }

    #[test]
    fn unterminated_sysex_is_closed_by_next_status() {
        let v = all(&[0xF0, 0x7E, 0x01, 0x80, 0x10, 0x00]);
        assert_eq!(v, vec![Msg::SysEx(vec![0xF0, 0x7E, 0x01, 0xF7]), Msg::NoteOff { ch: 0, note: 0x10, vel: 0 }]);
    }

    #[test]
    fn hex_round_trip() {
        assert_eq!(parse_hex("B0 10 41"), Some(vec![0xB0, 0x10, 0x41]));
        assert_eq!(parse_hex("0xb0,0x7f"), Some(vec![0xB0, 0x7F]));
        assert_eq!(parse_hex("B"), None);
        assert_eq!(hex(&[0xF0, 0x7F]), "F0 7F");
    }
}
