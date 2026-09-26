//! Per-frame work on the capture threads: YUYV passthrough, MJPEG → RGBA, picture statistics,
//! signal detection, and frame-rate measurement. Nothing here allocates per frame.

use se_proto::Ts;

/// Sparse luma statistics (0–1) over a fixed sample grid.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LumaStats {
    pub mean: f32,
    pub max: f32,
    pub stddev: f32,
}

const GRID_X: usize = 32;
const GRID_Y: usize = 18;

fn finish(sum: f64, sum2: f64, max: u8, n: usize) -> LumaStats {
    if n == 0 {
        return LumaStats::default();
    }
    let mean = sum / n as f64;
    let var = (sum2 / n as f64 - mean * mean).max(0.0);
    LumaStats { mean: (mean / 255.0) as f32, max: max as f32 / 255.0, stddev: (var.sqrt() / 255.0) as f32 }
}

/// Stats from the Y samples of a packed YUYV frame.
pub fn yuyv_stats(data: &[u8], width: u32, height: u32, stride: u32) -> LumaStats {
    let (w, h, stride) = (width as usize, height as usize, stride as usize);
    let (mut sum, mut sum2, mut max, mut n) = (0f64, 0f64, 0u8, 0usize);
    for gy in 0..GRID_Y {
        let y = (gy * 2 + 1) * h / (GRID_Y * 2);
        for gx in 0..GRID_X {
            let x = (gx * 2 + 1) * w / (GRID_X * 2);
            let Some(&v) = data.get(y * stride + x * 2) else { continue };
            sum += v as f64;
            sum2 += (v as f64) * (v as f64);
            max = max.max(v);
            n += 1;
        }
    }
    finish(sum, sum2, max, n)
}

/// Stats from an RGBA frame (luma ≈ (r + 2g + b) / 4).
pub fn rgba_stats(data: &[u8], width: u32, height: u32, stride: u32) -> LumaStats {
    let (w, h, stride) = (width as usize, height as usize, stride as usize);
    let (mut sum, mut sum2, mut max, mut n) = (0f64, 0f64, 0u8, 0usize);
    for gy in 0..GRID_Y {
        let y = (gy * 2 + 1) * h / (GRID_Y * 2);
        for gx in 0..GRID_X {
            let x = (gx * 2 + 1) * w / (GRID_X * 2);
            let o = y * stride + x * 4;
            let Some(px) = data.get(o..o + 3) else { continue };
            let v = ((px[0] as u16 + 2 * px[1] as u16 + px[2] as u16) / 4) as u8;
            sum += v as f64;
            sum2 += (v as f64) * (v as f64);
            max = max.max(v);
            n += 1;
        }
    }
    finish(sum, sum2, max, n)
}

/// Copy `height` rows of `row_bytes` between buffers with different strides.
pub fn copy_rows(src: &[u8], src_stride: usize, dst: &mut [u8], dst_stride: usize, row_bytes: usize, height: usize) -> bool {
    if src_stride == dst_stride && src.len() >= dst_stride * height && dst.len() >= dst_stride * height {
        dst[..dst_stride * height].copy_from_slice(&src[..dst_stride * height]);
        return true;
    }
    if src.len() < src_stride * (height - 1) + row_bytes || dst.len() < dst_stride * (height - 1) + row_bytes {
        return false;
    }
    for y in 0..height {
        dst[y * dst_stride..y * dst_stride + row_bytes].copy_from_slice(&src[y * src_stride..y * src_stride + row_bytes]);
    }
    true
}

/// MJPEG → RGBA with one reused libjpeg-turbo handle.
pub struct MjpegDecoder {
    dec: turbojpeg::Decompressor,
}

impl MjpegDecoder {
    pub fn new() -> Result<MjpegDecoder, String> {
        let mut dec = turbojpeg::Decompressor::new().map_err(|e| e.to_string())?;
        // Chroma upsampling quality is irrelevant at 1080p30 camera noise levels; fancy
        // upsampling costs ~15 % more CPU.
        let _ = dec.set_fast_upsample(true);
        Ok(MjpegDecoder { dec })
    }

