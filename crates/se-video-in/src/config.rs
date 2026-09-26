//! `sources/<name>.toml`: camera and media-file source definitions.
//!
//! ```toml
//! # camera
//! label  = "Kit (HDMI 1)"
//! device = "pci-0000:05:00.0-video-index0"   # stable identity (glob ok), /dev/v4l/by-*/… or /dev/videoN
//! format = "yuyv"                            # yuyv | mjpeg
//! size   = [1920, 1080]
//! fps    = 60
//! [controls]                                 # initial camera controls (v4l2-ctl names)
//! brightness = 128
//! [color]                                    # GPU color correction (applied by the renderer)
//! contrast = 1.05
//! lut = "assets/luts/kit.cube"
//!
//! # media file
//! file    = "assets/brb.mp4"
//! loop    = true
//! rate    = 1.0
//! hwaccel = "auto"                           # auto | cuda | none
//! ```

use se_devices::v4l2;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq)]
pub struct Color {
    pub brightness: f64,
    pub contrast: f64,
    pub saturation: f64,
    pub gamma: f64,
    pub temperature: f64,
    pub tint: f64,
    /// Project-relative `.cube` path ("" = none).
    pub lut: String,
    pub lut_amount: f64,
}

impl Default for Color {
    fn default() -> Self {
        Color { brightness: 0.0, contrast: 1.0, saturation: 1.0, gamma: 1.0, temperature: 0.0, tint: 0.0, lut: String::new(), lut_amount: 1.0 }
    }
}

/// (field, default, range, description) for the declared `source.<n>.color.*` addresses.
pub const COLOR_FIELDS: [(&str, f64, [f64; 2], &str); 6] = [
    ("brightness", 0.0, [-1.0, 1.0], "added to RGB"),
    ("contrast", 1.0, [0.0, 3.0], "around mid grey"),
    ("saturation", 1.0, [0.0, 3.0], "0 = monochrome"),
    ("gamma", 1.0, [0.2, 5.0], "power applied to RGB"),
    ("temperature", 0.0, [-1.0, 1.0], "white balance: negative = cooler, positive = warmer"),
    ("tint", 0.0, [-1.0, 1.0], "negative = green, positive = magenta"),
];

