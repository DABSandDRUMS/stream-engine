//! The component library. Every screen is built from these, so the whole app shares one look:
//! calm surfaces, clear hierarchy, big obvious targets, plain words (§15.1).

use crate::theme::{Theme, font, font_bold, font_medium, font_mono, font_semibold, mix, radius, spacing, type_scale};
use egui::{Align, Align2, Color32, CornerRadius, Layout, Pos2, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui, Vec2};

/// Nerd Font glyphs used as the icon set.
pub mod icon {
    pub const SCENE: &str = "\u{f03d}";
    pub const PRESET: &str = "\u{f0e7}";
    pub const PATCH: &str = "\u{f1b2}";
    pub const EFFECT: &str = "\u{f0d0}";
    pub const RULE: &str = "\u{f0e8}";
    pub const BINDING: &str = "\u{f0c1}";
    pub const TIMELINE: &str = "\u{f017}";
    pub const LIGHT: &str = "\u{f0eb}";
    pub const MIX: &str = "\u{f1de}";
    pub const CHAT: &str = "\u{f086}";
    pub const QUEUE: &str = "\u{f001}";
    pub const MOD: &str = "\u{f132}";
    pub const WARN: &str = "\u{f071}";
    pub const LIVE: &str = "\u{f111}";
    pub const REC: &str = "\u{f144}";
    pub const LINK: &str = "\u{f0c1}";
    pub const UNLINK: &str = "\u{f127}";
    pub const GPU: &str = "\u{f2db}";
    pub const DEVICE: &str = "\u{f287}";
    pub const CONTROLLER: &str = "\u{f11b}";
    pub const BOT: &str = "\u{f27a}";
    pub const ALERT: &str = "\u{f0f3}";
    pub const SESSION: &str = "\u{f02d}";
    pub const PERF: &str = "\u{f0e4}";
    pub const SETTINGS: &str = "\u{f013}";
    pub const CHECK: &str = "\u{f00c}";
    pub const CROSS: &str = "\u{f00d}";
    pub const PLAY: &str = "\u{f04b}";
    pub const PAUSE: &str = "\u{f04c}";
    pub const STOP: &str = "\u{f04d}";
    pub const SEARCH: &str = "\u{f002}";
    pub const CONSOLE: &str = "\u{f120}";
    pub const TRACE: &str = "\u{f126}";
    pub const SIM: &str = "\u{f0c3}";
    pub const SCOPE: &str = "\u{f201}";
    pub const HOME: &str = "\u{f015}";
    pub const LAYERS: &str = "\u{f009}";
    pub const WAND: &str = "\u{f0d0}";
    pub const MUSIC: &str = "\u{f001}";
    pub const USERS: &str = "\u{f0c0}";
    pub const FILM: &str = "\u{f008}";
    pub const SLIDERS: &str = "\u{f1de}";
    pub const PLUS: &str = "\u{f067}";
    pub const MINUS: &str = "\u{f068}";
    pub const RIGHT: &str = "\u{f054}";
    pub const LEFT: &str = "\u{f053}";
    pub const DOWN: &str = "\u{f078}";
    pub const UP: &str = "\u{f077}";
    pub const EYE: &str = "\u{f06e}";
    pub const EYE_OFF: &str = "\u{f070}";
    pub const TRASH: &str = "\u{f1f8}";
    pub const EDIT: &str = "\u{f044}";
    pub const BOLT: &str = "\u{f0e7}";
    pub const GIFT: &str = "\u{f06b}";
    pub const STAR: &str = "\u{f005}";
    pub const HEART: &str = "\u{f004}";
    pub const CLOCK: &str = "\u{f017}";
    pub const MIC: &str = "\u{f130}";
    pub const VOLUME: &str = "\u{f028}";
    pub const MUTE: &str = "\u{f026}";
    pub const KEYBOARD: &str = "\u{f11c}";
    pub const IMAGE: &str = "\u{f03e}";
    pub const CAMERA: &str = "\u{f030}";
    pub const PALETTE: &str = "\u{f1fc}";
    pub const ROCKET: &str = "\u{f135}";
    pub const INFO: &str = "\u{f05a}";
    pub const HELP: &str = "\u{f059}";
    pub const SHUFFLE: &str = "\u{f074}";
    pub const EXPAND: &str = "\u{f065}";
    pub const GRID: &str = "\u{f00a}";
    pub const LIST: &str = "\u{f03a}";
    pub const BROOM: &str = "\u{f12d}";
    pub const HAND: &str = "\u{f256}";
    pub const COPY: &str = "\u{f0c5}";
    pub const SAVE: &str = "\u{f0c7}";
    pub const UNDO: &str = "\u{f0e2}";
    pub const DRUM: &str = "\u{f001}";
    pub const TWITCH: &str = "\u{f1e8}";
    pub const YOUTUBE: &str = "\u{f167}";
    pub const TIKTOK: &str = "\u{e07b}";
    pub const SPARKLE: &str = "\u{f0d0}";
    pub const DOT: &str = "\u{f111}";
    pub const WIDE: &str = "\u{f108}";
    pub const TALL: &str = "\u{f10b}";
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LedState {
    Idle,
    Armed,
    Active,
    Error,
    Healthy,
    Modulated,
}

pub fn led_color(t: &Theme, s: LedState) -> Color32 {
    match s {
        LedState::Idle => t.text_faint,
        LedState::Armed => t.tally_preview(),
        LedState::Active => t.tally_program(),
        LedState::Error => t.bright_red,
        LedState::Healthy => t.healthy(),
        LedState::Modulated => t.modulated(),
    }
}

/// A small status dot.
pub fn led(ui: &mut Ui, t: &Theme, s: LedState) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(12.0), Sense::hover());
    let c = led_color(t, s);
    if s != LedState::Idle {
        ui.painter().circle_filled(rect.center(), 6.0, c.gamma_multiply(0.22));
    }
    ui.painter().circle_filled(rect.center(), 3.5, c);
    resp
}

