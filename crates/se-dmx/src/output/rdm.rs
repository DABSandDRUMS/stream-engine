//! RDM (ANSI E1.20): request encoding, response decoding, discovery-unique-branch (DUB) reply
//! decoding and the controller side of device discovery.
//!
//! Packet layout (E1.20 §6.2, offsets include the start code):
//!
//! | offset | field                                   |
//! |--------|-----------------------------------------|
//! | 0      | START Code `0xCC`                       |
//! | 1      | Sub-START Code `0x01`                   |
//! | 2      | Message Length (24 + PDL)               |
//! | 3..9   | Destination UID                         |
//! | 9..15  | Source UID                              |
//! | 15     | Transaction Number                      |
//! | 16     | Port ID (request) / Response Type       |
//! | 17     | Message Count                           |
//! | 18..20 | Sub-Device                              |
//! | 20     | Command Class                           |
//! | 21..23 | Parameter ID                            |
//! | 23     | Parameter Data Length (PDL)             |
//! | 24..   | Parameter Data                          |
//! | 24+PDL | Checksum (16-bit sum of bytes 0..24+PDL) |
//!
//! Everything here is control-path code (discovery and device queries run off the output thread);
//! allocation is fine.

use std::collections::BTreeSet;
use std::fmt;
use std::io;

/// RDM start code (the DMX512 alternate START code for RDM).
pub const START_CODE: u8 = 0xCC;
/// E1.20 sub-START code for RDM messages.
pub const SUB_START_CODE: u8 = 0x01;
/// Largest parameter data length a message may carry (E1.20 §6.2.4).
pub const MAX_PDL: usize = 231;
/// Highest assignable UID; discovery searches `0..=MAX_UID` (`FFFF:FFFFFFFF` is broadcast).
pub const MAX_UID: u64 = 0xFFFF_FFFF_FFFE;
/// Upper bound on transport calls one [`discover`] run may make before it gives up.
pub const MAX_DISCOVERY_REQUESTS: u32 = 20_000;

/// Header bytes before the parameter data (start code through PDL).
const HEADER_LEN: usize = 24;
/// Attempts for the DUB over the whole UID space before concluding the bus is empty.
const FULL_RANGE_DUB_ATTEMPTS: u32 = 3;
/// Attempts to mute a device that answered a DUB before treating the answer as a phantom.
const MUTE_ATTEMPTS: u32 = 3;
/// Attempts for the mandatory DEVICE_INFO GET.
const DEVICE_INFO_ATTEMPTS: u32 = 2;
/// Port ID used in controller requests (E1.20: 1..=255, 1 = first port).
const PORT_ID: u8 = 1;
/// Maximum number of 0xFE preamble bytes in a DUB response.
const DUB_MAX_PREAMBLE: usize = 7;
/// Preamble separator of a DUB response.
const DUB_SEPARATOR: u8 = 0xAA;
/// Encoded EUID (12) + encoded checksum (4) bytes following the separator.
const DUB_BODY_LEN: usize = 16;

/// A 48-bit RDM unique ID: `manufacturer << 32 | device`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize)]
pub struct Uid(pub u64);

impl Uid {
    /// All-call broadcast `FFFF:FFFFFFFF`.
    pub const BROADCAST: Uid = Uid(0xFFFF_FFFF_FFFF);

    pub const fn new(manufacturer: u16, device: u32) -> Uid {
        Uid(((manufacturer as u64) << 32) | device as u64)
    }

    pub fn manufacturer(self) -> u16 {
        (self.0 >> 32) as u16
    }

    pub fn device(self) -> u32 {
        self.0 as u32
    }

    /// True for the all-call broadcast and manufacturer broadcasts (`mmmm:FFFFFFFF`), which
    /// responders never answer (except DISC_UNIQUE_BRANCH).
    pub fn is_broadcast(self) -> bool {
        self.device() == 0xFFFF_FFFF
    }

    /// Big-endian wire form.
    pub fn to_bytes(self) -> [u8; 6] {
        let b = self.0.to_be_bytes();
        [b[2], b[3], b[4], b[5], b[6], b[7]]
    }

    /// Reads the first six bytes (big-endian wire form).
    ///
    /// # Panics
    /// If `bytes` is shorter than six bytes.
    pub fn from_bytes(bytes: &[u8]) -> Uid {
        let mut b = [0u8; 8];
        b[2..].copy_from_slice(&bytes[..6]);
        Uid(u64::from_be_bytes(b))
    }
}

impl fmt::Display for Uid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04X}:{:08X}", self.manufacturer(), self.device())
    }
}

/// Command classes (E1.20 Table A-1).
pub mod cc {
    pub const DISCOVERY: u8 = 0x10;
    pub const DISCOVERY_RESPONSE: u8 = 0x11;
    pub const GET: u8 = 0x20;
    pub const GET_RESPONSE: u8 = 0x21;
    pub const SET: u8 = 0x30;
    pub const SET_RESPONSE: u8 = 0x31;
}

