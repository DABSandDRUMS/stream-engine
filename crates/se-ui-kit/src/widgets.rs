//! The component library. Every screen is built from these, so the whole app shares one look:
//! neutral surfaces, clear hierarchy, consistent targets, plain words (§15.1).

use crate::motion;
use crate::theme::{Theme, font, font_bold, font_medium, font_mono, font_semibold, mix, radius, spacing, type_scale};
use egui::{
    Align, Align2, Color32, CornerRadius, CursorIcon, Layout, Pos2, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui, Vec2, WidgetInfo, WidgetType,
};

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
    pub const POWER: &str = "\u{f011}";
    pub const SEARCH: &str = "\u{f002}";
    pub const CONSOLE: &str = "\u{f120}";
    pub const TRACE: &str = "\u{f126}";
    pub const SIM: &str = "\u{f0c3}";
    pub const SCOPE: &str = "\u{f201}";
    pub const HOME: &str = "\u{f015}";
    pub const LAYERS: &str = "\u{f009}";
    pub const WAND: &str = "\u{f0d0}";
    /// Modulation: a setting following a signal.
    pub const WAVE: &str = "\u{f201}";
    pub const TROPHY: &str = "\u{f091}";
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
    ui.painter().circle_filled(rect.center(), 3.5, c);
    resp
}

// ---- buttons -----------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// The one main action on a screen (contrasting neutral fill).
    Primary,
    /// Normal action (subtle outline).
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
            Size::Large => 40.0,
        }
    }
    fn text(self) -> f32 {
        match self {
            Size::Small => type_scale::SMALL,
            Size::Medium => type_scale::BODY,
            Size::Large => type_scale::BODY,
        }
    }
}

/// Semantic (fill, foreground, border), with flat hover and pressed feedback.
fn kind_colors(t: &Theme, kind: Kind, hover: f32, press: f32, enabled: bool) -> (Color32, Color32, Color32) {
    if !enabled {
        let solid = kind != Kind::Ghost;
        return (if solid { t.surface_hi } else { Color32::TRANSPARENT }, t.text_faint, if solid { t.border } else { Color32::TRANSPARENT });
    }
    match kind {
        Kind::Primary => (mix(t.fg, t.bg, 0.1 * hover + 0.08 * press), t.bg, Color32::TRANSPARENT),
        Kind::Secondary => (mix(t.surface, t.fg, 0.04 * hover + 0.035 * press), t.fg, mix(t.border, t.fg, 0.12 * hover + 0.08 * press)),
        Kind::Ghost => (mix(t.surface_hi, t.fg, 0.035 * press).gamma_multiply(hover.max(press)), t.fg, Color32::TRANSPARENT),
        Kind::Danger => (mix(t.surface, t.red, 0.055 + 0.04 * hover + 0.035 * press), mix(t.red, t.fg, 0.4), mix(t.border, t.red, 0.3)),
        Kind::Live => {
            let fill = mix(t.tally_program(), t.bg, 0.06 * hover + 0.06 * press);
            (fill, contrast(fill), Color32::TRANSPARENT)
        }
    }
}

/// Hover and press amounts (0–1, eased) of an interactive widget.
fn hover_press(ui: &Ui, resp: &Response, enabled: bool) -> (f32, f32) {
    let ctx = ui.ctx();
    (
        motion::t(ctx, resp.id.with("hover"), enabled && resp.hovered(), motion::FAST),
        motion::t(ctx, resp.id.with("press"), enabled && resp.is_pointer_button_down_on(), motion::FAST),
    )
}

/// Keyboard focus ring around `rect`. A click never gives a kit control focus, so the ring only
/// shows while someone moves through the screen with Tab or the arrow keys.
pub(crate) fn focus_ring(ui: &Ui, t: &Theme, resp: &Response, rect: Rect, corner: u8) {
    let k = motion::t(ui.ctx(), resp.id.with("focus"), resp.has_focus(), motion::BASE);
    if k > 0.0 {
        ui.painter().rect_stroke(
            rect.expand(2.0),
            CornerRadius::same(corner.saturating_add(2)),
            Stroke::new(1.5, t.accent.gamma_multiply(k)),
            StrokeKind::Middle,
        );
    }
}

/// Pointer cursor over live controls, "not allowed" over disabled ones.
pub(crate) fn cursor(resp: Response, enabled: bool) -> Response {
    if enabled {
        return resp.on_hover_cursor(CursorIcon::PointingHand);
    }
    if resp.contains_pointer() {
        resp.ctx.set_cursor_icon(CursorIcon::NotAllowed);
    }
    resp
}

pub(crate) fn sense(enabled: bool) -> Sense {
    if enabled { Sense::click() } else { Sense::hover() }
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
    button_impl(ui, t, icon, label, label, kind, size, min_width, enabled)
}

