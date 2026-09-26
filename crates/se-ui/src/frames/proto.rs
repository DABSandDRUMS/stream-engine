//! Wire codec for `frames.sock` (docs/frames-protocol.md): fixed-size little-endian messages
//! behind an 8-byte `se_hdr`; file descriptors travel as `SCM_RIGHTS` next to the message.

use std::fmt;
use std::os::fd::OwnedFd;

pub const MAGIC: u32 = 0x5246_4553;
pub const VERSION: u16 = 1;

pub const T_HELLO: u16 = 1;
pub const T_RELEASE: u16 = 3;
pub const T_CANVAS: u16 = 10;
pub const T_FRAME: u16 = 11;
pub const T_GOODBYE: u16 = 12;

/// `se_hello.client` value of the UI.
pub const CLIENT_UI: u32 = 2;
/// `se_hello.flags` bit 0: the client imports dmabufs (otherwise the engine uses memfd shm).
pub const FLAG_DMABUF: u32 = 1;
/// `DRM_FORMAT_ABGR8888`: R8G8B8A8 in memory order.
pub const DRM_FORMAT_ABGR8888: u32 = 0x3432_4241;
/// `se_goodbye.canvas` value meaning every canvas.
pub const ALL_CANVASES: u32 = 0xffff_ffff;

pub const CANVAS_COUNT: u32 = 4;
/// Upper bound accepted for `se_canvas.buffer_count` (the engine uses 3–4).
pub const MAX_BUFFERS: u32 = 16;
/// Upper bound accepted for canvas width/height.
pub const MAX_DIMENSION: u32 = 16384;

pub const HDR_LEN: usize = 8;
pub const HELLO_LEN: usize = 24;
pub const RELEASE_LEN: usize = 24;
pub const CANVAS_LEN: usize = 80;
pub const FRAME_LEN: usize = 40;
pub const GOODBYE_LEN: usize = 16;

/// `se_hello` (client → engine).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Hello {
    pub client: u32,
    pub want: u32,
    pub flags: u32,
}

/// `se_release` (client → engine).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Release {
    pub canvas: u32,
    pub buffer: u32,
    pub seq: u64,
}

/// `se_canvas` (engine → client), validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CanvasDesc {
    pub canvas: u32,
    pub width: u32,
    pub height: u32,
    /// 0 = shm fallback (memfds of RGBA8 rows), else a DRM fourcc.
    pub drm_fourcc: u32,
    pub modifier: u64,
    pub offsets: [u32; 4],
    pub strides: [u32; 4],
    pub planes: u32,
    pub buffer_count: u32,
    pub generation: u32,
}

impl CanvasDesc {
    pub fn is_shm(&self) -> bool {
        self.drm_fourcc == 0
    }
}

/// `se_frame` (engine → client), validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub canvas: u32,
    pub buffer: u32,
    pub seq: u64,
    pub monotonic_ns: u64,
    pub generation: u32,
    pub has_fence: bool,
}

/// `se_goodbye` (engine → client), validated.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Goodbye {
    /// 1 shutdown, 2 device lost, 3 canvas removed (other values are passed through).
    pub reason: u32,
    /// `None` = every canvas.
    pub canvas: Option<u32>,
}

/// A decoded engine message together with the descriptors it owns.
#[derive(Debug)]
pub enum EngineMsg {
    /// `buffer_count` fds: dmabufs, or memfds when `drm_fourcc == 0`.
    Canvas(CanvasDesc, Vec<OwnedFd>),
    /// The sync_file fence when `has_fence`.
    Frame(Frame, Option<OwnedFd>),
    Goodbye(Goodbye),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProtoError {
    Short(usize),
    Magic(u32),
    Version(u16),
    UnknownType(u16),
    Length { ty: u16, len: usize, want: usize },
    Fds { ty: u16, got: usize, want: usize },
    Field { ty: u16, field: &'static str, value: u64 },
}

impl fmt::Display for ProtoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Short(len) => write!(f, "message of {len} bytes is shorter than the header"),
            Self::Magic(m) => write!(f, "bad magic {m:#010x}"),
            Self::Version(v) => write!(f, "unsupported protocol version {v}"),
            Self::UnknownType(t) => write!(f, "unknown message type {t}"),
            Self::Length { ty, len, want } => write!(f, "type {ty}: {len} bytes, expected {want}"),
            Self::Fds { ty, got, want } => write!(f, "type {ty}: {got} fds attached, expected {want}"),
            Self::Field { ty, field, value } => write!(f, "type {ty}: invalid {field} = {value}"),
        }
    }
}

