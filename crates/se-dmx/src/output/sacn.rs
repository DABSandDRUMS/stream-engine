//! ANSI E1.31-2018 (Streaming ACN) data packet for a full 512-slot universe.
//!
//! The packet is built once by [`SacnPacket::new`]; per frame only the sequence number and the
//! slot data are rewritten in place, so sending a frame never allocates.

use std::net::Ipv4Addr;

/// UDP port for E1.31 (ACN-SDT-MULTICAST).
pub const PORT: u16 = 5568;
/// Length of a data packet carrying 512 slots.
pub const PACKET_LEN: usize = 638;
/// Highest valid priority (E1.31 §6.2.3).
pub const MAX_PRIORITY: u8 = 200;

/// ACN Packet Identifier (root layer, E1.31 §5.3).
const ACN_PACKET_IDENTIFIER: [u8; 12] = *b"ASC-E1.17\0\0\0";
const VECTOR_ROOT_E131_DATA: u32 = 0x0000_0004;
const VECTOR_E131_DATA_PACKET: u32 = 0x0000_0002;
const VECTOR_DMP_SET_PROPERTY: u8 = 0x02;
/// DMP address type & data type: absolute, single-byte data, range addressing.
const DMP_ADDRESS_DATA_TYPE: u8 = 0xA1;
const SOURCE_NAME_LEN: usize = 64;

// Field offsets (E1.31 Table 4-1).
const ROOT_FLAGS_LEN: usize = 16;
const CID: usize = 22;
const FRAMING_FLAGS_LEN: usize = 38;
const SOURCE_NAME: usize = 44;
const PRIORITY: usize = 108;
const SEQUENCE: usize = 111;
const OPTIONS: usize = 112;
const UNIVERSE: usize = 113;
const DMP_FLAGS_LEN: usize = 115;
const DATA: usize = 126;

/// Options bit 6: the source is terminating the stream for this universe.
const OPTION_STREAM_TERMINATED: u8 = 0x40;

/// Multicast group for `universe` (1..=63999): `239.255.<hi>.<lo>`.
pub fn multicast_addr(universe: u16) -> Ipv4Addr {
    let [hi, lo] = universe.to_be_bytes();
    Ipv4Addr::new(239, 255, hi, lo)
}

/// PDU flags (0x7) and length: the PDU runs from `offset` to the end of the packet.
fn flags_and_length(offset: usize) -> [u8; 2] {
    (0x7000 | (PACKET_LEN - offset) as u16).to_be_bytes()
}

/// A complete E1.31 data packet for one universe.
#[derive(Clone)]
pub struct SacnPacket {
    buf: [u8; PACKET_LEN],
}

impl SacnPacket {
    /// Builds the packet with all slots zero and sequence 0. `source_name` is truncated to 63
    /// bytes (at a UTF-8 boundary) so the field stays NUL-terminated; `priority` is clamped to
    /// [`MAX_PRIORITY`]. `universe` must be 1..=63999.
    pub fn new(cid: [u8; 16], source_name: &str, universe: u16, priority: u8) -> Self {
        let mut buf = [0u8; PACKET_LEN];
        // Root layer
        buf[0..2].copy_from_slice(&0x0010u16.to_be_bytes()); // preamble size
        // 2..4 post-amble size = 0
        buf[4..16].copy_from_slice(&ACN_PACKET_IDENTIFIER);
        buf[ROOT_FLAGS_LEN..ROOT_FLAGS_LEN + 2].copy_from_slice(&flags_and_length(ROOT_FLAGS_LEN));
        buf[18..22].copy_from_slice(&VECTOR_ROOT_E131_DATA.to_be_bytes());
        buf[CID..CID + 16].copy_from_slice(&cid);
        // Framing layer
        buf[FRAMING_FLAGS_LEN..FRAMING_FLAGS_LEN + 2].copy_from_slice(&flags_and_length(FRAMING_FLAGS_LEN));
        buf[40..44].copy_from_slice(&VECTOR_E131_DATA_PACKET.to_be_bytes());
        let mut name_len = source_name.len().min(SOURCE_NAME_LEN - 1);
        while !source_name.is_char_boundary(name_len) {
            name_len -= 1;
        }
        buf[SOURCE_NAME..SOURCE_NAME + name_len].copy_from_slice(&source_name.as_bytes()[..name_len]);
        buf[PRIORITY] = priority.min(MAX_PRIORITY);
        // 109..111 synchronization address = 0 (unsynchronized), sequence and options = 0
        buf[UNIVERSE..UNIVERSE + 2].copy_from_slice(&universe.to_be_bytes());
        // DMP layer
        buf[DMP_FLAGS_LEN..DMP_FLAGS_LEN + 2].copy_from_slice(&flags_and_length(DMP_FLAGS_LEN));
        buf[117] = VECTOR_DMP_SET_PROPERTY;
        buf[118] = DMP_ADDRESS_DATA_TYPE;
        // 119..121 first property address = 0
        buf[121..123].copy_from_slice(&1u16.to_be_bytes()); // address increment
        buf[123..125].copy_from_slice(&513u16.to_be_bytes()); // property value count: start code + 512
        // 125 DMX512 START code = 0
        SacnPacket { buf }
    }