/// Parameter IDs (E1.20 Table A-3).
pub mod pid {
    pub const DISC_UNIQUE_BRANCH: u16 = 0x0001;
    pub const DISC_MUTE: u16 = 0x0002;
    pub const DISC_UN_MUTE: u16 = 0x0003;
    pub const DEVICE_INFO: u16 = 0x0060;
    pub const DEVICE_MODEL_DESCRIPTION: u16 = 0x0080;
    pub const MANUFACTURER_LABEL: u16 = 0x0081;
    pub const DEVICE_LABEL: u16 = 0x0082;
    pub const SOFTWARE_VERSION_LABEL: u16 = 0x00C0;
    pub const DMX_PERSONALITY_DESCRIPTION: u16 = 0x00E1;
    pub const DMX_START_ADDRESS: u16 = 0x00F0;
}

/// Response types (E1.20 Table A-2), carried in the port-id slot of responses.
pub mod response_type {
    pub const ACK: u8 = 0x00;
    pub const ACK_TIMER: u8 = 0x01;
    pub const NACK_REASON: u8 = 0x02;
    pub const ACK_OVERFLOW: u8 = 0x03;
}

fn checksum(bytes: &[u8]) -> u16 {
    bytes.iter().fold(0u16, |sum, &b| sum.wrapping_add(u16::from(b)))
}

/// Encodes one RDM message including the `0xCC` start code and the trailing checksum.
/// `port_id` is the port ID for requests or the response type for responses; the message count
/// is always 0 (controllers send 0).
///
/// # Panics
/// If `data` exceeds [`MAX_PDL`] bytes.
pub fn encode(dest: Uid, src: Uid, tn: u8, port_id: u8, sub_device: u16, cc: u8, pid: u16, data: &[u8]) -> Vec<u8> {
    assert!(data.len() <= MAX_PDL, "RDM parameter data is {} bytes, the maximum is {MAX_PDL}", data.len());
    let len = HEADER_LEN + data.len();
    let mut p = Vec::with_capacity(len + 2);
    p.extend_from_slice(&[START_CODE, SUB_START_CODE, len as u8]);
    p.extend_from_slice(&dest.to_bytes());
    p.extend_from_slice(&src.to_bytes());
    p.extend_from_slice(&[tn, port_id, 0]);
    p.extend_from_slice(&sub_device.to_be_bytes());
    p.push(cc);
    p.extend_from_slice(&pid.to_be_bytes());
    p.push(data.len() as u8);
    p.extend_from_slice(data);
    let sum = checksum(&p);
    p.extend_from_slice(&sum.to_be_bytes());
    p
}

/// A decoded RDM message (normally a responder's reply).
#[derive(Debug, Clone)]
pub struct Response {
    pub src: Uid,
    pub dest: Uid,
    pub tn: u8,
    pub response_type: u8,
    pub cc: u8,
    pub pid: u16,
    pub data: Vec<u8>,
}

/// Decodes one RDM message starting at its `0xCC` start code. Validates the start codes, the
/// message length against the PDL and the received byte count, and the checksum. Bytes after the
/// checksum (line noise after the response) are ignored.
pub fn decode(bytes: &[u8]) -> Result<Response, String> {
    if bytes.len() < HEADER_LEN + 2 {
        return Err(format!("RDM message too short ({} bytes, minimum {})", bytes.len(), HEADER_LEN + 2));
    }
    if bytes[0] != START_CODE || bytes[1] != SUB_START_CODE {
        return Err(format!("bad RDM start codes {:02X} {:02X}", bytes[0], bytes[1]));
    }
    let len = usize::from(bytes[2]);
    let pdl = usize::from(bytes[23]);
    if len != HEADER_LEN + pdl {
        return Err(format!("RDM message length {len} does not match parameter data length {pdl}"));
    }
    if bytes.len() < len + 2 {
        return Err(format!("RDM message truncated ({} of {} bytes)", bytes.len(), len + 2));
    }
    let expected = u16::from_be_bytes([bytes[len], bytes[len + 1]]);
    let actual = checksum(&bytes[..len]);
    if expected != actual {
        return Err(format!("RDM checksum mismatch (packet {expected:04X}, computed {actual:04X})"));
    }
    Ok(Response {
        dest: Uid::from_bytes(&bytes[3..9]),
        src: Uid::from_bytes(&bytes[9..15]),
        tn: bytes[15],
        response_type: bytes[16],
        cc: bytes[20],
        pid: u16::from_be_bytes([bytes[21], bytes[22]]),
        data: bytes[HEADER_LEN..len].to_vec(),
    })
}

/// Outcome of one DISC_UNIQUE_BRANCH request.
#[derive(Debug, PartialEq)]
pub enum Dub {
    /// Nothing answered.
    None,
    /// Exactly one well-formed answer.
    One(Uid),
    /// Bytes arrived but do not form a valid answer (several responders talked at once).
    Collision,
}