impl std::error::Error for ProtoError {}

fn header(ty: u16) -> [u8; HDR_LEN] {
    let mut h = [0u8; HDR_LEN];
    h[0..4].copy_from_slice(&MAGIC.to_le_bytes());
    h[4..6].copy_from_slice(&ty.to_le_bytes());
    h[6..8].copy_from_slice(&VERSION.to_le_bytes());
    h
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    b[at..at + 4].copy_from_slice(&v.to_le_bytes());
}

fn put64(b: &mut [u8], at: usize, v: u64) {
    b[at..at + 8].copy_from_slice(&v.to_le_bytes());
}

fn get16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}

fn get32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn get64(b: &[u8], at: usize) -> u64 {
    u64::from(get32(b, at)) | (u64::from(get32(b, at + 4)) << 32)
}

impl Hello {
    pub fn encode(&self) -> [u8; HELLO_LEN] {
        let mut b = [0u8; HELLO_LEN];
        b[..HDR_LEN].copy_from_slice(&header(T_HELLO));
        put32(&mut b, 8, self.client);
        put32(&mut b, 12, self.want);
        put32(&mut b, 16, self.flags);
        b
    }
}

impl Release {
    pub fn encode(&self) -> [u8; RELEASE_LEN] {
        let mut b = [0u8; RELEASE_LEN];
        b[..HDR_LEN].copy_from_slice(&header(T_RELEASE));
        put32(&mut b, 8, self.canvas);
        put32(&mut b, 12, self.buffer);
        put64(&mut b, 16, self.seq);
        b
    }
}

/// Validates the header and returns the message type.
fn check_header(b: &[u8]) -> Result<u16, ProtoError> {
    if b.len() < HDR_LEN {
        return Err(ProtoError::Short(b.len()));
    }
    let magic = get32(b, 0);
    if magic != MAGIC {
        return Err(ProtoError::Magic(magic));
    }
    let version = get16(b, 6);
    if version != VERSION {
        return Err(ProtoError::Version(version));
    }
    Ok(get16(b, 4))
}

fn check_len(ty: u16, b: &[u8], want: usize) -> Result<(), ProtoError> {
    if b.len() == want { Ok(()) } else { Err(ProtoError::Length { ty, len: b.len(), want }) }
}

fn check_fds(ty: u16, fds: &[OwnedFd], want: usize) -> Result<(), ProtoError> {
    if fds.len() == want { Ok(()) } else { Err(ProtoError::Fds { ty, got: fds.len(), want }) }
}

fn field(ty: u16, field: &'static str, value: impl Into<u64>) -> ProtoError {
    ProtoError::Field { ty, field, value: value.into() }
}

