//! Key image rendering: Omarchy theme colors + the Omarchy (Nerd) font, drawn in software
//! (anti-aliased signed-distance shapes + `ab_glyph` text), rotated and JPEG-encoded for
//! the device. Visuals are value types so unchanged keys are never re-sent.

use ab_glyph::{Font, FontVec, PxScale, ScaleFont, point};
use std::hash::{Hash, Hasher};
use std::path::PathBuf;
use std::sync::Arc;

/// Decoded once per configuration load; both supported device sizes are cached.
#[derive(Clone, Debug)]
pub struct Artwork {
    pub path: String,
    content_id: [u8; 20],
    rgb: Arc<[u8]>,
    xl_rgb: Arc<[u8]>,
}

impl Artwork {
    pub fn load(root: &std::path::Path, path: &str) -> Result<Self, String> {
        let rel = std::path::Path::new(path);
        if path.is_empty() || rel.components().any(|c| !matches!(c, std::path::Component::Normal(_))) {
            return Err("`image` must be a nonempty path relative to the project root (no `..`)".into());
        }
        let file = root.join(rel);
        let bytes = std::fs::read(&file).map_err(|e| format!("image `{}`: {e}", file.display()))?;
        let decoded = image::load_from_memory_with_format(&bytes, image::ImageFormat::Png)
            .map_err(|e| format!("image `{}`: cannot decode PNG: {e}", file.display()))?;
        if decoded.width() != 72 || decoded.height() != 72 || decoded.color() != image::ColorType::Rgb8 {
            return Err(format!("image `{}` must be a 72×72, 8-bit RGB PNG (got {}×{} {:?})", file.display(), decoded.width(), decoded.height(), decoded.color()));
        }
        let rgb = decoded.into_rgb8();
        let content_id = sha1_smol::Sha1::from(rgb.as_raw().as_slice()).digest().bytes();
        let xl_rgb = image::imageops::resize(&rgb, 96, 96, image::imageops::FilterType::Lanczos3).into_raw().into();
        Ok(Self { path: path.into(), content_id, rgb: rgb.into_raw().into(), xl_rgb })
    }

    fn pixels(&self, size: u32) -> &[u8] {
        match size {
            72 => &self.rgb,
            96 => &self.xl_rgb,
            _ => unreachable!("unsupported deck key size"),
        }
    }
}

impl PartialEq for Artwork {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path && self.content_id == other.content_id
    }
}

impl Eq for Artwork {}

impl Hash for Artwork {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.content_id.hash(state);
    }
}

pub type Rgb = [u8; 3];

/// Everything that determines a key image.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Default)]
pub struct KeyVisual {
    /// Unassigned key: plain black.
    pub blank: bool,
    pub bg: Rgb,
    pub fg: Rgb,
    /// State edge color and width in px.
    pub edge: Option<(Rgb, u8)>,
    pub icon: String,
    pub label: String,
    /// Full-key artwork already includes its label and icon.
    pub artwork: Option<Arc<Artwork>>,
    /// Live local HH:MM:SS, separate from the static label.
    pub clock: Option<Arc<str>>,
    pub disabled: bool,
    /// Bottom progress bar: filled fraction in 1/64 steps and its color.
    pub progress: Option<(u8, Rgb)>,
    /// Small LED dot in the top-right corner.
    pub badge: Option<Rgb>,
    pub pressed: bool,
    pub dim: bool,
}

impl KeyVisual {
    pub fn id(&self) -> u64 {
        let mut h = std::collections::hash_map::DefaultHasher::new();
        self.hash(&mut h);
        h.finish()
    }
}

/// Theme colors used on keys (subset of `se_ui_kit::Theme`, as RGB).
#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub bg: Rgb,
    pub bg_dark: Rgb,
    pub bg_light: Rgb,
    pub fg: Rgb,
    pub fg_dim: Rgb,
    pub accent: Rgb,
    pub muted: Rgb,
    pub red: Rgb,
    pub bright_red: Rgb,
    pub yellow: Rgb,
    pub green: Rgb,
    pub cyan: Rgb,
    pub blue: Rgb,
    pub magenta: Rgb,
    pub orange: Rgb,
}

