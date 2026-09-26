//! Wire format of `frames.sock` (`docs/frames-protocol.md`).
//!
//! Every message is one `SOCK_SEQPACKET` record: the 8-byte `se_hdr` followed by the
//! message body, laid out exactly like the C structs of the protocol document (natural
//! alignment, little-endian). File descriptors travel as `SCM_RIGHTS` on the record they
//! belong to and are handled by [`crate::sys`], not here.

use thiserror::Error;

/// `se_hdr.magic`: "SEFR".
pub const MAGIC: u32 = 0x5246_4553;
/// `se_hdr.version`.
pub const VERSION: u16 = 1;
/// Size of `struct se_hdr`.
pub const HDR_SIZE: usize = 8;

/// Client → engine: `se_hello`.
pub const MSG_HELLO: u16 = 1;
/// Client → engine: `se_release`.
pub const MSG_RELEASE: u16 = 3;
/// Engine → client: `se_canvas`.
pub const MSG_CANVAS: u16 = 10;
/// Engine → client: `se_frame`.
pub const MSG_FRAME: u16 = 11;
/// Engine → client: `se_goodbye`.
pub const MSG_GOODBYE: u16 = 12;

pub const CANVAS_WIDE: u32 = 0;
pub const CANVAS_TALL: u32 = 1;
pub const CANVAS_PREVIEW: u32 = 2;
pub const CANVAS_ATLAS: u32 = 3;
/// Number of canvas ids (`0..MAX_CANVASES`).
pub const MAX_CANVASES: usize = 4;
/// `want` mask selecting every canvas.
pub const ALL_CANVASES_MASK: u32 = (1 << MAX_CANVASES) - 1;
/// Canvas names indexed by canvas id.
pub const CANVAS_NAMES: [&str; MAX_CANVASES] = ["wide", "tall", "preview", "atlas"];

/// `se_hello.client`: OBS plugin.
pub const CLIENT_OBS: u32 = 1;
/// `se_hello.client`: stream-engine UI.
pub const CLIENT_UI: u32 = 2;
/// `se_hello.client`: anything else (test clients, tools).
pub const CLIENT_OTHER: u32 = 3;

/// `se_hello.flags` bit 0: the client imports dmabufs (else it gets the shm fallback).
pub const HELLO_FLAG_DMABUF: u32 = 1;

/// `DRM_FORMAT_ABGR8888` ("AB24"): byte order R, G, B, A = Vulkan `R8G8B8A8_UNORM`.
pub const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;
/// `DRM_FORMAT_MOD_LINEAR`.
pub const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// `se_goodbye.reason`: the engine is stopping.
pub const GOODBYE_SHUTDOWN: u32 = 1;
/// `se_goodbye.reason`: the engine lost its GPU device; new `se_canvas` follow.
pub const GOODBYE_DEVICE_LOST: u32 = 2;
/// `se_goodbye.reason`: the canvas is no longer exported.
pub const GOODBYE_CANVAS_REMOVED: u32 = 3;
/// `se_goodbye.canvas`: applies to every canvas.
pub const ALL_CANVASES: u32 = 0xffff_ffff;

/// Minimum buffers per canvas ring (`se_canvas.buffer_count`).
pub const MIN_BUFFERS: usize = 3;
/// Maximum buffers per canvas ring (`se_canvas.buffer_count`).
pub const MAX_BUFFERS: usize = 4;

/// Largest message of the protocol (`se_canvas`).
pub const MAX_MSG_SIZE: usize = CanvasMsg::SIZE;

/// Name of a canvas id (`wide`, `tall`, `preview`, `atlas`).
pub fn canvas_name(canvas: u32) -> Option<&'static str> {
    CANVAS_NAMES.get(canvas as usize).copied()
}

/// Canvas id of a canvas name.
pub fn canvas_by_name(name: &str) -> Option<u32> {
    CANVAS_NAMES.iter().position(|n| *n == name).map(|i| i as u32)
}

/// A decode failure. Every variant means the peer violated the protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum ProtoError {
    #[error("message is {got} bytes, expected {expected}")]
    Length { expected: usize, got: usize },
    #[error("bad magic {0:#010x}")]
    Magic(u32),
    #[error("unsupported protocol version {0}")]
    Version(u16),
    #[error("message type {got}, expected {expected}")]
    Type { expected: u16, got: u16 },
    #[error("invalid value {value} in field `{field}`")]
    Field { field: &'static str, value: u64 },
}