// ---- buttons -----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The one main action on a screen (accent fill).
    Primary,
    /// Normal action (surface fill).
    Secondary,
    /// Low-emphasis (text only until hovered).
    Ghost,
    /// Destructive / stop (red).
    Danger,
    /// Go live / on-air (tally red fill).
    Live,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Size {
    Small,
    Medium,
    Large,
}

impl Size {
    fn height(self) -> f32 {
        match self {
            Size::Small => 28.0,
            Size::Medium => 36.0,
            Size::Large => 48.0,
        }
    }
    fn text(self) -> f32 {
        match self {
            Size::Small => type_scale::SMALL + 0.5,
            Size::Medium => type_scale::BODY,
            Size::Large => type_scale::LARGE + 1.0,
        }
    }
}

fn kind_colors(t: &Theme, kind: Kind, hovered: bool, pressed: bool, enabled: bool) -> (Color32, Color32, Color32) {
    let (fill, text, stroke) = match kind {
        Kind::Primary => (t.accent, t.on_accent, Color32::TRANSPARENT),
        Kind::Secondary => (t.surface_hi, t.fg, t.border),
        Kind::Ghost => (if hovered { t.surface_hi } else { Color32::TRANSPARENT }, t.fg, Color32::TRANSPARENT),
        Kind::Danger => (mix(t.surface, t.bright_red, 0.18), mix(t.fg, t.bright_red, 0.55), mix(t.border, t.bright_red, 0.5)),
        Kind::Live => (t.tally_program(), Color32::WHITE, Color32::TRANSPARENT),
    };
    let mut fill = fill;
    if enabled && kind != Kind::Ghost {
        if pressed {
            fill = mix(fill, Color32::BLACK, 0.15);
        } else if hovered {
            fill = mix(fill, Color32::WHITE, 0.08);
        }
    }
    if !enabled {
        return (
            if kind == Kind::Ghost { Color32::TRANSPARENT } else { t.surface_hi },
            t.text_faint,
            if kind == Kind::Ghost { Color32::TRANSPARENT } else { t.border },
        );
    }
    (fill, text, stroke)
}

/// A button with an optional leading icon. Width grows with the label (or pass `min_width`).
/// Gap between an icon and its label (icon glyphs draw wider than their advance, so this is
/// laid out explicitly rather than with spaces).
const ICON_GAP: f32 = 7.0;

/// Icon + label laid out as two galleys with a fixed gap: (icon, label, total width, height).
fn icon_text(
    ui: &Ui,
    icon: Option<&str>,
    label: &str,
    fid: egui::FontId,
    color: Color32,
) -> (Option<std::sync::Arc<egui::Galley>>, Option<std::sync::Arc<egui::Galley>>, f32, f32) {
    let ig = icon.filter(|i| !i.is_empty()).map(|i| ui.painter().layout_no_wrap(i.to_string(), fid.clone(), color));
    let lg = (!label.is_empty()).then(|| ui.painter().layout_no_wrap(label.to_string(), fid, color));
    let w = ig.as_ref().map_or(0.0, |g| ink(g).1) + lg.as_ref().map_or(0.0, |g| g.size().x) + if ig.is_some() && lg.is_some() { ICON_GAP } else { 0.0 };
    let h = ig.as_ref().map_or(0.0, |g| g.size().y).max(lg.as_ref().map_or(0.0, |g| g.size().y));
    (ig, lg, w, h)
}

/// (left overhang, painted width) of a galley: icon glyphs often draw outside their advance.
fn ink(g: &egui::Galley) -> (f32, f32) {
    let left = g.mesh_bounds.min.x.min(0.0);
    let right = g.mesh_bounds.max.x.max(g.size().x);
    (-left, right - left)
}

fn paint_icon_text(
    p: &egui::Painter,
    center: Pos2,
    parts: (Option<std::sync::Arc<egui::Galley>>, Option<std::sync::Arc<egui::Galley>>, f32, f32),
    color: Color32,
) {
    let (ig, lg, w, _) = parts;
    let mut x = center.x - w / 2.0;
    if let Some(g) = ig {
        let (overhang, gw) = ink(&g);
        p.galley_with_override_text_color(Pos2::new(x + overhang, center.y - g.size().y / 2.0), g, color);
        x += gw + ICON_GAP;
    }
    if let Some(g) = lg {
        p.galley_with_override_text_color(Pos2::new(x, center.y - g.size().y / 2.0), g, color);
    }
}