/// Decodes the raw bytes received after a DUB request: 0..=7 `0xFE` preamble bytes, the `0xAA`
/// separator, the 6 UID bytes each sent as `b | 0xAA, b | 0x55`, and the 16-bit sum of those 12
/// encoded bytes sent the same way (MSB first). Empty input is [`Dub::None`]; anything else that
/// fails structure or checksum is [`Dub::Collision`]. Trailing bytes are ignored.
pub fn decode_dub(bytes: &[u8]) -> Dub {
    if bytes.is_empty() {
        return Dub::None;
    }
    let preamble = bytes.iter().take_while(|&&b| b == 0xFE).count();
    if preamble > DUB_MAX_PREAMBLE || bytes.get(preamble) != Some(&DUB_SEPARATOR) {
        return Dub::Collision;
    }
    let Some(body) = bytes.get(preamble + 1..preamble + 1 + DUB_BODY_LEN) else {
        return Dub::Collision;
    };
    let mut decoded = [0u8; 8];
    for (out, &[hi, lo]) in decoded.iter_mut().zip(body.as_chunks::<2>().0) {
        if hi & 0xAA != 0xAA || lo & 0x55 != 0x55 {
            return Dub::Collision;
        }
        *out = hi & lo;
    }
    let sum = checksum(&body[..12]);
    if u16::from_be_bytes([decoded[6], decoded[7]]) != sum {
        return Dub::Collision;
    }
    Dub::One(Uid::from_bytes(&decoded[..6]))
}

/// Raw DUB response bytes for `uid` with `preamble` (clamped to 0..=7) `0xFE` bytes, as a
/// responder transmits them.
pub fn encode_dub_response(uid: Uid, preamble: usize) -> Vec<u8> {
    let preamble = preamble.min(DUB_MAX_PREAMBLE);
    let mut out = Vec::with_capacity(preamble + 1 + DUB_BODY_LEN);
    out.resize(preamble, 0xFE);
    out.push(DUB_SEPARATOR);
    for b in uid.to_bytes() {
        out.extend_from_slice(&[b | 0xAA, b | 0x55]);
    }
    let sum = checksum(&out[preamble + 1..]);
    for b in sum.to_be_bytes() {
        out.extend_from_slice(&[b | 0xAA, b | 0x55]);
    }
    out
}

/// A physical RDM port.
pub trait RdmTransport {
    /// Sends a DISC_UNIQUE_BRANCH packet (as built by [`encode`]) and collects the raw reply.
    fn dub(&mut self, packet: &[u8]) -> io::Result<Dub>;
    /// Sends any other RDM packet and waits for the response. Broadcast requests return
    /// `Ok(None)` right after sending; `Ok(None)` also means no response within the timeout.
    /// An undecodable response is an error of kind [`io::ErrorKind::InvalidData`].
    fn request(&mut self, packet: &[u8]) -> io::Result<Option<Response>>;
}

/// Standard device information plus the best-effort descriptive labels.
#[derive(Clone, Debug, serde::Serialize)]
pub struct DeviceInfo {
    pub uid: Uid,
    pub protocol: u16,
    pub model_id: u16,
    pub category: u16,
    pub software_version: u32,
    pub footprint: u16,
    pub personality: u8,
    pub personalities: u8,
    pub start_address: u16,
    pub sub_devices: u16,
    pub sensors: u8,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub label: Option<String>,
}

/// Controller-side request sequencing: transaction numbers and the request budget.
struct Session<'a, T: RdmTransport> {
    t: &'a mut T,
    src: Uid,
    tn: u8,
    used: u32,
    limit: u32,
}

impl<'a, T: RdmTransport> Session<'a, T> {
    fn new(t: &'a mut T, src: Uid, limit: u32) -> Self {
        Session { t, src, tn: 0, used: 0, limit }
    }

    fn next_tn(&mut self) -> io::Result<u8> {
        if self.used >= self.limit {
            return Err(io::Error::other(format!("RDM discovery aborted after {} requests (persistent bus noise or a misbehaving responder)", self.used)));
        }
        self.used += 1;
        let tn = self.tn;
        self.tn = self.tn.wrapping_add(1);
        Ok(tn)
    }

    fn dub(&mut self, lower: u64, upper: u64) -> io::Result<Dub> {
        let tn = self.next_tn()?;
        let mut range = [0u8; 12];
        range[..6].copy_from_slice(&Uid(lower).to_bytes());
        range[6..].copy_from_slice(&Uid(upper).to_bytes());
        let packet = encode(Uid::BROADCAST, self.src, tn, PORT_ID, 0, cc::DISCOVERY, pid::DISC_UNIQUE_BRANCH, &range);
        self.t.dub(&packet)
    }

    fn request(&mut self, dest: Uid, cc: u8, pid: u16, data: &[u8]) -> io::Result<Option<Response>> {
        let tn = self.next_tn()?;
        let packet = encode(dest, self.src, tn, PORT_ID, 0, cc, pid, data);
        self.t.request(&packet)
    }