/// `a11y`: the words a screen reader (or a UI test) finds the button by.
#[allow(clippy::too_many_arguments)]
fn button_impl(ui: &mut Ui, t: &Theme, icon: Option<&str>, label: &str, a11y: &str, kind: Kind, size: Size, min_width: f32, enabled: bool) -> Response {
    let enabled = enabled && ui.is_enabled();
    let parts = icon_text(ui, icon, label, font_medium(size.text()), t.fg);
    let pad = if size == Size::Small { 10.0 } else { 14.0 };
    let w = (parts.2 + pad * 2.0).max(min_width).max(size.height());
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, size.height()), sense(enabled));
    if ui.is_rect_visible(rect) {
        let (h, p) = hover_press(ui, &resp, enabled);
        let (fill, fg, stroke) = kind_colors(t, kind, h, p, enabled);
        let painter = ui.painter();
        painter.rect(rect, CornerRadius::same(radius::CONTROL), fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
        paint_icon_text(painter, rect.center(), parts, fg);
        focus_ring(ui, t, &resp, rect, radius::CONTROL);
    }
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, a11y));
    cursor(resp, enabled)
}

pub fn button(ui: &mut Ui, t: &Theme, label: &str, kind: Kind) -> Response {
    button_ex(ui, t, None, label, kind, Size::Medium, 0.0, true)
}

pub fn icon_label_button(ui: &mut Ui, t: &Theme, icon: &str, label: &str, kind: Kind) -> Response {
    button_ex(ui, t, Some(icon), label, kind, Size::Medium, 0.0, true)
}

/// Small square icon-only button (tooltip required: icons alone are not self-explanatory).
/// Replaces `ui.small_button(icon)`.
pub fn icon_button(ui: &mut Ui, t: &Theme, icon: &str, tooltip: &str) -> Response {
    icon_button_ex(ui, t, icon, tooltip, Kind::Ghost, true)
}

/// [`icon_button`] with a look (`Secondary` = framed square, `Danger` = delete) and an enabled
/// flag; the tooltip also shows while disabled.
pub fn icon_button_ex(ui: &mut Ui, t: &Theme, icon: &str, tooltip: &str, kind: Kind, enabled: bool) -> Response {
    let r = button_impl(ui, t, Some(icon), "", tooltip, kind, Size::Small, 0.0, enabled);
    r.on_hover_text(tooltip)
}

/// Choice chip (pick one or several from a set: event kinds, filters, lights). Selection has
/// a quiet theme tint and a defined edge, without becoming a primary action.
pub fn chip(ui: &mut Ui, t: &Theme, icon: &str, label: &str, selected: bool) -> Response {
    let enabled = ui.is_enabled();
    let parts = icon_text(ui, Some(icon), label, font_medium(type_scale::SMALL + 0.5), t.fg);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(parts.2 + 24.0, 28.0), sense(enabled));
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Button, enabled, selected, label));
    if ui.is_rect_visible(rect) {
        let (h, p) = hover_press(ui, &resp, enabled);
        let s = motion::t(ui.ctx(), resp.id.with("selected"), selected, motion::BASE);
        let fill = mix(mix(t.surface, t.fg, 0.035 * h + 0.03 * p), t.selection, s);
        let stroke = mix(mix(t.border, t.fg, 0.12 * h), mix(t.border, t.accent, 0.45), s);
        let painter = ui.painter();
        painter.rect(rect, CornerRadius::same(radius::CONTROL), fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
        paint_icon_text(painter, rect.center(), parts, mix(mix(t.text_dim, t.fg, 0.4 + 0.3 * h), t.fg, s));
        focus_ring(ui, t, &resp, rect, radius::CONTROL);
    }
    cursor(resp, enabled)
}