impl Palette {
    /// From the active Omarchy theme (`~/.local/state/omarchy/current/theme/colors.toml`).
    pub fn load() -> Palette {
        Palette::from_theme(&se_ui_kit::Theme::load())
    }

    pub fn from_theme(t: &se_ui_kit::Theme) -> Palette {
        let c = |a: [u8; 4]| [a[0], a[1], a[2]];
        Palette {
            bg: c(t.bg.to_array()),
            bg_dark: c(t.bg_dark.to_array()),
            bg_light: c(t.bg_light.to_array()),
            fg: c(t.fg.to_array()),
            fg_dim: c(t.fg_dim.to_array()),
            accent: c(t.accent.to_array()),
            muted: c(t.muted.to_array()),
            red: c(t.red.to_array()),
            bright_red: c(t.bright_red.to_array()),
            yellow: c(t.yellow.to_array()),
            green: c(t.green.to_array()),
            cyan: c(t.cyan.to_array()),
            blue: c(t.blue.to_array()),
            magenta: c(t.magenta.to_array()),
            orange: c(t.orange.to_array()),
        }
    }

    /// `#rrggbb` or a theme token (`red`, `accent`, …).
    pub fn color(&self, s: &str) -> Option<Rgb> {
        if let Some(h) = s.strip_prefix('#') {
            let b = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
            return match h.len() {
                6 | 8 => Some([b(0)?, b(2)?, b(4)?]),
                _ => None,
            };
        }
        Some(match s {
            "bg" | "background" => self.bg,
            "bg_dark" => self.bg_dark,
            "bg_light" => self.bg_light,
            "fg" | "foreground" => self.fg,
            "accent" => self.accent,
            "muted" => self.muted,
            "red" => self.red,
            "bright_red" | "live" | "program" => self.bright_red,
            "yellow" | "preview" | "armed" => self.yellow,
            "green" | "healthy" => self.green,
            "cyan" => self.cyan,
            "blue" => self.blue,
            "magenta" | "modulation" => self.magenta,
            "orange" => self.orange,
            _ => return None,
        })
    }
}

/// Black or near-white text, whichever reads better (same rule as `se_ui_kit::widgets::contrast`).
pub fn contrast(bg: Rgb) -> Rgb {
    let l = 0.2126 * bg[0] as f32 + 0.7152 * bg[1] as f32 + 0.0722 * bg[2] as f32;
    if l > 140.0 { [20, 20, 24] } else { [240, 240, 235] }
}

pub fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    [m(a[0], b[0]), m(a[1], b[1]), m(a[2], b[2])]
}

/// Nerd Font glyphs for `icon = "<name>"` (a literal glyph also works).
pub fn icon_glyph(name: &str) -> Option<&'static str> {
    use se_ui_kit::icon as i;
    Some(match name {
        "scene" | "video" | "camera" => i::SCENE,
        "preset" | "bolt" => i::PRESET,
        "patch" => i::PATCH,
        "effect" | "fx" | "magic" => i::EFFECT,
        "rule" => i::RULE,
        "binding" | "link" => i::BINDING,
        "timeline" | "clock" => i::TIMELINE,
        "light" | "lights" | "bulb" => i::LIGHT,
        "mix" | "mixer" | "sliders" => i::MIX,
        "chat" => i::CHAT,
        "queue" | "music" | "song" => i::QUEUE,
        "mod" | "shield" => i::MOD,
        "warn" | "panic" | "warning" => i::WARN,
        "live" | "dot" => i::LIVE,
        "rec" | "record" => i::REC,
        "device" | "usb" => i::DEVICE,
        "controller" | "gamepad" => i::CONTROLLER,
        "bot" => i::BOT,
        "alert" | "bell" => i::ALERT,
        "session" | "book" => i::SESSION,
        "perf" | "gauge" => i::PERF,
        "settings" | "gear" => i::SETTINGS,
        "check" | "ok" => i::CHECK,
        "cross" | "close" => i::CROSS,
        "play" | "take" => i::PLAY,
        "stop" => i::STOP,
        "search" => i::SEARCH,
        "console" | "terminal" => i::CONSOLE,
        "trace" => i::TRACE,
        "sim" => i::SIM,
        "scope" | "chart" => i::SCOPE,
        "mic" | "voice" => "\u{f130}",
        "page" | "menu" => "\u{f0c9}",
        "next" | "forward" => "\u{f051}",
        "prev" | "back" => "\u{f048}",
        "fire" | "hype" => "\u{f06d}",
        "marker" | "bookmark" => "\u{f02e}",
        "clean" | "eraser" => "\u{f12d}",
        "toggle_on" => "\u{f205}",
        "toggle_off" | "toggle" => "\u{f204}",
        "hand" | "hold" => "\u{f256}",
        "star" => "\u{f005}",
        "heart" => "\u{f004}",
        "coffee" | "brb" => "\u{f0f4}",
        "sun" | "brightness" => "\u{f185}",
        "moon" => "\u{f186}",
        "drum" | "drums" => "\u{f569}",
        _ => return None,
    })
}