    /// Mutes `uid`; true once it acknowledged. Garbled replies count as failed attempts.
    fn mute(&mut self, uid: Uid) -> io::Result<bool> {
        for _ in 0..MUTE_ATTEMPTS {
            match self.request(uid, cc::DISCOVERY, pid::DISC_MUTE, &[]) {
                Ok(Some(r)) if r.src == uid && r.cc == cc::DISCOVERY_RESPONSE && r.pid == pid::DISC_MUTE && r.response_type == response_type::ACK => {
                    return Ok(true);
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::InvalidData => {}
                Err(e) => return Err(e),
            }
        }
        Ok(false)
    }

    /// GET with an ACK'd response from `dest`, `None` for no response, NACK or deferred replies.
    fn get(&mut self, dest: Uid, pid: u16, attempts: u32) -> io::Result<Option<Vec<u8>>> {
        for _ in 0..attempts {
            match self.request(dest, cc::GET, pid, &[])? {
                Some(r) if r.src == dest && r.cc == cc::GET_RESPONSE && r.pid == pid => {
                    return Ok((r.response_type == response_type::ACK).then_some(r.data));
                }
                _ => {}
            }
        }
        Ok(None)
    }

    /// Best-effort label GET: transport failures other than hard IO errors read as absent.
    fn label(&mut self, dest: Uid, pid: u16) -> io::Result<Option<String>> {
        match self.get(dest, pid, 1) {
            Ok(data) => Ok(data.and_then(|d| parse_label(&d))),
            Err(e) if matches!(e.kind(), io::ErrorKind::InvalidData | io::ErrorKind::TimedOut) => Ok(None),
            Err(e) => Err(e),
        }
    }
}

/// ASCII label, up to the first NUL, trailing whitespace trimmed; empty = absent.
fn parse_label(data: &[u8]) -> Option<String> {
    let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
    let s = String::from_utf8_lossy(&data[..end]).trim_end().to_string();
    (!s.is_empty()).then_some(s)
}

fn split(ranges: &mut Vec<(u64, u64)>, lo: u64, hi: u64) {
    if lo < hi {
        let mid = lo + (hi - lo) / 2;
        // Upper half first so the lower half is searched next (ascending discovery order).
        ranges.push((mid + 1, hi));
        ranges.push((lo, mid));
    }
}

/// Finds every responder on the port: un-mutes all devices, then binary-searches the UID space
/// with DISC_UNIQUE_BRANCH. A clean single answer is confirmed with DISC_MUTE (a device that does
/// not acknowledge is treated as a phantom produced by a collision and the range is split
/// further); collisions split the range down to single UIDs. Returns the UIDs in ascending order,
/// each exactly once. Fails after [`MAX_DISCOVERY_REQUESTS`] transport calls.
pub fn discover(t: &mut impl RdmTransport, src: Uid) -> io::Result<Vec<Uid>> {
    discover_bounded(t, src, MAX_DISCOVERY_REQUESTS)
}

fn discover_bounded(t: &mut impl RdmTransport, src: Uid, limit: u32) -> io::Result<Vec<Uid>> {
    let mut s = Session::new(t, src, limit);
    s.request(Uid::BROADCAST, cc::DISCOVERY, pid::DISC_UN_MUTE, &[])?;
    let mut found = BTreeSet::new();
    let mut ranges = vec![(0u64, MAX_UID)];
    while let Some((lo, hi)) = ranges.pop() {
        let attempts = if (lo, hi) == (0, MAX_UID) { FULL_RANGE_DUB_ATTEMPTS } else { 1 };
        let mut reply = Dub::None;
        for _ in 0..attempts {
            reply = s.dub(lo, hi)?;
            if reply != Dub::None {
                break;
            }
        }
        match reply {
            Dub::None => {}
            Dub::One(uid) if (lo..=hi).contains(&uid.0) && !found.contains(&uid) => {
                if s.mute(uid)? {
                    found.insert(uid);
                    // Search the same branch again: other unmuted devices may share it.
                    ranges.push((lo, hi));
                } else {
                    split(&mut ranges, lo, hi);
                }
            }
            // Collision, an answer outside the requested range, or an already-found device that
            // lost its mute: all mean "more than a clean single answer here".
            _ if lo == hi => {
                let uid = Uid(lo);
                if !found.contains(&uid) && s.mute(uid)? {
                    found.insert(uid);
                }
            }
            _ => split(&mut ranges, lo, hi),
        }
    }
    Ok(found.into_iter().collect())
}

/// GET DEVICE_INFO from `dest` plus best-effort MANUFACTURER_LABEL, DEVICE_MODEL_DESCRIPTION and
/// DEVICE_LABEL. `Ok(None)` when the device does not answer DEVICE_INFO with an ACK.
pub fn device_info(t: &mut impl RdmTransport, src: Uid, dest: Uid) -> io::Result<Option<DeviceInfo>> {
    let mut s = Session::new(t, src, u32::MAX);
    let Some(d) = s.get(dest, pid::DEVICE_INFO, DEVICE_INFO_ATTEMPTS)? else {
        return Ok(None);
    };
    if d.len() < 19 {
        return Err(io::Error::new(io::ErrorKind::InvalidData, format!("DEVICE_INFO from {dest} has {} bytes of parameter data, expected 19", d.len())));
    }
    let u16_at = |i: usize| u16::from_be_bytes([d[i], d[i + 1]]);
    let mut info = DeviceInfo {
        uid: dest,
        protocol: u16_at(0),
        model_id: u16_at(2),
        category: u16_at(4),
        software_version: u32::from_be_bytes([d[6], d[7], d[8], d[9]]),
        footprint: u16_at(10),
        personality: d[12],
        personalities: d[13],
        start_address: u16_at(14),
        sub_devices: u16_at(16),
        sensors: d[18],
        manufacturer: None,
        model: None,
        label: None,
    };
    info.manufacturer = s.label(dest, pid::MANUFACTURER_LABEL)?;
    info.model = s.label(dest, pid::DEVICE_MODEL_DESCRIPTION)?;
    info.label = s.label(dest, pid::DEVICE_LABEL)?;
    Ok(Some(info))
}

#[cfg(test)]
mod tests {
    use super::*;