/// Hold-to-confirm button (destructive actions in Show mode, §15.1): a ring fills while you hold
/// the mouse (or Space/Enter when it has keyboard focus); letting go early drains it.
/// Returns true once, when the hold completes.
pub fn hold_button(ui: &mut Ui, t: &Theme, label: &str, color: Color32, hold_secs: f32) -> bool {
    const RING: f32 = 7.0;
    let id = ui.make_persistent_id(("hold", label));
    let enabled = ui.is_enabled();
    let galley = ui.painter().layout_no_wrap(label.to_string(), font_semibold(type_scale::BODY), t.fg);
    let size = Vec2::new(galley.size().x + 32.0 + RING * 2.0 + spacing::S, 36.0);
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click_and_drag() } else { Sense::hover() });
    let now = ui.input(|i| i.time);
    let keys = resp.has_focus() && ui.input(|i| i.key_down(egui::Key::Space) || i.key_down(egui::Key::Enter));
    let held = enabled && (resp.is_pointer_button_down_on() || keys);
    let start: Option<f64> = ui.data(|d| d.get_temp(id));
    let progress = |s: f64| if s.is_infinite() { 1.0 } else { ((now - s) as f32 / hold_secs).clamp(0.0, 1.0) };
    let drain_id = id.with("drain");
    let mut fired = false;
    let prog = if held {
        let s = start.unwrap_or(now);
        if now - s >= hold_secs as f64 {
            fired = true;
            ui.data_mut(|d| d.insert_temp(id, f64::INFINITY));
        } else if start.is_none() {
            ui.data_mut(|d| d.insert_temp(id, s));
        }
        ui.ctx().request_repaint();
        progress(s)
    } else {
        if let Some(s) = start {
            ui.data_mut(|d| {
                d.remove::<f64>(id);
                d.insert_temp(drain_id, (progress(s), now));
            });
        }
        // let go early: the ring drains instead of snapping back to empty
        match ui.data(|d| d.get_temp::<(f32, f64)>(drain_id)) {
            Some((p0, at)) if !motion::reduced(ui.ctx()) && ((now - at) as f32) < motion::SLOW => {
                ui.ctx().request_repaint();
                p0 * (1.0 - motion::ease_out((now - at) as f32 / motion::SLOW))
            }
            Some(_) => {
                ui.data_mut(|d| d.remove::<(f32, f64)>(drain_id));
                0.0
            }
            None => 0.0,
        }
    };
    if ui.is_rect_visible(rect) {
        let (h, p) = hover_press(ui, &resp, enabled);
        let r = rect;
        let cr = CornerRadius::same(radius::CONTROL);
        let painter = ui.painter();
        let base = if enabled { color } else { t.text_faint };
        painter.rect(r, cr, mix(t.surface, base, 0.06 + 0.04 * h + 0.03 * p), Stroke::new(1.0, mix(t.border, base, 0.35)), StrokeKind::Inside);
        if prog > 0.0 {
            painter.rect_filled(Rect::from_min_size(r.min, Vec2::new(r.width() * prog, r.height())), cr, mix(t.surface, base, 0.16));
        }
        // the ring: a faint track that fills clockwise from the top
        let c = Pos2::new(r.left() + 16.0 + RING, r.center().y);
        let ring_c = base;
        painter.circle_stroke(c, RING, Stroke::new(2.0, ring_c.gamma_multiply(0.35)));
        if prog > 0.0 {
            let n = (prog * 32.0).ceil().max(2.0) as usize;
            let pts: Vec<Pos2> = (0..=n)
                .map(|i| {
                    let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * prog * i as f32 / n as f32;
                    c + RING * Vec2::angled(a)
                })
                .collect();
            painter.add(egui::Shape::line(pts, Stroke::new(2.0, ring_c)));
        }
        let fg = if enabled { t.fg } else { t.text_faint };
        let gp = Pos2::new(c.x + RING + spacing::S, r.center().y - galley.size().y / 2.0);
        painter.galley_with_override_text_color(gp, galley, fg);
        focus_ring(ui, t, &resp, rect, radius::CONTROL);
    }
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, enabled, label));
    cursor(resp, enabled).on_hover_text(format!("Press and hold to {}", label.to_lowercase()));
    fired
}

// ---- toggles, tabs, inputs -----------------------------------------------------------------------

/// On/off switch: a fixed round thumb slides between neutral tracks. Press feedback stays flat.
/// Returns the response (`changed()` when flipped).
pub fn toggle(ui: &mut Ui, t: &Theme, on: &mut bool) -> Response {
    let enabled = ui.is_enabled();
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(40.0, 22.0), sense(enabled));
    if resp.clicked() {
        *on = !*on;
        resp.mark_changed();
    }
    if ui.is_rect_visible(rect) {
        let k = motion::t(ui.ctx(), resp.id.with("on"), *on, motion::SLOW);
        let (h, p) = hover_press(ui, &resp, enabled);
        let painter = ui.painter();
        let track = mix(mix(t.border, t.fg, 0.04 * h + 0.03 * p), mix(t.fg, t.bg, 0.08 * h + 0.06 * p), k);
        painter.rect_filled(rect, CornerRadius::same(radius::PILL), track);
        let x = egui::lerp(rect.left() + 11.0..=rect.right() - 11.0, k);
        let knob = Rect::from_center_size(Pos2::new(x, rect.center().y), Vec2::splat(16.0));
        painter.rect_filled(knob, CornerRadius::same(radius::PILL), mix(if t.light { Color32::WHITE } else { t.fg }, t.bg, k));
        focus_ring(ui, t, &resp, rect, radius::PILL);
    }
    resp.widget_info(|| WidgetInfo::selected(WidgetType::Checkbox, enabled, *on, ""));
    cursor(resp, enabled)
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

/// Underlined tabs; the underline glides to the picked tab. Returns true when the selection
/// changed.
pub fn tabs(ui: &mut Ui, t: &Theme, selected: &mut usize, labels: &[&str]) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = spacing::XL;
        let x0 = ui.max_rect().left();
        let mut bar = None;
        for (i, label) in labels.iter().enumerate() {
            let on = *selected == i;
            let fid = font_medium(type_scale::BODY);
            let galley = ui.painter().layout_no_wrap(label.to_string(), fid, t.fg);
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(galley.size().x, 34.0), Sense::click());
            let h = motion::t(ui.ctx(), resp.id.with("hover"), resp.hovered(), motion::FAST);
            let s = motion::t(ui.ctx(), resp.id.with("selected"), on, motion::BASE);
            let c = mix(mix(t.text_dim, t.fg, 0.5 * h), t.fg, s);
            ui.painter().galley_with_override_text_color(Pos2::new(rect.left(), rect.center().y - galley.size().y / 2.0 - 2.0), galley, c);
            if on {
                bar = Some(rect);
            }
            focus_ring(ui, t, &resp, rect.expand2(Vec2::new(6.0, -2.0)), radius::CONTROL);
            if cursor(resp, true).clicked() && !on {
                *selected = i;
                changed = true;
            }
        }
        if let Some(r) = bar {
            let id = ui.id().with("underline");
            let l = x0 + motion::value(ui.ctx(), id.with(0), r.left() - x0, motion::SLOW);
            let rr = x0 + motion::value(ui.ctx(), id.with(1), r.right() - x0, motion::SLOW);
            ui.painter().rect_filled(Rect::from_min_max(Pos2::new(l, r.bottom() - 2.0), Pos2::new(rr, r.bottom())), CornerRadius::same(1), t.accent);
        }
    });
    let r = ui.min_rect();
    ui.painter().hline(r.x_range(), ui.cursor().top(), Stroke::new(1.0, t.border));
    ui.add_space(spacing::M);
    changed
}