/// A button with an optional leading icon. Width grows with the label (or pass `min_width`).
#[allow(clippy::too_many_arguments)]
pub fn button_ex(ui: &mut Ui, t: &Theme, icon: Option<&str>, label: &str, kind: Kind, size: Size, min_width: f32, enabled: bool) -> Response {
    let parts = icon_text(ui, icon, label, font_medium(size.text()), t.fg);
    let pad = if size == Size::Small { 10.0 } else { 16.0 };
    let w = (parts.2 + pad * 2.0).max(min_width).max(size.height());
    let sense = if enabled { Sense::click() } else { Sense::hover() };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, size.height()), sense);
    if ui.is_rect_visible(rect) {
        let (fill, fg, stroke) = kind_colors(t, kind, resp.hovered(), resp.is_pointer_button_down_on(), enabled);
        let p = ui.painter();
        p.rect(rect, CornerRadius::same(radius::CONTROL), fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
        paint_icon_text(p, rect.center(), parts, fg);
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

pub fn button(ui: &mut Ui, t: &Theme, label: &str, kind: Kind) -> Response {
    button_ex(ui, t, None, label, kind, Size::Medium, 0.0, true)
}

pub fn icon_label_button(ui: &mut Ui, t: &Theme, icon: &str, label: &str, kind: Kind) -> Response {
    button_ex(ui, t, Some(icon), label, kind, Size::Medium, 0.0, true)
}

/// Square icon-only button (tooltip required: icons alone are not self-explanatory).
pub fn icon_button(ui: &mut Ui, t: &Theme, icon: &str, tooltip: &str) -> Response {
    button_ex(ui, t, Some(icon), "", Kind::Ghost, Size::Small, 0.0, true).on_hover_text(tooltip)
}

/// Choice chip (pick one or several from a set: event kinds, filters, lights). Selected =
/// accent tint + accent border; unselected = the secondary look.
pub fn chip(ui: &mut Ui, t: &Theme, icon: &str, label: &str, selected: bool) -> Response {
    let parts = icon_text(ui, Some(icon), label, font_medium(type_scale::SMALL + 0.5), t.fg);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(parts.2 + 24.0, 28.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let (fill, stroke) = if selected {
            (mix(t.surface, t.accent, if resp.hovered() { 0.26 } else { 0.18 }), t.accent)
        } else {
            (if resp.hovered() { mix(t.surface_hi, t.fg, 0.06) } else { t.surface_hi }, t.border)
        };
        let p = ui.painter();
        p.rect(rect, CornerRadius::same(radius::PILL), fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
        paint_icon_text(p, rect.center(), parts, if selected { t.fg } else { mix(t.text_dim, t.fg, 0.4) });
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Hold-to-confirm button (destructive actions in Show mode, §15.1).
/// Returns true once, when the hold completes.
pub fn hold_button(ui: &mut Ui, t: &Theme, label: &str, color: Color32, hold_secs: f32) -> bool {
    let id = ui.make_persistent_id(("hold", label));
    let fid = font_semibold(type_scale::BODY);
    let galley = ui.painter().layout_no_wrap(label.to_string(), fid, t.fg);
    let size = Vec2::new(galley.size().x + 32.0, 36.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    let now = ui.input(|i| i.time);
    let mut start: Option<f64> = ui.data(|d| d.get_temp(id));
    let mut fired = false;
    if resp.is_pointer_button_down_on() {
        let s = *start.get_or_insert(now);
        ui.data_mut(|d| d.insert_temp(id, s));
        if now - s >= hold_secs as f64 {
            fired = true;
            ui.data_mut(|d| d.insert_temp(id, f64::INFINITY));
        }
        ui.ctx().request_repaint();
    } else {
        start = None;
        ui.data_mut(|d| d.remove::<f64>(id));
    }
    let prog = start.map(|s| if s.is_infinite() { 1.0 } else { ((now - s) as f32 / hold_secs).clamp(0.0, 1.0) }).unwrap_or(0.0);
    let p = ui.painter();
    let r = CornerRadius::same(radius::CONTROL);
    p.rect(rect, r, mix(t.surface, color, if resp.hovered() { 0.22 } else { 0.14 }), Stroke::new(1.0, mix(t.border, color, 0.6)), StrokeKind::Inside);
    if prog > 0.0 {
        p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * prog, rect.height())), r, color);
    }
    let fg = if prog > 0.5 { Color32::WHITE } else { mix(t.fg, color, 0.5) };
    p.galley_with_override_text_color(rect.center() - galley.size() / 2.0, galley, fg);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("Press and hold to {}", label.to_lowercase()));
    fired
}

// ---- toggles, tabs, inputs -----------------------------------------------------------------------

/// On/off switch. Returns the response (`changed()` when flipped).
pub fn toggle(ui: &mut Ui, t: &Theme, on: &mut bool) -> Response {
    let size = Vec2::new(40.0, 22.0);
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click());
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    let k = ui.ctx().animate_bool_responsive(resp.id, *on);
    let p = ui.painter();
    let track = mix(t.inset, t.accent, k);
    p.rect(rect, CornerRadius::same(radius::PILL), track, Stroke::new(1.0, mix(t.border, t.accent, k)), StrokeKind::Inside);
    let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, k);
    p.circle_filled(Pos2::new(x, rect.center().y), 8.0, if *on { t.on_accent } else { t.fg });
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A labelled switch row: text on the left, switch on the right.
pub fn toggle_row(ui: &mut Ui, t: &Theme, label: &str, help: &str, on: &mut bool) -> Response {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(label).font(font_medium(type_scale::BODY)).color(t.fg));
            if !help.is_empty() {
                ui.label(RichText::new(help).size(type_scale::SMALL).color(t.text_dim));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| toggle(ui, t, on)).inner
    })
    .inner
}