    const DEST: Uid = Uid(0x1234_5678_9ABC);
    const SRC: Uid = Uid(0x454E_0000_0001);

    #[test]
    fn uid_parts_bytes_and_display() {
        let u = Uid(0x454E_0405_589A);
        assert_eq!(u.manufacturer(), 0x454E);
        assert_eq!(u.device(), 0x0405_589A);
        assert_eq!(u.to_bytes(), [0x45, 0x4E, 0x04, 0x05, 0x58, 0x9A]);
        assert_eq!(Uid::from_bytes(&[0x45, 0x4E, 0x04, 0x05, 0x58, 0x9A, 0xFF]), u);
        assert_eq!(u.to_string(), "454E:0405589A");
        assert_eq!(Uid::new(0x454E, 0x0405_589A), u);
        assert_eq!(Uid(0x1).to_string(), "0000:00000001");
        assert!(Uid::BROADCAST.is_broadcast());
        assert!(Uid::new(0x454E, 0xFFFF_FFFF).is_broadcast());
        assert!(!Uid(MAX_UID).is_broadcast());
    }

    /// GET DEVICE_INFO 454E:00000001 → 1234:56789ABC, TN 5, port 1, root device.
    /// Checksum by hand: CC+01+18 = 0x0E5; dest 12+34+56+78+9A+BC = 0x26A; src 45+4E+00+00+00+01
    /// = 0x094; TN 05 + port 01 + count 00 + sub-device 00 00 = 0x006; CC 20 + PID 00 60 + PDL 00
    /// = 0x080. Total 0x0E5+0x26A+0x094+0x006+0x080 = 0x469.
    const GET_DEVICE_INFO: [u8; 26] = [
        0xCC, // 0: START Code
        0x01, // 1: Sub-START Code
        0x18, // 2: Message Length = 24
        0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, // 3..9: Destination UID
        0x45, 0x4E, 0x00, 0x00, 0x00, 0x01, // 9..15: Source UID
        0x05, // 15: Transaction Number
        0x01, // 16: Port ID
        0x00, // 17: Message Count
        0x00, 0x00, // 18..20: Sub-Device (root)
        0x20, // 20: Command Class GET_COMMAND
        0x00, 0x60, // 21..23: PID DEVICE_INFO
        0x00, // 23: PDL
        0x04, 0x69, // 24..26: Checksum
    ];

    #[test]
    fn encodes_get_device_info_byte_for_byte() {
        let p = encode(DEST, SRC, 5, 1, 0, cc::GET, pid::DEVICE_INFO, &[]);
        assert_eq!(p, GET_DEVICE_INFO);
    }

    #[test]
    fn encodes_parameter_data_and_sub_device() {
        // SET DMX_START_ADDRESS = 0x0101 on sub-device 0x0203.
        let p = encode(DEST, SRC, 0xFF, 1, 0x0203, cc::SET, pid::DMX_START_ADDRESS, &[0x01, 0x01]);
        let mut expected = vec![
            0xCC, 0x01, 0x1A, // start codes, length 24 + 2
            0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, // dest
            0x45, 0x4E, 0x00, 0x00, 0x00, 0x01, // src
            0xFF, 0x01, 0x00, // TN, port, message count
            0x02, 0x03, // sub-device
            0x30, // SET_COMMAND
            0x00, 0xF0, // DMX_START_ADDRESS
            0x02, // PDL
            0x01, 0x01, // data
        ];
        // CC+01+1A = 0x0E7; dest = 0x26A; src = 0x094; FF+01+00 = 0x100; 02+03 = 0x005; 30 = 0x030;
        // 00+F0 = 0x0F0; 02 = 0x002; 01+01 = 0x002. Total = 0x60E.
        expected.extend_from_slice(&[0x06, 0x0E]);
        assert_eq!(p, expected);
    }

    #[test]
    #[should_panic(expected = "maximum is 231")]
    fn encode_rejects_oversized_parameter_data() {
        encode(DEST, SRC, 0, 1, 0, cc::SET, pid::DEVICE_LABEL, &[0u8; 232]);
    }