/// Segmented control (small choice between 2–4 options); the selected surface glides to the pick.
/// Returns true when changed.
pub fn segmented(ui: &mut Ui, t: &Theme, selected: &mut usize, labels: &[&str]) -> bool {
    let mut changed = false;
    let enabled = ui.is_enabled();
    let fid = font_medium(type_scale::SMALL + 0.5);
    let widths: Vec<f32> = labels.iter().map(|l| ui.painter().layout_no_wrap(l.to_string(), fid.clone(), t.fg).size().x + 24.0).collect();
    let total: f32 = widths.iter().sum::<f32>() + 6.0;
    // one auto id per control, so several segmented controls with the same labels can share a parent
    let base = ui.next_auto_id();
    ui.skip_ahead_auto_ids(1);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(total, 32.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(radius::CONTROL), t.surface_hi);
    if let Some(w) = widths.get(*selected) {
        let x = motion::value(ui.ctx(), base.with("pill-x"), widths[..*selected].iter().sum(), motion::SLOW);
        let pw = motion::value(ui.ctx(), base.with("pill-w"), *w, motion::SLOW);
        let pill = Rect::from_min_size(Pos2::new(rect.left() + 3.0 + x, rect.top() + 3.0), Vec2::new(pw, rect.height() - 6.0));
        ui.painter().rect_filled(pill, CornerRadius::same(radius::CONTROL - 2), t.surface);
    }
    let mut x = rect.left() + 3.0;
    for (i, (label, w)) in labels.iter().zip(&widths).enumerate() {
        let r = Rect::from_min_size(Pos2::new(x, rect.top() + 3.0), Vec2::new(*w, rect.height() - 6.0));
        let resp = ui.interact(r, base.with(i), sense(enabled));
        let on = *selected == i;
        let h = motion::t(ui.ctx(), resp.id.with("hover"), enabled && resp.hovered() && !on, motion::FAST);
        let s = motion::t(ui.ctx(), resp.id.with("selected"), on, motion::BASE);
        if h > 0.0 {
            ui.painter().rect_filled(r, CornerRadius::same(radius::CONTROL - 2), t.fg.gamma_multiply(0.05 * h));
        }
        ui.painter().text(r.center(), Align2::CENTER_CENTER, *label, fid.clone(), mix(mix(t.text_dim, t.fg, 0.45 * h), t.fg, s));
        focus_ring(ui, t, &resp, r, radius::CONTROL - 2);
        if cursor(resp, enabled).clicked() && !on {
            *selected = i;
            changed = true;
        }
        x += w;
    }
    changed
}

/// The app's top-level switch (Overview / Edit / Clipping): icon + label per tab in one
/// muted group with a flat selected surface. `badges` may put a small count on a tab.
/// Returns the clicked tab (only when it changed).
pub fn master_tabs(ui: &mut Ui, t: &Theme, selected: usize, tabs: &[(&str, &str, Option<(String, Color32)>)]) -> Option<usize> {
    let fid = font_medium(type_scale::BODY);
    let parts: Vec<_> = tabs.iter().map(|(ic, label, _)| icon_text(ui, Some(ic), label, fid.clone(), t.fg)).collect();
    let badge_w = |b: &Option<(String, Color32)>| b.as_ref().map_or(0.0, |(s, _)| 10.0 + 8.0 * s.chars().count() as f32 + 12.0);
    let widths: Vec<f32> = parts.iter().zip(tabs).map(|(p, (_, _, b))| p.2 + 28.0 + badge_w(b)).collect();
    let total: f32 = widths.iter().sum::<f32>() + 8.0;
    let base = ui.next_auto_id();
    ui.skip_ahead_auto_ids(1);
    let (rect, _) = ui.allocate_exact_size(Vec2::new(total, 36.0), Sense::hover());
    ui.painter().rect_filled(rect, CornerRadius::same(radius::CONTROL), t.surface_hi);
    if let Some(w) = widths.get(selected) {
        let x = motion::value(ui.ctx(), base.with("pill-x"), widths[..selected].iter().sum(), motion::SLOW);
        let pw = motion::value(ui.ctx(), base.with("pill-w"), *w, motion::SLOW);
        let pill = Rect::from_min_size(Pos2::new(rect.left() + 4.0 + x, rect.top() + 4.0), Vec2::new(pw, rect.height() - 8.0));
        ui.painter().rect_filled(pill, CornerRadius::same(radius::CONTROL - 2), t.surface);
    }
    let mut x = rect.left() + 4.0;
    let mut clicked = None;
    for (i, (((_, label, badge), w), part)) in tabs.iter().zip(&widths).zip(parts).enumerate() {
        let r = Rect::from_min_size(Pos2::new(x, rect.top() + 4.0), Vec2::new(*w, rect.height() - 8.0));
        let resp = ui.interact(r, base.with(i), Sense::click());
        resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, true, *label));
        let on = selected == i;
        let h = motion::t(ui.ctx(), resp.id.with("hover"), resp.hovered() && !on, motion::FAST);
        let s = motion::t(ui.ctx(), resp.id.with("selected"), on, motion::BASE);
        if h > 0.0 {
            ui.painter().rect_filled(r, CornerRadius::same(radius::CONTROL), t.surface.gamma_multiply(h));
        }
        let content_w = r.width() - badge_w(badge);
        paint_icon_text(ui.painter(), Pos2::new(r.left() + content_w / 2.0, r.center().y), part, mix(mix(t.text_dim, t.fg, 0.45 * h), t.fg, s));
        if let Some((txt, bc)) = badge {
            let bw = badge_w(badge) - 12.0;
            let br = Rect::from_center_size(Pos2::new(r.left() + content_w + bw / 2.0 - 6.0, r.center().y), Vec2::new(bw, 20.0));
            ui.painter().rect_filled(br, CornerRadius::same(4), mix(t.surface_hi, *bc, 0.08));
            ui.painter().text(br.center(), Align2::CENTER_CENTER, txt, font_medium(type_scale::SMALL), mix(*bc, t.fg, 0.55));
        }
        focus_ring(ui, t, &resp, r, radius::CONTROL);
        if cursor(resp, true).clicked() && !on {
            clicked = Some(i);
        }
        x += w;
    }
    clicked
}