/// Byte offsets of every field, derived from the C structs with natural alignment.
/// `tests::c_layout` checks them against `#[repr(C)]` mirrors of the C definitions.
pub(crate) mod off {
    pub const MAGIC: usize = 0;
    pub const TYPE: usize = 4;
    pub const VERSION: usize = 6;

    pub const HELLO_CLIENT: usize = 8;
    pub const HELLO_WANT: usize = 12;
    pub const HELLO_FLAGS: usize = 16;
    pub const HELLO_RESERVED: usize = 20;

    pub const RELEASE_CANVAS: usize = 8;
    pub const RELEASE_BUFFER: usize = 12;
    pub const RELEASE_SEQ: usize = 16;

    pub const CANVAS_CANVAS: usize = 8;
    pub const CANVAS_WIDTH: usize = 12;
    pub const CANVAS_HEIGHT: usize = 16;
    pub const CANVAS_FOURCC: usize = 20;
    pub const CANVAS_MODIFIER: usize = 24;
    pub const CANVAS_OFFSETS: usize = 32;
    pub const CANVAS_STRIDES: usize = 48;
    pub const CANVAS_PLANES: usize = 64;
    pub const CANVAS_BUFFER_COUNT: usize = 68;
    pub const CANVAS_GENERATION: usize = 72;
    pub const CANVAS_RESERVED: usize = 76;

    pub const FRAME_CANVAS: usize = 8;
    pub const FRAME_BUFFER: usize = 12;
    pub const FRAME_SEQ: usize = 16;
    pub const FRAME_MONOTONIC_NS: usize = 24;
    pub const FRAME_GENERATION: usize = 32;
    pub const FRAME_HAS_FENCE: usize = 36;

    pub const GOODBYE_REASON: usize = 8;
    pub const GOODBYE_CANVAS: usize = 12;
}

fn get_u16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn get_u32(b: &[u8], at: usize) -> u32 {
    let mut v = [0; 4];
    v.copy_from_slice(&b[at..at + 4]);
    u32::from_le_bytes(v)
}

fn get_u64(b: &[u8], at: usize) -> u64 {
    let mut v = [0; 8];
    v.copy_from_slice(&b[at..at + 8]);
    u64::from_le_bytes(v)
}

fn put_u16(b: &mut [u8], at: usize, v: u16) {
    b[at..at + 2].copy_from_slice(&v.to_le_bytes());
}

fn put_u32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put_u64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

/// Checks the header (magic, version) of a received record and returns its message type.
pub fn peek_type(buf: &[u8]) -> Result<u16, ProtoError> {
    if buf.len() < HDR_SIZE {
        return Err(ProtoError::Length { expected: HDR_SIZE, got: buf.len() });
    }
    let magic = get_u32(buf, off::MAGIC);
    if magic != MAGIC {
        return Err(ProtoError::Magic(magic));
    }
    let version = get_u16(buf, off::VERSION);
    if version != VERSION {
        return Err(ProtoError::Version(version));
    }
    Ok(get_u16(buf, off::TYPE))
}

fn check(buf: &[u8], ty: u16, size: usize) -> Result<(), ProtoError> {
    let got = peek_type(buf)?;
    if got != ty {
        return Err(ProtoError::Type { expected: ty, got });
    }
    if buf.len() != size {
        return Err(ProtoError::Length { expected: size, got: buf.len() });
    }
    Ok(())
}

/// Writes the header and zeroes the body. Panics if `buf` is shorter than `size`.
fn header(buf: &mut [u8], ty: u16, size: usize) -> &mut [u8] {
    let out = &mut buf[..size];
    out.fill(0);
    put_u32(out, off::MAGIC, MAGIC);
    put_u16(out, off::TYPE, ty);
    put_u16(out, off::VERSION, VERSION);
    out
}

/// `struct se_hello` (type 1, client → engine). `reserved` is written as 0 and ignored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Hello {
    /// [`CLIENT_OBS`], [`CLIENT_UI`] or [`CLIENT_OTHER`].
    pub client: u32,
    /// Bitmask of wanted canvases (`1 << canvas`).
    pub want: u32,
    /// [`HELLO_FLAG_DMABUF`].
    pub flags: u32,
}

impl Hello {
    pub const TYPE: u16 = MSG_HELLO;
    pub const SIZE: usize = 24;