    /// GET_RESPONSE DEVICE_INFO-style reply 1234:56789ABC → 454E:00000001 with 2 data bytes.
    fn response_bytes() -> Vec<u8> {
        let mut r = vec![
            0xCC, 0x01, 0x1A, // start codes, length 26
            0x45, 0x4E, 0x00, 0x00, 0x00, 0x01, // dest = controller
            0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, // src = responder
            0x05, // TN
            0x00, // response type ACK
            0x00, // message count
            0x00, 0x00, // sub-device
            0x21, // GET_COMMAND_RESPONSE
            0x00, 0xF0, // DMX_START_ADDRESS
            0x02, // PDL
            0x00, 0x2A, // start address 42
        ];
        // CC+01+1A = 0x0E7; dest = 0x094; src = 0x26A; 05+00+00 = 0x005; sub-device 0; 21 = 0x021;
        // 00+F0 = 0x0F0; 02 = 0x002; 00+2A = 0x02A. Total = 0x527.
        r.extend_from_slice(&[0x05, 0x27]);
        r
    }

    #[test]
    fn decodes_response() {
        let r = decode(&response_bytes()).unwrap();
        assert_eq!(r.src, DEST);
        assert_eq!(r.dest, SRC);
        assert_eq!(r.tn, 5);
        assert_eq!(r.response_type, response_type::ACK);
        assert_eq!(r.cc, cc::GET_RESPONSE);
        assert_eq!(r.pid, pid::DMX_START_ADDRESS);
        assert_eq!(r.data, [0x00, 0x2A]);
        // trailing line noise after the checksum is ignored
        let mut noisy = response_bytes();
        noisy.extend_from_slice(&[0x00, 0xFF]);
        assert_eq!(decode(&noisy).unwrap().data, [0x00, 0x2A]);
    }

    #[test]
    fn decode_rejects_bad_checksum_length_and_start_codes() {
        let good = response_bytes();
        let mut bad_sum = good.clone();
        *bad_sum.last_mut().unwrap() ^= 1;
        assert!(decode(&bad_sum).unwrap_err().contains("checksum"));

        let mut flipped_data = good.clone();
        flipped_data[25] ^= 0x10;
        assert!(decode(&flipped_data).unwrap_err().contains("checksum"));

        assert!(decode(&good[..good.len() - 1]).unwrap_err().contains("truncated"));
        assert!(decode(&good[..20]).unwrap_err().contains("too short"));

        let mut bad_len = good.clone();
        bad_len[2] = 0x19; // claims 25 bytes but PDL says 2
        assert!(decode(&bad_len).unwrap_err().contains("does not match"));

        let mut bad_start = good.clone();
        bad_start[0] = 0x00;
        assert!(decode(&bad_start).unwrap_err().contains("start codes"));
        let mut bad_sub = good;
        bad_sub[1] = 0x02;
        assert!(decode(&bad_sub).unwrap_err().contains("start codes"));
    }

    /// DUB response for 1234:56789ABC, hand-encoded: each UID byte b as (b|AA, b|55); checksum is
    /// the sum of those 12 bytes = BA+57+BE+75+FE+57+FA+7D+BA+DF+BE+FD = 0x0864 → (08|AA, 08|55,
    /// 64|AA, 64|55) = AA 5D EE 75.
    const DUB_BODY: [u8; 17] = [
        0xAA, // separator
        0xBA, 0x57, // 0x12
        0xBE, 0x75, // 0x34
        0xFE, 0x57, // 0x56
        0xFA, 0x7D, // 0x78
        0xBA, 0xDF, // 0x9A
        0xBE, 0xFD, // 0xBC
        0xAA, 0x5D, // checksum MSB 0x08
        0xEE, 0x75, // checksum LSB 0x64
    ];

    #[test]
    fn decodes_dub_with_any_preamble_length() {
        for preamble in 0..=7 {
            let mut bytes = vec![0xFE; preamble];
            bytes.extend_from_slice(&DUB_BODY);
            assert_eq!(decode_dub(&bytes), Dub::One(DEST), "preamble {preamble}");
            assert_eq!(encode_dub_response(DEST, preamble), bytes, "encoder, preamble {preamble}");
        }
        // trailing garbage after a complete response is ignored
        let mut trailing = DUB_BODY.to_vec();
        trailing.push(0x13);
        assert_eq!(decode_dub(&trailing), Dub::One(DEST));
    }

    #[test]
    fn dub_collisions_and_silence() {
        assert_eq!(decode_dub(&[]), Dub::None);
        // eight preamble bytes is not a valid response
        let mut long = vec![0xFE; 8];
        long.extend_from_slice(&DUB_BODY);
        assert_eq!(decode_dub(&long), Dub::Collision);
        // missing separator
        assert_eq!(decode_dub(&[0xFE, 0xFE, 0xBA, 0x57]), Dub::Collision);
        // truncated
        assert_eq!(decode_dub(&DUB_BODY[..16]), Dub::Collision);
        // a single noise byte
        assert_eq!(decode_dub(&[0x00]), Dub::Collision);
        // checksum mismatch (UID byte changed but still validly encoded)
        let mut bad = DUB_BODY;
        bad[12] = 0xFF; // 0xBC|0x55 → 0xFF decodes 0xBE
        assert_eq!(decode_dub(&bad), Dub::Collision);
        // encoding violation: (b | 0xAA) byte missing a forced bit
        let mut unforced = DUB_BODY;
        unforced[1] = 0x3A;
        assert_eq!(decode_dub(&unforced), Dub::Collision);
        // two responses ORed/ANDed on the wire
        let a = encode_dub_response(Uid(0x0001_0000_0003), 7);
        let b = encode_dub_response(Uid(0x7FFF_0000_0004), 7);
        let or: Vec<u8> = a.iter().zip(&b).map(|(x, y)| x | y).collect();
        assert_eq!(decode_dub(&or), Dub::Collision);
    }