/// Horizontal slider with a label above and the value on the right.
pub fn labeled_slider(ui: &mut Ui, t: &Theme, label: &str, value: &mut f32, range: std::ops::RangeInclusive<f32>, suffix: &str) -> Response {
    // 0–1 ranges read as percentages; everything else as whole numbers with the unit
    let pct = *range.start() >= 0.0 && *range.end() <= 1.0;
    let fmt = move |v: f64| if pct { format!("{:.0}%", v * 100.0) } else { format!("{v:.0}{suffix}") };
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new(label).font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(fmt(*value as f64)).font(font_mono(type_scale::SMALL)).color(t.fg));
            });
        });
        ui.spacing_mut().slider_width = ui.available_width();
        let _ = fmt;
        ui.add(egui::Slider::new(value, range).show_value(false))
    })
    .inner
}

// ---- layout: pages, cards, headers --------------------------------------------------------------

/// Page title + one-line explanation, with room for actions on the right.
pub fn page_header(ui: &mut Ui, t: &Theme, title: &str, subtitle: &str, actions: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).font(font_semibold(type_scale::TITLE)).color(t.fg));
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
                ui.label(RichText::new(icon).size(type_scale::LARGE).color(t.text_dim));
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

/// Compact section heading inside a page or card.
pub fn section(ui: &mut Ui, t: &Theme, icon: &str, title: &str) {
    ui.add_space(spacing::XS);
    ui.horizontal(|ui| {
        if !icon.is_empty() {
            ui.label(RichText::new(icon).size(type_scale::SMALL + 1.0).color(t.text_dim));
        }
        ui.label(RichText::new(title).font(font_medium(type_scale::SMALL)).color(t.text_dim));
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
    focus_ring(ui, t, &resp, rect, radius::CONTROL);
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

// ---- list/detail and inspector grammar ----------------------------------------------------------
//
// Every editing surface follows one shape: a list pane (what exists, one "+" to create) and a
// detail pane (the selected thing, as collapsible inspector sections of label/control rows).

/// Width of the label column in [`prop_row`], so controls line up down an inspector.
pub const PROP_LABEL_W: f32 = 112.0;

/// Two full-height columns: a fixed-width list pane and a detail pane that takes the rest.
pub fn split<L, R>(ui: &mut Ui, list_w: f32, left: impl FnOnce(&mut Ui) -> L, right: impl FnOnce(&mut Ui) -> R) -> (L, R) {
    let h = ui.available_height();
    let gap = spacing::L;
    let w = ui.available_width();
    let list_w = list_w.min((w - gap) * 0.5).max(0.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        let l = ui.allocate_ui_with_layout(Vec2::new(list_w, h), Layout::top_down(Align::Min), |ui| {
            ui.set_width(list_w);
            left(ui)
        });
        let r = ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), h), Layout::top_down(Align::Min), |ui| {
            ui.set_width(ui.available_width());
            right(ui)
        });
        (l.inner, r.inner)
    })
    .inner
}

/// List pane heading: title, optional count, and one "+" create button. Returns true on click.
pub fn pane_header(ui: &mut Ui, t: &Theme, title: &str, count: Option<usize>, create: Option<&str>) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        ui.set_min_height(28.0);
        ui.label(RichText::new(title).font(font_semibold(type_scale::BODY)).color(t.fg));
        if let Some(n) = count {
            ui.label(RichText::new(n.to_string()).size(type_scale::SMALL).color(t.text_faint));
        }
        if let Some(tip) = create {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                clicked = icon_button_ex(ui, t, icon::PLUS, tip, Kind::Secondary, ui.is_enabled()).clicked();
            });
        }
    });
    ui.add_space(spacing::S);
    clicked
}

