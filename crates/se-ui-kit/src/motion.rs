//! Motion: one easing curve (ease-out cubic), three durations, and a global "reduce motion"
//! switch. Every kit animation goes through here, so reduced motion makes all of them instant
//! and nothing repaints unless something is actually moving.

use egui::emath::TSTransform;
use egui::{Context, Id, LayerId, Order, Pos2, Ui};

/// Hover and press feedback.
pub const FAST: f32 = 0.09;
/// Focus rings, colour fades, popups fading and scaling in.
pub const BASE: f32 = 0.12;
/// Things that travel: tab underlines, selected pills, knobs, panels opening.
pub const SLOW: f32 = 0.18;

/// Where the reduce-motion flag lives in egui's memory (per context, so tests stay isolated).
fn flag_id() -> Id {
    Id::new("se-ui-kit.reduce-motion")
}

/// Turn every kit animation (and egui's own fades and scroll animations) off or back on.
pub fn set_reduced(ctx: &Context, reduced: bool) {
    ctx.data_mut(|d| d.insert_temp(flag_id(), reduced));
    ctx.all_styles_mut(|s| s.animation_time = egui_animation_time(reduced));
    ctx.request_repaint();
}

/// True when the user asked for reduced motion.
pub fn reduced(ctx: &Context) -> bool {
    ctx.data(|d| d.get_temp(flag_id())).unwrap_or(false)
}

/// The `Style::animation_time` egui's built-in widgets use (popup fade-in, collapsing).
pub fn egui_animation_time(reduced: bool) -> f32 {
    if reduced { 0.0 } else { BASE }
}

/// Ease-out cubic: fast start, gentle landing.
pub fn ease_out(x: f32) -> f32 {
    let x = 1.0 - x.clamp(0.0, 1.0);
    1.0 - x * x * x
}

/// 0 → 1 while `on` is true (1 → 0 when it turns false), eased over `dur` seconds.
/// Repaints only while in flight.
pub fn t(ctx: &Context, id: Id, on: bool, dur: f32) -> f32 {
    let dur = if reduced(ctx) { 0.0 } else { dur };
    ctx.animate_bool_with_time_and_easing(id, on, dur, egui::emath::easing::cubic_out)
}

#[derive(Clone, Copy)]
struct Slide {
    from: f32,
    to: f32,
    start: f64,
    /// Last pass that drew it: something that was hidden for a while jumps instead of gliding.
    pass: u64,
}

/// A value that glides to `target` over `dur` seconds whenever the target changes (tab
/// underlines, selected pills, sidebar highlight). The first call, and the first call after it
/// wasn't drawn for a frame, jump straight to the target.
pub fn value(ctx: &Context, id: Id, target: f32, dur: f32) -> f32 {
    let (now, pass) = (ctx.input(|i| i.time), ctx.cumulative_pass_nr());
    let prev: Option<Slide> = ctx.data(|d| d.get_temp(id)).filter(|s: &Slide| s.pass + 1 >= pass);
    let (s, v, moving) = match prev {
        Some(s) if !reduced(ctx) => {
            let p = if dur <= 0.0 { 1.0 } else { ((now - s.start) as f32 / dur).clamp(0.0, 1.0) };
            let v = s.from + (s.to - s.from) * ease_out(p);
            if s.to == target { (Slide { pass, ..s }, v, p < 1.0) } else { (Slide { from: v, to: target, start: now, pass }, v, true) }
        }
        _ => (Slide { from: target, to: target, start: now, pass }, target, false),
    };
    ctx.data_mut(|d| d.insert_temp(id, s));
    if moving {
        ctx.request_repaint();
    }
    v
}

/// Scales every popup, menu and tooltip up from 96% while it fades in (egui fades them over
/// `Style::animation_time`). Installed once by [`crate::Theme::apply`].
#[derive(Default)]
pub(crate) struct PopupMotion {
    layers: Vec<LayerId>,
}

impl egui::Plugin for PopupMotion {
    fn debug_name(&self) -> &'static str {
        "se-ui-kit popup motion"
    }

    fn on_end_pass(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        if reduced(&ctx) {
            return;
        }
        let now = ctx.input(|i| i.time);
        self.layers.clear();
        ctx.memory(|m| self.layers.extend(m.layer_ids().filter(|l| matches!(l.order, Order::Foreground | Order::Tooltip))));
        for layer in &self.layers {
            let Some(st) = egui::AreaState::load(&ctx, layer.id) else { continue };
            let (Some(shown), Some(size)) = (st.last_became_visible_at, st.size) else { continue };
            let age = (now - shown) as f32;
            if !(0.0..BASE).contains(&age) {
                continue;
            }
            let r = egui::Rect::from_min_size(st.left_top_pos(), size);
            // grow out of the edge the popup hangs from (its pivot), centred horizontally
            let y = match st.pivot.y() {
                egui::Align::Min => r.top(),
                egui::Align::Center => r.center().y,
                egui::Align::Max => r.bottom(),
            };
            let o = Pos2::new(r.center().x, y).to_vec2();
            let s = 0.96 + 0.04 * ease_out(age / BASE);
            ctx.transform_layer_shapes(*layer, TSTransform::from_translation(o) * TSTransform::from_scaling(s) * TSTransform::from_translation(-o));
            ctx.request_repaint();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reduced_motion_jumps_to_the_new_target_without_repaint_animation() {
        let ctx = Context::default();
        let mut output = ctx.run_ui(egui::RawInput::default(), |ui| {
            let ctx = ui.ctx();
            let id = Id::new("tab-selection");
            assert_eq!(value(ctx, id, 0.0, SLOW), 0.0);
            set_reduced(ctx, true);
            assert_eq!(value(ctx, id, 240.0, SLOW), 240.0);
            assert_eq!(t(ctx, id.with("selected"), true, SLOW), 1.0);
            assert_eq!(t(ctx, id.with("selected"), false, SLOW), 0.0);
            set_reduced(ctx, false);
        });
        output.textures_delta.clear();
    }
}