/// Underlined tabs. Returns true when the selection changed.
pub fn tabs(ui: &mut Ui, t: &Theme, selected: &mut usize, labels: &[&str]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = spacing::XL;
        for (i, label) in labels.iter().enumerate() {
            let on = *selected == i;
            let fid = if on { font_semibold(type_scale::BODY + 0.5) } else { font_medium(type_scale::BODY + 0.5) };
            let galley = ui.painter().layout_no_wrap(label.to_string(), fid, t.fg);
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(galley.size().x, 34.0), Sense::click());
            let c = if on {
                t.fg
            } else if resp.hovered() {
                mix(t.text_dim, t.fg, 0.5)
            } else {
                t.text_dim
            };
            ui.painter().galley_with_override_text_color(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0 - 2.0), galley, c);
            if on {
                let bar = Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - 3.0), rect.right_bottom());
                ui.painter().rect_filled(bar, CornerRadius::same(2), t.accent);
            }
            if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() && !on {
                *selected = i;
                changed = true;
            }
        }
    });
    let r = ui.min_rect();
    ui.painter().hline(r.x_range(), ui.cursor().top(), Stroke::new(1.0, t.border));
    ui.add_space(spacing::M);
    changed
}

/// Segmented control (small choice between 2–4 options). Returns true when changed.
pub fn segmented(ui: &mut Ui, t: &Theme, selected: &mut usize, labels: &[&str]) -> bool {
    let mut changed = false;
    let fid = font_medium(type_scale::SMALL + 0.5);
    let widths: Vec<f32> = labels.iter().map(|l| ui.painter().layout_no_wrap(l.to_string(), fid.clone(), t.fg).size().x + 24.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 6.0;
    // one auto id per control, so several segmented controls with the same labels can share a parent
    let base = ui.next_auto_id();
    ui.skip_ahead_auto_ids(1);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(total, 32.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(radius::CONTROL), t.inset);
    let mut x = rect.left() + 3.0;
    for (i, (label, w)) in labels.iter().zip(&widths).enumerate() {
        let r = Rect::from_min_size(Pos2::new(x, rect.top() + 3.0), Vec2::new(*w, rect.height() - 6.0));
        let resp = ui.interact(r, base.with(i), Sense::click()).on_hover_cursor(egui::CursorIcon::PointingHand);
        let on = *selected == i;
        if on {
            ui.painter().rect(r, CornerRadius::same(radius::CONTROL - 2), t.surface_hi, Stroke::new(1.0, t.border), StrokeKind::Inside);
        }
        let c = if on { t.fg } else { t.text_dim };
        ui.painter().text(r.center(), Align2::CENTER_CENTER, *label, fid.clone(), c);
        if resp.clicked() && !on {
            *selected = i;
            changed = true;
        }
        x += w;
    }
    changed
}

/// Horizontal slider with a label above and the value on the right.
pub fn labeled_slider(ui: &mut Ui, t: &Theme, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>, suffix: &str) -> Response {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                // 0–1 ranges read as percentages; everything else as whole numbers with the unit
                let txt = if *range.start() >= 0.0 && *range.end() <= 1.0 { format!("{:.0}%", *value * 100.0) } else { format!("{:.0}{suffix}", value) };
                ui.label(RichText::new(txt).font(font_mono(type_scale::SMALL)).color(t.fg));
            });
        });
        ui.spacing_mut().slider_width = ui.available_width();
        ui.spacing_mut().interact_size.y = 20.0;
        ui.add(egui::Slider::new(value, range).show_value(false))
    })
    .inner
}

// ---- layout: pages, cards, headers --------------------------------------------------------------

/// Page title + one-line explanation, with room for actions on the right.
pub fn page_header(ui: &mut Ui, t: &Theme, title: &str, subtitle: &str, actions: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).font(font_bold(type_scale::TITLE)).color(t.fg));
            if !subtitle.is_empty() {
                ui.add_space(2.0);
                ui.label(RichText::new(subtitle).size(type_scale::BODY).color(t.text_dim));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), actions);
    });
    ui.add_space(spacing::L);
}

/// A plain surface card (no title).
pub fn panel<R>(ui: &mut Ui, t: &Theme, body: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(t.surface)
        .stroke(Stroke::new(1.0, t.border))
        .corner_radius(CornerRadius::same(radius::CARD))
        .inner_margin(egui::Margin::same(spacing::L as i8))
        .show(ui, body)
        .inner
}

/// A surface card with a title row (optional subtitle and right-side actions).
pub fn titled<R>(ui: &mut Ui, t: &Theme, title: &str, subtitle: &str, actions: impl FnOnce(&mut Ui), body: impl FnOnce(&mut Ui) -> R) -> R {
    panel(ui, t, |ui| {
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.label(RichText::new(title).font(font_semibold(type_scale::LARGE)).color(t.fg));
                if !subtitle.is_empty() {
                    ui.label(RichText::new(subtitle).size(type_scale::SMALL + 0.5).color(t.text_dim));
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), actions);
        });
        ui.add_space(spacing::M);
        body(ui)
    })
}