/// Decodes and validates one engine → client message. `fds` are the descriptors received with
/// it; on error they are dropped (closed) here.
pub fn decode_engine(b: &[u8], mut fds: Vec<OwnedFd>) -> Result<EngineMsg, ProtoError> {
    let ty = check_header(b)?;
    match ty {
        T_CANVAS => {
            check_len(ty, b, CANVAS_LEN)?;
            let mut offsets = [0u32; 4];
            let mut strides = [0u32; 4];
            for i in 0..4 {
                offsets[i] = get32(b, 32 + 4 * i);
                strides[i] = get32(b, 48 + 4 * i);
            }
            let d = CanvasDesc {
                canvas: get32(b, 8),
                width: get32(b, 12),
                height: get32(b, 16),
                drm_fourcc: get32(b, 20),
                modifier: get64(b, 24),
                offsets,
                strides,
                planes: get32(b, 64),
                buffer_count: get32(b, 68),
                generation: get32(b, 72),
            };
            if d.canvas >= CANVAS_COUNT {
                return Err(field(ty, "canvas", d.canvas));
            }
            if !(1..=MAX_DIMENSION).contains(&d.width) {
                return Err(field(ty, "width", d.width));
            }
            if !(1..=MAX_DIMENSION).contains(&d.height) {
                return Err(field(ty, "height", d.height));
            }
            if d.planes != 1 {
                return Err(field(ty, "planes", d.planes));
            }
            if !(1..=MAX_BUFFERS).contains(&d.buffer_count) {
                return Err(field(ty, "buffer_count", d.buffer_count));
            }
            let min_stride = if d.is_shm() { d.width * 4 } else { 1 };
            if d.strides[0] < min_stride {
                return Err(field(ty, "strides[0]", d.strides[0]));
            }
            check_fds(ty, &fds, d.buffer_count as usize)?;
            Ok(EngineMsg::Canvas(d, fds))
        }
        T_FRAME => {
            check_len(ty, b, FRAME_LEN)?;
            let has_fence = get32(b, 36);
            let f = Frame {
                canvas: get32(b, 8),
                buffer: get32(b, 12),
                seq: get64(b, 16),
                monotonic_ns: get64(b, 24),
                generation: get32(b, 32),
                has_fence: has_fence == 1,
            };
            if f.canvas >= CANVAS_COUNT {
                return Err(field(ty, "canvas", f.canvas));
            }
            if has_fence > 1 {
                return Err(field(ty, "has_fence", has_fence));
            }
            if f.buffer >= MAX_BUFFERS {
                return Err(field(ty, "buffer", f.buffer));
            }
            check_fds(ty, &fds, usize::from(f.has_fence))?;
            Ok(EngineMsg::Frame(f, fds.pop()))
        }
        T_GOODBYE => {
            check_len(ty, b, GOODBYE_LEN)?;
            check_fds(ty, &fds, 0)?;
            let canvas = get32(b, 12);
            let canvas = match canvas {
                ALL_CANVASES => None,
                c if c < CANVAS_COUNT => Some(c),
                c => return Err(field(ty, "canvas", c)),
            };
            Ok(EngineMsg::Goodbye(Goodbye { reason: get32(b, 8), canvas }))
        }
        other => Err(ProtoError::UnknownType(other)),
    }
}

/// Engine-side encoders and a client-message decoder, used by the fake engine in tests.
#[cfg(test)]
pub mod testing {
    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum ClientMsg {
        Hello(Hello),
        Release(Release),
    }

    pub fn decode_client(b: &[u8]) -> Result<ClientMsg, ProtoError> {
        let ty = check_header(b)?;
        match ty {
            T_HELLO => {
                check_len(ty, b, HELLO_LEN)?;
                Ok(ClientMsg::Hello(Hello { client: get32(b, 8), want: get32(b, 12), flags: get32(b, 16) }))
            }
            T_RELEASE => {
                check_len(ty, b, RELEASE_LEN)?;
                Ok(ClientMsg::Release(Release { canvas: get32(b, 8), buffer: get32(b, 12), seq: get64(b, 16) }))
            }
            other => Err(ProtoError::UnknownType(other)),
        }
    }

    impl CanvasDesc {
        pub fn encode(&self) -> [u8; CANVAS_LEN] {
            let mut b = [0u8; CANVAS_LEN];
            b[..HDR_LEN].copy_from_slice(&header(T_CANVAS));
            put32(&mut b, 8, self.canvas);
            put32(&mut b, 12, self.width);
            put32(&mut b, 16, self.height);
            put32(&mut b, 20, self.drm_fourcc);
            put64(&mut b, 24, self.modifier);
            for i in 0..4 {
                put32(&mut b, 32 + 4 * i, self.offsets[i]);
                put32(&mut b, 48 + 4 * i, self.strides[i]);
            }
            put32(&mut b, 64, self.planes);
            put32(&mut b, 68, self.buffer_count);
            put32(&mut b, 72, self.generation);
            b
        }
    }

    impl Frame {
        pub fn encode(&self) -> [u8; FRAME_LEN] {
            let mut b = [0u8; FRAME_LEN];
            b[..HDR_LEN].copy_from_slice(&header(T_FRAME));
            put32(&mut b, 8, self.canvas);
            put32(&mut b, 12, self.buffer);
            put64(&mut b, 16, self.seq);
            put64(&mut b, 24, self.monotonic_ns);
            put32(&mut b, 32, self.generation);
            put32(&mut b, 36, u32::from(self.has_fence));
            b
        }
    }

