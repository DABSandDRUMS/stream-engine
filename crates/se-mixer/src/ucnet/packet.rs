//! UCNET framing: `"UC\0\x01"` magic, u16 LE payload length, 2-byte message code, 4 connection
//! identity bytes, body. Port of `util/messageProtocol.ts` + `util/DataClient.ts`.

use thiserror::Error;

pub const MAGIC: [u8; 4] = [0x55, 0x43, 0x00, 0x01];
/// Control (TCP) and meter-source (UDP) port of StudioLive III consoles.
pub const CONTROL_PORT: u16 = 53000;
/// Consoles broadcast their announcement to this UDP port every 3 s.
pub const DISCOVERY_PORT: u16 = 47809;
/// Default connection identity bytes (`CByte.A`, `CByte.B` in the reference).
pub const CBYTES: [u8; 4] = [0x68, 0x00, 0x65, 0x00];
/// Header before the body: magic (4) + length (2) + code (2) + identity (4).
pub const HEADER_LEN: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Code(pub [u8; 2]);

impl Code {
    pub const KEEP_ALIVE: Code = Code(*b"KA");
    pub const HELLO: Code = Code(*b"UM");
    pub const JSON: Code = Code(*b"JM");
    pub const PARAM_VALUE: Code = Code(*b"PV");
    pub const PARAM_CHARS: Code = Code(*b"PC");
    pub const PARAM_STRING: Code = Code(*b"PS");
    pub const PARAM_STR_LIST: Code = Code(*b"PL");
    pub const FILE_REQUEST: Code = Code(*b"FR");
    pub const FILE_DATA: Code = Code(*b"FD");
    pub const ZLIB: Code = Code(*b"ZB");
    pub const BINARY_OBJECT: Code = Code(*b"BO");
    pub const CHUNK: Code = Code(*b"CK");
    pub const METER8: Code = Code(*b"MB");
    /// Fader positions over TCP (`fdrs`) and meters over UDP (`levl`, `redu`, `rtan`).
    pub const METER16: Code = Code(*b"MS");
    /// Discovery announcement (UDP broadcast).
    pub const DISCOVERY: Code = Code(*b"DA");

    pub fn as_str(&self) -> &str {
        std::str::from_utf8(&self.0).unwrap_or("??")
    }
}

impl std::fmt::Debug for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PacketError {
    #[error("not a UCNET packet (bad magic)")]
    BadMagic,
    #[error("truncated packet ({0} bytes)")]
    Truncated(usize),
    #[error("length field says {expected} bytes, packet has {actual}")]
    LengthMismatch { expected: usize, actual: usize },
    #[error("payload too large for UCNET ({0} bytes)")]
    TooLarge(usize),
}

/// A decoded packet borrowing its body.
#[derive(Debug, Clone, Copy)]
pub struct Frame<'a> {
    pub code: Code,
    pub cbytes: [u8; 4],
    pub body: &'a [u8],
}

/// Encode a packet with the default identity bytes.
pub fn encode(code: Code, body: &[u8]) -> Result<Vec<u8>, PacketError> {
    encode_with(code, CBYTES, body)
}

/// Encode a packet with explicit identity bytes (the meter hello uses `[0, 0, 0x65, 0]`).
pub fn encode_with(code: Code, cbytes: [u8; 4], body: &[u8]) -> Result<Vec<u8>, PacketError> {
    let len = 2 + 4 + body.len();
    let len16 = u16::try_from(len).map_err(|_| PacketError::TooLarge(len))?;
    let mut out = Vec::with_capacity(HEADER_LEN + body.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&len16.to_le_bytes());
    out.extend_from_slice(&code.0);
    out.extend_from_slice(&cbytes);
    out.extend_from_slice(body);
    Ok(out)
}

/// Decode a complete packet; the length field must match.
pub fn parse(packet: &[u8]) -> Result<Frame<'_>, PacketError> {
    let f = parse_lenient(packet)?;
    let expected = u16::from_le_bytes([packet[4], packet[5]]) as usize + 6;
    if expected != packet.len() {
        return Err(PacketError::LengthMismatch { expected, actual: packet.len() });
    }
    Ok(f)
}