/// Small uppercase group label inside a list pane ("CAMERAS", "ON EVERY SCENE").
pub fn group_label(ui: &mut Ui, t: &Theme, text: &str) {
    ui.add_space(spacing::S);
    ui.label(RichText::new(text.to_uppercase()).font(font_medium(type_scale::SMALL - 1.0)).color(t.text_faint).extra_letter_spacing(0.6));
    ui.add_space(spacing::XS);
}

/// Detail pane title block: kind icon, title, one-line kind/description, actions on the right.
pub fn detail_header(ui: &mut Ui, t: &Theme, icon: &str, title: &str, subtitle: &str, actions: impl FnOnce(&mut Ui)) {
    ui.horizontal(|ui| {
        let (tile, _) = ui.allocate_exact_size(Vec2::splat(36.0), Sense::hover());
        ui.painter().rect(tile, CornerRadius::same(radius::CONTROL), t.surface_hi, Stroke::new(1.0, t.border), StrokeKind::Inside);
        ui.painter().text(tile.center(), Align2::CENTER_CENTER, icon, font(16.0), t.fg);
        ui.add_space(spacing::S);
        ui.vertical(|ui| {
            ui.label(RichText::new(title).font(font_semibold(type_scale::HEADING)).color(t.fg));
            if !subtitle.is_empty() {
                ui.label(RichText::new(subtitle).size(type_scale::SMALL + 0.5).color(t.text_dim));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), actions);
    });
    ui.add_space(spacing::M);
}

/// Collapsible inspector section: chevron + label, optional trailing controls (e.g. "+ Add"),
/// body indented under a hairline. Open state persists per `id_salt`. Returns the body result
/// while open.
pub fn inspector_section<R>(
    ui: &mut Ui,
    t: &Theme,
    id_salt: impl std::hash::Hash + std::fmt::Debug,
    title: &str,
    default_open: bool,
    trailing: impl FnOnce(&mut Ui),
    body: impl FnOnce(&mut Ui) -> R,
) -> Option<R> {
    let id = ui.make_persistent_id(("inspector_section", id_salt));
    let mut open: bool = ui.data(|d| d.get_temp(id)).unwrap_or(default_open);
    let top = ui.cursor().top();
    ui.painter().hline(ui.max_rect().x_range(), top, Stroke::new(1.0, t.border));
    ui.add_space(spacing::S);
    ui.horizontal(|ui| {
        ui.set_min_height(28.0);
        let text = format!("{}  {}", if open { icon::DOWN } else { icon::RIGHT }, title);
        let g = ui.painter().layout_no_wrap(text, font_semibold(type_scale::SMALL + 0.5), t.fg);
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(g.size().x + 8.0, 28.0), Sense::click());
        let c = if resp.hovered() { t.fg } else { mix(t.fg, t.text_dim, 0.35) };
        ui.painter().galley_with_override_text_color(Pos2::new(rect.left(), rect.center().y - g.size().y / 2.0), g, c);
        focus_ring(ui, t, &resp, rect, radius::CONTROL);
        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            open = !open;
            ui.data_mut(|d| d.insert_temp(id, open));
        }
        ui.with_layout(Layout::right_to_left(Align::Center), trailing);
    });
    if !open {
        ui.add_space(spacing::XS);
        return None;
    }
    ui.add_space(spacing::XS);
    let r = body(ui);
    ui.add_space(spacing::M);
    Some(r)
}

/// One inspector property: a fixed-width dim label, then the control(s) filling the row.
pub fn prop_row<R>(ui: &mut Ui, t: &Theme, label: &str, body: impl FnOnce(&mut Ui) -> R) -> R {
    ui.horizontal(|ui| {
        ui.set_min_height(30.0);
        ui.allocate_ui_with_layout(Vec2::new(PROP_LABEL_W, 30.0), Layout::left_to_right(Align::Center), |ui| {
            ui.set_min_width(PROP_LABEL_W);
            ui.add(egui::Label::new(RichText::new(label).size(type_scale::SMALL + 0.5).color(t.text_dim)).truncate());
        });
        body(ui)
    })
    .inner
}