    impl Goodbye {
        pub fn encode(&self) -> [u8; GOODBYE_LEN] {
            let mut b = [0u8; GOODBYE_LEN];
            b[..HDR_LEN].copy_from_slice(&header(T_GOODBYE));
            put32(&mut b, 8, self.reason);
            put32(&mut b, 12, self.canvas.unwrap_or(ALL_CANVASES));
            b
        }
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::frames::testutil::{memfd, open_fds_named};

    fn memfds(name: &str, n: usize) -> Vec<OwnedFd> {
        (0..n).map(|_| memfd(name, 0)).collect()
    }

    fn canvas_desc() -> CanvasDesc {
        CanvasDesc {
            canvas: 2,
            width: 1920,
            height: 1080,
            drm_fourcc: DRM_FORMAT_ABGR8888,
            modifier: 0x0300_0000_0060_6014,
            offsets: [0, 0, 0, 0],
            strides: [7680, 0, 0, 0],
            planes: 1,
            buffer_count: 4,
            generation: 7,
        }
    }

    #[test]
    fn client_messages_round_trip_with_c_layout() {
        let h = Hello { client: CLIENT_UI, want: 0b1010, flags: FLAG_DMABUF };
        let b = h.encode();
        assert_eq!(&b[..8], &[0x53, 0x45, 0x46, 0x52, 1, 0, 1, 0], "magic 'SEFR', type 1, version 1");
        assert_eq!(&b[8..24], &[2, 0, 0, 0, 10, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(decode_client(&b), Ok(ClientMsg::Hello(h)));

        let r = Release { canvas: 3, buffer: 1, seq: 0x0102_0304_0506_0708 };
        let b = r.encode();
        assert_eq!(&b[4..6], &[3, 0]);
        assert_eq!(&b[16..24], &[8, 7, 6, 5, 4, 3, 2, 1]);
        assert_eq!(decode_client(&b), Ok(ClientMsg::Release(r)));
    }

    #[test]
    fn canvas_round_trip_and_field_offsets() {
        let d = canvas_desc();
        let b = d.encode();
        // C layout: modifier at 24, offsets at 32, strides at 48, planes/buffer_count/generation at 64/68/72.
        assert_eq!(get64(&b, 24), d.modifier);
        assert_eq!(get32(&b, 48), 7680);
        assert_eq!((get32(&b, 64), get32(&b, 68), get32(&b, 72)), (1, 4, 7));
        match decode_engine(&b, memfds("se-proto-canvas", 4)).unwrap() {
            EngineMsg::Canvas(got, fds) => {
                assert_eq!(got, d);
                assert_eq!(fds.len(), 4);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn frame_decodes_from_hand_written_bytes() {
        let mut b = vec![0x53, 0x45, 0x46, 0x52, 11, 0, 1, 0];
        b.extend_from_slice(&1u32.to_le_bytes()); // canvas
        b.extend_from_slice(&3u32.to_le_bytes()); // buffer
        b.extend_from_slice(&99u64.to_le_bytes()); // seq
        b.extend_from_slice(&123_456_789u64.to_le_bytes()); // monotonic_ns
        b.extend_from_slice(&5u32.to_le_bytes()); // generation
        b.extend_from_slice(&1u32.to_le_bytes()); // has_fence
        assert_eq!(b.len(), FRAME_LEN);
        let want = Frame { canvas: 1, buffer: 3, seq: 99, monotonic_ns: 123_456_789, generation: 5, has_fence: true };
        assert_eq!(want.encode().as_slice(), b.as_slice());
        match decode_engine(&b, memfds("se-proto-frame", 1)).unwrap() {
            EngineMsg::Frame(f, Some(_fence)) => assert_eq!(f, want),
            other => panic!("unexpected {other:?}"),
        }
        let no_fence = Frame { has_fence: false, ..want };
        assert!(matches!(decode_engine(&no_fence.encode(), vec![]), Ok(EngineMsg::Frame(f, None)) if f == no_fence));
    }

    #[test]
    fn goodbye_round_trip() {
        for g in [Goodbye { reason: 1, canvas: None }, Goodbye { reason: 3, canvas: Some(0) }] {
            assert!(matches!(decode_engine(&g.encode(), vec![]), Ok(EngineMsg::Goodbye(got)) if got == g));
        }
    }

    #[test]
    fn malformed_messages_are_rejected() {
        let good = canvas_desc().encode();
        let cases: Vec<(Vec<u8>, usize, ProtoError)> = vec![
            (good[..5].to_vec(), 0, ProtoError::Short(5)),
            (
                {
                    let mut b = good;
                    b[0] = 0;
                    b.to_vec()
                },
                4,
                ProtoError::Magic(0x5246_4500),
            ),
            (
                {
                    let mut b = good;
                    b[6] = 2;
                    b.to_vec()
                },
                4,
                ProtoError::Version(2),
            ),
            (
                {
                    let mut b = good;
                    b[4] = 99;
                    b.to_vec()
                },
                4,
                ProtoError::UnknownType(99),
            ),
            (good[..79].to_vec(), 4, ProtoError::Length { ty: T_CANVAS, len: 79, want: CANVAS_LEN }),
            (good.to_vec(), 3, ProtoError::Fds { ty: T_CANVAS, got: 3, want: 4 }),
            (CanvasDesc { canvas: 4, ..canvas_desc() }.encode().to_vec(), 4, field(T_CANVAS, "canvas", 4u32)),
            (CanvasDesc { width: 0, ..canvas_desc() }.encode().to_vec(), 4, field(T_CANVAS, "width", 0u32)),
            (CanvasDesc { height: MAX_DIMENSION + 1, ..canvas_desc() }.encode().to_vec(), 4, field(T_CANVAS, "height", MAX_DIMENSION + 1)),
            (CanvasDesc { planes: 2, ..canvas_desc() }.encode().to_vec(), 4, field(T_CANVAS, "planes", 2u32)),
            (CanvasDesc { buffer_count: 0, ..canvas_desc() }.encode().to_vec(), 0, field(T_CANVAS, "buffer_count", 0u32)),
            (CanvasDesc { drm_fourcc: 0, strides: [1919 * 4, 0, 0, 0], ..canvas_desc() }.encode().to_vec(), 4, field(T_CANVAS, "strides[0]", 1919 * 4u32)),
            (
                Frame { canvas: 0, buffer: 0, seq: 1, monotonic_ns: 0, generation: 1, has_fence: true }.encode().to_vec(),
                0,
                ProtoError::Fds { ty: T_FRAME, got: 0, want: 1 },
            ),
            (
                {
                    let mut b = Frame { canvas: 0, buffer: 0, seq: 1, monotonic_ns: 0, generation: 1, has_fence: false }.encode();
                    put32(&mut b, 36, 2);
                    b.to_vec()
                },
                0,
                field(T_FRAME, "has_fence", 2u32),
            ),
            (Frame { canvas: 9, buffer: 0, seq: 1, monotonic_ns: 0, generation: 1, has_fence: false }.encode().to_vec(), 0, field(T_FRAME, "canvas", 9u32)),
            (Goodbye { reason: 1, canvas: Some(7) }.encode().to_vec(), 0, field(T_GOODBYE, "canvas", 7u32)),
            (Goodbye { reason: 1, canvas: None }.encode().to_vec(), 1, ProtoError::Fds { ty: T_GOODBYE, got: 1, want: 0 }),
            (Hello { client: 2, want: 1, flags: 0 }.encode().to_vec(), 0, ProtoError::UnknownType(T_HELLO)),
        ];
        for (i, (bytes, nfds, want)) in cases.into_iter().enumerate() {
            let name = format!("se-proto-malformed-{i}");
            let fds = memfds(&name, nfds);
            assert_eq!(open_fds_named(&name), nfds);
            let err = decode_engine(&bytes, fds).expect_err(&format!("case {i} must fail"));
            assert_eq!(err, want, "case {i}");
            assert_eq!(open_fds_named(&name), 0, "case {i}: the failed decode must close every fd");
        }
    }
}
