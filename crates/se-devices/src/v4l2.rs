//! Minimal, safe V4L2 access over raw ioctls: capabilities, formats and modes (including DV
//! timings for HDMI capture cards), controls, input status, events, and mmap streaming.
//!
//! Struct layouts come from the system `linux/videodev2.h` (bindgen at build time); request
//! codes are computed from those sizes exactly like the kernel's `_IOWR` macros.

use std::io;
use std::mem::size_of;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};

#[allow(non_camel_case_types, non_upper_case_globals, non_snake_case, dead_code, unnecessary_transmutes, clippy::all)]
pub mod sys {
    include!(concat!(env!("OUT_DIR"), "/v4l2_sys.rs"));
}

use sys::*;

const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;

const fn ioc(dir: u32, nr: u32, size: usize) -> libc::c_ulong {
    ((dir << 30) | ((size as u32) << 16) | ((b'V' as u32) << 8) | nr) as libc::c_ulong
}
const fn ior<T>(nr: u32) -> libc::c_ulong {
    ioc(IOC_READ, nr, size_of::<T>())
}
const fn iow<T>(nr: u32) -> libc::c_ulong {
    ioc(IOC_WRITE, nr, size_of::<T>())
}
const fn iowr<T>(nr: u32) -> libc::c_ulong {
    ioc(IOC_READ | IOC_WRITE, nr, size_of::<T>())
}

pub const VIDIOC_QUERYCAP: libc::c_ulong = ior::<v4l2_capability>(0);
pub const VIDIOC_ENUM_FMT: libc::c_ulong = iowr::<v4l2_fmtdesc>(2);
pub const VIDIOC_G_FMT: libc::c_ulong = iowr::<v4l2_format>(4);
pub const VIDIOC_S_FMT: libc::c_ulong = iowr::<v4l2_format>(5);
pub const VIDIOC_REQBUFS: libc::c_ulong = iowr::<v4l2_requestbuffers>(8);
pub const VIDIOC_QUERYBUF: libc::c_ulong = iowr::<v4l2_buffer>(9);
pub const VIDIOC_QBUF: libc::c_ulong = iowr::<v4l2_buffer>(15);
pub const VIDIOC_DQBUF: libc::c_ulong = iowr::<v4l2_buffer>(17);
pub const VIDIOC_STREAMON: libc::c_ulong = iow::<libc::c_int>(18);
pub const VIDIOC_STREAMOFF: libc::c_ulong = iow::<libc::c_int>(19);
pub const VIDIOC_G_PARM: libc::c_ulong = iowr::<v4l2_streamparm>(21);
pub const VIDIOC_S_PARM: libc::c_ulong = iowr::<v4l2_streamparm>(22);
pub const VIDIOC_ENUMINPUT: libc::c_ulong = iowr::<v4l2_input>(26);
pub const VIDIOC_QUERYMENU: libc::c_ulong = iowr::<v4l2_querymenu>(37);
pub const VIDIOC_G_INPUT: libc::c_ulong = ior::<libc::c_int>(38);
pub const VIDIOC_G_EXT_CTRLS: libc::c_ulong = iowr::<v4l2_ext_controls>(71);
pub const VIDIOC_S_EXT_CTRLS: libc::c_ulong = iowr::<v4l2_ext_controls>(72);
pub const VIDIOC_ENUM_FRAMESIZES: libc::c_ulong = iowr::<v4l2_frmsizeenum>(74);
pub const VIDIOC_ENUM_FRAMEINTERVALS: libc::c_ulong = iowr::<v4l2_frmivalenum>(75);
pub const VIDIOC_S_DV_TIMINGS: libc::c_ulong = iowr::<v4l2_dv_timings>(87);
pub const VIDIOC_G_DV_TIMINGS: libc::c_ulong = iowr::<v4l2_dv_timings>(88);
pub const VIDIOC_DQEVENT: libc::c_ulong = ior::<v4l2_event>(89);
pub const VIDIOC_SUBSCRIBE_EVENT: libc::c_ulong = iow::<v4l2_event_subscription>(90);
pub const VIDIOC_ENUM_DV_TIMINGS: libc::c_ulong = iowr::<v4l2_enum_dv_timings>(98);
pub const VIDIOC_QUERY_DV_TIMINGS: libc::c_ulong = ior::<v4l2_dv_timings>(99);
pub const VIDIOC_QUERY_EXT_CTRL: libc::c_ulong = iowr::<v4l2_query_ext_ctrl>(103);

/// `v4l2_fourcc('Y','U','Y','V')`.
pub const fn fourcc(c: &[u8; 4]) -> u32 {
    (c[0] as u32) | ((c[1] as u32) << 8) | ((c[2] as u32) << 16) | ((c[3] as u32) << 24)
}