/// Card frame: kind icon + title + status dot; body inside.
pub fn card<R>(ui: &mut Ui, t: &Theme, icon: &str, title: &str, state: LedState, body: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(t.surface)
        .stroke(Stroke::new(1.0, if state == LedState::Error { mix(t.border, t.bright_red, 0.6) } else { t.border }))
        .corner_radius(CornerRadius::same(radius::CARD))
        .inner_margin(egui::Margin::same(spacing::L as i8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(icon).size(type_scale::LARGE).color(t.accent));
                ui.label(RichText::new(title).font(font_semibold(type_scale::LARGE)).color(t.fg));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if state != LedState::Idle {
                        led(ui, t, state);
                    }
                });
            });
            ui.add_space(spacing::S);
            body(ui)
        })
        .inner
}

/// Section label (small caps-style heading inside a page or card).
pub fn section(ui: &mut Ui, t: &Theme, icon: &str, title: &str) {
    ui.add_space(spacing::XS);
    ui.horizontal(|ui| {
        if !icon.is_empty() {
            ui.label(RichText::new(icon).size(type_scale::SMALL + 1.0).color(t.text_dim));
        }
        ui.label(RichText::new(title).font(font_semibold(type_scale::SMALL + 0.5)).color(t.text_dim));
    });
    ui.add_space(spacing::XS);
}

/// "▸ Details" disclosure for technical information (closed by default). Returns the body's
/// result while open.
pub fn details<R>(ui: &mut Ui, t: &Theme, id_salt: impl std::hash::Hash + std::fmt::Debug, label: &str, body: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    let id = ui.make_persistent_id(("details", id_salt));
    let mut open: bool = ui.data(|d| d.get_temp(id)).unwrap_or(false);
    let text = format!("{}  {label}", if open { icon::DOWN } else { icon::RIGHT });
    let fid = font_medium(type_scale::SMALL + 0.5);
    let g = ui.painter().layout_no_wrap(text, fid, t.text_dim);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(g.size().x + 4.0, 24.0), Sense::click());
    let c = if resp.hovered() { t.fg } else { t.text_dim };
    ui.painter().galley_with_override_text_color(Pos2::new(rect.left(), rect.center().y - g.size().y / 2.0), g, c);
    if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        open = !open;
        ui.data_mut(|d| d.insert_temp(id, open));
    }
    if !open {
        return None;
    }
    Some(
        ui.horizontal(|ui| {
            ui.add_space(spacing::M);
            ui.vertical(body).inner
        })
        .inner,
    )
}

/// Dim helper text.
pub fn hint(ui: &mut Ui, t: &Theme, text: &str) -> Response {
    ui.label(RichText::new(text).size(type_scale::SMALL + 0.5).color(t.text_dim))
}

/// Friendly "nothing here yet" block: icon, one line, one sentence, optional action.
pub fn empty_state(ui: &mut Ui, t: &Theme, icon: &str, title: &str, body: &str, action: Option<&str>) -> bool {
    let mut clicked = false;
    ui.vertical_centered(|ui| {
        ui.add_space(spacing::XL);
        ui.label(RichText::new(icon).size(34.0).color(t.text_faint));
        ui.add_space(spacing::S);
        ui.label(RichText::new(title).font(font_semibold(type_scale::LARGE)).color(t.fg));
        if !body.is_empty() {
            ui.add_space(2.0);
            ui.label(RichText::new(body).size(type_scale::BODY).color(t.text_dim));
        }
        if let Some(a) = action {
            ui.add_space(spacing::M);
            clicked = button(ui, t, a, Kind::Primary).clicked();
        }
        ui.add_space(spacing::XL);
    });
    clicked
}

/// Colored badge: dot + short text in a pill.
pub fn badge(ui: &mut Ui, t: &Theme, text: &str, color: Color32) -> Response {
    let fid = font_medium(type_scale::SMALL);
    let galley = ui.painter().layout_no_wrap(text.to_string(), fid, color);
    let size = Vec2::new(galley.size().x + 28.0, 24.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(radius::PILL), mix(t.surface, color, 0.16));
    p.circle_filled(Pos2::new(rect.left() + 11.0, rect.center().y), 3.5, color);
    p.galley_with_override_text_color(Pos2::new(rect.left() + 20.0, rect.center().y - galley.size().y / 2.0), galley, mix(color, t.fg, 0.35));
    resp
}

/// Status pill: icon + text colored by health.
pub fn pill(ui: &mut Ui, t: &Theme, icon: &str, text: &str, s: LedState) -> Response {
    let c = if s == LedState::Idle { t.text_dim } else { led_color(t, s) };
    let parts = icon_text(ui, Some(icon), text, font_medium(type_scale::SMALL + 0.5), c);
    let size = Vec2::new(parts.2 + 24.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let fill = if resp.hovered() { t.surface_hi } else { mix(t.surface, c, if s == LedState::Idle { 0.0 } else { 0.1 }) };
    ui.painter().rect_filled(rect, CornerRadius::same(radius::PILL), fill);
    paint_icon_text(ui.painter(), rect.center(), parts, c);
    resp
}

/// A single-line text field with comfortable padding and the standard control height. Use it
/// for every one-line input so fields line up with the buttons next to them.
pub fn field<'t>(text: &'t mut dyn egui::TextBuffer) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text).margin(egui::Margin::symmetric(10, 8)).min_size(Vec2::new(0.0, 34.0)).vertical_align(Align::Center)
}