    /// Size from the JPEG header; None for truncated/corrupt frames (no SOI/EOI or bad header).
    pub fn header(&mut self, jpeg: &[u8]) -> Option<(u32, u32)> {
        if jpeg.len() < 4 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
            return None;
        }
        // Some UVC cameras pad the payload; the EOI marker must be near the end.
        let tail = &jpeg[jpeg.len().saturating_sub(64)..];
        if !tail.windows(2).any(|w| w == [0xFF, 0xD9]) {
            return None;
        }
        let h = self.dec.read_header(jpeg).ok()?;
        Some((h.width as u32, h.height as u32))
    }

    /// Decode into `dst` (RGBA, `stride` bytes per row).
    pub fn decode_rgba(&mut self, jpeg: &[u8], dst: &mut [u8], width: u32, height: u32, stride: u32) -> Result<(), String> {
        let img =
            turbojpeg::Image { pixels: dst, width: width as usize, pitch: stride as usize, height: height as usize, format: turbojpeg::PixelFormat::RGBA };
        self.dec.decompress(jpeg, img).map_err(|e| e.to_string())
    }
}

/// Picture/timeout/input-status → `signal` with hysteresis.
#[derive(Debug)]
pub struct SignalDetector {
    pub timeout_ns: u64,
    pub black_level: f32,
    pub hold_ns: u64,
    last_frame: Option<Ts>,
    bad_since: Option<Ts>,
    good_run: u32,
    input_ok: bool,
    signal: bool,
    pub last_stats: LumaStats,
}

/// Stddev below this (≈1 code value) means a generated flat frame (no-signal pattern).
const FLAT_STDDEV: f32 = 1.0 / 255.0;
const GOOD_FRAMES_TO_RECOVER: u32 = 3;

impl SignalDetector {
    pub fn new(timeout_ms: u64, black_level: f32, hold_ms: u64) -> SignalDetector {
        SignalDetector {
            timeout_ns: timeout_ms * 1_000_000,
            black_level,
            hold_ns: hold_ms * 1_000_000,
            last_frame: None,
            bad_since: None,
            good_run: 0,
            input_ok: true,
            signal: false,
            last_stats: LumaStats::default(),
        }
    }

    pub fn picture(&self, s: &LumaStats) -> bool {
        s.max >= self.black_level && s.stddev >= FLAT_STDDEV
    }

    /// Feed one frame's statistics.
    pub fn frame(&mut self, now: Ts, s: LumaStats) {
        self.last_frame = Some(now);
        self.last_stats = s;
        if self.input_ok && self.picture(&s) {
            self.good_run += 1;
            self.bad_since = None;
            if self.good_run >= GOOD_FRAMES_TO_RECOVER {
                self.signal = true;
            }
        } else {
            self.good_run = 0;
            let since = *self.bad_since.get_or_insert(now);
            if now.saturating_sub(since) >= self.hold_ns {
                self.signal = false;
            }
        }
    }

    /// Input status from the driver (capture cards report HDMI lock).
    pub fn input(&mut self, ok: bool) {
        self.input_ok = ok;
        if !ok {
            self.signal = false;
            self.good_run = 0;
        }
    }

    /// Current signal state, applying the frame timeout.
    pub fn tick(&mut self, now: Ts) -> bool {
        match self.last_frame {
            Some(t) if now.saturating_sub(t) <= self.timeout_ns => {}
            _ => {
                self.signal = false;
                self.good_run = 0;
            }
        }
        self.signal
    }

    pub fn reset(&mut self) {
        self.last_frame = None;
        self.bad_since = None;
        self.good_run = 0;
        self.signal = false;
    }
}

/// Frames per second over ~1 s windows.
#[derive(Debug, Default)]
pub struct FpsMeter {
    start: Option<Ts>,
    frames: u32,
    pub fps: f64,
}

impl FpsMeter {
    pub fn frame(&mut self, now: Ts) {
        if self.start.is_none() {
            self.start = Some(now);
            self.frames = 0;
            return;
        }
        self.frames += 1;
    }

    /// Close the window if ≥ 1 s passed; returns the new rate when it closes.
    pub fn tick(&mut self, now: Ts) -> Option<f64> {
        let start = self.start?;
        let el = now.saturating_sub(start);
        if el < 1_000_000_000 {
            return None;
        }
        self.fps = self.frames as f64 * 1e9 / el as f64;
        self.start = Some(now);
        self.frames = 0;
        Some(self.fps)
    }