pub const PIX_YUYV: u32 = fourcc(b"YUYV");
pub const PIX_MJPEG: u32 = fourcc(b"MJPG");
pub const PIX_JPEG: u32 = fourcc(b"JPEG");
pub const PIX_NV12: u32 = fourcc(b"NV12");
pub const PIX_UYVY: u32 = fourcc(b"UYVY");

pub fn fourcc_str(f: u32) -> String {
    f.to_le_bytes().iter().map(|&b| if b.is_ascii_graphic() { b as char } else { ' ' }).collect::<String>().trim_end().to_string()
}

/// Our name for a pixel format (`yuyv`, `mjpeg`, …), as used in `sources/*.toml`.
pub fn format_name(f: u32) -> String {
    match f {
        PIX_YUYV => "yuyv".into(),
        PIX_MJPEG | PIX_JPEG => "mjpeg".into(),
        PIX_NV12 => "nv12".into(),
        PIX_UYVY => "uyvy".into(),
        other => fourcc_str(other).to_lowercase(),
    }
}

pub fn parse_format_name(s: &str) -> Option<u32> {
    match s.to_ascii_lowercase().as_str() {
        "yuyv" | "yuy2" => Some(PIX_YUYV),
        "mjpeg" | "mjpg" => Some(PIX_MJPEG),
        "jpeg" => Some(PIX_JPEG),
        "nv12" => Some(PIX_NV12),
        "uyvy" => Some(PIX_UYVY),
        _ => None,
    }
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).trim().to_string()
}

fn cchars(b: &[libc::c_char]) -> String {
    // SAFETY: c_char and u8 have the same size and alignment.
    let bytes = unsafe { std::slice::from_raw_parts(b.as_ptr() as *const u8, b.len()) };
    cstr(bytes)
}

/// Convert a V4L2 control or menu name to the address-safe form `v4l2-ctl` uses
/// (`"White Balance, Automatic"` → `white_balance_automatic`).
pub fn control_var_name(name: &str) -> String {
    let mut s = String::with_capacity(name.len());
    let mut underscore = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            if underscore {
                s.push('_');
            }
            underscore = false;
            s.push(ch.to_ascii_lowercase());
        } else if !s.is_empty() {
            underscore = true;
        }
    }
    s
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Caps {
    pub driver: String,
    pub card: String,
    pub bus_info: String,
    pub version: u32,
    pub device_caps: u32,
}

impl Caps {
    pub fn is_capture(&self) -> bool {
        self.device_caps & V4L2_CAP_VIDEO_CAPTURE != 0 && self.device_caps & V4L2_CAP_STREAMING != 0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Fract {
    pub num: u32,
    pub den: u32,
}

impl Fract {
    /// Frames per second for a frame interval.
    pub fn fps(self) -> f64 {
        if self.num == 0 { 0.0 } else { self.den as f64 / self.num as f64 }
    }
}

/// One capture mode: format + size + available frame rates.
#[derive(Clone, Debug, PartialEq)]
pub struct Mode {
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub fps: Vec<f64>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PixFormat {
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub bytesperline: u32,
    pub sizeimage: u32,
    pub colorspace: u32,
    pub ycbcr_enc: u32,
    pub quantization: u32,
}

impl PixFormat {
    /// YCbCr matrix: `bt709` or `bt601` (driver default per colorspace otherwise).
    pub fn matrix(&self) -> &'static str {
        match self.ycbcr_enc {
            V4L2_YCBCR_ENC_709 => "bt709",
            V4L2_YCBCR_ENC_601 => "bt601",
            _ => match self.colorspace {
                V4L2_COLORSPACE_REC709 => "bt709",
                _ => "bt601",
            },
        }
    }

    /// Quantization: `full` or `limited` (V4L2 defaults: limited for YCbCr except JPEG/sRGB-JPEG).
    pub fn range(&self) -> &'static str {
        match self.quantization {
            V4L2_QUANTIZATION_FULL_RANGE => "full",
            V4L2_QUANTIZATION_LIM_RANGE => "limited",
            _ => {
                if self.colorspace == V4L2_COLORSPACE_JPEG || self.fourcc == PIX_MJPEG || self.fourcc == PIX_JPEG {
                    "full"
                } else {
                    "limited"
                }
            }
        }
    }
}

/// Detected/configured HDMI timing of a DV-capable input (capture cards).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DvTiming {
    pub width: u32,
    pub height: u32,
    pub interlaced: bool,
    pub pixelclock: u64,
    pub fps: f64,
    raw: RawDv,
}

#[derive(Clone, Copy, Default)]
struct RawDv(v4l2_dv_timings);

impl std::fmt::Debug for RawDv {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("v4l2_dv_timings")
    }
}