/// Selectable tile for "what kind of thing?" choosers (new source, new trigger). Fixed size so a
/// row of them reads as one set.
pub fn kind_tile(ui: &mut Ui, t: &Theme, icon: &str, title: &str, body: &str, selected: bool) -> Response {
    let size = Vec2::new(220.0, 92.0);
    let enabled = ui.is_enabled();
    let (rect, resp) = ui.allocate_exact_size(size, sense(enabled));
    let (hover, _) = hover_press(ui, &resp, enabled);
    let fill = if selected { mix(t.surface, t.accent, 0.12) } else { mix(t.surface, t.surface_hi, hover) };
    let stroke = if selected { t.accent } else { mix(t.border, t.text_faint, hover) };
    let p = ui.painter();
    p.rect(rect, CornerRadius::same(radius::CARD), fill, Stroke::new(1.0, stroke), StrokeKind::Inside);
    p.text(rect.left_top() + Vec2::new(14.0, 14.0), Align2::LEFT_TOP, icon, font(16.0), if selected { t.accent } else { t.fg });
    p.text(rect.left_top() + Vec2::new(40.0, 14.0), Align2::LEFT_TOP, title, font_semibold(type_scale::BODY), t.fg);
    let g = p.layout(body.to_string(), font(type_scale::SMALL), t.text_dim, size.x - 28.0);
    p.galley(rect.left_top() + Vec2::new(14.0, 42.0), g, t.text_dim);
    focus_ring(ui, t, &resp, rect, radius::CARD);
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Dim helper text.
pub fn hint(ui: &mut Ui, t: &Theme, text: &str) -> Response {
    ui.label(RichText::new(text).size(type_scale::SMALL + 0.5).color(t.text_dim))
}

/// Quiet setup empty state: an outlined icon tile, clear heading, supporting text, one action.
pub fn empty_state(ui: &mut Ui, t: &Theme, icon: &str, title: &str, body: &str, action: Option<&str>) -> bool {
    let mut clicked = false;
    ui.vertical_centered(|ui| {
        ui.add_space(spacing::XL);
        let (tile, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
        ui.painter().rect(tile, CornerRadius::same(radius::CARD), t.surface, Stroke::new(1.0, t.border), StrokeKind::Inside);
        ui.painter().text(tile.center(), Align2::CENTER_CENTER, icon, font(18.0), t.text_dim);
        ui.add_space(spacing::L);
        ui.label(RichText::new(title).font(font_semibold(type_scale::LARGE)).color(t.fg));
        if !body.is_empty() {
            ui.add_space(2.0);
            ui.add(egui::Label::new(RichText::new(body).size(type_scale::BODY).color(t.text_dim)).wrap());
        }
        if let Some(a) = action {
            ui.add_space(spacing::L);
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
    p.rect(rect, CornerRadius::same(radius::CONTROL), mix(t.surface, color, 0.06), Stroke::new(1.0, mix(t.border, color, 0.12)), StrokeKind::Inside);
    p.circle_filled(Pos2::new(rect.left() + 11.0, rect.center().y), 3.0, color);
    p.galley_with_override_text_color(Pos2::new(rect.left() + 20.0, rect.center().y - galley.size().y / 2.0), galley, mix(color, t.fg, 0.6));
    resp
}

/// Status pill: icon + text colored by health.
pub fn pill(ui: &mut Ui, t: &Theme, icon: &str, text: &str, s: LedState) -> Response {
    let c = if s == LedState::Idle { t.text_dim } else { mix(led_color(t, s), t.fg, 0.4) };
    let parts = icon_text(ui, Some(icon), text, font_medium(type_scale::SMALL + 0.5), c);
    let size = Vec2::new(parts.2 + 24.0, 28.0);
    let (rect, resp) = ui.allocate_exact_size(size, sense(ui.is_enabled()));
    let (h, press) = hover_press(ui, &resp, ui.is_enabled());
    let fill = mix(t.surface, t.fg, 0.035 * h + 0.03 * press);
    ui.painter().rect(rect, CornerRadius::same(radius::CONTROL), fill, Stroke::new(1.0, t.border), StrokeKind::Inside);
    paint_icon_text(ui.painter(), rect.center(), parts, c);
    focus_ring(ui, t, &resp, rect, radius::CONTROL);
    cursor(resp, ui.is_enabled())
}

/// A single-line text field with comfortable padding and the standard control height. Use it
/// for every one-line input so fields line up with the buttons next to them.
pub fn field<'t>(text: &'t mut dyn egui::TextBuffer) -> egui::TextEdit<'t> {
    egui::TextEdit::singleline(text).margin(egui::Margin::symmetric(12, 8)).min_size(Vec2::new(0.0, 36.0)).vertical_align(Align::Center)
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
        Tone::Info => t.text_dim,
        Tone::Warn => t.yellow,
        Tone::Danger => t.bright_red,
        Tone::Ok => t.green,
    };
    let mut clicked = false;
    egui::Frame::new()
        .fill(mix(t.surface, c, if tone == Tone::Info { 0.0 } else { 0.035 }))
        .stroke(Stroke::new(1.0, mix(t.border, c, if tone == Tone::Info { 0.0 } else { 0.18 })))
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
                        clicked = button(ui, t, a, Kind::Secondary).clicked();
                    });
                }
            });
        });
    clicked
}

/// Sidebar navigation item. `badge` shows a count/dot on the right.
pub fn nav_item(ui: &mut Ui, t: &Theme, icon: &str, label: &str, selected: bool, badge: Option<(String, Color32)>) -> Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 36.0), sense(ui.is_enabled()));
    let (hover, press) = hover_press(ui, &resp, ui.is_enabled());
    let sel = motion::t(ui.ctx(), resp.id.with("selected"), selected, motion::SLOW);
    let p = ui.painter();
    if sel > 0.0 || hover > 0.0 {
        p.rect_filled(rect, CornerRadius::same(radius::CONTROL), mix(t.chrome, t.fg, 0.055 * sel + 0.035 * hover + 0.025 * press));
    }
    if sel > 0.0 {
        p.rect_filled(
            Rect::from_min_size(rect.left_top() + Vec2::new(0.0, 9.0 + (1.0 - sel) * 9.0), Vec2::new(2.0, (rect.height() - 18.0) * sel)),
            CornerRadius::same(2),
            t.accent.gamma_multiply(sel),
        );
    }
    let c = mix(t.text_dim, t.fg, (sel + 0.5 * hover).min(1.0));
    let y = rect.center().y;
    p.text(Pos2::new(rect.left() + 20.0, y), Align2::CENTER_CENTER, icon, font(type_scale::BODY), c);
    p.text(Pos2::new(rect.left() + 40.0, y), Align2::LEFT_CENTER, label, font_medium(type_scale::BODY), c);
    if let Some((txt, col)) = badge {
        let fid = font_medium(type_scale::SMALL);
        let g = ui.painter().layout_no_wrap(txt, fid, t.text_dim);
        let bw = (g.size().x + 12.0).max(20.0);
        let br = Rect::from_center_size(Pos2::new(rect.right() - 12.0 - bw / 2.0, rect.center().y), Vec2::new(bw, 20.0));
        ui.painter().rect_filled(br, CornerRadius::same(4), mix(t.surface_hi, col, 0.08));
        ui.painter().galley_with_override_text_color(br.center() - g.size() / 2.0, g, mix(col, t.fg, 0.55));
    }
    focus_ring(ui, t, &resp, rect, radius::CONTROL);
    cursor(resp, ui.is_enabled())
}

