//! Theme tokens mapped from the active Omarchy theme (§16.2), watched live.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Stroke, TextStyle, Visuals};
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    pub light: bool,
    pub bg: Color32,
    pub bg_dark: Color32,
    pub bg_darker: Color32,
    pub bg_light: Color32,
    pub fg: Color32,
    pub fg_dim: Color32,
    pub fg_bright: Color32,
    pub accent: Color32,
    pub selection: Color32,
    pub muted: Color32,
    pub red: Color32,
    pub yellow: Color32,
    pub green: Color32,
    pub cyan: Color32,
    pub blue: Color32,
    pub magenta: Color32,
    pub orange: Color32,
    pub bright_red: Color32,
}

impl Theme {
    /// Tally: on program.
    pub fn tally_program(&self) -> Color32 {
        self.bright_red
    }
    /// Tally: on preview / armed.
    pub fn tally_preview(&self) -> Color32 {
        self.yellow
    }
    pub fn healthy(&self) -> Color32 {
        self.green
    }
    pub fn modulated(&self) -> Color32 {
        self.magenta
    }
}

#[derive(Deserialize, Default)]
#[serde(default)]
struct ColorsToml {
    mode: Option<String>,
    accent: Option<String>,
    selection: Option<String>,
    muted: Option<String>,
    background: Option<String>,
    dark_background: Option<String>,
    darker_background: Option<String>,
    lighter_background: Option<String>,
    foreground: Option<String>,
    dark_foreground: Option<String>,
    light_foreground: Option<String>,
    bright_foreground: Option<String>,
    red: Option<String>,
    yellow: Option<String>,
    orange: Option<String>,
    green: Option<String>,
    cyan: Option<String>,
    blue: Option<String>,
    magenta: Option<String>,
    bright_red: Option<String>,
}

pub fn hex(s: &str) -> Option<Color32> {
    let h = s.trim().trim_start_matches('#');
    let b = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok();
    match h.len() {
        6 => Some(Color32::from_rgb(b(0)?, b(2)?, b(4)?)),
        8 => Some(Color32::from_rgba_unmultiplied(b(0)?, b(2)?, b(4)?, b(6)?)),
        _ => None,
    }
}

impl Default for Theme {
    /// Fallback (Kanagawa-like) used when no Omarchy theme is present.
    fn default() -> Self {
        Theme::parse(include_str!("default_colors.toml"))
    }
}

impl Theme {
    pub fn parse(src: &str) -> Theme {
        let c: ColorsToml = toml::from_str(src).unwrap_or_default();
        let g = |v: &Option<String>, d: Color32| v.as_deref().and_then(hex).unwrap_or(d);
        let light = c.mode.as_deref() == Some("light");
        let bg = g(&c.background, Color32::from_rgb(0x1f, 0x1f, 0x28));
        let fg = g(&c.foreground, Color32::from_rgb(0xdc, 0xd7, 0xba));
        Theme {
            light,
            bg,
            bg_dark: g(&c.dark_background, bg.gamma_multiply(0.8)),
            bg_darker: g(&c.darker_background, bg.gamma_multiply(0.6)),
            bg_light: g(&c.lighter_background, bg),
            fg,
            fg_dim: g(&c.dark_foreground, fg.gamma_multiply(0.6)),
            fg_bright: g(&c.bright_foreground, fg),
            accent: g(&c.accent, fg),
            selection: g(&c.selection, bg),
            muted: g(&c.muted, fg.gamma_multiply(0.4)),
            red: g(&c.red, Color32::from_rgb(0xc3, 0x40, 0x43)),
            yellow: g(&c.yellow, Color32::from_rgb(0xc0, 0xa3, 0x6e)),
            green: g(&c.green, Color32::from_rgb(0x76, 0x94, 0x6a)),
            cyan: g(&c.cyan, Color32::from_rgb(0x6a, 0x95, 0x89)),
            blue: g(&c.blue, Color32::from_rgb(0x7e, 0x9c, 0xd8)),
            magenta: g(&c.magenta, Color32::from_rgb(0x95, 0x7f, 0xb8)),
            orange: g(&c.orange, Color32::from_rgb(0xc1, 0x71, 0x58)),
            bright_red: g(&c.bright_red, g(&c.red, Color32::RED)),
        }
    }