/// Tone of a [`callout`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tone {
    /// Something to do next (accent).
    Info,
    /// Needs attention (yellow).
    Warn,
    /// A problem, or on air (red).
    Danger,
    /// Good news (green).
    Ok,
}

/// The one banner style: icon, bold title, one sentence, and at most one action button on the
/// right. Returns true when the action was clicked.
pub fn callout(ui: &mut Ui, t: &Theme, tone: Tone, icon: &str, title: &str, body: &str, action: Option<&str>) -> bool {
    let c = match tone {
        Tone::Info => t.accent,
        Tone::Warn => t.yellow,
        Tone::Danger => t.bright_red,
        Tone::Ok => t.green,
    };
    let mut clicked = false;
    egui::Frame::new()
        .fill(mix(t.surface, c, 0.12))
        .stroke(Stroke::new(1.0, mix(t.border, c, 0.45)))
        .corner_radius(CornerRadius::same(radius::CARD))
        .inner_margin(egui::Margin::symmetric(16, 12))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal_top(|ui| {
                // icon in a fixed slot, centred on the title line
                let (slot, _) = ui.allocate_exact_size(Vec2::new(24.0, 22.0), Sense::hover());
                ui.painter().text(slot.center(), Align2::CENTER_CENTER, icon, font(type_scale::LARGE), c);
                ui.add_space(spacing::S);
                let right = if action.is_some() { 200.0 } else { 0.0 };
                ui.allocate_ui_with_layout(Vec2::new((ui.available_width() - right).max(120.0), 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.label(RichText::new(title).font(font_semibold(type_scale::BODY + 0.5)).color(t.fg));
                    if !body.is_empty() {
                        ui.add(egui::Label::new(RichText::new(body).color(t.text_dim)).wrap());
                    }
                });
                if let Some(a) = action {
                    // a fixed-height slot: in a top-aligned row a right-to-left layout would
                    // otherwise claim all the remaining height
                    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 40.0), Layout::right_to_left(Align::Center), |ui| {
                        clicked = button(ui, t, a, if tone == Tone::Info { Kind::Primary } else { Kind::Secondary }).clicked();
                    });
                }
            });
        });
    clicked
}

/// Sidebar navigation item. `badge` shows a count/dot on the right.
pub fn nav_item(ui: &mut Ui, t: &Theme, icon: &str, label: &str, selected: bool, badge: Option<(String, Color32)>) -> Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 42.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(radius::CONTROL), mix(t.chrome, t.accent, 0.16));
        p.rect_filled(Rect::from_min_size(rect.left_top() + Vec2::new(0.0, 10.0), Vec2::new(3.0, rect.height() - 20.0)), CornerRadius::same(2), t.accent);
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(radius::CONTROL), mix(t.chrome, t.fg, 0.06));
    }
    let c = if selected { t.fg } else { t.text_dim };
    p.text(Pos2::new(rect.left() + 26.0, rect.center().y), Align2::CENTER_CENTER, icon, font(type_scale::LARGE), if selected { t.accent } else { c });
    p.text(
        Pos2::new(rect.left() + 48.0, rect.center().y),
        Align2::LEFT_CENTER,
        label,
        if selected { font_semibold(type_scale::BODY + 0.5) } else { font_medium(type_scale::BODY + 0.5) },
        c,
    );
    if let Some((txt, col)) = badge {
        let fid = font_semibold(type_scale::SMALL);
        let g = ui.painter().layout_no_wrap(txt, fid, Color32::WHITE);
        let bw = (g.size().x + 12.0).max(20.0);
        let br = Rect::from_center_size(Pos2::new(rect.right() - 12.0 - bw / 2.0, rect.center().y), Vec2::new(bw, 20.0));
        ui.painter().rect_filled(br, CornerRadius::same(radius::PILL), col);
        ui.painter().galley_with_override_text_color(br.center() - g.size() / 2.0, g, contrast(col));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Clickable list row: icon, title, subtitle, trailing text.
pub fn list_row(ui: &mut Ui, t: &Theme, icon: &str, title: &str, subtitle: &str, trailing: &str, selected: bool) -> Response {
    let w = ui.available_width();
    let h = if subtitle.is_empty() { 40.0 } else { 54.0 };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(radius::CONTROL), mix(t.surface, t.accent, 0.14));
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(radius::CONTROL), t.surface_hi);
    }
    let mut x = rect.left() + 12.0;
    if !icon.is_empty() {
        p.text(Pos2::new(x + 8.0, rect.center().y), Align2::CENTER_CENTER, icon, font(type_scale::BODY + 1.0), if selected { t.accent } else { t.text_dim });
        x += 28.0;
    }
    let right = rect.right() - 12.0;
    if !trailing.is_empty() {
        p.text(Pos2::new(right, rect.center().y), Align2::RIGHT_CENTER, trailing, font(type_scale::SMALL + 0.5), t.text_dim);
    }
    if subtitle.is_empty() {
        p.text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, title, font_medium(type_scale::BODY), t.fg);
    } else {
        p.text(Pos2::new(x, rect.center().y - 9.0), Align2::LEFT_CENTER, title, font_medium(type_scale::BODY), t.fg);
        p.text(Pos2::new(x, rect.center().y + 10.0), Align2::LEFT_CENTER, subtitle, font(type_scale::SMALL), t.text_dim);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Label/value row for read-only facts.
pub fn fact(ui: &mut Ui, t: &Theme, label: &str, value: &str) {
    ui.horizontal(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(150.0, 20.0), Layout::left_to_right(Align::Center), |ui| {
            ui.set_min_width(150.0);
            ui.add(egui::Label::new(RichText::new(label).color(t.text_dim)).truncate());
        });
        ui.label(RichText::new(value).color(t.fg));
    });
}