impl Color {
    pub fn get(&self, field: &str) -> f64 {
        match field {
            "brightness" => self.brightness,
            "contrast" => self.contrast,
            "saturation" => self.saturation,
            "gamma" => self.gamma,
            "temperature" => self.temperature,
            "tint" => self.tint,
            _ => 0.0,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct SignalCfg {
    /// No frame for this long → no signal.
    pub timeout_ms: u64,
    /// Brightest sampled luma below this (0–1) → black picture.
    pub black_level: f32,
    /// A bad picture must persist this long before `signal` turns false.
    pub hold_ms: u64,
}

impl Default for SignalCfg {
    fn default() -> Self {
        SignalCfg { timeout_ms: 500, black_level: 0.12, hold_ms: 700 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct CameraDef {
    pub device: String,
    pub fourcc: u32,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    pub buffers: u32,
    /// Initial control values by control name (validated against the device when opened).
    pub controls: BTreeMap<String, toml::Value>,
    pub signal: SignalCfg,
}

impl CameraDef {
    /// Settings that require reopening the device when they change.
    pub fn same_stream(&self, o: &CameraDef) -> bool {
        (&self.device, self.fourcc, self.width, self.height, self.buffers) == (&o.device, o.fourcc, o.width, o.height, o.buffers)
            && (self.fps - o.fps).abs() < 1e-6
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HwAccel {
    Auto,
    Cuda,
    None,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FileDef {
    /// Absolute path.
    pub path: PathBuf,
    /// As written in the file (project-relative).
    pub rel: String,
    pub looping: bool,
    pub rate: f64,
    pub hwaccel: HwAccel,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Camera(CameraDef),
    File(FileDef),
}

#[derive(Clone, Debug, PartialEq)]
pub struct SourceDef {
    pub name: String,
    pub label: String,
    pub kind: Kind,
    pub color: Color,
}

impl SourceDef {
    pub fn camera(&self) -> Option<&CameraDef> {
        match &self.kind {
            Kind::Camera(c) => Some(c),
            Kind::File(_) => None,
        }
    }
    pub fn file(&self) -> Option<&FileDef> {
        match &self.kind {
            Kind::File(f) => Some(f),
            Kind::Camera(_) => None,
        }
    }
    pub fn kind_str(&self) -> &'static str {
        match self.kind {
            Kind::Camera(_) => "camera",
            Kind::File(_) => "file",
        }
    }
}

fn num(t: &toml::Table, k: &str) -> Option<f64> {
    match t.get(k)? {
        toml::Value::Integer(i) => Some(*i as f64),
        toml::Value::Float(f) => Some(*f),
        _ => None,
    }
}

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

const CAMERA_KEYS: &[&str] = &["label", "device", "format", "size", "fps", "buffers", "controls", "color", "lut", "lut_amount", "signal", "fx"];
const FILE_KEYS: &[&str] = &["label", "file", "loop", "rate", "hwaccel", "color", "lut", "lut_amount", "fx"];

/// Parse one source file. `root` resolves media paths.
pub fn parse(name: &str, t: &toml::Table, root: &Path) -> Result<SourceDef, String> {
    if !valid_name(name) {
        return Err(format!("source name `{name}` must be [A-Za-z0-9_-]"));
    }
    let s = |k: &str| t.get(k).and_then(toml::Value::as_str).map(str::to_string);
    let label = s("label").unwrap_or_else(|| name.to_string());
    let mut color = Color::default();
    if let Some(c) = t.get("color") {
        let c = c.as_table().ok_or("`color` must be a table")?;
        for (k, v) in c {
            let f = match v {
                toml::Value::Integer(i) => *i as f64,
                toml::Value::Float(f) => *f,
                toml::Value::String(sv) if k == "lut" => {
                    color.lut = sv.clone();
                    continue;
                }
                _ => return Err(format!("color.{k} must be a number")),
            };
            match k.as_str() {
                "brightness" => color.brightness = f,
                "contrast" => color.contrast = f,
                "saturation" => color.saturation = f,
                "gamma" => color.gamma = f,
                "temperature" => color.temperature = f,
                "tint" => color.tint = f,
                "lut_amount" => color.lut_amount = f,
                other => return Err(format!("unknown color setting `{other}`")),
            }
        }
    }
    if let Some(l) = s("lut") {
        color.lut = l;
    }
    if let Some(a) = num(t, "lut_amount") {
        color.lut_amount = a;
    }
    for (k, _, range, _) in COLOR_FIELDS {
        let v = color.get(k);
        if v < range[0] || v > range[1] {
            return Err(format!("color.{k} = {v} is outside {range:?}"));
        }
    }
    if !(0.0..=1.0).contains(&color.lut_amount) {
        return Err("lut_amount must be within 0..1".into());
    }
    if !color.lut.is_empty() && !color.lut.to_ascii_lowercase().ends_with(".cube") {
        return Err(format!("lut `{}` must be a .cube file", color.lut));
    }

    let kind = match (t.get("device"), t.get("file")) {
        (Some(_), Some(_)) => return Err("a source has either `device` (camera) or `file` (media), not both".into()),
        (None, None) => return Err("missing `device` (camera) or `file` (media file)".into()),
        (Some(d), None) => {
            if let Some(k) = t.keys().find(|k| !CAMERA_KEYS.contains(&k.as_str())) {
                return Err(format!("unknown camera setting `{k}`"));
            }
            let device = d.as_str().filter(|s| !s.is_empty()).ok_or("`device` must be a non-empty string")?.to_string();
            let fmt = s("format").unwrap_or_else(|| "yuyv".into());
            let fourcc = match v4l2::parse_format_name(&fmt) {
                Some(f) if f == v4l2::PIX_YUYV || f == v4l2::PIX_MJPEG => f,
                _ => return Err(format!("format `{fmt}` is not supported (yuyv | mjpeg)")),
            };
            let (width, height) = match t.get("size") {
                Some(toml::Value::Array(a)) if a.len() == 2 => {
                    let w = a[0].as_integer().ok_or("size must be [width, height]")?;
                    let h = a[1].as_integer().ok_or("size must be [width, height]")?;
                    if !(16..=8192).contains(&w) || !(16..=8192).contains(&h) {
                        return Err(format!("size {w}x{h} is out of range"));
                    }
                    (w as u32, h as u32)
                }
                Some(_) => return Err("size must be [width, height]".into()),
                None => (1920, 1080),
            };
            if fourcc == v4l2::PIX_YUYV && width % 2 != 0 {
                return Err("YUYV width must be even".into());
            }
            let fps = num(t, "fps").unwrap_or(30.0);
            if !(1.0..=240.0).contains(&fps) {
                return Err(format!("fps {fps} is out of range"));
            }
            let buffers = t.get("buffers").and_then(toml::Value::as_integer).unwrap_or(4);
            if !(2..=16).contains(&buffers) {
                return Err("buffers must be 2..16".into());
            }
            let controls = match t.get("controls") {
                Some(toml::Value::Table(c)) => c.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
                Some(_) => return Err("`controls` must be a table".into()),
                None => BTreeMap::new(),
            };
            let mut signal = SignalCfg::default();
            if let Some(sg) = t.get("signal") {
                let sg = sg.as_table().ok_or("`signal` must be a table")?;
                if let Some(v) = sg.get("timeout") {
                    signal.timeout_ms = dur_ms(v).ok_or("signal.timeout must be a duration like \"500ms\"")?;
                }
                if let Some(v) = sg.get("hold") {
                    signal.hold_ms = dur_ms(v).ok_or("signal.hold must be a duration like \"700ms\"")?;
                }
                if let Some(v) = num(sg, "black_level") {
                    if !(0.0..=1.0).contains(&v) {
                        return Err("signal.black_level must be within 0..1".into());
                    }
                    signal.black_level = v as f32;
                }
            }
            Kind::Camera(CameraDef { device, fourcc, width, height, fps, buffers: buffers as u32, controls, signal })
        }
        (None, Some(f)) => {
            if let Some(k) = t.keys().find(|k| !FILE_KEYS.contains(&k.as_str())) {
                return Err(format!("unknown media setting `{k}`"));
            }
            let rel = f.as_str().filter(|s| !s.is_empty()).ok_or("`file` must be a non-empty string")?.to_string();
            let path = if Path::new(&rel).is_absolute() { PathBuf::from(&rel) } else { root.join(&rel) };
            let rate = num(t, "rate").unwrap_or(1.0);
            if !(0.05..=8.0).contains(&rate) {
                return Err(format!("rate {rate} is out of range (0.05..8)"));
            }
            let hwaccel = match s("hwaccel").as_deref().unwrap_or("auto") {
                "auto" => HwAccel::Auto,
                "cuda" | "nvdec" => HwAccel::Cuda,
                "none" | "cpu" => HwAccel::None,
                other => return Err(format!("hwaccel `{other}` (auto | cuda | none)")),
            };
            Kind::File(FileDef { path, rel, looping: t.get("loop").and_then(toml::Value::as_bool).unwrap_or(true), rate, hwaccel })
        }
    };
    Ok(SourceDef { name: name.to_string(), label, kind, color })
}

fn dur_ms(v: &toml::Value) -> Option<u64> {
    match v {
        toml::Value::Integer(i) if *i >= 0 => Some(*i as u64),
        toml::Value::String(s) => se_proto::parse_duration_ms(s),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(src: &str) -> toml::Table {
        toml::from_str(src).unwrap()
    }

    #[test]
    fn camera_defaults_and_fields() {
        let d = parse(
            "cam_kit",
            &t(r#"
                label = "Kit"
                device = "pci-0000:05:00.0-video-index0"
                size = [1920, 1080]
                fps = 60
                fx = [{ name = "grade" }]
                [controls]
                brightness = 128
                [color]
                contrast = 1.1
                lut = "assets/luts/kit.cube"
                [signal]
                timeout = "250ms"
                black_level = 0.2
            "#),
            Path::new("/p"),
        )
        .unwrap();
        let c = d.camera().unwrap();
        assert_eq!(c.fourcc, v4l2::PIX_YUYV);
        assert_eq!((c.width, c.height, c.fps, c.buffers), (1920, 1080, 60.0, 4));
        assert_eq!(c.controls["brightness"].as_integer(), Some(128));
        assert_eq!(c.signal, SignalCfg { timeout_ms: 250, black_level: 0.2, hold_ms: 700 });
        assert_eq!(d.color.contrast, 1.1);
        assert_eq!(d.color.lut, "assets/luts/kit.cube");
        assert_eq!(d.kind_str(), "camera");
    }

    #[test]
    fn mjpeg_camera() {
        let d = parse("cam_room", &t("device = \"usb-Sonix*\"\nformat = \"MJPEG\"\nfps = 30"), Path::new("/p")).unwrap();
        assert_eq!(d.camera().unwrap().fourcc, v4l2::PIX_MJPEG);
    }

    #[test]
    fn media_file() {
        let d = parse("brb_loop", &t("file = \"assets/brb.mp4\"\nrate = 0.5\nhwaccel = \"none\""), Path::new("/p")).unwrap();
        let f = d.file().unwrap();
        assert_eq!(f.path, PathBuf::from("/p/assets/brb.mp4"));
        assert!(f.looping);
        assert_eq!((f.rate, f.hwaccel), (0.5, HwAccel::None));
    }

    #[test]
    fn rejects_bad_files() {
        let root = Path::new("/p");
        for (src, needle) in [
            ("", "missing"),
            ("device = \"x\"\nfile = \"y\"", "not both"),
            ("device = \"x\"\nformat = \"h264\"", "not supported"),
            ("device = \"x\"\nsize = [1921, 1080]", "even"),
            ("device = \"x\"\nsize = [1920]", "size"),
            ("device = \"x\"\nfps = 0", "fps"),
            ("device = \"x\"\nwat = 1", "unknown camera setting"),
            ("device = \"x\"\n[color]\ncontrast = 9.0", "outside"),
            ("device = \"x\"\nlut = \"a.png\"", ".cube"),
            ("file = \"a.mp4\"\nrate = 0", "rate"),
            ("file = \"a.mp4\"\nhwaccel = \"vaapi\"", "hwaccel"),
            ("file = \"a.mp4\"\nsize = [1, 2]", "unknown media setting"),
        ] {
            let e = parse("s", &t(src), root).unwrap_err();
            assert!(e.contains(needle), "{src:?}: {e}");
        }
        assert!(parse("bad name", &t("device = \"x\""), root).is_err());
    }

    #[test]
    fn stream_settings_comparison() {
        let a = parse("a", &t("device = \"x\"\nfps = 60"), Path::new("/")).unwrap();
        let mut b = a.clone();
        if let Kind::Camera(c) = &mut b.kind {
            c.controls.insert("gain".into(), toml::Value::Integer(3));
        }
        assert!(a.camera().unwrap().same_stream(b.camera().unwrap()));
        if let Kind::Camera(c) = &mut b.kind {
            c.fps = 30.0;
        }
        assert!(!a.camera().unwrap().same_stream(b.camera().unwrap()));
    }
}
