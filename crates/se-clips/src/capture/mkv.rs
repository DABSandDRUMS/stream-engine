//! A streaming Matroska input of raw pictures for the encoder pipe. Unlike rawvideo stdin this
//! carries each engine presentation timestamp explicitly, including gaps. A cluster per frame
//! avoids timestamp range limits; the header is generated once per file.

use std::io::{self, Write};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Feeders give up when the encoder stops reading for this long.
pub(super) const LOSS_TIMEOUT: Duration = Duration::from_secs(10);

/// Raw picture layouts ffmpeg's Matroska demuxer maps from a VfW FOURCC.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Layout {
    Rgba,
    Bgra,
    /// Packed 4:2:2.
    Yuy2,
    /// Y plane, then interleaved UV at half height.
    Nv12,
}

impl Layout {
    fn fourcc(self) -> &'static [u8; 4] {
        match self {
            Layout::Rgba => b"RGBA",
            Layout::Bgra => b"BGRA",
            Layout::Yuy2 => b"YUY2",
            Layout::Nv12 => b"NV12",
        }
    }

    fn bits(self) -> u16 {
        match self {
            Layout::Rgba | Layout::Bgra => 32,
            Layout::Yuy2 => 16,
            Layout::Nv12 => 12,
        }
    }

    /// Bytes of one tightly packed picture.
    pub(super) fn size(self, width: u32, height: u32) -> usize {
        let (w, h) = (width as usize, height as usize);
        match self {
            Layout::Rgba | Layout::Bgra => w * h * 4,
            Layout::Yuy2 => w * h * 2,
            Layout::Nv12 => w * h + w * h.div_ceil(2),
        }
    }

    /// Packed row bytes and row count of each plane.
    pub(super) fn planes(self, width: u32, height: u32) -> [(usize, usize); 2] {
        let (w, h) = (width as usize, height as usize);
        match self {
            Layout::Rgba | Layout::Bgra => [(w * 4, h), (0, 0)],
            Layout::Yuy2 => [(w * 2, h), (0, 0)],
            Layout::Nv12 => [(w, h), (w, h.div_ceil(2))],
        }
    }

    /// True when the source carries YUV (and needs an input matrix/range for conversion).
    pub(super) fn is_yuv(self) -> bool {
        matches!(self, Layout::Yuy2 | Layout::Nv12)
    }
}

pub(super) fn write_cancel(output: &mut UnixStream, mut bytes: &[u8], stop: &AtomicBool) -> Result<(), String> {
    let mut progress = Instant::now();
    while !bytes.is_empty() {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        match output.write(bytes) {
            Ok(0) => return Err("encoder pipe closed".into()),
            Ok(n) => {
                bytes = &bytes[n..];
                progress = Instant::now();
            }
            Err(e) if matches!(e.kind(), io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut | io::ErrorKind::Interrupted) => {
                if progress.elapsed() > LOSS_TIMEOUT {
                    return Err("encoder stopped reading pictures for 10 seconds".into());
                }
            }
            Err(e) => return Err(format!("encoder pipe: {e}")),
        }
    }
    Ok(())
}

fn size(n: u64, out: &mut Vec<u8>) {
    let count = (1..=8).find(|bytes| n < (1u64 << (7 * bytes)) - 1).unwrap_or(8);
    out.extend_from_slice(&(n | (1 << (7 * count))).to_be_bytes()[8 - count..]);
}
fn element(id: &[u8], body: &[u8], out: &mut Vec<u8>) {
    out.extend_from_slice(id);
    size(body.len() as u64, out);
    out.extend_from_slice(body);
}
fn uint(id: &[u8], n: u64, out: &mut Vec<u8>) {
    let bytes = n.to_be_bytes();
    let begin = bytes.iter().position(|b| *b != 0).unwrap_or(7);
    element(id, &bytes[begin..], out);
}

pub(super) fn header(width: u32, height: u32, fps: u32, layout: Layout) -> Vec<u8> {
    let mut out = Vec::new();
    let mut ebml = Vec::new();
    for (id, n) in [(0x4286u16, 1), (0x42f7, 1), (0x42f2, 4), (0x42f3, 8)] {
        uint(&id.to_be_bytes(), n, &mut ebml);
    }
    element(&[0x42, 0x82], b"matroska", &mut ebml);
    uint(&[0x42, 0x87], 4, &mut ebml);
    uint(&[0x42, 0x85], 2, &mut ebml);
    element(&[0x1a, 0x45, 0xdf, 0xa3], &ebml, &mut out);
    out.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff]);
    let mut info = Vec::new();
    uint(&[0x2a, 0xd7, 0xb1], 1000, &mut info);
    element(&[0x4d, 0x80], b"stream-engine", &mut info);
    element(&[0x57, 0x41], b"stream-engine", &mut info);
    element(&[0x15, 0x49, 0xa9, 0x66], &info, &mut out);
    let mut track = Vec::new();
    uint(&[0xd7], 1, &mut track);
    uint(&[0x73, 0xc5], 1, &mut track);
    uint(&[0x83], 1, &mut track);
    uint(&[0x23, 0xe3, 0x83], 1_000_000_000 / fps as u64, &mut track);
    element(&[0x86], b"V_MS/VFW/FOURCC", &mut track);
    let mut bitmap = [0u8; 40];
    bitmap[..4].copy_from_slice(&40u32.to_le_bytes());
    bitmap[4..8].copy_from_slice(&width.to_le_bytes());
    // Negative height: top-down rows for RGB; YUV FOURCCs are always top-down.
    let rows = if layout.is_yuv() { height as i32 } else { -(height as i32) };
    bitmap[8..12].copy_from_slice(&rows.to_le_bytes());
    bitmap[12..14].copy_from_slice(&1u16.to_le_bytes());
    bitmap[14..16].copy_from_slice(&layout.bits().to_le_bytes());
    bitmap[16..20].copy_from_slice(layout.fourcc());
    bitmap[20..24].copy_from_slice(&(layout.size(width, height) as u32).to_le_bytes());
    element(&[0x63, 0xa2], &bitmap, &mut track);
    let mut video = Vec::new();
    uint(&[0xb0], width as u64, &mut video);
    uint(&[0xba], height as u64, &mut video);
    element(&[0xe0], &video, &mut track);
    let mut tracks = Vec::new();
    element(&[0xae], &track, &mut tracks);
    element(&[0x16, 0x54, 0xae, 0x6b], &tracks, &mut out);
    out
}

/// One picture at `time_us` after the file origin. `picture` is tightly packed.
pub(super) fn write_picture(output: &mut UnixStream, time_us: u64, picture: &[u8], stop: &AtomicBool) -> Result<(), String> {
    // Fixed-width legal EBML sizes and uints avoid allocating a header per picture.
    let block_len = picture.len() as u64 + 4;
    let cluster_len = 10 + 9 + block_len;
    let mut bytes = [0u8; 35];
    bytes[..4].copy_from_slice(&[0x1f, 0x43, 0xb6, 0x75]);
    bytes[4..12].copy_from_slice(&(cluster_len | (1 << 56)).to_be_bytes());
    bytes[12..14].copy_from_slice(&[0xe7, 0x88]);
    bytes[14..22].copy_from_slice(&time_us.to_be_bytes());
    bytes[22] = 0xa3;
    bytes[23..31].copy_from_slice(&(block_len | (1 << 56)).to_be_bytes());
    bytes[31..].copy_from_slice(&[0x81, 0, 0, 0x80]);
    write_cancel(output, &bytes, stop)?;
    write_cancel(output, picture, stop)
}