// ---- tiles, pads, media -------------------------------------------------------------------------

/// Pad (preset/scene button) with icon, label, state, optional progress bar (remaining hold).
#[allow(clippy::too_many_arguments)]
pub fn pad(
    ui: &mut Ui,
    t: &Theme,
    size: Vec2,
    icon: &str,
    label: &str,
    color: Option<Color32>,
    state: LedState,
    progress: Option<f32>,
    hint: Option<&str>,
) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    let active = matches!(state, LedState::Active | LedState::Armed);
    let tint = color.unwrap_or(t.accent);
    let base = if active { mix(t.surface, tint, 0.32) } else { t.surface_hi };
    let fill = if resp.is_pointer_button_down_on() {
        mix(base, Color32::BLACK, 0.18)
    } else if resp.hovered() {
        mix(base, t.fg, 0.07)
    } else {
        base
    };
    let r = CornerRadius::same(radius::TILE);
    let edge = match state {
        LedState::Idle => t.border,
        s => led_color(t, s),
    };
    p.rect(rect, r, fill, Stroke::new(if active { 2.0 } else { 1.0 }, edge), StrokeKind::Inside);
    // colored top strip = the preset's own color
    if let Some(c) = color {
        let strip = Rect::from_min_size(rect.min + Vec2::new(10.0, 0.0), Vec2::new(rect.width() - 20.0, 3.0));
        p.rect_filled(strip, CornerRadius::same(2), c);
    }
    let icon_c = if active { t.fg } else { color.unwrap_or(t.text_dim) };
    p.text(rect.center_top() + Vec2::new(0.0, size.y * 0.36), Align2::CENTER_CENTER, icon, font((size.y * 0.24).clamp(14.0, 26.0)), icon_c);
    let lf = font_semibold((size.y * 0.15).clamp(12.0, 15.5));
    let galley = ui.painter().layout(label.to_string(), lf, t.fg, size.x - 12.0);
    let gp = Pos2::new(rect.center().x - galley.size().x / 2.0, rect.bottom() - size.y * 0.2 - galley.size().y / 2.0);
    ui.painter().galley(gp, galley, t.fg);
    if let Some(h) = hint {
        ui.painter().text(rect.left_top() + Vec2::new(8.0, 7.0), Align2::LEFT_TOP, h, font_mono(10.5), t.text_faint);
    }
    if active {
        ui.painter().circle_filled(rect.right_top() + Vec2::new(-10.0, 10.0), 4.0, edge);
    }
    if let Some(pr) = progress {
        let w = rect.width() - 16.0;
        let bar = Rect::from_min_size(rect.left_bottom() + Vec2::new(8.0, -7.0), Vec2::new(w, 3.0));
        ui.painter().rect_filled(bar, CornerRadius::same(2), mix(t.surface, edge, 0.25));
        ui.painter().rect_filled(Rect::from_min_size(bar.min, Vec2::new(w * pr.clamp(0.0, 1.0), 3.0)), CornerRadius::same(2), edge);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Black or white, whichever reads better on `bg`.
pub fn contrast(bg: Color32) -> Color32 {
    let l = 0.2126 * bg.r() as f32 + 0.7152 * bg.g() as f32 + 0.0722 * bg.b() as f32;
    if l > 140.0 { Color32::from_rgb(20, 20, 24) } else { Color32::from_rgb(245, 245, 240) }
}

/// Draw a texture thumbnail with a tally border and label.
pub fn thumbnail(ui: &mut Ui, t: &Theme, size: Vec2, tex: Option<(egui::TextureId, Rect)>, label: &str, tally: Option<LedState>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    video_frame(ui, t, rect, tex, label, tally, None);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Paint a video frame into `rect`: letterboxed texture (or a friendly "no picture" state), a
/// caption chip, and a tally outline. `badge` = (text, color) chip in the top-left corner.
pub fn video_frame(ui: &Ui, t: &Theme, rect: Rect, tex: Option<(egui::TextureId, Rect)>, label: &str, tally: Option<LedState>, badge: Option<(&str, Color32)>) {
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = ui.painter_at(rect.expand(3.0));
    let r = CornerRadius::same(radius::TILE);
    p.rect_filled(rect, r, Color32::from_rgb(8, 9, 11));
    if let Some((id, uv)) = tex {
        p.image(id, rect, uv, Color32::WHITE);
    } else {
        p.text(rect.center() - Vec2::new(0.0, 8.0), Align2::CENTER_CENTER, icon::CAMERA, font(18.0), t.text_faint);
        p.text(rect.center() + Vec2::new(0.0, 14.0), Align2::CENTER_CENTER, "No picture", font(type_scale::SMALL), t.text_faint);
    }
    if !label.is_empty() {
        let fid = font_medium(type_scale::SMALL);
        let g = ui.painter().layout_no_wrap(label.to_string(), fid, Color32::WHITE);
        let chip = Rect::from_min_size(rect.left_bottom() + Vec2::new(8.0, -8.0 - 22.0), Vec2::new(g.size().x + 14.0, 22.0));
        p.rect_filled(chip, CornerRadius::same(6), Color32::from_black_alpha(170));
        p.galley(chip.center() - g.size() / 2.0, g, Color32::WHITE);
    }
    if let Some((txt, c)) = badge {
        let fid = font_bold(type_scale::SMALL);
        let g = ui.painter().layout_no_wrap(txt.to_string(), fid, Color32::WHITE);
        let chip = Rect::from_min_size(rect.left_top() + Vec2::new(8.0, 8.0), Vec2::new(g.size().x + 14.0, 22.0));
        p.rect_filled(chip, CornerRadius::same(6), c);
        p.galley(chip.center() - g.size() / 2.0, g, Color32::WHITE);
    }
    let (w, c) = match tally {
        Some(s @ (LedState::Active | LedState::Armed | LedState::Error)) => (3.0, led_color(t, s)),
        _ => (1.0, t.border),
    };
    p.rect_stroke(rect, r, Stroke::new(w, c), StrokeKind::Outside);
}

// ---- audio & signals ----------------------------------------------------------------------------

/// Vertical level meter (0–1 linear) with a peak line.
pub fn meter(ui: &mut Ui, t: &Theme, size: Vec2, level: f32, peak: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter_at(rect);
    let r = CornerRadius::same(3);
    p.rect_filled(rect, r, t.inset);
    let db = |x: f32| ((20.0 * x.max(1e-6).log10() + 60.0) / 60.0).clamp(0.0, 1.0);
    let h = rect.height() * db(level);
    let c = if level > 0.89 {
        t.bright_red
    } else if level > 0.5 {
        t.yellow
    } else {
        t.green
    };
    p.rect_filled(Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - h), rect.right_bottom()), r, c);
    let py = rect.bottom() - rect.height() * db(peak);
    p.line_segment([Pos2::new(rect.left(), py), Pos2::new(rect.right(), py)], Stroke::new(1.5, t.fg));
    resp
}