/// Resolve a font family to a file with fontconfig.
fn font_file(family: &str, style: &str) -> Option<PathBuf> {
    let pat = if style.is_empty() { family.to_string() } else { format!("{family}:{style}") };
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", &pat]).output().ok()?;
    let p = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    p.is_file().then_some(p)
}

/// The Omarchy font family (`omarchy font current`).
pub fn omarchy_font() -> Option<String> {
    let out = std::process::Command::new("omarchy").args(["font", "current"]).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

pub struct Fonts {
    pub family: String,
    regular: FontVec,
    bold: Option<FontVec>,
    /// Glyph fallback (Nerd Font symbols) when the main font lacks an icon.
    symbols: Option<FontVec>,
}

impl Fonts {
    /// Load the Omarchy font (fallback: fontconfig `monospace`).
    pub fn load(family: Option<&str>) -> Result<Fonts, String> {
        let fam = family.map(String::from).or_else(omarchy_font).unwrap_or_else(|| "monospace".into());
        let read = |p: PathBuf| std::fs::read(&p).ok().and_then(|b| FontVec::try_from_vec(b).ok());
        let regular = font_file(&fam, "").and_then(read).ok_or_else(|| format!("font `{fam}` not found (fc-match)"))?;
        let bold = font_file(&fam, "bold").and_then(read);
        let symbols = font_file("Symbols Nerd Font", "").and_then(read);
        Ok(Fonts { family: fam, regular, bold, symbols })
    }

    pub fn from_bytes(family: &str, regular: Vec<u8>) -> Result<Fonts, String> {
        Ok(Fonts { family: family.into(), regular: FontVec::try_from_vec(regular).map_err(|e| e.to_string())?, bold: None, symbols: None })
    }

    fn pick(&self, c: char, bold: bool) -> &FontVec {
        let main = if bold { self.bold.as_ref().unwrap_or(&self.regular) } else { &self.regular };
        if main.glyph_id(c).0 != 0 {
            return main;
        }
        if let Some(s) = &self.symbols
            && s.glyph_id(c).0 != 0
        {
            return s;
        }
        main
    }

    fn width(&self, text: &str, px: f32, bold: bool) -> f32 {
        text.chars().map(|c| self.pick(c, bold).as_scaled(PxScale::from(px)).h_advance(self.pick(c, bold).glyph_id(c))).sum()
    }
}

/// Software canvas (linear RGB floats) with anti-aliased primitives.
struct Canvas {
    s: usize,
    px: Vec<[f32; 3]>,
}

fn sd_round_rect(x: f32, y: f32, cx: f32, cy: f32, hw: f32, hh: f32, r: f32) -> f32 {
    let qx = (x - cx).abs() - (hw - r);
    let qy = (y - cy).abs() - (hh - r);
    let ox = qx.max(0.0);
    let oy = qy.max(0.0);
    (ox * ox + oy * oy).sqrt() + qx.max(qy).min(0.0) - r
}

impl Canvas {
    fn new(s: usize) -> Canvas {
        Canvas { s, px: vec![[0.0; 3]; s * s] }
    }

    fn blend(&mut self, x: i32, y: i32, c: Rgb, a: f32) {
        if x < 0 || y < 0 || x >= self.s as i32 || y >= self.s as i32 || a <= 0.0 {
            return;
        }
        let a = a.min(1.0);
        let p = &mut self.px[y as usize * self.s + x as usize];
        for i in 0..3 {
            p[i] = p[i] * (1.0 - a) + c[i] as f32 * a;
        }
    }

    /// Fill (width = None) or stroke inside (Some(w)) a rounded rectangle.
    fn round_rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, r: f32, c: Rgb, stroke: Option<f32>) {
        let (cx, cy, hw, hh) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0, (x1 - x0) / 2.0, (y1 - y0) / 2.0);
        for y in y0.floor() as i32..y1.ceil() as i32 {
            for x in x0.floor() as i32..x1.ceil() as i32 {
                let d = sd_round_rect(x as f32 + 0.5, y as f32 + 0.5, cx, cy, hw, hh, r);
                let cov = match stroke {
                    None => (0.5 - d).clamp(0.0, 1.0),
                    Some(w) => (0.5 - d).clamp(0.0, 1.0) * (d + w + 0.5).clamp(0.0, 1.0),
                };
                self.blend(x, y, c, cov);
            }
        }
    }

    fn circle(&mut self, cx: f32, cy: f32, r: f32, c: Rgb) {
        for y in (cy - r - 1.0).floor() as i32..(cy + r + 1.0).ceil() as i32 {
            for x in (cx - r - 1.0).floor() as i32..(cx + r + 1.0).ceil() as i32 {
                let d = ((x as f32 + 0.5 - cx).powi(2) + (y as f32 + 0.5 - cy).powi(2)).sqrt() - r;
                self.blend(x, y, c, (0.5 - d).clamp(0.0, 1.0));
            }
        }
    }

    /// Eight-character seven-segment clock, legible even before fonts finish loading.
    fn clock(&mut self, text: &str, color: Rgb) {
        let sf = self.s as f32;
        let unit = sf / 72.0;
        let mut x = 4.0 * unit;
        let y = 25.0 * unit;
        let w = 8.0 * unit;
        let h = 20.0 * unit;
        let thick = 1.3 * unit;
        let digit = [
            0b0111111, 0b0000110, 0b1011011, 0b1001111, 0b1100110,
            0b1101101, 0b1111101, 0b0000111, 0b1111111, 0b1101111,
        ];
        for ch in text.chars() {
            if ch == ':' {
                self.circle(x + 2.0 * unit, y + h * 0.32, unit, color);
                self.circle(x + 2.0 * unit, y + h * 0.72, unit, color);
                x += 4.0 * unit;
                continue;
            }
            let Some(n) = ch.to_digit(10) else { continue };
            let segments = [
                (x + thick, y, x + w - thick, y + thick),
                (x + w - thick, y + thick, x + w, y + h / 2.0),
                (x + w - thick, y + h / 2.0, x + w, y + h - thick),
                (x + thick, y + h - thick, x + w - thick, y + h),
                (x, y + h / 2.0, x + thick, y + h - thick),
                (x, y + thick, x + thick, y + h / 2.0),
                (x + thick, y + h / 2.0 - thick / 2.0, x + w - thick, y + h / 2.0 + thick / 2.0),
            ];
            for (bit, (x0, y0, x1, y1)) in segments.into_iter().enumerate() {
                if digit[n as usize] & (1 << bit) != 0 {
                    self.round_rect(x0, y0, x1, y1, thick * 0.35, color, None);
                }
            }
            x += 9.5 * unit;
        }
    }

    fn text(&mut self, fonts: &Fonts, text: &str, px: f32, center_x: f32, baseline: f32, c: Rgb, bold: bool) {
        let w = fonts.width(text, px, bold);
        let mut x = center_x - w / 2.0;
        for ch in text.chars() {
            let f = fonts.pick(ch, bold);
            let id = f.glyph_id(ch);
            let adv = f.as_scaled(PxScale::from(px)).h_advance(id);
            let g = id.with_scale_and_position(PxScale::from(px), point(x, baseline));
            if let Some(og) = f.outline_glyph(g) {
                let b = og.px_bounds();
                let (bx, by) = (b.min.x as i32, b.min.y as i32);
                og.draw(|gx, gy, cov| self.blend(bx + gx as i32, by + gy as i32, c, cov));
            }
            x += adv;
        }
    }

    fn rgb(&self, rotate180: bool, dim: bool) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.s * self.s * 3);
        let k = if dim { 0.45 } else { 1.0 };
        let mut push = |p: &[f32; 3]| {
            for v in p {
                out.push((v * k).round().clamp(0.0, 255.0) as u8);
            }
        };
        if rotate180 {
            self.px.iter().rev().for_each(&mut push);
        } else {
            self.px.iter().for_each(&mut push);
        }
        out
    }
}

