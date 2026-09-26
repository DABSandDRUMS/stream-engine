//! Preview / program monitors: a live schematic of the scene layout from the state tree.

use crate::app::App;
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Stroke, StrokeKind, Vec2};
use se_proto::Value;
use se_ui_kit::widgets::LedState;

pub fn preview_program_row(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let program = app.m.str("show.scene.program").to_string();
    let preview = app.m.str("show.scene.preview").to_string();
    let avail = ui.available_width();
    let w = ((avail - 3.0 * 8.0) / (2.0 + 9.0 / 16.0 * 9.0 / 16.0 * 1.0 + 0.35)).clamp(160.0, 900.0);
    let h = w * 9.0 / 16.0;
    ui.horizontal(|ui| {
        monitor(app, ui, &preview, "PREVIEW", "wide", Vec2::new(w, h), LedState::Armed);
        monitor(app, ui, &program, "PROGRAM", "wide", Vec2::new(w, h), LedState::Active);
        monitor(app, ui, &program, "PROGRAM tall", "tall", Vec2::new(h * 9.0 / 16.0, h), LedState::Active);
    });
    let _ = t;
}

fn monitor(app: &mut App, ui: &mut egui::Ui, scene: &str, label: &str, canvas: &str, size: Vec2, tally: LedState) {
    let t = app.t.clone();
    let (rect, _resp) = ui.allocate_exact_size(size, egui::Sense::click());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(3), Color32::BLACK);
    schematic(app, &p, rect, scene, canvas);
    let c = se_ui_kit::widgets::led_color(&t, tally);
    p.rect_stroke(rect, CornerRadius::same(3), Stroke::new(3.0, c), StrokeKind::Inside);
    p.text(rect.left_top() + Vec2::new(8.0, 6.0), Align2::LEFT_TOP, format!("{label} · {scene}"), FontId::proportional(12.0), c);
}

/// Node boxes of `scene` on `canvas`, from the live scene addresses (so bindings and edits show).
fn schematic(app: &App, p: &egui::Painter, rect: Rect, scene: &str, canvas: &str) {
    let t = &app.t;
    let prefix = format!("scene.{scene}.node.");
    let suffix = format!(".rect.{canvas}");
    for (a, v) in app.m.state.range(prefix.clone()..) {
        if !a.starts_with(&prefix) {
            break;
        }
        let Some(node) = a.strip_prefix(&prefix).and_then(|r| r.strip_suffix(&suffix)) else { continue };
        let Some([x, y, w, h]) = v.as_vec4() else { continue };
        let op = app.m.get(&format!("{prefix}{node}.opacity")).and_then(Value::as_f64).unwrap_or(1.0) as f32;
        let ox = app.m.get(&format!("{prefix}{node}.offset_x")).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let oy = app.m.get(&format!("{prefix}{node}.offset_y")).and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let (cw, ch) = if canvas == "tall" { (1080.0, 1920.0) } else { (1920.0, 1080.0) };
        let r = Rect::from_min_size(
            Pos2::new(rect.left() + (x + ox / cw) * rect.width(), rect.top() + (y + oy / ch) * rect.height()),
            Vec2::new(w * rect.width(), h * rect.height()),
        );
        p.rect_filled(r, CornerRadius::same(4), t.bg_light.gamma_multiply(0.4 + 0.6 * op));
        p.rect_stroke(r, CornerRadius::same(4), Stroke::new(1.0, t.muted), StrokeKind::Inside);
        p.text(r.center(), Align2::CENTER_CENTER, node, FontId::proportional(11.0), t.fg_dim);
    }
}