    pub fn omarchy_path() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        let p = PathBuf::from(home).join(".local/state/omarchy/current/theme/colors.toml");
        p.exists().then_some(p)
    }

    pub fn load() -> Theme {
        Theme::omarchy_path().and_then(|p| std::fs::read_to_string(p).ok()).map(|s| Theme::parse(&s)).unwrap_or_default()
    }

    /// Apply to egui: every surface, stroke, and text color comes from the tokens.
    pub fn apply(&self, ctx: &egui::Context, zoom: f32) {
        let mut v = if self.light { Visuals::light() } else { Visuals::dark() };
        v.override_text_color = Some(self.fg);
        v.panel_fill = self.bg;
        v.window_fill = self.bg_dark;
        v.extreme_bg_color = self.bg_darker;
        v.faint_bg_color = self.bg_light;
        v.code_bg_color = self.bg_darker;
        v.hyperlink_color = self.blue;
        v.warn_fg_color = self.yellow;
        v.error_fg_color = self.bright_red;
        v.selection.bg_fill = self.selection;
        v.selection.stroke = Stroke::new(1.0, self.accent);
        v.window_stroke = Stroke::new(1.0, self.muted);
        v.window_corner_radius = CornerRadius::same(6);
        v.menu_corner_radius = CornerRadius::same(4);
        let r = CornerRadius::same(4);
        for (w, fill, stroke) in [
            (&mut v.widgets.noninteractive, self.bg, self.muted),
            (&mut v.widgets.inactive, self.bg_light, self.muted),
            (&mut v.widgets.hovered, self.selection, self.accent),
            (&mut v.widgets.active, self.selection, self.accent),
            (&mut v.widgets.open, self.bg_light, self.accent),
        ] {
            w.bg_fill = fill;
            w.weak_bg_fill = fill;
            w.bg_stroke = Stroke::new(1.0, stroke);
            w.fg_stroke = Stroke::new(1.0, self.fg);
            w.corner_radius = r;
        }
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, self.fg_dim);
        ctx.set_theme(if self.light { egui::Theme::Light } else { egui::Theme::Dark });
        ctx.all_styles_mut(|s| {
            s.visuals = v.clone();
            s.spacing.item_spacing = egui::vec2(spacing::S, spacing::S);
            s.spacing.button_padding = egui::vec2(spacing::M, spacing::S);
            s.spacing.interact_size.y = 24.0;
            s.text_styles = [
                (TextStyle::Small, FontId::new(type_scale::SMALL * zoom, FontFamily::Proportional)),
                (TextStyle::Body, FontId::new(type_scale::BODY * zoom, FontFamily::Proportional)),
                (TextStyle::Button, FontId::new(type_scale::BODY * zoom, FontFamily::Proportional)),
                (TextStyle::Heading, FontId::new(type_scale::HEADING * zoom, FontFamily::Proportional)),
                (TextStyle::Monospace, FontId::new(type_scale::BODY * zoom, FontFamily::Monospace)),
            ]
            .into();
        });
    }
}

pub mod spacing {
    pub const XS: f32 = 2.0;
    pub const S: f32 = 6.0;
    pub const M: f32 = 10.0;
    pub const L: f32 = 16.0;
    pub const XL: f32 = 24.0;
}

pub mod type_scale {
    pub const SMALL: f32 = 11.0;
    pub const BODY: f32 = 13.5;
    pub const LARGE: f32 = 16.0;
    pub const HEADING: f32 = 18.0;
    pub const DISPLAY: f32 = 26.0;
}