/// Fit a label into at most `lines` lines within `max_w`, shrinking the font down to `min_px`.
fn layout_label(fonts: &Fonts, label: &str, start_px: f32, min_px: f32, max_w: f32, lines: usize) -> (f32, Vec<String>) {
    let mut px = start_px;
    while px >= min_px {
        if fonts.width(label, px, true) <= max_w {
            return (px, vec![label.to_string()]);
        }
        if lines > 1 {
            // greedy wrap on spaces
            let mut out: Vec<String> = Vec::new();
            let mut cur = String::new();
            for word in label.split_whitespace() {
                let cand = if cur.is_empty() { word.to_string() } else { format!("{cur} {word}") };
                if fonts.width(&cand, px, true) <= max_w || cur.is_empty() {
                    cur = cand;
                } else {
                    out.push(std::mem::take(&mut cur));
                    cur = word.to_string();
                }
            }
            if !cur.is_empty() {
                out.push(cur);
            }
            if out.len() <= lines && out.iter().all(|l| fonts.width(l, px, true) <= max_w) {
                return (px, out);
            }
        }
        px -= 0.5;
    }
    // truncate at the minimum size
    let mut s = String::new();
    for c in label.chars() {
        if fonts.width(&format!("{s}{c}…"), min_px, true) > max_w {
            s.push('…');
            return (min_px, vec![s]);
        }
        s.push(c);
    }
    (min_px, vec![s])
}