/// Clickable list row: icon, title, subtitle, trailing text.
pub fn list_row(ui: &mut Ui, t: &Theme, icon: &str, title: &str, subtitle: &str, trailing: &str, selected: bool) -> Response {
    let w = ui.available_width();
    let h = if subtitle.is_empty() { 40.0 } else { 54.0 };
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, h), sense(ui.is_enabled()));
    resp.widget_info(|| WidgetInfo::labeled(WidgetType::Button, ui.is_enabled(), title));
    let (hover, press) = hover_press(ui, &resp, ui.is_enabled());
    let sel = motion::t(ui.ctx(), resp.id.with("selected"), selected, motion::SLOW);
    let p = ui.painter();
    if sel > 0.0 || hover > 0.0 {
        p.rect_filled(rect, CornerRadius::same(radius::CONTROL), mix(mix(t.surface, t.fg, 0.035 * hover + 0.03 * press), t.selection, sel));
    }
    let mut x = rect.left() + 12.0;
    if !icon.is_empty() {
        p.text(Pos2::new(x + 8.0, rect.center().y), Align2::CENTER_CENTER, icon, font(type_scale::BODY), mix(t.text_dim, t.fg, sel));
        x += 28.0;
    }
    let right = rect.right() - 12.0;
    if !trailing.is_empty() {
        p.text(Pos2::new(right, rect.center().y), Align2::RIGHT_CENTER, trailing, font(type_scale::SMALL), t.text_dim);
    }
    if subtitle.is_empty() {
        p.text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, title, font_medium(type_scale::BODY), t.fg);
    } else {
        p.text(Pos2::new(x, rect.center().y - 9.0), Align2::LEFT_CENTER, title, font_medium(type_scale::BODY), t.fg);
        p.text(Pos2::new(x, rect.center().y + 10.0), Align2::LEFT_CENTER, subtitle, font(type_scale::SMALL), t.text_dim);
    }
    focus_ring(ui, t, &resp, rect, radius::CONTROL);
    cursor(resp, ui.is_enabled())
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
    let (rect, resp) = ui.allocate_exact_size(size, sense(ui.is_enabled()));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    let active = matches!(state, LedState::Active | LedState::Armed);
    let tint = color.unwrap_or(t.accent);
    let (hover, press) = hover_press(ui, &resp, ui.is_enabled());
    let lit = motion::t(ui.ctx(), resp.id.with("active"), active, motion::SLOW);
    let fill = mix(mix(t.surface, tint, 0.08 * lit), t.fg, 0.035 * hover + 0.03 * press);
    let r = CornerRadius::same(radius::TILE);
    let edge = mix(
        t.border,
        match state {
            LedState::Idle => tint,
            s => led_color(t, s),
        },
        lit,
    );
    p.rect(rect, r, fill, Stroke::new(1.0, edge), StrokeKind::Inside);
    // A small color key identifies the user's preset without saturating the whole pad.
    if let Some(c) = color {
        let strip = Rect::from_min_size(rect.min + Vec2::new(12.0, rect.height() - 3.0), Vec2::new(rect.width() - 24.0, 2.0));
        p.rect_filled(strip, CornerRadius::same(1), c);
    }
    let icon_c = if active { t.fg } else { t.text_dim };
    p.text(rect.center_top() + Vec2::new(0.0, size.y * 0.36), Align2::CENTER_CENTER, icon, font((size.y * 0.2).clamp(14.0, 22.0)), icon_c);
    let lf = font_medium((size.y * 0.15).clamp(12.0, 14.0));
    let galley = ui.painter().layout(label.to_string(), lf, t.fg, size.x - 16.0);
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
    focus_ring(ui, t, &resp, rect, radius::TILE);
    cursor(resp, ui.is_enabled())
}

/// Black or white, whichever has the greater WCAG contrast on `bg`.
pub fn contrast(bg: Color32) -> Color32 {
    crate::theme::contrasting_foreground(bg)
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
    p.rect_filled(rect, r, if tex.is_some() { Color32::from_rgb(8, 9, 11) } else { t.inset });
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
        p.galley_with_override_text_color(chip.center() - g.size() / 2.0, g, contrast(c));
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