/// Horizontal level meter (0–1 linear).
pub fn meter_h(ui: &mut Ui, t: &Theme, size: Vec2, level: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter_at(rect);
    let r = CornerRadius::same(radius::PILL);
    p.rect_filled(rect, r, t.inset);
    let db = ((20.0 * level.max(1e-6).log10() + 60.0) / 60.0).clamp(0.0, 1.0);
    let c = if level > 0.89 {
        t.bright_red
    } else if level > 0.5 {
        t.yellow
    } else {
        t.green
    };
    p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * db, rect.height())), r, c);
    resp
}

/// Vertical fader bound to a 0–1 value. Returns the response (changed when dragged).
pub fn fader(ui: &mut Ui, t: &Theme, size: Vec2, value: &mut f32, modulated: bool) -> Response {
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    if let Some(pos) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.clicked())
    {
        let v = 1.0 - ((pos.y - rect.top()) / rect.height()).clamp(0.0, 1.0);
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            resp.mark_changed();
        }
    }
    let p = ui.painter_at(rect.expand(6.0));
    let track = Rect::from_center_size(rect.center(), Vec2::new(4.0, rect.height()));
    p.rect_filled(track, CornerRadius::same(2), t.inset);
    let y = rect.bottom() - rect.height() * value.clamp(0.0, 1.0);
    let fill = Rect::from_min_max(Pos2::new(track.left(), y), track.right_bottom());
    p.rect_filled(fill, CornerRadius::same(2), if modulated { t.modulated() } else { t.accent });
    let cap = Rect::from_center_size(Pos2::new(rect.center().x, y), Vec2::new(rect.width().min(26.0), 12.0));
    p.rect(cap, CornerRadius::same(4), if resp.dragged() { t.accent } else { t.fg }, Stroke::new(1.0, t.border), StrokeKind::Inside);
    resp.on_hover_cursor(egui::CursorIcon::ResizeVertical)
}

/// Signal scope (line plot of recent values, 0–1 unless `range` given).
pub fn scope(ui: &mut Ui, t: &Theme, size: Vec2, values: &[f32], range: Option<(f32, f32)>, color: Option<Color32>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(radius::CONTROL), t.inset);
    if values.len() >= 2 {
        let (lo, hi) = range.unwrap_or_else(|| {
            let mn = values.iter().copied().fold(f32::INFINITY, f32::min).min(0.0);
            let mx = values.iter().copied().fold(f32::NEG_INFINITY, f32::max).max(1.0);
            (mn, mx)
        });
        let span = (hi - lo).max(1e-6);
        let pts: Vec<Pos2> = values
            .iter()
            .enumerate()
            .map(|(i, v)| {
                Pos2::new(rect.left() + rect.width() * i as f32 / (values.len() - 1) as f32, rect.bottom() - rect.height() * ((v - lo) / span).clamp(0.0, 1.0))
            })
            .collect();
        p.add(egui::Shape::line(pts, Stroke::new(1.5, color.unwrap_or(t.accent))));
    }
    resp
}

/// Frame that marks program-affecting controls with a red edge (§15.1).
pub fn live_edge<R>(ui: &mut Ui, t: &Theme, live: bool, body: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .stroke(Stroke::new(if live { 2.0 } else { 0.0 }, if live { t.tally_program() } else { Color32::TRANSPARENT }))
        .corner_radius(CornerRadius::same(radius::CONTROL))
        .inner_margin(egui::Margin::same(2))
        .show(ui, body)
        .inner
}