    /// Encodes into `buf` and returns the message length. Panics if `buf` is too short.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let b = header(buf, Self::TYPE, Self::SIZE);
        put_u32(b, off::HELLO_CLIENT, self.client);
        put_u32(b, off::HELLO_WANT, self.want);
        put_u32(b, off::HELLO_FLAGS, self.flags);
        put_u32(b, off::HELLO_RESERVED, 0);
        Self::SIZE
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        check(buf, Self::TYPE, Self::SIZE)?;
        Ok(Self { client: get_u32(buf, off::HELLO_CLIENT), want: get_u32(buf, off::HELLO_WANT), flags: get_u32(buf, off::HELLO_FLAGS) })
    }
}

/// `struct se_release` (type 3, client → engine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Release {
    pub canvas: u32,
    pub buffer: u32,
    pub seq: u64,
}

impl Release {
    pub const TYPE: u16 = MSG_RELEASE;
    pub const SIZE: usize = 24;

    /// Encodes into `buf` and returns the message length. Panics if `buf` is too short.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let b = header(buf, Self::TYPE, Self::SIZE);
        put_u32(b, off::RELEASE_CANVAS, self.canvas);
        put_u32(b, off::RELEASE_BUFFER, self.buffer);
        put_u64(b, off::RELEASE_SEQ, self.seq);
        Self::SIZE
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        check(buf, Self::TYPE, Self::SIZE)?;
        Ok(Self { canvas: get_u32(buf, off::RELEASE_CANVAS), buffer: get_u32(buf, off::RELEASE_BUFFER), seq: get_u64(buf, off::RELEASE_SEQ) })
    }
}

/// `struct se_canvas` (type 10, engine → client). `buffer_count` fds travel with it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CanvasMsg {
    pub canvas: u32,
    pub width: u32,
    pub height: u32,
    /// [`DRM_FORMAT_ABGR8888`] for dmabufs, 0 for the shm fallback.
    pub drm_fourcc: u32,
    pub modifier: u64,
    pub offsets: [u32; 4],
    pub strides: [u32; 4],
    pub planes: u32,
    pub buffer_count: u32,
    pub generation: u32,
}

impl CanvasMsg {
    pub const TYPE: u16 = MSG_CANVAS;
    pub const SIZE: usize = 80;

    /// Encodes into `buf` and returns the message length. Panics if `buf` is too short.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let b = header(buf, Self::TYPE, Self::SIZE);
        put_u32(b, off::CANVAS_CANVAS, self.canvas);
        put_u32(b, off::CANVAS_WIDTH, self.width);
        put_u32(b, off::CANVAS_HEIGHT, self.height);
        put_u32(b, off::CANVAS_FOURCC, self.drm_fourcc);
        put_u64(b, off::CANVAS_MODIFIER, self.modifier);
        for i in 0..4 {
            put_u32(b, off::CANVAS_OFFSETS + 4 * i, self.offsets[i]);
            put_u32(b, off::CANVAS_STRIDES + 4 * i, self.strides[i]);
        }
        put_u32(b, off::CANVAS_PLANES, self.planes);
        put_u32(b, off::CANVAS_BUFFER_COUNT, self.buffer_count);
        put_u32(b, off::CANVAS_GENERATION, self.generation);
        put_u32(b, off::CANVAS_RESERVED, 0);
        Self::SIZE
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        check(buf, Self::TYPE, Self::SIZE)?;
        let mut offsets = [0; 4];
        let mut strides = [0; 4];
        for i in 0..4 {
            offsets[i] = get_u32(buf, off::CANVAS_OFFSETS + 4 * i);
            strides[i] = get_u32(buf, off::CANVAS_STRIDES + 4 * i);
        }
        Ok(Self {
            canvas: get_u32(buf, off::CANVAS_CANVAS),
            width: get_u32(buf, off::CANVAS_WIDTH),
            height: get_u32(buf, off::CANVAS_HEIGHT),
            drm_fourcc: get_u32(buf, off::CANVAS_FOURCC),
            modifier: get_u64(buf, off::CANVAS_MODIFIER),
            offsets,
            strides,
            planes: get_u32(buf, off::CANVAS_PLANES),
            buffer_count: get_u32(buf, off::CANVAS_BUFFER_COUNT),
            generation: get_u32(buf, off::CANVAS_GENERATION),
        })
    }

    /// Bytes a buffer must at least span: `offsets[0] + strides[0] * height`.
    pub fn min_buffer_len(&self) -> u64 {
        u64::from(self.offsets[0]) + u64::from(self.strides[0]) * u64::from(self.height)
    }
}