    // ---- simulated bus ----

    #[derive(Clone, Copy, PartialEq)]
    enum Wire {
        /// Overlapping transmissions corrupt each other beyond recognition.
        Garbage,
        /// Overlapping transmissions combine as a wired-AND (can yield valid-looking phantoms).
        And,
    }

    struct Responder {
        uid: Uid,
        muted: bool,
        /// Answers DUBs but never acknowledges DISC_MUTE.
        deaf_to_mute: bool,
    }

    struct Bus {
        responders: Vec<Responder>,
        wire: Wire,
        dubs: u32,
        requests: u32,
        /// Every DUB answers with noise.
        noisy: bool,
    }

    impl Bus {
        fn new(uids: &[u64], wire: Wire) -> Bus {
            let responders = uids.iter().map(|&u| Responder { uid: Uid(u), muted: false, deaf_to_mute: false }).collect();
            Bus { responders, wire, dubs: 0, requests: 0, noisy: false }
        }
    }

    impl RdmTransport for Bus {
        fn dub(&mut self, packet: &[u8]) -> io::Result<Dub> {
            self.dubs += 1;
            let req = decode(packet).expect("controller sent a malformed DUB");
            assert_eq!((req.dest, req.cc, req.pid), (Uid::BROADCAST, cc::DISCOVERY, pid::DISC_UNIQUE_BRANCH));
            assert_eq!(req.data.len(), 12);
            if self.noisy {
                return Ok(decode_dub(&[0x55, 0x12]));
            }
            let (lo, hi) = (Uid::from_bytes(&req.data[..6]).0, Uid::from_bytes(&req.data[6..]).0);
            assert!(lo <= hi && hi <= MAX_UID);
            let answers: Vec<Vec<u8>> =
                self.responders.iter().filter(|r| !r.muted && (lo..=hi).contains(&r.uid.0)).map(|r| encode_dub_response(r.uid, 7)).collect();
            let wire = match answers.len() {
                0 => Vec::new(),
                1 => answers[0].clone(),
                _ => match self.wire {
                    Wire::Garbage => vec![0xFE, 0xFE, 0x13, 0x37],
                    Wire::And => answers.iter().skip(1).fold(answers[0].clone(), |acc, a| acc.iter().zip(a).map(|(x, y)| x & y).collect()),
                },
            };
            Ok(decode_dub(&wire))
        }

        fn request(&mut self, packet: &[u8]) -> io::Result<Option<Response>> {
            self.requests += 1;
            let req = decode(packet).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            assert_eq!(req.cc, cc::DISCOVERY);
            match req.pid {
                pid::DISC_UN_MUTE => {
                    assert_eq!(req.dest, Uid::BROADCAST);
                    self.responders.iter_mut().for_each(|r| r.muted = false);
                    Ok(None)
                }
                pid::DISC_MUTE => {
                    let Some(r) = self.responders.iter_mut().find(|r| r.uid == req.dest) else { return Ok(None) };
                    if r.deaf_to_mute {
                        return Ok(None);
                    }
                    r.muted = true;
                    let reply = encode(req.src, r.uid, req.tn, response_type::ACK, 0, cc::DISCOVERY_RESPONSE, pid::DISC_MUTE, &[0, 0]);
                    Ok(Some(decode(&reply).unwrap()))
                }
                other => panic!("unexpected PID {other:04X} during discovery"),
            }
        }
    }

    fn run(uids: &[u64], wire: Wire, max_calls: u32) {
        let mut bus = Bus::new(uids, wire);
        let found = discover(&mut bus, SRC).unwrap();
        let mut expected: Vec<Uid> = uids.iter().map(|&u| Uid(u)).collect();
        expected.sort();
        assert_eq!(found, expected, "{} responders", uids.len());
        let calls = bus.dubs + bus.requests;
        assert!(calls <= max_calls, "{} responders took {calls} requests (bound {max_calls})", uids.len());
        assert!(bus.responders.iter().all(|r| r.muted), "every responder ends muted");
    }

    /// Deterministic xorshift64* for reproducible random UIDs.
    fn random_uids(n: usize, mut seed: u64) -> Vec<u64> {
        let mut set = BTreeSet::new();
        while set.len() < n {
            seed ^= seed >> 12;
            seed ^= seed << 25;
            seed ^= seed >> 27;
            let v = seed.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 16;
            if v <= MAX_UID {
                set.insert(v);
            }
        }
        set.into_iter().collect()
    }