/// Render a key to RGB (`size`×`size`×3). `rotate180` for the device.
pub fn render_rgb(fonts: Option<&Fonts>, v: &KeyVisual, size: u32, rotate180: bool) -> Vec<u8> {
    let s = size as usize;
    let sf = size as f32;
    let mut c = if let Some(artwork) = &v.artwork {
        let scale = if v.pressed { 0.65 } else { 1.0 };
        Canvas { s, px: artwork.pixels(size).chunks_exact(3).map(|p| [p[0] as f32 * scale, p[1] as f32 * scale, p[2] as f32 * scale]).collect() }
    } else {
        Canvas::new(s)
    };
    if v.blank {
        return c.rgb(rotate180, false);
    }
    let bg = if v.pressed { mix(v.bg, [0, 0, 0], 0.35) } else { v.bg };
    let inset = sf * 0.03;
    let radius = sf * 0.14;
    if v.artwork.is_none() {
        c.round_rect(inset, inset, sf - inset, sf - inset, radius, bg, None);
    }
    if let Some((ec, w)) = v.edge {
        c.round_rect(inset, inset, sf - inset, sf - inset, radius, ec, Some(w as f32 * sf / 72.0));
    }
    if let Some(fonts) = fonts.filter(|_| v.artwork.is_none() && v.clock.is_none()) {
        let has_icon = !v.icon.is_empty();
        let max_w = sf - sf * 0.16;
        if has_icon {
            let ipx = if v.label.is_empty() { sf * 0.5 } else { sf * 0.36 };
            let base = if v.label.is_empty() { sf * 0.68 } else { sf * 0.47 };
            c.text(fonts, &v.icon, ipx, sf / 2.0, base, v.fg, false);
        }
        if !v.label.is_empty() {
            let (px, lines) = if has_icon {
                layout_label(fonts, &v.label, sf * 0.2, sf * 0.12, max_w, 2)
            } else {
                layout_label(fonts, &v.label, sf * 0.26, sf * 0.12, max_w, 3)
            };
            let line_h = px * 1.05;
            let block = line_h * lines.len() as f32;
            let top = if has_icon { sf * 0.56 } else { (sf - block) / 2.0 };
            for (i, l) in lines.iter().enumerate() {
                let baseline = top + line_h * (i as f32 + 1.0) - px * 0.22;
                c.text(fonts, l, px, sf / 2.0, baseline, v.fg, true);
            }
        }
    }
    if let Some(clock) = &v.clock {
        c.round_rect(sf * 0.035, sf * 0.30, sf * 0.965, sf * 0.67, sf * 0.04, [0, 0, 0], None);
        c.clock(clock, v.fg);
    }
    if let Some((frac, pc)) = v.progress {
        let w = (sf - sf * 0.22) * (frac.min(64) as f32 / 64.0);
        if w > 0.5 {
            let x0 = sf * 0.11;
            c.round_rect(x0, sf * 0.86, x0 + w, sf * 0.86 + sf * 0.05, sf * 0.02, pc, None);
        }
    }
    if let Some(bc) = v.badge {
        c.circle(sf * 0.84, sf * 0.16, sf * 0.055, bc);
    }
    if v.disabled {
        let x = sf * 0.84;
        let y = sf * 0.16;
        c.circle(x, y, sf * 0.09, [230, 70, 70]);
        c.round_rect(x - sf * 0.055, y - sf * 0.015, x + sf * 0.055, y + sf * 0.015, sf * 0.005, [15, 15, 15], None);
    }
    c.rgb(rotate180, v.dim)
}

