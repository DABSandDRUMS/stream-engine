//! Theme tokens mapped from the active Omarchy theme (§16.2), watched live.
//!
//! Colors come from Omarchy; everything else (surfaces, text tiers, radii, spacing, type) is
//! derived here so every screen shares one calm, readable look.

use egui::{Color32, CornerRadius, FontData, FontDefinitions, FontFamily, FontId, Margin, Stroke, TextStyle, Visuals};
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
    // ---- derived surfaces (opaque) ----
    /// Sidebar / top bar.
    pub chrome: Color32,
    /// Cards and panels on the page.
    pub surface: Color32,
    /// Hovered or raised surface, inputs.
    pub surface_hi: Color32,
    /// Hairline borders and dividers.
    pub border: Color32,
    /// Secondary text.
    pub text_dim: Color32,
    /// Hints, placeholders, disabled.
    pub text_faint: Color32,
    /// Text on the accent color.
    pub on_accent: Color32,
    /// Recessed wells: slider rails, switch tracks, meters, input fields. Contrasts with both
    /// `surface` and `surface_hi`.
    pub inset: Color32,
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

/// Linear mix of two colors (`t` = 0 → `a`, 1 → `b`), opaque.
pub fn mix(a: Color32, b: Color32, t: f32) -> Color32 {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round().clamp(0.0, 255.0) as u8;
    Color32::from_rgb(l(a.r(), b.r()), l(a.g(), b.g()), l(a.b(), b.b()))
}