/// `struct se_frame` (type 11, engine → client). One sync_file fd travels with it when
/// `has_fence` is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct FrameMsg {
    pub canvas: u32,
    pub buffer: u32,
    pub seq: u64,
    pub monotonic_ns: u64,
    pub generation: u32,
    pub has_fence: bool,
}

impl FrameMsg {
    pub const TYPE: u16 = MSG_FRAME;
    pub const SIZE: usize = 40;

    /// Encodes into `buf` and returns the message length. Panics if `buf` is too short.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let b = header(buf, Self::TYPE, Self::SIZE);
        put_u32(b, off::FRAME_CANVAS, self.canvas);
        put_u32(b, off::FRAME_BUFFER, self.buffer);
        put_u64(b, off::FRAME_SEQ, self.seq);
        put_u64(b, off::FRAME_MONOTONIC_NS, self.monotonic_ns);
        put_u32(b, off::FRAME_GENERATION, self.generation);
        put_u32(b, off::FRAME_HAS_FENCE, u32::from(self.has_fence));
        Self::SIZE
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        check(buf, Self::TYPE, Self::SIZE)?;
        let has_fence = match get_u32(buf, off::FRAME_HAS_FENCE) {
            0 => false,
            1 => true,
            v => {
                return Err(ProtoError::Field { field: "has_fence", value: u64::from(v) });
            }
        };
        Ok(Self {
            canvas: get_u32(buf, off::FRAME_CANVAS),
            buffer: get_u32(buf, off::FRAME_BUFFER),
            seq: get_u64(buf, off::FRAME_SEQ),
            monotonic_ns: get_u64(buf, off::FRAME_MONOTONIC_NS),
            generation: get_u32(buf, off::FRAME_GENERATION),
            has_fence,
        })
    }
}

/// `struct se_goodbye` (type 12, engine → client).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Goodbye {
    /// [`GOODBYE_SHUTDOWN`], [`GOODBYE_DEVICE_LOST`] or [`GOODBYE_CANVAS_REMOVED`].
    pub reason: u32,
    /// Canvas id or [`ALL_CANVASES`].
    pub canvas: u32,
}

impl Goodbye {
    pub const TYPE: u16 = MSG_GOODBYE;
    pub const SIZE: usize = 16;

    /// Encodes into `buf` and returns the message length. Panics if `buf` is too short.
    pub fn encode(&self, buf: &mut [u8]) -> usize {
        let b = header(buf, Self::TYPE, Self::SIZE);
        put_u32(b, off::GOODBYE_REASON, self.reason);
        put_u32(b, off::GOODBYE_CANVAS, self.canvas);
        Self::SIZE
    }

