//! Custom widgets: every "thing" is a card/pad with a kind icon, color, and state LED (§15.1).

use crate::theme::Theme;
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2};

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
    pub const BOT: &str = "\u{f544}";
    pub const ALERT: &str = "\u{f0f3}";
    pub const SESSION: &str = "\u{f02d}";
    pub const PERF: &str = "\u{f0e4}";
    pub const SETTINGS: &str = "\u{f013}";
    pub const CHECK: &str = "\u{f00c}";
    pub const CROSS: &str = "\u{f00d}";
    pub const PLAY: &str = "\u{f04b}";
    pub const STOP: &str = "\u{f04d}";
    pub const SEARCH: &str = "\u{f002}";
    pub const CONSOLE: &str = "\u{f120}";
    pub const TRACE: &str = "\u{f126}";
    pub const SIM: &str = "\u{f0c3}";
    pub const SCOPE: &str = "\u{f201}";
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
        LedState::Idle => t.muted,
        LedState::Armed => t.tally_preview(),
        LedState::Active => t.tally_program(),
        LedState::Error => t.bright_red,
        LedState::Healthy => t.healthy(),
        LedState::Modulated => t.modulated(),
    }
}

/// A small status LED.
pub fn led(ui: &mut Ui, t: &Theme, s: LedState) -> Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(10.0), Sense::hover());
    let c = led_color(t, s);
    ui.painter().circle_filled(rect.center(), 4.0, c);
    if s != LedState::Idle {
        ui.painter().circle_stroke(rect.center(), 5.0, Stroke::new(1.0, c.gamma_multiply(0.5)));
    }
    resp
}

/// Pad (preset/scene button) with icon, label, state, optional progress ring (remaining hold).
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
    let p = ui.painter_at(rect.expand(2.0));
    let base = color.unwrap_or(t.bg_light);
    let fill = if resp.is_pointer_button_down_on() {
        base.gamma_multiply(0.7)
    } else if resp.hovered() {
        base.gamma_multiply(1.15)
    } else {
        base
    };
    p.rect_filled(rect, CornerRadius::same(6), fill);
    let edge = match state {
        LedState::Idle => t.muted,
        s => led_color(t, s),
    };
    p.rect_stroke(rect, CornerRadius::same(6), Stroke::new(if state == LedState::Idle { 1.0 } else { 2.5 }, edge), StrokeKind::Inside);
    let text = if color.is_some() { contrast(base) } else { t.fg };
    p.text(rect.center_top() + Vec2::new(0.0, size.y * 0.32), Align2::CENTER_CENTER, icon, FontId::proportional(size.y * 0.24), text);
    p.text(rect.center_bottom() - Vec2::new(0.0, size.y * 0.26), Align2::CENTER_CENTER, label, FontId::proportional((size.y * 0.16).clamp(10.0, 16.0)), text);
    p.circle_filled(rect.right_top() + Vec2::new(-8.0, 8.0), 3.5, led_color(t, state));
    if let Some(h) = hint {
        p.text(rect.left_top() + Vec2::new(6.0, 5.0), Align2::LEFT_TOP, h, FontId::proportional(10.0), text.gamma_multiply(0.7));
    }
    if let Some(pr) = progress {
        let w = rect.width() - 8.0;
        let bar = Rect::from_min_size(rect.left_bottom() + Vec2::new(4.0, -6.0), Vec2::new(w * pr.clamp(0.0, 1.0), 3.0));
        p.rect_filled(bar, CornerRadius::same(1), edge);
    }
    resp
}

/// Black or white, whichever reads better on `bg`.
pub fn contrast(bg: Color32) -> Color32 {
    let l = 0.2126 * bg.r() as f32 + 0.7152 * bg.g() as f32 + 0.0722 * bg.b() as f32;
    if l > 140.0 { Color32::from_rgb(20, 20, 24) } else { Color32::from_rgb(240, 240, 235) }
}

/// Vertical level meter (0–1 linear) with a peak line.
pub fn meter(ui: &mut Ui, t: &Theme, size: Vec2, level: f32, peak: f32) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(2), t.bg_darker);
    let db = |x: f32| ((20.0 * x.max(1e-6).log10() + 60.0) / 60.0).clamp(0.0, 1.0);
    let h = rect.height() * db(level);
    let c = if level > 0.89 {
        t.bright_red
    } else if level > 0.5 {
        t.yellow
    } else {
        t.green
    };
    p.rect_filled(Rect::from_min_max(Pos2::new(rect.left(), rect.bottom() - h), rect.right_bottom()), CornerRadius::same(2), c);
    let py = rect.bottom() - rect.height() * db(peak);
    p.line_segment([Pos2::new(rect.left(), py), Pos2::new(rect.right(), py)], Stroke::new(1.5, t.fg));
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
    let p = ui.painter_at(rect.expand(4.0));
    let track = Rect::from_center_size(rect.center(), Vec2::new(4.0, rect.height()));
    p.rect_filled(track, CornerRadius::same(2), t.bg_darker);
    let y = rect.bottom() - rect.height() * value.clamp(0.0, 1.0);
    let fill = Rect::from_min_max(Pos2::new(track.left(), y), track.right_bottom());
    p.rect_filled(fill, CornerRadius::same(2), if modulated { t.modulated() } else { t.accent });
    let cap = Rect::from_center_size(Pos2::new(rect.center().x, y), Vec2::new(rect.width(), 10.0));
    p.rect_filled(cap, CornerRadius::same(3), if resp.dragged() { t.accent } else { t.fg_dim });
    resp
}