fn luminance(c: Color32) -> f32 {
    (0.2126 * c.r() as f32 + 0.7152 * c.g() as f32 + 0.0722 * c.b() as f32) / 255.0
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
        let bg = g(&c.background, Color32::from_rgb(0x1f, 0x1f, 0x28));
        let fg = g(&c.foreground, Color32::from_rgb(0xdc, 0xd7, 0xba));
        let light = c.mode.as_deref().map(|m| m == "light").unwrap_or(luminance(bg) > 0.5);
        let accent = g(&c.accent, fg);
        // Surfaces step from the page background toward the foreground; the chrome (sidebar,
        // top bar) steps the other way so the page reads as the brightest plane.
        let away = if light { Color32::WHITE } else { Color32::BLACK };
        Theme {
            light,
            bg,
            bg_dark: g(&c.dark_background, mix(bg, away, 0.25)),
            bg_darker: g(&c.darker_background, mix(bg, away, 0.45)),
            bg_light: g(&c.lighter_background, mix(bg, fg, 0.06)),
            fg,
            fg_dim: mix(fg, bg, 0.38),
            fg_bright: g(&c.bright_foreground, fg),
            accent,
            selection: g(&c.selection, mix(bg, accent, 0.25)),
            muted: g(&c.muted, mix(fg, bg, 0.7)),
            red: g(&c.red, Color32::from_rgb(0xc3, 0x40, 0x43)),
            yellow: g(&c.yellow, Color32::from_rgb(0xc0, 0xa3, 0x6e)),
            green: g(&c.green, Color32::from_rgb(0x76, 0x94, 0x6a)),
            cyan: g(&c.cyan, Color32::from_rgb(0x6a, 0x95, 0x89)),
            blue: g(&c.blue, Color32::from_rgb(0x7e, 0x9c, 0xd8)),
            magenta: g(&c.magenta, Color32::from_rgb(0x95, 0x7f, 0xb8)),
            orange: g(&c.orange, Color32::from_rgb(0xc1, 0x71, 0x58)),
            bright_red: g(&c.bright_red, g(&c.red, Color32::RED)),
            chrome: mix(bg, away, if light { 0.5 } else { 0.35 }),
            surface: mix(bg, fg, 0.045),
            surface_hi: mix(bg, fg, 0.085),
            border: mix(bg, fg, 0.13),
            text_dim: mix(fg, bg, 0.38),
            text_faint: mix(fg, bg, 0.58),
            on_accent: if luminance(accent) > 0.55 { mix(bg, Color32::BLACK, 0.3) } else { Color32::WHITE },
            inset: mix(bg, away, 0.32),
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

    /// Apply to egui: every surface, stroke, and text color comes from the tokens, so any plain
    /// egui widget left in a view still looks and moves like the kit.
    pub fn apply(&self, ctx: &egui::Context, zoom: f32) {
        let mut v = if self.light { Visuals::light() } else { Visuals::dark() };
        v.override_text_color = Some(self.fg);
        v.panel_fill = self.bg;
        v.window_fill = self.surface;
        v.extreme_bg_color = mix(self.bg, if self.light { Color32::WHITE } else { Color32::BLACK }, 0.2);
        v.faint_bg_color = self.surface;
        v.code_bg_color = self.surface_hi;
        v.text_edit_bg_color = Some(self.inset);
        v.hyperlink_color = self.accent;
        v.warn_fg_color = self.yellow;
        v.error_fg_color = self.bright_red;
        v.selection.bg_fill = mix(self.bg, self.accent, 0.3);
        v.selection.stroke = Stroke::new(1.0, self.accent);
        v.text_cursor.stroke = Stroke::new(2.0, self.accent);
        v.window_stroke = Stroke::new(1.0, self.border);
        v.window_corner_radius = CornerRadius::same(radius::CARD);
        v.menu_corner_radius = CornerRadius::same(radius::CONTROL + 2);
        v.window_shadow = egui::Shadow { offset: [0, 8], blur: 24, spread: 0, color: Color32::from_black_alpha(90) };
        v.popup_shadow = egui::Shadow { offset: [0, 8], blur: 22, spread: 0, color: Color32::from_black_alpha(85) };
        v.indent_has_left_vline = false;
        v.striped = false;
        v.slider_trailing_fill = true;
        v.handle_shape = egui::style::HandleShape::Circle;
        v.interact_cursor = Some(egui::CursorIcon::PointingHand);
        let r = CornerRadius::same(radius::CONTROL);
        // `bg_fill` paints slider rails and other recessed parts, `weak_bg_fill` paints buttons
        let lift = mix(self.surface_hi, self.fg, 0.07);
        for (w, fill, stroke) in [
            (&mut v.widgets.noninteractive, self.surface, self.border),
            (&mut v.widgets.inactive, self.surface_hi, self.border),
            (&mut v.widgets.hovered, lift, mix(self.border, self.fg, 0.18)),
            (&mut v.widgets.active, mix(self.surface_hi, self.accent, 0.2), self.accent),
            (&mut v.widgets.open, mix(self.surface_hi, self.accent, 0.12), self.accent),
        ] {
            w.bg_fill = self.inset;
            w.weak_bg_fill = fill;
            w.bg_stroke = Stroke::new(1.0, stroke);
            w.fg_stroke = Stroke::new(1.0, self.fg);
            w.corner_radius = r;
            w.expansion = 0.0;
        }
        v.widgets.noninteractive.fg_stroke = Stroke::new(1.0, self.text_dim);
        v.widgets.noninteractive.bg_stroke = Stroke::new(1.0, self.border);
        let reduced = crate::motion::reduced(ctx);
        ctx.add_plugin(crate::motion::PopupMotion::default());
        ctx.set_theme(if self.light { egui::Theme::Light } else { egui::Theme::Dark });
        ctx.all_styles_mut(|s| {
            s.visuals = v.clone();
            s.animation_time = crate::motion::egui_animation_time(reduced);
            s.spacing.item_spacing = egui::vec2(spacing::S, spacing::S);
            s.spacing.button_padding = egui::vec2(spacing::M + 2.0, spacing::S);
            s.spacing.interact_size.y = 32.0;
            s.spacing.slider_rail_height = 4.0;
            s.spacing.window_margin = Margin::same(spacing::L as i8);
            s.spacing.menu_margin = Margin::same(6);
            s.spacing.combo_width = 180.0;
            s.spacing.combo_height = 320.0;
            s.spacing.text_edit_width = 240.0;
            s.spacing.tooltip_width = 320.0;
            s.spacing.icon_width = 18.0;
            s.spacing.icon_width_inner = 10.0;
            s.spacing.icon_spacing = spacing::S;
            // thin floating scroll bars that fade in while the pointer is over the list
            s.spacing.scroll = egui::style::ScrollStyle {
                bar_width: 8.0,
                floating_width: 4.0,
                handle_min_length: 24.0,
                bar_inner_margin: 3.0,
                bar_outer_margin: 2.0,
                foreground_color: true,
                dormant_handle_opacity: 0.0,
                active_handle_opacity: 0.28,
                interact_handle_opacity: 0.5,
                dormant_background_opacity: 0.0,
                active_background_opacity: 0.0,
                interact_background_opacity: 0.0,
                ..egui::style::ScrollStyle::floating()
            };
            s.interaction.selectable_labels = false;
            s.interaction.tooltip_delay = 0.35;
            s.interaction.tooltip_grace_time = 0.3;
            s.interaction.show_tooltips_only_when_still = true;
            s.text_styles = [
                (TextStyle::Small, FontId::new(type_scale::SMALL * zoom, FontFamily::Proportional)),
                (TextStyle::Body, FontId::new(type_scale::BODY * zoom, FontFamily::Proportional)),
                (TextStyle::Button, FontId::new(type_scale::BODY * zoom, FontFamily::Name(fonts::MEDIUM.into()))),
                (TextStyle::Heading, FontId::new(type_scale::HEADING * zoom, FontFamily::Name(fonts::SEMIBOLD.into()))),
                (TextStyle::Monospace, FontId::new(type_scale::BODY * zoom, FontFamily::Monospace)),
            ]
            .into();
        });
    }
}

/// Spacing scale (points).
pub mod spacing {
    pub const XS: f32 = 4.0;
    pub const S: f32 = 8.0;
    pub const M: f32 = 12.0;
    pub const L: f32 = 16.0;
    pub const XL: f32 = 24.0;
    pub const XXL: f32 = 32.0;
}

/// Corner radii (points).
pub mod radius {
    pub const CONTROL: u8 = 8;
    pub const CARD: u8 = 12;
    pub const TILE: u8 = 10;
    pub const PILL: u8 = 255;
}

/// Type scale (points).
pub mod type_scale {
    pub const SMALL: f32 = 12.5;
    pub const BODY: f32 = 14.5;
    pub const LARGE: f32 = 16.5;
    pub const HEADING: f32 = 20.0;
    pub const TITLE: f32 = 26.0;
    pub const DISPLAY: f32 = 34.0;
}

/// Font family names installed by [`install_font`].
pub mod fonts {
    pub const MEDIUM: &str = "medium";
    pub const SEMIBOLD: &str = "semibold";
    pub const BOLD: &str = "bold";
}

/// `FontId` helpers for the type scale.
pub fn font(size: f32) -> FontId {
    FontId::new(size, FontFamily::Proportional)
}
pub fn font_medium(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(fonts::MEDIUM.into()))
}
pub fn font_semibold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(fonts::SEMIBOLD.into()))
}
pub fn font_bold(size: f32) -> FontId {
    FontId::new(size, FontFamily::Name(fonts::BOLD.into()))
}
pub fn font_mono(size: f32) -> FontId {
    FontId::new(size, FontFamily::Monospace)
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

/// Install the fonts: Inter (bundled, OFL) for all interface text in four weights, and the
/// Omarchy font for numbers/timecode (monospace) and as the icon set (its Nerd Font glyphs are
/// the fallback of every family). Returns the Omarchy font's name when found.
pub fn install_font(ctx: &egui::Context, family: Option<&str>) -> Option<String> {
    let mut defs = FontDefinitions::default();
    for (key, bytes) in [
        ("inter", &include_bytes!("../assets/fonts/Inter-Regular.ttf")[..]),
        ("inter-medium", &include_bytes!("../assets/fonts/Inter-Medium.ttf")[..]),
        ("inter-semibold", &include_bytes!("../assets/fonts/Inter-SemiBold.ttf")[..]),
        ("inter-bold", &include_bytes!("../assets/fonts/Inter-Bold.ttf")[..]),
        // monochrome emoji for chat, commands and labels (OFL; newer than egui's built-in set)
        ("noto-emoji", &include_bytes!("../assets/fonts/NotoEmoji.ttf")[..]),
    ] {
        defs.font_data.insert(key.into(), Arc::new(FontData::from_static(bytes)));
    }
    let fam = family.map(str::to_string).or_else(omarchy_font_name);
    let mono = fam.as_deref().and_then(|f| font_file(f, "")).and_then(|p| std::fs::read(p).ok());
    let fallback: Vec<String> = defs.families.get(&FontFamily::Proportional).cloned().unwrap_or_default();
    let chain = |first: &str| {
        let mut v = vec![first.to_string()];
        if mono.is_some() {
            v.push("omarchy".into());
        }
        v.push("noto-emoji".into());
        v.extend(fallback.iter().cloned());
        v
    };
    let families = [
        (FontFamily::Proportional, chain("inter")),
        (FontFamily::Name(fonts::MEDIUM.into()), chain("inter-medium")),
        (FontFamily::Name(fonts::SEMIBOLD.into()), chain("inter-semibold")),
        (FontFamily::Name(fonts::BOLD.into()), chain("inter-bold")),
    ];
    if let Some(b) = mono {
        defs.font_data.insert("omarchy".into(), Arc::new(FontData::from_owned(b)));
        defs.families.entry(FontFamily::Monospace).or_default().insert(0, "omarchy".into());
    }
    for (f, list) in families {
        defs.families.insert(f, list);
    }
    ctx.set_fonts(defs);
    fam
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