    /// Writes the sequence number and the 512 slots in place.
    pub fn set(&mut self, sequence: u8, data: &[u8; 512]) {
        self.buf[SEQUENCE] = sequence;
        self.buf[DATA..].copy_from_slice(data);
    }

    /// Sets or clears the Stream_Terminated option (send three terminated packets when a source
    /// stops, E1.31 §6.7.1).
    pub fn set_terminated(&mut self, on: bool) {
        if on {
            self.buf[OPTIONS] |= OPTION_STREAM_TERMINATED;
        } else {
            self.buf[OPTIONS] &= !OPTION_STREAM_TERMINATED;
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_CID: [u8; 16] = [0x5E, 0x6B, 0x2A, 0x10, 0x9C, 0x4D, 0x4E, 0x77, 0x8A, 0x01, 0xC3, 0xD2, 0xE5, 0xF6, 0x07, 0x18];

    fn pattern() -> [u8; 512] {
        std::array::from_fn(|i| (i * 7 + 3) as u8)
    }

    #[test]
    fn full_packet_byte_for_byte() {
        let mut expected: Vec<u8> = Vec::with_capacity(PACKET_LEN);
        // ---- Root layer ----
        expected.extend_from_slice(&[0x00, 0x10]); // 0-1: Preamble Size
        expected.extend_from_slice(&[0x00, 0x00]); // 2-3: Post-amble Size
        // 4-15: ACN Packet Identifier "ASC-E1.17\0\0\0"
        expected.extend_from_slice(&[0x41, 0x53, 0x43, 0x2D, 0x45, 0x31, 0x2E, 0x31, 0x37, 0x00, 0x00, 0x00]);
        expected.extend_from_slice(&[0x72, 0x6E]); // 16-17: Flags & Length = 0x7000 | 622
        expected.extend_from_slice(&[0x00, 0x00, 0x00, 0x04]); // 18-21: Vector VECTOR_ROOT_E131_DATA
        expected.extend_from_slice(&[0x5E, 0x6B, 0x2A, 0x10, 0x9C, 0x4D, 0x4E, 0x77]); // 22-29: CID
        expected.extend_from_slice(&[0x8A, 0x01, 0xC3, 0xD2, 0xE5, 0xF6, 0x07, 0x18]); // 30-37: CID
        assert_eq!(expected.len(), 38);
        // ---- Framing layer ----
        expected.extend_from_slice(&[0x72, 0x58]); // 38-39: Flags & Length = 0x7000 | 600
        expected.extend_from_slice(&[0x00, 0x00, 0x00, 0x02]); // 40-43: Vector VECTOR_E131_DATA_PACKET
        // 44-107: Source Name "stream-engine", NUL padded to 64 bytes
        expected.extend_from_slice(&[0x73, 0x74, 0x72, 0x65, 0x61, 0x6D, 0x2D, 0x65, 0x6E, 0x67, 0x69, 0x6E, 0x65]);
        expected.extend_from_slice(&[0x00; 51]);
        expected.push(0x64); // 108: Priority = 100
        expected.extend_from_slice(&[0x00, 0x00]); // 109-110: Synchronization Address = 0
        expected.push(0x07); // 111: Sequence Number = 7
        expected.push(0x00); // 112: Options
        expected.extend_from_slice(&[0x00, 0x01]); // 113-114: Universe = 1
        assert_eq!(expected.len(), 115);
        // ---- DMP layer ----
        expected.extend_from_slice(&[0x72, 0x0B]); // 115-116: Flags & Length = 0x7000 | 523
        expected.push(0x02); // 117: Vector VECTOR_DMP_SET_PROPERTY
        expected.push(0xA1); // 118: Address Type & Data Type
        expected.extend_from_slice(&[0x00, 0x00]); // 119-120: First Property Address
        expected.extend_from_slice(&[0x00, 0x01]); // 121-122: Address Increment
        expected.extend_from_slice(&[0x02, 0x01]); // 123-124: Property value count = 513
        expected.push(0x00); // 125: DMX512 START Code
        assert_eq!(expected.len(), 126);
        expected.extend_from_slice(&pattern()); // 126-637: slots 1..512
        assert_eq!(expected.len(), 638);

        let mut p = SacnPacket::new(TEST_CID, "stream-engine", 1, 100);
        p.set(7, &pattern());
        assert_eq!(p.bytes(), expected.as_slice());
    }

    #[test]
    fn multicast_addresses() {
        assert_eq!(multicast_addr(1), Ipv4Addr::new(239, 255, 0, 1));
        assert_eq!(multicast_addr(256), Ipv4Addr::new(239, 255, 1, 0));
        assert_eq!(multicast_addr(63999), Ipv4Addr::new(239, 255, 249, 255));
    }

    #[test]
    fn universe_priority_and_options() {
        let mut p = SacnPacket::new(TEST_CID, "x", 0x1234, 255);
        assert_eq!(&p.bytes()[113..115], &[0x12, 0x34]);
        assert_eq!(p.bytes()[108], 200, "priority clamps to 200");
        p.set_terminated(true);
        assert_eq!(p.bytes()[112], 0x40);
        p.set_terminated(true);
        assert_eq!(p.bytes()[112], 0x40);
        p.set_terminated(false);
        assert_eq!(p.bytes()[112], 0x00);
    }

    #[test]
    fn long_source_name_keeps_terminator_and_utf8_boundary() {
        let ascii = "a".repeat(100);
        let p = SacnPacket::new(TEST_CID, &ascii, 1, 100);
        assert_eq!(&p.bytes()[44..107], "a".repeat(63).as_bytes());
        assert_eq!(p.bytes()[107], 0, "last source-name byte stays NUL");

        // 62 ASCII bytes + a 2-byte 'é' would end at byte 64: the 'é' must be dropped whole.
        let name = format!("{}é", "b".repeat(62));
        let p = SacnPacket::new(TEST_CID, &name, 1, 100);
        assert_eq!(&p.bytes()[44..106], "b".repeat(62).as_bytes());
        assert_eq!(&p.bytes()[106..108], &[0, 0]);
        assert_eq!(p.bytes()[108], 100, "name never spills into the priority field");
    }

    #[test]
    fn set_rewrites_in_place_without_allocating() {
        assert!(se_alloc::installed(), "se-dmx tests run with the counting allocator");
        let mut p = SacnPacket::new(TEST_CID, "stream-engine", 1, 100);
        let a = pattern();
        let b = [0xFFu8; 512];
        let scope = se_alloc::Scope::begin();
        for seq in 0..=255u8 {
            p.set(seq, if seq % 2 == 0 { &a } else { &b });
            std::hint::black_box(p.bytes());
        }
        assert_eq!(scope.allocs(), 0);
        assert_eq!(p.bytes()[111], 255);
        assert!(p.bytes()[126..].iter().all(|&v| v == 0xFF));
    }
}