    pub fn reset(&mut self) {
        *self = FpsMeter::default();
    }
}

/// Dropped frames from V4L2 sequence numbers.
#[derive(Debug, Default)]
pub struct DropCounter {
    last: Option<u32>,
    pub dropped: u64,
}

impl DropCounter {
    pub fn frame(&mut self, seq: u32) {
        if let Some(l) = self.last {
            let gap = seq.wrapping_sub(l);
            if gap > 1 && gap < 1_000_000 {
                self.dropped += (gap - 1) as u64;
            }
        }
        self.last = Some(seq);
    }
    pub fn restart(&mut self) {
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MS: u64 = 1_000_000;

    fn yuyv(w: u32, h: u32, f: impl Fn(u32, u32) -> u8) -> Vec<u8> {
        let mut v = vec![128u8; (w * h * 2) as usize];
        for y in 0..h {
            for x in 0..w {
                v[((y * w + x) * 2) as usize] = f(x, y);
            }
        }
        v
    }

    #[test]
    fn yuyv_stats_see_flat_black_and_live_pictures() {
        let flat = yuyv(64, 36, |_, _| 16);
        let s = yuyv_stats(&flat, 64, 36, 128);
        assert!((s.mean - 16.0 / 255.0).abs() < 1e-3 && s.stddev < 1e-4);
        let live = yuyv(64, 36, |x, y| ((x * 7 + y * 13) % 200 + 30) as u8);
        let s = yuyv_stats(&live, 64, 36, 128);
        assert!(s.stddev > 0.05 && s.max > 0.5, "{s:?}");
        let det = SignalDetector::new(500, 0.12, 700);
        assert!(!det.picture(&yuyv_stats(&flat, 64, 36, 128)));
        assert!(det.picture(&s));
        // dark but noisy (lens cap) counts as black
        let dark = yuyv(64, 36, |x, _| 16 + (x % 5) as u8);
        assert!(!det.picture(&yuyv_stats(&dark, 64, 36, 128)));
    }

    #[test]
    fn stats_respect_stride_padding() {
        // stride 160 for a 64-px row (128 bytes): padding bytes must not be sampled
        let (w, h, stride) = (64u32, 8u32, 160u32);
        let mut v = vec![255u8; (stride * h) as usize];
        for y in 0..h {
            for x in 0..w * 2 {
                v[(y * stride + x) as usize] = 20;
            }
        }
        assert!((yuyv_stats(&v, w, h, stride).max - 20.0 / 255.0).abs() < 1e-3);
    }

    #[test]
    fn detector_hysteresis_timeout_and_input() {
        let live = LumaStats { mean: 0.4, max: 0.9, stddev: 0.1 };
        let flat = LumaStats { mean: 0.06, max: 0.06, stddev: 0.0 };
        let mut d = SignalDetector::new(500, 0.12, 700);
        let mut t = 0;
        for _ in 0..2 {
            t += 16 * MS;
            d.frame(t, live);
        }
        assert!(!d.tick(t), "needs 3 good frames");
        t += 16 * MS;
        d.frame(t, live);
        assert!(d.tick(t));
        // a short black glitch doesn't drop the signal
        for _ in 0..10 {
            t += 16 * MS;
            d.frame(t, flat);
        }
        assert!(d.tick(t));
        for _ in 0..40 {
            t += 16 * MS;
            d.frame(t, flat);
        }
        assert!(!d.tick(t), "flat for > hold");
        for _ in 0..3 {
            t += 16 * MS;
            d.frame(t, live);
        }
        assert!(d.tick(t));
        // frames stop → timeout
        assert!(d.tick(t + 400 * MS));
        assert!(!d.tick(t + 600 * MS));
        // input status overrides pictures
        for _ in 0..3 {
            t += 16 * MS;
            d.frame(t, live);
        }
        d.input(false);
        t += 16 * MS;
        d.frame(t, live);
        assert!(!d.tick(t));
    }

    #[test]
    fn fps_meter_and_drops() {
        let mut m = FpsMeter::default();
        let mut t = 0;
        for _ in 0..61 {
            m.frame(t);
            t += 16_666_667;
        }
        let fps = m.tick(t - 16_666_667).unwrap();
        assert!((fps - 60.0).abs() < 0.1, "{fps}");
        assert!(m.tick(t).is_none(), "window just restarted");

        let mut d = DropCounter::default();
        for s in [10, 11, 12, 15, 16] {
            d.frame(s);
        }
        assert_eq!(d.dropped, 2);
        d.restart();
        d.frame(0);
        assert_eq!(d.dropped, 2, "stream restart resets the sequence");
    }

    #[test]
    fn copy_rows_with_different_strides() {
        let src: Vec<u8> = (0..30).collect(); // 3 rows × stride 10, 8 bytes used
        let mut dst = vec![0u8; 24];
        assert!(copy_rows(&src, 10, &mut dst, 8, 8, 3));
        assert_eq!(&dst[8..16], &src[10..18]);
        assert!(!copy_rows(&src[..15], 10, &mut dst, 8, 8, 3), "short source is rejected");
    }

    fn synthetic_jpeg(w: usize, h: usize) -> Vec<u8> {
        let mut px = vec![0u8; w * h * 4];
        for y in 0..h {
            for x in 0..w {
                let o = (y * w + x) * 4;
                px[o] = (x * 255 / w) as u8;
                px[o + 1] = (y * 255 / h) as u8;
                px[o + 2] = 128;
                px[o + 3] = 255;
            }
        }
        let img = turbojpeg::Image { pixels: px.as_slice(), width: w, pitch: w * 4, height: h, format: turbojpeg::PixelFormat::RGBA };
        turbojpeg::compress(img, 90, turbojpeg::Subsamp::Sub2x1).unwrap().to_vec()
    }

    #[test]
    fn mjpeg_decodes_into_a_reused_rgba_buffer() {
        let jpeg = synthetic_jpeg(64, 32);
        let mut dec = MjpegDecoder::new().unwrap();
        assert_eq!(dec.header(&jpeg), Some((64, 32)));
        let stride = 64 * 4 + 16;
        let mut buf = vec![7u8; stride * 32];
        dec.decode_rgba(&jpeg, &mut buf, 64, 32, stride as u32).unwrap();
        // gradient survives: left dark red, right bright red, alpha opaque, padding untouched
        let (l, r) = (&buf[4 * 2..4 * 2 + 4], &buf[4 * 61..4 * 61 + 4]);
        assert!(l[0] < 30 && r[0] > 220, "{l:?} {r:?}");
        assert_eq!(buf[3], 255);
        assert_eq!(buf[64 * 4], 7, "row padding is not written");
        let s = rgba_stats(&buf, 64, 32, stride as u32);
        assert!(s.stddev > 0.05);
        // corrupt / truncated frames are rejected before touching the output
        assert_eq!(dec.header(&jpeg[..jpeg.len() / 2]), None);
        assert_eq!(dec.header(&[0u8; 100]), None);
    }

    #[test]
    fn frame_paths_do_not_allocate_after_warmup() {
        let jpeg = synthetic_jpeg(64, 32);
        let mut dec = MjpegDecoder::new().unwrap();
        let mut rgba = vec![0u8; 64 * 32 * 4];
        let src = yuyv(64, 32, |x, y| (x + y) as u8);
        let mut dst = vec![0u8; src.len()];
        dec.decode_rgba(&jpeg, &mut rgba, 64, 32, 256).unwrap();
        let mut det = SignalDetector::new(500, 0.12, 700);
        let mut fps = FpsMeter::default();
        let mut drops = DropCounter::default();
        let scope = se_alloc::Scope::begin();
        for i in 0..10u32 {
            assert!(dec.header(&jpeg).is_some());
            dec.decode_rgba(&jpeg, &mut rgba, 64, 32, 256).unwrap();
            det.frame(i as u64 * 16 * MS, rgba_stats(&rgba, 64, 32, 256));
            copy_rows(&src, 128, &mut dst, 128, 128, 32);
            det.frame(i as u64 * 16 * MS, yuyv_stats(&dst, 64, 32, 128));
            fps.frame(i as u64 * 16 * MS);
            drops.frame(i);
            det.tick(i as u64 * 16 * MS);
        }
        assert_eq!(scope.allocs(), 0, "Rust allocations in the per-frame path");
    }
}