    #[test]
    fn discovery_finds_every_responder_once() {
        for wire in [Wire::Garbage, Wire::And] {
            // empty bus: un-mute + 3 full-range DUB attempts
            let mut empty = Bus::new(&[], wire);
            assert_eq!(discover(&mut empty, SRC).unwrap(), Vec::<Uid>::new());
            assert_eq!((empty.requests, empty.dubs), (1, 3));

            run(&[0x454E_0405_589A], wire, 10);
            // adjacent UIDs share 47 prefix bits: one collision per level, then single answers
            run(&[0x0001_0000_0010, 0x0001_0000_0011], wire, 3 * 48 + 20);
            // extremes of the searchable space
            run(&[0, MAX_UID], wire, 3 * 48 + 20);
            run(&random_uids(50, 0x9E37_79B9_7F4A_7C15), wire, 50 * 48);
        }
    }

    #[test]
    fn phantom_that_never_mutes_is_skipped() {
        let mut bus = Bus::new(&[0x0002_0000_0001, 0x0002_0000_0100], Wire::Garbage);
        bus.responders[0].deaf_to_mute = true;
        let found = discover(&mut bus, SRC).unwrap();
        assert_eq!(found, [Uid(0x0002_0000_0100)]);
        assert!(bus.dubs + bus.requests < 1000);
    }

    #[test]
    fn persistent_noise_hits_the_request_bound() {
        let mut bus = Bus::new(&[], Wire::Garbage);
        bus.noisy = true;
        let err = discover_bounded(&mut bus, SRC, 500).unwrap_err();
        assert!(err.to_string().contains("aborted after 500 requests"), "{err}");
        assert_eq!(bus.dubs + bus.requests, 500);
    }

    /// Responder for DEVICE_INFO and label GETs.
    struct InfoDevice {
        uid: Uid,
        labels: bool,
        gets: Vec<u16>,
    }

    impl RdmTransport for InfoDevice {
        fn dub(&mut self, _packet: &[u8]) -> io::Result<Dub> {
            unreachable!("device_info never sends DUB")
        }

        fn request(&mut self, packet: &[u8]) -> io::Result<Option<Response>> {
            let req = decode(packet).unwrap();
            assert_eq!(req.cc, cc::GET);
            self.gets.push(req.pid);
            if req.dest != self.uid {
                return Ok(None);
            }
            let (kind, data): (u8, Vec<u8>) = match req.pid {
                pid::DEVICE_INFO => (
                    response_type::ACK,
                    vec![
                        0x01, 0x00, // RDM protocol 1.0
                        0x12, 0x34, // model id
                        0x05, 0x09, // category: dimmer / fluorescent … (0x0509)
                        0x01, 0x02, 0x03, 0x04, // software version id
                        0x00, 0x07, // footprint 7
                        0x02, 0x03, // personality 2 of 3
                        0x01, 0x2D, // start address 301
                        0x00, 0x00, // sub-devices
                        0x01, // sensors
                    ],
                ),
                _ if !self.labels => (response_type::NACK_REASON, vec![0x00, 0x00]),
                pid::MANUFACTURER_LABEL => (response_type::ACK, b"Acme Lighting".to_vec()),
                pid::DEVICE_MODEL_DESCRIPTION => (response_type::ACK, b"Par 7\0\0\0".to_vec()),
                pid::DEVICE_LABEL => (response_type::ACK, b"   ".to_vec()),
                other => panic!("unexpected PID {other:04X}"),
            };
            let reply = encode(req.src, self.uid, req.tn, kind, 0, cc::GET_RESPONSE, req.pid, &data);
            Ok(Some(decode(&reply).unwrap()))
        }
    }

    #[test]
    fn device_info_parses_fields_and_labels() {
        let mut dev = InfoDevice { uid: DEST, labels: true, gets: Vec::new() };
        let info = device_info(&mut dev, SRC, DEST).unwrap().unwrap();
        assert_eq!(info.uid, DEST);
        assert_eq!(info.protocol, 0x0100);
        assert_eq!(info.model_id, 0x1234);
        assert_eq!(info.category, 0x0509);
        assert_eq!(info.software_version, 0x0102_0304);
        assert_eq!(info.footprint, 7);
        assert_eq!((info.personality, info.personalities), (2, 3));
        assert_eq!(info.start_address, 301);
        assert_eq!(info.sub_devices, 0);
        assert_eq!(info.sensors, 1);
        assert_eq!(info.manufacturer.as_deref(), Some("Acme Lighting"));
        assert_eq!(info.model.as_deref(), Some("Par 7"));
        assert_eq!(info.label, None, "blank label reads as absent");
        assert_eq!(dev.gets, [pid::DEVICE_INFO, pid::MANUFACTURER_LABEL, pid::DEVICE_MODEL_DESCRIPTION, pid::DEVICE_LABEL]);

        let mut nack = InfoDevice { uid: DEST, labels: false, gets: Vec::new() };
        let info = device_info(&mut nack, SRC, DEST).unwrap().unwrap();
        assert_eq!((info.manufacturer, info.model, info.label), (None, None, None));

        let mut absent = InfoDevice { uid: Uid(1), labels: true, gets: Vec::new() };
        assert!(device_info(&mut absent, SRC, DEST).unwrap().is_none());
        assert_eq!(absent.gets, [pid::DEVICE_INFO; 2], "DEVICE_INFO is retried once");
    }
}