/// The Omarchy font (`omarchy font current`), resolved to a file with fontconfig.
pub fn omarchy_font_name() -> Option<String> {
    let out = std::process::Command::new("omarchy").args(["font", "current"]).output().ok()?;
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

fn font_file(family: &str, style: &str) -> Option<PathBuf> {
    let pat = if style.is_empty() { family.to_string() } else { format!("{family}:{style}") };
    let out = std::process::Command::new("fc-match").args(["-f", "%{file}", &pat]).output().ok()?;
    let p = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());
    p.exists().then_some(p)
}

/// Install the Omarchy font (Nerd Font glyphs = icon set) for all text.
pub fn install_font(ctx: &egui::Context, family: Option<&str>) -> Option<String> {
    let fam = family.map(str::to_string).or_else(omarchy_font_name)?;
    let regular = font_file(&fam, "")?;
    let bytes = std::fs::read(&regular).ok()?;
    let mut defs = FontDefinitions::default();
    defs.font_data.insert("omarchy".into(), Arc::new(FontData::from_owned(bytes)));
    if let Some(b) = font_file(&fam, "bold").and_then(|p| std::fs::read(p).ok()) {
        defs.font_data.insert("omarchy-bold".into(), Arc::new(FontData::from_owned(b)));
        defs.families.insert(FontFamily::Name("bold".into()), vec!["omarchy-bold".into(), "omarchy".into()]);
    } else {
        defs.families.insert(FontFamily::Name("bold".into()), vec!["omarchy".into()]);
    }
    for f in [FontFamily::Proportional, FontFamily::Monospace] {
        defs.families.entry(f).or_default().insert(0, "omarchy".into());
    }
    ctx.set_fonts(defs);
    Some(fam)
}

/// Watches the Omarchy theme directory and font; sends updates and requests repaints.
pub struct ThemeWatcher {
    pub rx: crossbeam_channel::Receiver<ThemeChange>,
    _watcher: Option<notify::RecommendedWatcher>,
}

pub enum ThemeChange {
    Theme(Theme),
    Font(String),
}

impl ThemeWatcher {
    pub fn start(ctx: egui::Context) -> ThemeWatcher {
        use notify::Watcher;
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut watcher = None;
        if let Some(home) = std::env::var_os("HOME") {
            let dir = PathBuf::from(home).join(".local/state/omarchy");
            let (t2, c2) = (tx.clone(), ctx.clone());
            let w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
                if let Ok(ev) = res
                    && ev.paths.iter().any(|p| p.to_string_lossy().contains("current"))
                {
                    std::thread::sleep(std::time::Duration::from_millis(80));
                    let _ = t2.send(ThemeChange::Theme(Theme::load()));
                    c2.request_repaint();
                }
            });
            if let Ok(mut w) = w
                && w.watch(&dir, notify::RecursiveMode::Recursive).is_ok()
            {
                watcher = Some(w);
            }
        }
        // font: poll (the font-set hook also touches the state dir, which the watcher sees)
        std::thread::Builder::new()
            .name("se-font-watch".into())
            .spawn(move || {
                let mut cur = omarchy_font_name();
                loop {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    let f = omarchy_font_name();
                    if let Some(name) = f.clone().filter(|_| f != cur) {
                        cur = f;
                        if tx.send(ThemeChange::Font(name)).is_err() {
                            break;
                        }
                        ctx.request_repaint();
                    }
                }
            })
            .ok();
        ThemeWatcher { rx, _watcher: watcher }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_omarchy_colors() {
        let t = Theme::parse("mode = \"light\"\nbackground = \"#ffffff\"\nred = \"#ff0000\"\n");
        assert!(t.light);
        assert_eq!(t.bg, Color32::WHITE);
        assert_eq!(t.red, Color32::from_rgb(255, 0, 0));
        assert_eq!(hex("#11223344"), Some(Color32::from_rgba_unmultiplied(0x11, 0x22, 0x33, 0x44)));
    }
}