/// Decode ignoring the length field. UDP meter and discovery datagrams carry the port
/// (53000) there instead of the payload length.
pub fn parse_lenient(packet: &[u8]) -> Result<Frame<'_>, PacketError> {
    if packet.len() < 4 || packet[..4] != MAGIC {
        return Err(PacketError::BadMagic);
    }
    if packet.len() < HEADER_LEN {
        return Err(PacketError::Truncated(packet.len()));
    }
    Ok(Frame { code: Code([packet[6], packet[7]]), cbytes: [packet[8], packet[9], packet[10], packet[11]], body: &packet[HEADER_LEN..] })
}

/// Splits the TCP byte stream into packets (a payload may span reads, one read may carry
/// several packets).
#[derive(Default)]
pub struct Reassembler {
    buf: Vec<u8>,
    start: usize,
}

impl Reassembler {
    pub fn push(&mut self, data: &[u8]) {
        if self.start > 0 && self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
        } else if self.start > 64 * 1024 {
            self.buf.drain(..self.start);
            self.start = 0;
        }
        self.buf.extend_from_slice(data);
    }

    /// Next complete packet, `Ok(None)` when more bytes are needed. A bad magic means the
    /// stream is out of sync; the connection must be dropped.
    pub fn next_packet(&mut self) -> Result<Option<Vec<u8>>, PacketError> {
        let avail = &self.buf[self.start..];
        if avail.len() < 6 {
            if !avail.is_empty() && !MAGIC.starts_with(&avail[..avail.len().min(4)]) {
                return Err(PacketError::BadMagic);
            }
            return Ok(None);
        }
        if avail[..4] != MAGIC {
            return Err(PacketError::BadMagic);
        }
        let total = u16::from_le_bytes([avail[4], avail[5]]) as usize + 6;
        if total < HEADER_LEN {
            return Err(PacketError::Truncated(total));
        }
        if avail.len() < total {
            return Ok(None);
        }
        let p = avail[..total].to_vec();
        self.start += total;
        Ok(Some(p))
    }

    /// Bytes buffered but not yet returned.
    pub fn pending(&self) -> usize {
        self.buf.len() - self.start
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_matches_reference_layout() {
        // createPacket("KA") in the reference: magic, len = 6 (code + identity), "KA", 68 00 65 00
        let p = encode(Code::KEEP_ALIVE, &[]).unwrap();
        assert_eq!(p, [0x55, 0x43, 0x00, 0x01, 0x06, 0x00, b'K', b'A', 0x68, 0x00, 0x65, 0x00]);
        let f = parse(&p).unwrap();
        assert_eq!(f.code, Code::KEEP_ALIVE);
        assert!(f.body.is_empty());
    }

    #[test]
    fn parse_rejects_length_mismatch_but_lenient_accepts() {
        let mut p = encode(Code::PARAM_VALUE, b"line/ch1/mute\0\0\0\0\0\x80\x3f").unwrap();
        p.push(0);
        assert!(matches!(parse(&p), Err(PacketError::LengthMismatch { .. })));
        assert_eq!(parse_lenient(&p).unwrap().code, Code::PARAM_VALUE);
        assert!(matches!(parse(b"XX\0\x01\x06\0KA\0\0\0\0"), Err(PacketError::BadMagic)));
    }

    #[test]
    fn reassembles_split_and_coalesced_packets() {
        let a = encode(Code::KEEP_ALIVE, &[]).unwrap();
        let b = encode(Code::JSON, &[1u8; 300]).unwrap();
        let mut stream = a.clone();
        stream.extend_from_slice(&b);
        let mut r = Reassembler::default();
        // byte-by-byte for the first part, then the rest in one go
        for byte in &stream[..20] {
            r.push(std::slice::from_ref(byte));
        }
        assert_eq!(r.next_packet().unwrap(), Some(a));
        assert_eq!(r.next_packet().unwrap(), None);
        r.push(&stream[20..]);
        assert_eq!(r.next_packet().unwrap(), Some(b));
        assert_eq!(r.next_packet().unwrap(), None);
        assert_eq!(r.pending(), 0);
    }

    #[test]
    fn desync_is_an_error() {
        let mut r = Reassembler::default();
        r.push(b"garbage!");
        assert_eq!(r.next_packet(), Err(PacketError::BadMagic));
    }
}