impl PartialEq for RawDv {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

impl DvTiming {
    fn from_raw(t: v4l2_dv_timings) -> DvTiming {
        // SAFETY: `bt` is the only member used for BT.656/1120 timings (type 0).
        let bt = unsafe { t.__bindgen_anon_1.bt };
        let total_w = (bt.width + bt.hfrontporch + bt.hsync + bt.hbackporch) as u64;
        let mut total_h = (bt.height + bt.vfrontporch + bt.vsync + bt.vbackporch) as u64;
        if bt.interlaced != 0 {
            total_h += (bt.il_vfrontporch + bt.il_vsync + bt.il_vbackporch) as u64;
        }
        let fps = if total_w * total_h > 0 { bt.pixelclock as f64 / (total_w * total_h) as f64 } else { 0.0 };
        DvTiming {
            width: bt.width,
            height: bt.height,
            interlaced: bt.interlaced != 0,
            pixelclock: bt.pixelclock,
            fps: (fps * 1000.0).round() / 1000.0,
            raw: RawDv(t),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ControlKind {
    Integer,
    Boolean,
    Menu,
    IntegerMenu,
    Button,
    Integer64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct MenuItem {
    pub index: i64,
    /// Display label (`"Manual Mode"`, or the number for integer menus).
    pub label: String,
    /// Address-safe option name (`manual_mode`).
    pub name: String,
}

/// One device control with its real range.
#[derive(Clone, Debug, PartialEq)]
pub struct Control {
    pub id: u32,
    /// Address-safe name (`white_balance_temperature`).
    pub name: String,
    /// Driver label (`White Balance Temperature`).
    pub label: String,
    pub kind: ControlKind,
    pub min: i64,
    pub max: i64,
    pub step: i64,
    pub default: i64,
    pub flags: u32,
    pub menu: Vec<MenuItem>,
    /// Current device value (None for write-only/buttons).
    pub value: Option<i64>,
}

impl Control {
    pub fn inactive(&self) -> bool {
        self.flags & V4L2_CTRL_FLAG_INACTIVE != 0
    }
    pub fn read_only(&self) -> bool {
        self.flags & (V4L2_CTRL_FLAG_READ_ONLY | V4L2_CTRL_FLAG_GRABBED) != 0
    }
    /// Auto-mode controls must be applied before the manual values they gate.
    pub fn is_auto_mode(&self) -> bool {
        self.name.contains("auto") || self.name.ends_with("_automatic") || self.name.ends_with("_continuous")
    }
}

/// An open V4L2 device node.
pub struct Device {
    fd: OwnedFd,
    path: PathBuf,
}

fn xioctl<T>(fd: RawFd, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
    loop {
        // SAFETY: `arg` points to a properly sized, initialized struct for `req`.
        let r = unsafe { libc::ioctl(fd, req as _, arg as *mut T) };
        if r != -1 {
            return Ok(());
        }
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::Interrupted {
            return Err(e);
        }
    }
}

impl Device {
    /// Open read/write. `nonblock` makes DQBUF return `WouldBlock` instead of sleeping.
    pub fn open(path: impl AsRef<Path>, nonblock: bool) -> io::Result<Device> {
        let path = path.as_ref().to_path_buf();
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut flags = libc::O_RDWR | libc::O_CLOEXEC;
        if nonblock {
            flags |= libc::O_NONBLOCK;
        }
        // SAFETY: valid NUL-terminated path.
        let fd = unsafe { libc::open(c.as_ptr(), flags) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd was just opened and is owned by nobody else.
        Ok(Device { fd: unsafe { OwnedFd::from_raw_fd(fd) }, path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn ioctl<T>(&self, req: libc::c_ulong, arg: &mut T) -> io::Result<()> {
        xioctl(self.fd.as_raw_fd(), req, arg)
    }

    pub fn caps(&self) -> io::Result<Caps> {
        let mut c = v4l2_capability::default();
        self.ioctl(VIDIOC_QUERYCAP, &mut c)?;
        let device_caps = if c.capabilities & V4L2_CAP_DEVICE_CAPS != 0 { c.device_caps } else { c.capabilities };
        Ok(Caps { driver: cstr(&c.driver), card: cstr(&c.card), bus_info: cstr(&c.bus_info), version: c.version, device_caps })
    }

    /// Pixel formats offered for video capture.
    pub fn formats(&self) -> io::Result<Vec<(u32, String)>> {
        let mut out = Vec::new();
        for index in 0.. {
            let mut d = v4l2_fmtdesc { index, type_: V4L2_BUF_TYPE_VIDEO_CAPTURE, ..Default::default() };
            match self.ioctl(VIDIOC_ENUM_FMT, &mut d) {
                Ok(()) => out.push((d.pixelformat, cstr(&d.description))),
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
                Err(e) => return Err(e),
            }
        }
        Ok(out)
    }

    /// Discrete (or the max of stepwise) frame sizes for a format; empty if unsupported.
    pub fn frame_sizes(&self, fourcc: u32) -> Vec<(u32, u32)> {
        let mut out = Vec::new();
        for index in 0.. {
            let mut s = v4l2_frmsizeenum { index, pixel_format: fourcc, ..Default::default() };
            if self.ioctl(VIDIOC_ENUM_FRAMESIZES, &mut s).is_err() {
                break;
            }
            // SAFETY: the union member matches `type_`.
            unsafe {
                if s.type_ == V4L2_FRMSIZE_TYPE_DISCRETE {
                    let d = s.__bindgen_anon_1.discrete;
                    out.push((d.width, d.height));
                } else {
                    let sw = s.__bindgen_anon_1.stepwise;
                    out.push((sw.max_width, sw.max_height));
                    if (sw.min_width, sw.min_height) != (sw.max_width, sw.max_height) {
                        out.push((sw.min_width, sw.min_height));
                    }
                    break;
                }
            }
        }
        out
    }

    /// Frame rates for a format and size (descending, deduplicated).
    pub fn frame_rates(&self, fourcc: u32, width: u32, height: u32) -> Vec<f64> {
        let mut out: Vec<f64> = Vec::new();
        for index in 0.. {
            let mut s = v4l2_frmivalenum { index, pixel_format: fourcc, width, height, ..Default::default() };
            if self.ioctl(VIDIOC_ENUM_FRAMEINTERVALS, &mut s).is_err() {
                break;
            }
            // SAFETY: the union member matches `type_`.
            unsafe {
                if s.type_ == V4L2_FRMIVAL_TYPE_DISCRETE {
                    let d = s.__bindgen_anon_1.discrete;
                    out.push(Fract { num: d.numerator, den: d.denominator }.fps());
                } else {
                    let sw = s.__bindgen_anon_1.stepwise;
                    out.push(Fract { num: sw.min.numerator, den: sw.min.denominator }.fps());
                    out.push(Fract { num: sw.max.numerator, den: sw.max.denominator }.fps());
                    break;
                }
            }
        }
        let mut out: Vec<f64> = out.into_iter().map(|f| (f * 1000.0).round() / 1000.0).collect();
        out.sort_by(|a, b| b.total_cmp(a));
        out.dedup();
        out
    }

    /// All DV timings the input supports (capture cards); empty if not a DV input.
    pub fn dv_timings(&self) -> Vec<DvTiming> {
        let mut out = Vec::new();
        for index in 0..256 {
            let mut e = v4l2_enum_dv_timings { index, ..Default::default() };
            if self.ioctl(VIDIOC_ENUM_DV_TIMINGS, &mut e).is_err() {
                break;
            }
            out.push(DvTiming::from_raw(e.timings));
        }
        out
    }

    /// Timing currently detected on the input (`ENOLINK`/`ENOLCK` when there is no signal).
    pub fn query_dv_timing(&self) -> io::Result<DvTiming> {
        let mut t = v4l2_dv_timings::default();
        self.ioctl(VIDIOC_QUERY_DV_TIMINGS, &mut t)?;
        Ok(DvTiming::from_raw(t))
    }

    pub fn current_dv_timing(&self) -> io::Result<DvTiming> {
        let mut t = v4l2_dv_timings::default();
        self.ioctl(VIDIOC_G_DV_TIMINGS, &mut t)?;
        Ok(DvTiming::from_raw(t))
    }

    pub fn set_dv_timing(&self, t: &DvTiming) -> io::Result<()> {
        let mut raw = t.raw.0;
        self.ioctl(VIDIOC_S_DV_TIMINGS, &mut raw)
    }

    /// Every mode the device can capture: format × size × frame rates. Capture cards without
    /// frame-size enumeration report one mode per supported DV timing.
    pub fn modes(&self) -> io::Result<Vec<Mode>> {
        let mut out = Vec::new();
        let dv = self.dv_timings();
        for (fourcc, _) in self.formats()? {
            let sizes = self.frame_sizes(fourcc);
            if sizes.is_empty() {
                if !dv.is_empty() {
                    for t in &dv {
                        if t.interlaced {
                            continue;
                        }
                        match out.iter_mut().find(|m: &&mut Mode| m.fourcc == fourcc && m.width == t.width && m.height == t.height) {
                            Some(m) => {
                                if !m.fps.contains(&t.fps) {
                                    m.fps.push(t.fps);
                                    m.fps.sort_by(|a, b| b.total_cmp(a));
                                }
                            }
                            None => out.push(Mode { fourcc, width: t.width, height: t.height, fps: vec![t.fps] }),
                        }
                    }
                } else if let Ok(f) = self.format() {
                    let fps = self.fps().ok().flatten().map(|f| vec![f]).unwrap_or_default();
                    out.push(Mode { fourcc, width: f.width, height: f.height, fps });
                }
                continue;
            }
            for (w, h) in sizes {
                let fps = self.frame_rates(fourcc, w, h);
                // some UVC firmwares list the same size twice: merge
                match out.iter_mut().find(|m: &&mut Mode| m.fourcc == fourcc && m.width == w && m.height == h) {
                    Some(m) => {
                        for f in fps {
                            if !m.fps.contains(&f) {
                                m.fps.push(f);
                            }
                        }
                        m.fps.sort_by(|a, b| b.total_cmp(a));
                    }
                    None => out.push(Mode { fourcc, width: w, height: h, fps }),
                }
            }
        }
        Ok(out)
    }

    fn pix(f: &v4l2_format) -> PixFormat {
        // SAFETY: `pix` is the active member for single-planar video capture.
        let p = unsafe { f.fmt.pix };
        // SAFETY: `ycbcr_enc` is the member used for YCbCr formats.
        let enc = unsafe { p.__bindgen_anon_1.ycbcr_enc };
        PixFormat {
            fourcc: p.pixelformat,
            width: p.width,
            height: p.height,
            bytesperline: p.bytesperline,
            sizeimage: p.sizeimage,
            colorspace: p.colorspace,
            ycbcr_enc: enc,
            quantization: p.quantization,
        }
    }

    pub fn format(&self) -> io::Result<PixFormat> {
        let mut f = v4l2_format { type_: V4L2_BUF_TYPE_VIDEO_CAPTURE, ..Default::default() };
        self.ioctl(VIDIOC_G_FMT, &mut f)?;
        Ok(Self::pix(&f))
    }

    /// Request a format; returns what the driver actually chose.
    pub fn set_format(&self, fourcc: u32, width: u32, height: u32) -> io::Result<PixFormat> {
        let mut f = v4l2_format { type_: V4L2_BUF_TYPE_VIDEO_CAPTURE, ..Default::default() };
        f.fmt.pix.width = width;
        f.fmt.pix.height = height;
        f.fmt.pix.pixelformat = fourcc;
        f.fmt.pix.field = V4L2_FIELD_NONE;
        self.ioctl(VIDIOC_S_FMT, &mut f)?;
        Ok(Self::pix(&f))
    }

    /// Current frame rate (None if the driver doesn't report one).
    pub fn fps(&self) -> io::Result<Option<f64>> {
        let mut p = v4l2_streamparm { type_: V4L2_BUF_TYPE_VIDEO_CAPTURE, ..Default::default() };
        self.ioctl(VIDIOC_G_PARM, &mut p)?;
        // SAFETY: `capture` is the active member for capture streams.
        let t = unsafe { p.parm.capture.timeperframe };
        Ok((t.numerator > 0 && t.denominator > 0).then(|| Fract { num: t.numerator, den: t.denominator }.fps()))
    }

    /// Request a frame rate; returns the rate the driver chose (None if it can't change it).
    pub fn set_fps(&self, fps: f64) -> io::Result<Option<f64>> {
        let mut p = v4l2_streamparm { type_: V4L2_BUF_TYPE_VIDEO_CAPTURE, ..Default::default() };
        self.ioctl(VIDIOC_G_PARM, &mut p)?;
        // SAFETY: `capture` is the active member for capture streams.
        let can = unsafe { p.parm.capture.capability } & V4L2_CAP_TIMEPERFRAME != 0;
        if !can || fps <= 0.0 {
            return self.fps();
        }
        let (num, den) = fps_to_fract(fps);
        p.parm.capture.timeperframe.numerator = num;
        p.parm.capture.timeperframe.denominator = den;
        self.ioctl(VIDIOC_S_PARM, &mut p)?;
        self.fps()
    }

    /// `V4L2_IN_ST_*` flags of the current input (0 = OK).
    pub fn input_status(&self) -> io::Result<u32> {
        let mut idx: libc::c_int = 0;
        if self.ioctl(VIDIOC_G_INPUT, &mut idx).is_err() {
            idx = 0;
        }
        let mut i = v4l2_input { index: idx as u32, ..Default::default() };
        self.ioctl(VIDIOC_ENUMINPUT, &mut i)?;
        Ok(i.status)
    }

    /// Enumerate user-facing controls (integer, boolean, menu, integer menu, button).
    pub fn controls(&self) -> io::Result<Vec<Control>> {
        let mut out = Vec::new();
        let mut id = V4L2_CTRL_FLAG_NEXT_CTRL;
        loop {
            let mut q = v4l2_query_ext_ctrl { id, ..Default::default() };
            match self.ioctl(VIDIOC_QUERY_EXT_CTRL, &mut q) {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
                Err(e) => return Err(e),
            }
            id = q.id | V4L2_CTRL_FLAG_NEXT_CTRL;
            if q.flags & V4L2_CTRL_FLAG_DISABLED != 0 {
                continue;
            }
            let kind = match q.type_ {
                V4L2_CTRL_TYPE_INTEGER => ControlKind::Integer,
                V4L2_CTRL_TYPE_BOOLEAN => ControlKind::Boolean,
                V4L2_CTRL_TYPE_MENU => ControlKind::Menu,
                V4L2_CTRL_TYPE_INTEGER_MENU => ControlKind::IntegerMenu,
                V4L2_CTRL_TYPE_BUTTON => ControlKind::Button,
                V4L2_CTRL_TYPE_INTEGER64 => ControlKind::Integer64,
                _ => continue,
            };
            let label = cchars(&q.name);
            let mut menu = Vec::new();
            if matches!(kind, ControlKind::Menu | ControlKind::IntegerMenu) {
                for index in q.minimum..=q.maximum {
                    let mut m = v4l2_querymenu { id: q.id, index: index as u32, ..Default::default() };
                    if self.ioctl(VIDIOC_QUERYMENU, &mut m).is_err() {
                        continue;
                    }
                    // SAFETY: the union member matches the control type (copied out of the
                    // packed struct before use).
                    let lbl = unsafe {
                        if kind == ControlKind::Menu {
                            let name = m.__bindgen_anon_1.name;
                            cstr(&name)
                        } else {
                            let value = m.__bindgen_anon_1.value;
                            value.to_string()
                        }
                    };
                    let name = if kind == ControlKind::Menu { control_var_name(&lbl) } else { lbl.clone() };
                    menu.push(MenuItem { index, label: lbl, name });
                }
            }
            let readable = q.flags & V4L2_CTRL_FLAG_WRITE_ONLY == 0 && kind != ControlKind::Button;
            let value = if readable { self.get_control(q.id, kind == ControlKind::Integer64).ok() } else { None };
            out.push(Control {
                id: q.id,
                name: control_var_name(&label),
                label,
                kind,
                min: q.minimum,
                max: q.maximum,
                step: q.step.max(1) as i64,
                default: q.default_value,
                flags: q.flags,
                menu,
                value,
            });
        }
        Ok(out)
    }

    /// Current value (`is64` for `INTEGER64` controls, whose value lives in `value64`).
    pub fn get_control(&self, id: u32, is64: bool) -> io::Result<i64> {
        let mut c = v4l2_ext_control { id, ..Default::default() };
        let mut cs = v4l2_ext_controls { count: 1, controls: &mut c, ..Default::default() };
        cs.__bindgen_anon_1.which = V4L2_CTRL_WHICH_CUR_VAL;
        self.ioctl(VIDIOC_G_EXT_CTRLS, &mut cs)?;
        // SAFETY: the driver wrote the member matching the control's size.
        Ok(unsafe { if is64 { c.__bindgen_anon_1.value64 } else { c.__bindgen_anon_1.value as i64 } })
    }

    pub fn set_control(&self, id: u32, value: i64, is64: bool) -> io::Result<()> {
        let mut c = v4l2_ext_control { id, ..Default::default() };
        if is64 {
            c.__bindgen_anon_1.value64 = value;
        } else {
            c.__bindgen_anon_1.value = value.clamp(i32::MIN as i64, i32::MAX as i64) as i32;
        }
        let mut cs = v4l2_ext_controls { count: 1, controls: &mut c, ..Default::default() };
        cs.__bindgen_anon_1.which = V4L2_CTRL_WHICH_CUR_VAL;
        self.ioctl(VIDIOC_S_EXT_CTRLS, &mut cs)
    }

    /// Subscribe to source-change events (resolution/signal changes on HDMI inputs).
    pub fn subscribe_source_change(&self) -> io::Result<()> {
        let mut s = v4l2_event_subscription { type_: V4L2_EVENT_SOURCE_CHANGE, ..Default::default() };
        self.ioctl(VIDIOC_SUBSCRIBE_EVENT, &mut s)
    }

    /// Dequeue one pending event type (`V4L2_EVENT_*`), if any.
    pub fn dequeue_event(&self) -> io::Result<Option<u32>> {
        let mut e = v4l2_event::default();
        match self.ioctl(VIDIOC_DQEVENT, &mut e) {
            Ok(()) => Ok(Some(e.type_)),
            Err(err) if matches!(err.raw_os_error(), Some(libc::ENOENT) | Some(libc::EAGAIN)) => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn raw_fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

/// Exact fraction for common rates (29.97 = 30000/1001), else 1/fps in microseconds.
pub fn fps_to_fract(fps: f64) -> (u32, u32) {
    for n in [24.0, 30.0, 60.0, 120.0] {
        if (fps - n * 1000.0 / 1001.0).abs() < 0.005 {
            return (1001, (n * 1000.0) as u32);
        }
    }
    if (fps - fps.round()).abs() < 1e-6 { (1, fps.round() as u32) } else { (1_000_000, (fps * 1_000_000.0).round() as u32) }
}

struct Mapped {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the mapping is plain memory owned by the stream; access is synchronized by the
// V4L2 queue protocol (we only read buffers we dequeued).
unsafe impl Send for Mapped {}

impl Drop for Mapped {
    fn drop(&mut self) {
        // SAFETY: ptr/len came from a successful mmap.
        unsafe { libc::munmap(self.ptr as *mut libc::c_void, self.len) };
    }
}

/// A dequeued buffer.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub index: u32,
    pub bytesused: u32,
    pub sequence: u32,
    pub flags: u32,
    /// Capture time in CLOCK_MONOTONIC ns when the driver provides a monotonic timestamp.
    pub monotonic_ns: Option<u64>,
}

impl Frame {
    pub fn is_error(&self) -> bool {
        self.flags & V4L2_BUF_FLAG_ERROR != 0
    }
}

/// What `poll` reported.
#[derive(Clone, Copy, Debug, Default)]
pub struct Ready {
    pub frame: bool,
    pub event: bool,
    pub error: bool,
}

/// An mmap capture stream (started on creation, stopped and unmapped on drop).
pub struct Stream<'d> {
    dev: &'d Device,
    bufs: Vec<Mapped>,
}

impl<'d> Stream<'d> {
    pub fn start(dev: &'d Device, count: u32) -> io::Result<Stream<'d>> {
        let mut rb = v4l2_requestbuffers { count, type_: V4L2_BUF_TYPE_VIDEO_CAPTURE, memory: V4L2_MEMORY_MMAP, ..Default::default() };
        dev.ioctl(VIDIOC_REQBUFS, &mut rb)?;
        if rb.count == 0 {
            return Err(io::Error::other("driver granted no buffers"));
        }
        let mut s = Stream { dev, bufs: Vec::with_capacity(rb.count as usize) };
        for index in 0..rb.count {
            let mut b = v4l2_buffer { index, type_: V4L2_BUF_TYPE_VIDEO_CAPTURE, memory: V4L2_MEMORY_MMAP, ..Default::default() };
            dev.ioctl(VIDIOC_QUERYBUF, &mut b)?;
            // SAFETY: `offset` is the active member for MMAP buffers.
            let offset = unsafe { b.m.offset };
            // SAFETY: mapping a driver buffer with the offset/length it reported.
            let ptr = unsafe {
                libc::mmap(std::ptr::null_mut(), b.length as usize, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, dev.raw_fd(), offset as libc::off_t)
            };
            if ptr == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            s.bufs.push(Mapped { ptr: ptr as *mut u8, len: b.length as usize });
        }
        for index in 0..rb.count {
            s.requeue(index)?;
        }
        let mut ty: libc::c_int = V4L2_BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
        dev.ioctl(VIDIOC_STREAMON, &mut ty)?;
        Ok(s)
    }

    pub fn buffer_count(&self) -> usize {
        self.bufs.len()
    }

    /// Wait up to `timeout_ms` for a frame, an event, or an error.
    pub fn wait(&self, timeout_ms: i32) -> io::Result<Ready> {
        let mut p = libc::pollfd { fd: self.dev.raw_fd(), events: libc::POLLIN | libc::POLLPRI, revents: 0 };
        loop {
            // SAFETY: one valid pollfd.
            let r = unsafe { libc::poll(&mut p, 1, timeout_ms) };
            if r < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            return Ok(Ready {
                frame: p.revents & libc::POLLIN != 0,
                event: p.revents & libc::POLLPRI != 0,
                error: p.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0,
            });
        }
    }

    /// Dequeue a filled buffer (None if none is ready on a non-blocking fd).
    pub fn dequeue(&mut self) -> io::Result<Option<Frame>> {
        let mut b = v4l2_buffer { type_: V4L2_BUF_TYPE_VIDEO_CAPTURE, memory: V4L2_MEMORY_MMAP, ..Default::default() };
        match self.dev.ioctl(VIDIOC_DQBUF, &mut b) {
            Ok(()) => {}
            Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => return Ok(None),
            Err(e) => return Err(e),
        }
        let monotonic = b.flags & V4L2_BUF_FLAG_TIMESTAMP_MASK == V4L2_BUF_FLAG_TIMESTAMP_MONOTONIC;
        let ts = b.timestamp.tv_sec as u64 * 1_000_000_000 + b.timestamp.tv_usec as u64 * 1000;
        Ok(Some(Frame {
            index: b.index,
            bytesused: b.bytesused.min(self.bufs.get(b.index as usize).map(|m| m.len as u32).unwrap_or(0)),
            sequence: b.sequence,
            flags: b.flags,
            monotonic_ns: (monotonic && ts > 0).then_some(ts),
        }))
    }

    /// Bytes of a dequeued buffer (valid until it is requeued).
    pub fn data(&self, f: &Frame) -> &[u8] {
        let m = &self.bufs[f.index as usize];
        // SAFETY: the buffer is dequeued (owned by us) and `bytesused <= len`.
        unsafe { std::slice::from_raw_parts(m.ptr, f.bytesused as usize) }
    }

    pub fn requeue(&mut self, index: u32) -> io::Result<()> {
        let mut b = v4l2_buffer { index, type_: V4L2_BUF_TYPE_VIDEO_CAPTURE, memory: V4L2_MEMORY_MMAP, ..Default::default() };
        self.dev.ioctl(VIDIOC_QBUF, &mut b)
    }

    pub fn device(&self) -> &Device {
        self.dev
    }
}

impl Drop for Stream<'_> {
    fn drop(&mut self) {
        let mut ty: libc::c_int = V4L2_BUF_TYPE_VIDEO_CAPTURE as libc::c_int;
        let _ = self.dev.ioctl(VIDIOC_STREAMOFF, &mut ty);
        self.bufs.clear();
        let mut rb = v4l2_requestbuffers { count: 0, type_: V4L2_BUF_TYPE_VIDEO_CAPTURE, memory: V4L2_MEMORY_MMAP, ..Default::default() };
        let _ = self.dev.ioctl(VIDIOC_REQBUFS, &mut rb);
    }
}

/// Readable description of `V4L2_IN_ST_*` flags.
pub fn input_status_str(st: u32) -> &'static str {
    if st & V4L2_IN_ST_NO_POWER != 0 {
        "no power"
    } else if st & V4L2_IN_ST_NO_SIGNAL != 0 {
        "no signal"
    } else if st & (V4L2_IN_ST_NO_SYNC | V4L2_IN_ST_NO_H_LOCK | V4L2_IN_ST_NO_V_LOCK) != 0 {
        "no sync"
    } else {
        "ok"
    }
}

/// True when the flags say there is no usable picture on the input.
pub fn input_has_no_signal(st: u32) -> bool {
    st & (V4L2_IN_ST_NO_POWER | V4L2_IN_ST_NO_SIGNAL | V4L2_IN_ST_NO_SYNC | V4L2_IN_ST_NO_H_LOCK | V4L2_IN_ST_NO_V_LOCK) != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_codes_match_the_kernel_abi() {
        // Values from the x86_64 kernel headers (as printed by `strace`).
        assert_eq!(VIDIOC_QUERYCAP, 0x8068_5600);
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_QUERYBUF, 0xc058_5609);
        assert_eq!(VIDIOC_QBUF, 0xc058_560f);
        assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_G_PARM, 0xc0cc_5615);
        assert_eq!(VIDIOC_ENUMINPUT, 0xc050_561a);
        assert_eq!(VIDIOC_G_EXT_CTRLS, 0xc020_5647);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
        assert_eq!(VIDIOC_ENUM_FRAMEINTERVALS, 0xc034_564b);
        assert_eq!(VIDIOC_DQEVENT, 0x8088_5659);
        assert_eq!(VIDIOC_SUBSCRIBE_EVENT, 0x4020_565a);
        assert_eq!(VIDIOC_QUERY_DV_TIMINGS, 0x8084_5663);
        assert_eq!(VIDIOC_QUERY_EXT_CTRL, 0xc0e8_5667);
    }

    #[test]
    fn control_names_follow_v4l2_ctl() {
        assert_eq!(control_var_name("White Balance, Automatic"), "white_balance_automatic");
        assert_eq!(control_var_name("Exposure Time, Absolute"), "exposure_time_absolute");
        assert_eq!(control_var_name("Auto Exposure"), "auto_exposure");
        assert_eq!(control_var_name("  Power Line Frequency "), "power_line_frequency");
        assert_eq!(control_var_name("50 Hz"), "50_hz");
        assert_eq!(control_var_name("Focus, Automatic Continuous"), "focus_automatic_continuous");
    }

    #[test]
    fn fourcc_round_trip() {
        assert_eq!(PIX_YUYV, 0x5659_5559);
        assert_eq!(fourcc_str(PIX_MJPEG), "MJPG");
        assert_eq!(format_name(PIX_MJPEG), "mjpeg");
        assert_eq!(parse_format_name("YUYV"), Some(PIX_YUYV));
        assert_eq!(parse_format_name("mjpg"), Some(PIX_MJPEG));
        assert_eq!(parse_format_name("h264"), None);
    }

    #[test]
    fn frame_rate_fractions() {
        assert_eq!(fps_to_fract(60.0), (1, 60));
        assert_eq!(fps_to_fract(29.97), (1001, 30000));
        assert_eq!(fps_to_fract(59.94), (1001, 60000));
        assert!((Fract { num: 1001, den: 30000 }.fps() - 29.97).abs() < 0.001);
    }

    #[test]
    fn colorimetry_defaults() {
        let hws = PixFormat { fourcc: PIX_YUYV, colorspace: V4L2_COLORSPACE_REC709, quantization: V4L2_QUANTIZATION_FULL_RANGE, ..Default::default() };
        assert_eq!((hws.matrix(), hws.range()), ("bt709", "full"));
        let uvc = PixFormat { fourcc: PIX_YUYV, colorspace: V4L2_COLORSPACE_SRGB, ..Default::default() };
        assert_eq!((uvc.matrix(), uvc.range()), ("bt601", "limited"));
        let mj = PixFormat { fourcc: PIX_MJPEG, colorspace: V4L2_COLORSPACE_SRGB, ..Default::default() };
        assert_eq!(mj.range(), "full");
    }
}
