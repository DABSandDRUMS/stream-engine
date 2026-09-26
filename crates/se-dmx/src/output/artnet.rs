//! Art-Net 4 ArtDmx packet (OpCode 0x5000, protocol version 14) for a full 512-slot universe.
//!
//! Built once by [`ArtDmx::new`]; per frame only sequence, physical port and slot data are
//! rewritten in place, so sending a frame never allocates.

/// UDP port for Art-Net.
pub const PORT: u16 = 6454;
/// Length of an ArtDmx packet carrying 512 slots.
pub const PACKET_LEN: usize = 18 + 512;

const ID: [u8; 8] = *b"Art-Net\0";
const OP_DMX: u16 = 0x5000;
const PROTOCOL_VERSION: u16 = 14;

// Field offsets (Art-Net 4, ArtDmx packet definition).
const SEQUENCE: usize = 12;
const PHYSICAL: usize = 13;
const SUB_UNI: usize = 14;
const NET: usize = 15;
const LENGTH: usize = 16;
const DATA: usize = 18;

/// A complete ArtDmx packet for one port-address.
#[derive(Clone)]
pub struct ArtDmx {
    buf: [u8; PACKET_LEN],
}

impl ArtDmx {
    /// `port_address` is the 15-bit Net(7):SubNet(4):Universe(4) address; bit 15 is ignored.
    pub fn new(port_address: u16) -> Self {
        let mut buf = [0u8; PACKET_LEN];
        buf[..8].copy_from_slice(&ID);
        buf[8..10].copy_from_slice(&OP_DMX.to_le_bytes());
        buf[10..12].copy_from_slice(&PROTOCOL_VERSION.to_be_bytes());
        let [net, sub_uni] = (port_address & 0x7FFF).to_be_bytes();
        buf[SUB_UNI] = sub_uni;
        buf[NET] = net;
        buf[LENGTH..LENGTH + 2].copy_from_slice(&512u16.to_be_bytes());
        ArtDmx { buf }
    }

    /// Writes sequence (1..=255 in order; 0 disables receiver re-ordering), the physical input
    /// port (informational) and the 512 slots in place.
    pub fn set(&mut self, sequence: u8, physical: u8, data: &[u8; 512]) {
        self.buf[SEQUENCE] = sequence;
        self.buf[PHYSICAL] = physical;
        self.buf[DATA..].copy_from_slice(data);
    }

    pub fn bytes(&self) -> &[u8] {
        &self.buf
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_for_port_address_zero() {
        let mut p = ArtDmx::new(0);
        let data: [u8; 512] = std::array::from_fn(|i| (i % 251) as u8);
        p.set(0x2A, 3, &data);
        let header: [u8; 18] = [
            0x41, 0x72, 0x74, 0x2D, 0x4E, 0x65, 0x74, 0x00, // 0-7: ID "Art-Net\0"
            0x00, 0x50, // 8-9: OpCode OpDmx 0x5000, little-endian
            0x00, // 10: ProtVerHi
            0x0E, // 11: ProtVerLo = 14
            0x2A, // 12: Sequence
            0x03, // 13: Physical
            0x00, // 14: SubUni
            0x00, // 15: Net
            0x02, // 16: LengthHi (512 = 0x0200, big-endian)
            0x00, // 17: Length
        ];
        assert_eq!(p.bytes().len(), 530);
        assert_eq!(&p.bytes()[..18], &header);
        assert_eq!(&p.bytes()[18..], &data);
    }

    #[test]
    fn port_address_split_and_masking() {
        let p = ArtDmx::new(0x1234);
        assert_eq!(&p.bytes()[14..16], &[0x34, 0x12], "SubUni = low byte, Net = high 7 bits");
        let p = ArtDmx::new(0xFFFF);
        assert_eq!(&p.bytes()[14..16], &[0xFF, 0x7F], "bit 15 is not part of the port-address");
        let p = ArtDmx::new(0x0100);
        assert_eq!(&p.bytes()[14..16], &[0x00, 0x01]);
    }

    #[test]
    fn set_rewrites_in_place_without_allocating() {
        assert!(se_alloc::installed(), "se-dmx tests run with the counting allocator");
        let mut p = ArtDmx::new(1);
        let data = [0x80u8; 512];
        let scope = se_alloc::Scope::begin();
        for seq in 1..=255u8 {
            p.set(seq, 0, &data);
            std::hint::black_box(p.bytes());
        }
        assert_eq!(scope.allocs(), 0);
        assert_eq!(p.bytes()[12], 255);
        assert_eq!(p.bytes()[14], 1);
        assert!(p.bytes()[18..].iter().all(|&v| v == 0x80));
    }
}