/// JPEG for the device (quality 90).
pub fn jpeg(rgb: &[u8], size: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(4096);
    let enc = jpeg_encoder::Encoder::new(&mut out, 90);
    enc.encode(rgb, size as u16, size as u16, jpeg_encoder::ColorType::Rgb).map_err(|e| e.to_string())?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fonts() -> Option<Fonts> {
        Fonts::load(None).ok()
    }

    #[test]
    fn blank_key_is_black_and_rotation_reverses_pixels() {
        let v = KeyVisual { blank: true, ..Default::default() };
        assert!(render_rgb(None, &v, 72, true).iter().all(|b| *b == 0));
        let v = KeyVisual { bg: [200, 10, 10], fg: [255, 255, 255], progress: Some((64, [0, 255, 0])), ..Default::default() };
        let a = render_rgb(None, &v, 72, false);
        let b = render_rgb(None, &v, 72, true);
        // pixel (x=36, y=64) (inside the progress bar) moves to (71-36, 71-64)
        let at = |img: &[u8], x: usize, y: usize| [img[(y * 72 + x) * 3], img[(y * 72 + x) * 3 + 1], img[(y * 72 + x) * 3 + 2]];
        assert_eq!(at(&a, 36, 64), [0, 255, 0]);
        assert_eq!(at(&b, 71 - 36, 71 - 64), [0, 255, 0]);
        // body color in the middle, black corner (rounded)
        assert_eq!(at(&a, 36, 20), [200, 10, 10]);
        assert_eq!(at(&a, 0, 0), [0, 0, 0]);
    }

    #[test]
    fn text_draws_foreground_pixels_and_jpeg_encodes() {
        let Some(f) = fonts() else {
            eprintln!("no system font; skipping text raster check");
            return;
        };
        let v = KeyVisual { bg: [0, 0, 0], fg: [255, 255, 255], icon: "\u{f0e7}".into(), label: "HYPE".into(), ..Default::default() };
        let img = render_rgb(Some(&f), &v, 72, false);
        let lit = img.chunks(3).filter(|p| p[0] > 128).count();
        assert!(lit > 40, "text rendered {lit} bright pixels");
        let j = jpeg(&img, 72).unwrap();
        assert_eq!(&j[..2], &[0xFF, 0xD8]);
        assert!(j.len() < 8000);
    }

    #[test]
    fn long_labels_wrap_or_shrink_to_fit() {
        let Some(f) = fonts() else { return };
        let (px, lines) = layout_label(&f, "chorus blast finale", 14.4, 8.6, 60.0, 2);
        assert!(lines.len() <= 2);
        assert!(lines.iter().all(|l| f.width(l, px, true) <= 60.0 || l.ends_with('…')));
    }

    #[test]
    fn palette_parses_hex_and_tokens() {
        let p = Palette::from_theme(&se_ui_kit::Theme::default());
        assert_eq!(p.color("#102030"), Some([0x10, 0x20, 0x30]));
        assert_eq!(p.color("red"), Some(p.red));
        assert_eq!(p.color("nope"), None);
        assert_eq!(contrast([255, 255, 255]), [20, 20, 24]);
    }
}