    pub fn decode(buf: &[u8]) -> Result<Self, ProtoError> {
        check(buf, Self::TYPE, Self::SIZE)?;
        Ok(Self { reason: get_u32(buf, off::GOODBYE_REASON), canvas: get_u32(buf, off::GOODBYE_CANVAS) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, offset_of, size_of};

    // `#[repr(C)]` mirrors of the C structs in docs/frames-protocol.md. `repr(C)` follows
    // the platform C ABI, so these give the offsets a C compiler assigns.
    #[repr(C)]
    struct SeHdr {
        magic: u32,
        ty: u16,
        version: u16,
    }
    #[repr(C)]
    struct SeHello {
        h: SeHdr,
        client: u32,
        want: u32,
        flags: u32,
        reserved: u32,
    }
    #[repr(C)]
    struct SeRelease {
        h: SeHdr,
        canvas: u32,
        buffer: u32,
        seq: u64,
    }
    #[repr(C)]
    struct SeCanvas {
        h: SeHdr,
        canvas: u32,
        width: u32,
        height: u32,
        drm_fourcc: u32,
        modifier: u64,
        offsets: [u32; 4],
        strides: [u32; 4],
        planes: u32,
        buffer_count: u32,
        generation: u32,
        reserved: u32,
    }
    #[repr(C)]
    struct SeFrame {
        h: SeHdr,
        canvas: u32,
        buffer: u32,
        seq: u64,
        monotonic_ns: u64,
        generation: u32,
        has_fence: u32,
    }
    #[repr(C)]
    struct SeGoodbye {
        h: SeHdr,
        reason: u32,
        canvas: u32,
    }

    #[test]
    fn c_layout() {
        assert_eq!(size_of::<SeHdr>(), HDR_SIZE);
        assert_eq!(offset_of!(SeHdr, magic), off::MAGIC);
        assert_eq!(offset_of!(SeHdr, ty), off::TYPE);
        assert_eq!(offset_of!(SeHdr, version), off::VERSION);

        assert_eq!(size_of::<SeHello>(), Hello::SIZE);
        assert_eq!(Hello::SIZE, 24);
        assert_eq!(offset_of!(SeHello, client), off::HELLO_CLIENT);
        assert_eq!(offset_of!(SeHello, want), off::HELLO_WANT);
        assert_eq!(offset_of!(SeHello, flags), off::HELLO_FLAGS);
        assert_eq!(offset_of!(SeHello, reserved), off::HELLO_RESERVED);

        assert_eq!(size_of::<SeRelease>(), Release::SIZE);
        assert_eq!(Release::SIZE, 24);
        assert_eq!(align_of::<SeRelease>(), 8);
        assert_eq!(offset_of!(SeRelease, canvas), off::RELEASE_CANVAS);
        assert_eq!(offset_of!(SeRelease, buffer), off::RELEASE_BUFFER);
        assert_eq!(offset_of!(SeRelease, seq), off::RELEASE_SEQ);

        // 8 (hdr) + 4 × u32 = 24, already 8-aligned for `modifier`; 24 + 8 + 16 + 16 +
        // 4 × u32 = 80, a multiple of the 8-byte struct alignment → no tail padding.
        assert_eq!(size_of::<SeCanvas>(), CanvasMsg::SIZE);
        assert_eq!(CanvasMsg::SIZE, 80);
        assert_eq!(offset_of!(SeCanvas, canvas), off::CANVAS_CANVAS);
        assert_eq!(offset_of!(SeCanvas, width), off::CANVAS_WIDTH);
        assert_eq!(offset_of!(SeCanvas, height), off::CANVAS_HEIGHT);
        assert_eq!(offset_of!(SeCanvas, drm_fourcc), off::CANVAS_FOURCC);
        assert_eq!(offset_of!(SeCanvas, modifier), off::CANVAS_MODIFIER);
        assert_eq!(offset_of!(SeCanvas, offsets), off::CANVAS_OFFSETS);
        assert_eq!(offset_of!(SeCanvas, strides), off::CANVAS_STRIDES);
        assert_eq!(offset_of!(SeCanvas, planes), off::CANVAS_PLANES);
        assert_eq!(offset_of!(SeCanvas, buffer_count), off::CANVAS_BUFFER_COUNT);
        assert_eq!(offset_of!(SeCanvas, generation), off::CANVAS_GENERATION);
        assert_eq!(offset_of!(SeCanvas, reserved), off::CANVAS_RESERVED);

        assert_eq!(size_of::<SeFrame>(), FrameMsg::SIZE);
        assert_eq!(FrameMsg::SIZE, 40);
        assert_eq!(offset_of!(SeFrame, canvas), off::FRAME_CANVAS);
        assert_eq!(offset_of!(SeFrame, buffer), off::FRAME_BUFFER);
        assert_eq!(offset_of!(SeFrame, seq), off::FRAME_SEQ);
        assert_eq!(offset_of!(SeFrame, monotonic_ns), off::FRAME_MONOTONIC_NS);
        assert_eq!(offset_of!(SeFrame, generation), off::FRAME_GENERATION);
        assert_eq!(offset_of!(SeFrame, has_fence), off::FRAME_HAS_FENCE);

        assert_eq!(size_of::<SeGoodbye>(), Goodbye::SIZE);
        assert_eq!(Goodbye::SIZE, 16);
        assert_eq!(offset_of!(SeGoodbye, reason), off::GOODBYE_REASON);
        assert_eq!(offset_of!(SeGoodbye, canvas), off::GOODBYE_CANVAS);

        assert_eq!(MAX_MSG_SIZE, 80);
    }

    fn sample_canvas() -> CanvasMsg {
        CanvasMsg {
            canvas: CANVAS_TALL,
            width: 1080,
            height: 1920,
            drm_fourcc: DRM_FORMAT_ABGR8888,
            modifier: 0x0300_0000_0060_6015,
            offsets: [0, 1, 2, 3],
            strides: [4352, 5, 6, 7],
            planes: 1,
            buffer_count: 4,
            generation: 9,
        }
    }

    #[test]
    fn golden_bytes() {
        let mut buf = [0xAAu8; 128];
        let n = CanvasMsg::encode(&sample_canvas(), &mut buf);
        assert_eq!(n, 80);
        let b = &buf[..n];
        assert_eq!(&b[0..4], b"SEFR");
        assert_eq!(&b[4..6], &10u16.to_le_bytes());
        assert_eq!(&b[6..8], &1u16.to_le_bytes());
        assert_eq!(&b[20..24], b"AB24");
        assert_eq!(&b[24..32], &0x0300_0000_0060_6015u64.to_le_bytes());
        assert_eq!(&b[48..52], &4352u32.to_le_bytes());
        assert_eq!(&b[68..72], &4u32.to_le_bytes());
        assert_eq!(&b[76..80], &[0, 0, 0, 0], "reserved is zeroed");
        assert_eq!(buf[80], 0xAA, "encode must not write past the message");

        let mut buf = [0xAAu8; 40];
        FrameMsg { canvas: 2, buffer: 3, seq: 0x0102_0304_0506_0708, monotonic_ns: 42, generation: 7, has_fence: true }.encode(&mut buf);
        assert_eq!(&buf[16..24], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(&buf[36..40], &1u32.to_le_bytes());
    }

    #[test]
    fn round_trips() {
        let mut buf = [0u8; MAX_MSG_SIZE];

        let hello = Hello { client: CLIENT_OBS, want: 0b0011, flags: HELLO_FLAG_DMABUF };
        let n = hello.encode(&mut buf);
        assert_eq!(Hello::decode(&buf[..n]), Ok(hello));
        assert_eq!(peek_type(&buf[..n]), Ok(MSG_HELLO));

        let release = Release { canvas: 3, buffer: 2, seq: u64::MAX };
        let n = release.encode(&mut buf);
        assert_eq!(Release::decode(&buf[..n]), Ok(release));

        let canvas = sample_canvas();
        let n = canvas.encode(&mut buf);
        assert_eq!(CanvasMsg::decode(&buf[..n]), Ok(canvas));

        for has_fence in [false, true] {
            let frame = FrameMsg { canvas: 1, buffer: 0, seq: 123_456_789_012, monotonic_ns: u64::MAX - 1, generation: 4, has_fence };
            let n = frame.encode(&mut buf);
            assert_eq!(FrameMsg::decode(&buf[..n]), Ok(frame));
        }

        let bye = Goodbye { reason: GOODBYE_DEVICE_LOST, canvas: ALL_CANVASES };
        let n = bye.encode(&mut buf);
        assert_eq!(Goodbye::decode(&buf[..n]), Ok(bye));
    }

    #[test]
    fn rejects() {
        let mut buf = [0u8; MAX_MSG_SIZE + 8];
        let n = Hello { client: CLIENT_UI, want: 1, flags: 0 }.encode(&mut buf);

        // Wrong type for the decoder.
        assert_eq!(Release::decode(&buf[..n]), Err(ProtoError::Type { expected: MSG_RELEASE, got: MSG_HELLO }));
        // Truncated and over-long records.
        assert_eq!(Hello::decode(&buf[..n - 1]), Err(ProtoError::Length { expected: 24, got: 23 }));
        assert_eq!(Hello::decode(&buf[..n + 4]), Err(ProtoError::Length { expected: 24, got: 28 }));
        assert_eq!(peek_type(&buf[..4]), Err(ProtoError::Length { expected: HDR_SIZE, got: 4 }));
        assert!(peek_type(&[]).is_err());

        let mut bad = buf;
        bad[0] ^= 0xff;
        assert!(matches!(Hello::decode(&bad[..n]), Err(ProtoError::Magic(_))));

        let mut bad = buf;
        bad[6] = 2;
        assert_eq!(Hello::decode(&bad[..n]), Err(ProtoError::Version(2)));

        let n = FrameMsg::default().encode(&mut buf);
        buf[off::FRAME_HAS_FENCE] = 2;
        assert_eq!(FrameMsg::decode(&buf[..n]), Err(ProtoError::Field { field: "has_fence", value: 2 }));
    }

    #[test]
    fn canvas_names() {
        for (i, name) in CANVAS_NAMES.iter().enumerate() {
            assert_eq!(canvas_by_name(name), Some(i as u32));
            assert_eq!(canvas_name(i as u32), Some(*name));
        }
        assert_eq!(canvas_by_name("side"), None);
        assert_eq!(canvas_name(4), None);
        assert_eq!(ALL_CANVASES_MASK, 0b1111);
    }
}