/// Hold-to-confirm button (destructive actions in Show mode, §15.1).
/// Returns true once, when the hold completes.
pub fn hold_button(ui: &mut Ui, t: &Theme, label: &str, color: Color32, hold_secs: f32) -> bool {
    let id = ui.make_persistent_id(("hold", label));
    let size = Vec2::new(ui.fonts_mut(|f| f.layout_no_wrap(label.into(), FontId::proportional(13.0), t.fg).size().x) + 24.0, 24.0);
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
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(4), t.bg_light);
    p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * prog, rect.height())), CornerRadius::same(4), color.gamma_multiply(0.8));
    p.rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.5, color), StrokeKind::Inside);
    p.text(rect.center(), Align2::CENTER_CENTER, label, FontId::proportional(13.0), t.fg);
    fired
}

/// Status pill: icon + text colored by health.
pub fn pill(ui: &mut Ui, t: &Theme, icon: &str, text: &str, s: LedState) -> Response {
    let c = led_color(t, s);

    ui.add(egui::Label::new(egui::RichText::new(format!("{icon} {text}")).color(if s == LedState::Idle { t.fg_dim } else { c })).sense(Sense::click()))
}

/// Signal scope (line plot of recent values, 0–1 unless `range` given).
pub fn scope(ui: &mut Ui, t: &Theme, size: Vec2, values: &[f32], range: Option<(f32, f32)>, color: Option<Color32>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(3), t.bg_darker);
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

/// Card frame: kind icon + title + LED; body inside.
pub fn card<R>(ui: &mut Ui, t: &Theme, icon: &str, title: &str, state: LedState, body: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .fill(t.bg_dark)
        .stroke(Stroke::new(1.0, if state == LedState::Idle { t.muted } else { led_color(t, state) }))
        .corner_radius(CornerRadius::same(6))
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(icon).color(t.accent));
                ui.label(egui::RichText::new(title).strong());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    led(ui, t, state);
                });
            });
            body(ui)
        })
        .inner
}

/// Section header with icon.
pub fn section(ui: &mut Ui, t: &Theme, icon: &str, title: &str) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(icon).color(t.accent));
        ui.label(egui::RichText::new(title.to_uppercase()).small().color(t.fg_dim).strong());
    });
}

/// Frame that marks program-affecting controls with a red edge (§15.1).
pub fn live_edge<R>(ui: &mut Ui, t: &Theme, live: bool, body: impl FnOnce(&mut Ui) -> R) -> R {
    egui::Frame::new()
        .stroke(Stroke::new(if live { 2.0 } else { 0.0 }, if live { t.tally_program() } else { Color32::TRANSPARENT }))
        .inner_margin(egui::Margin::same(2))
        .show(ui, body)
        .inner
}

/// Draw a texture thumbnail with a tally border and label.
pub fn thumbnail(ui: &mut Ui, t: &Theme, size: Vec2, tex: Option<(egui::TextureId, Rect)>, label: &str, tally: Option<LedState>) -> Response {
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(4), t.bg_darker);
    if let Some((id, uv)) = tex {
        p.image(id, rect.shrink(2.0), uv, Color32::WHITE);
    } else {
        p.text(rect.center(), Align2::CENTER_CENTER, "no signal", FontId::proportional(11.0), t.fg_dim);
    }
    let c = tally.map(|s| led_color(t, s)).unwrap_or(t.muted);
    p.rect_stroke(rect, CornerRadius::same(4), Stroke::new(if tally.is_some() { 3.0 } else { 1.0 }, c), StrokeKind::Inside);
    let lbl = Rect::from_min_size(rect.left_bottom() - Vec2::new(0.0, 18.0), Vec2::new(rect.width(), 18.0));
    p.rect_filled(lbl, CornerRadius::ZERO, Color32::from_black_alpha(140));
    p.text(lbl.left_center() + Vec2::new(6.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(11.0), Color32::WHITE);
    resp
}
