//! Canvas monitors (preview / program wide / program tall), scene thumbnails, and the
//! multiview — real engine textures from frames.sock (§4.5), with a live schematic of the
//! scene layout when no frames are arriving.

use crate::app::App;
use crate::frames::{Canvas, FrameTexture};
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use se_proto::Value;
use se_ui_kit::widgets::{LedState, led_color};

/// A presented frame older than this is flagged stale on the monitor.
const STALE_MS: u128 = 500;
/// Frames older than this are not shown at all (schematic instead).
const DEAD_MS: u128 = 3000;

pub fn canvas_size(canvas: &str) -> [f32; 2] {
    if canvas == "tall" { [1080.0, 1920.0] } else { [1920.0, 1080.0] }
}

/// Ask for `c` this frame at `hz` and return the latest texture (if any).
pub fn texture(app: &mut App, c: Canvas, hz: f32) -> Option<FrameTexture> {
    app.frames.want(c, hz);
    app.frames.texture(c).filter(|t| t.age.as_millis() < DEAD_MS)
}

/// Source tile rects in the multiview atlas (`render.atlas.layout`).
pub fn atlas_tiles(app: &App) -> Vec<(String, [f32; 4])> {
    app.m
        .get("render.atlas.layout")
        .and_then(Value::as_list)
        .unwrap_or(&[])
        .iter()
        .filter_map(|e| {
            let src = e.get_path("source").and_then(Value::as_str)?;
            let r = e.get_path("rect").and_then(Value::as_vec4)?;
            Some((src.to_string(), r))
        })
        .collect()
}

fn uv_of(r: [f32; 4]) -> Rect {
    Rect::from_min_size(Pos2::new(r[0], r[1]), Vec2::new(r[2], r[3]))
}

/// One monitor: texture when available, schematic otherwise, tally border and label.
#[allow(clippy::too_many_arguments)]
pub fn monitor(app: &mut App, ui: &mut egui::Ui, canvas: Canvas, scene: &str, label: &str, size: Vec2, tally: LedState, hz: f32) -> egui::Response {
    let t = app.t.clone();
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let tex = texture(app, canvas, hz);
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(3), Color32::BLACK);
    let canvas_name = match canvas {
        Canvas::Tall => "tall",
        _ => "wide",
    };
    match &tex {
        Some(ft) => {
            p.image(ft.id, rect, Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)), Color32::WHITE);
        }
        None => schematic(app, &p, rect, scene, canvas_name),
    }
    let c = led_color(&t, tally);
    p.rect_stroke(rect, CornerRadius::same(3), Stroke::new(3.0, c), StrokeKind::Inside);
    let title = if scene.is_empty() { label.to_string() } else { format!("{label} · {scene}") };
    let galley = p.layout_no_wrap(title, FontId::proportional(12.0), c);
    let bg = Rect::from_min_size(rect.left_top() + Vec2::new(4.0, 4.0), galley.size() + Vec2::new(8.0, 4.0));
    p.rect_filled(bg, CornerRadius::same(3), Color32::from_black_alpha(150));
    p.galley(bg.min + Vec2::new(4.0, 2.0), galley, c);
    match &tex {
        Some(ft) if ft.age.as_millis() > STALE_MS => {
            badge(&p, rect, &format!("STALE {:.1}s", ft.age.as_secs_f32()), t.yellow);
        }
        None if app.m.connected => badge(&p, rect, "no frames · schematic", t.fg_dim),
        _ => {}
    }
    resp
}

fn badge(p: &egui::Painter, rect: Rect, text: &str, color: Color32) {
    let g = p.layout_no_wrap(text.to_string(), FontId::proportional(11.0), color);
    let r = Rect::from_min_size(rect.right_bottom() - g.size() - Vec2::new(12.0, 8.0), g.size() + Vec2::new(8.0, 4.0));
    p.rect_filled(r, CornerRadius::same(3), Color32::from_black_alpha(160));
    p.galley(r.min + Vec2::new(4.0, 2.0), g, color);
}

/// Node boxes of `scene` on `canvas` from the live scene addresses (bindings and edits show),
/// filled with the node's source tile from the atlas when available.
pub fn schematic(app: &App, p: &egui::Painter, rect: Rect, scene: &str, canvas: &str) {
    let t = &app.t;
    let atlas = app.frames.texture(Canvas::Atlas).filter(|f| f.age.as_millis() < DEAD_MS);
    let tiles = if atlas.is_some() { atlas_tiles(app) } else { Vec::new() };
    let [cw, ch] = canvas_size(canvas);
    for n in scene_nodes(app, scene, canvas) {
        let r = Rect::from_min_size(
            Pos2::new(rect.left() + (n.rect[0] + n.offset[0] / cw) * rect.width(), rect.top() + (n.rect[1] + n.offset[1] / ch) * rect.height()),
            Vec2::new(n.rect[2] * rect.width(), n.rect[3] * rect.height()),
        );
        let radius = CornerRadius::same(((n.radius / cw) * rect.width()).clamp(0.0, 40.0) as u8);
        let tile = tiles.iter().find(|(s, _)| *s == n.src).map(|(_, r)| *r);
        match (atlas.as_ref(), tile) {
            (Some(a), Some(tr)) => {
                let uv = crop_uv(tr, n.crop);
                p.add(egui::Shape::Rect(egui::epaint::RectShape::filled(r, radius, Color32::WHITE.gamma_multiply(n.opacity)).with_texture(a.id, uv)));
            }
            _ => {
                p.rect_filled(r, radius, t.bg_light.gamma_multiply(0.4 + 0.6 * n.opacity));
                p.rect_stroke(r, radius, Stroke::new(1.0, t.muted), StrokeKind::Inside);
                p.text(r.center(), Align2::CENTER_CENTER, &n.id, FontId::proportional(11.0), t.fg_dim);
            }
        }
    }
}

/// Apply a node crop (`[l, t, r, b]` insets of the source) to a tile's atlas uv rect.
pub fn crop_uv(tile: [f32; 4], crop: [f32; 4]) -> Rect {
    let [x, y, w, h] = tile;
    let [l, tp, r, b] = crop;
    let uv = uv_of([x + l * w, y + tp * h, (w * (1.0 - l - r)).max(0.0), (h * (1.0 - tp - b)).max(0.0)]);
    if uv.width() <= 0.0 || uv.height() <= 0.0 { uv_of(tile) } else { uv }
}

#[derive(Clone, Debug)]
pub struct NodeView {
    pub id: String,
    pub src: String,
    pub rect: [f32; 4],
    pub crop: [f32; 4],
    pub radius: f32,
    pub opacity: f32,
    pub offset: [f32; 2],
    pub z: i64,
    pub visible: bool,
}

/// Live node geometry of a scene on one canvas (resolved state, z-sorted).
pub fn scene_nodes(app: &App, scene: &str, canvas: &str) -> Vec<NodeView> {
    let prefix = format!("scene.{scene}.node.");
    let suffix = format!(".rect.{canvas}");
    let mut out = Vec::new();
    for (a, v) in app.m.state.range(prefix.clone()..) {
        if !a.starts_with(&prefix) {
            break;
        }
        let Some(id) = a.strip_prefix(&prefix).and_then(|r| r.strip_suffix(&suffix)) else { continue };
        if id.contains('.') {
            continue;
        }
        let Some(rect) = v.as_vec4() else { continue };
        let g = |k: &str| app.m.get(&format!("{prefix}{id}.{k}"));
        let f = |k: &str, d: f64| g(k).and_then(Value::as_f64).unwrap_or(d) as f32;
        out.push(NodeView {
            id: id.to_string(),
            src: app.build.node_src(scene, id).unwrap_or(id).to_string(),
            rect,
            crop: g(&format!("crop.{canvas}")).and_then(Value::as_vec4).unwrap_or([0.0; 4]),
            radius: f(&format!("radius.{canvas}"), 0.0),
            opacity: f("opacity", 1.0),
            offset: [f("offset_x", 0.0), f("offset_y", 0.0)],
            z: g(&format!("z.{canvas}")).and_then(Value::as_i64).unwrap_or(0),
            visible: g("visible").is_none_or(Value::truthy),
        });
    }
    out.retain(|n| n.visible);
    out.sort_by_key(|n| n.z);
    out
}

/// Preview + program wide + program tall, sized to the available width.
pub fn preview_program_row(app: &mut App, ui: &mut egui::Ui) {
    let program = app.m.str("show.scene.program").to_string();
    let preview = app.m.str("show.scene.preview").to_string();
    let avail = ui.available_width();
    let gap = ui.spacing().item_spacing.x;
    // two 16:9 monitors + one 9:16 of the same height
    let w = ((avail - 2.0 * gap - 4.0) / (2.0 + (9.0 / 16.0) * (9.0 / 16.0))).clamp(120.0, 1400.0);
    let h = w * 9.0 / 16.0;
    let (pv, pg) = (app.preview_hz(), app.program_hz());
    ui.horizontal(|ui| {
        monitor(app, ui, Canvas::Preview, &preview, "PREVIEW", Vec2::new(w, h), LedState::Armed, pv);
        monitor(app, ui, Canvas::Wide, &program, "PROGRAM", Vec2::new(w, h), LedState::Active, pg);
        monitor(app, ui, Canvas::Tall, &program, "PROGRAM tall", Vec2::new(h * 9.0 / 16.0, h), LedState::Active, pg);
    });
}

/// One canvas filling the available space at its aspect (pop-out / dock panel).
pub fn single(app: &mut App, ui: &mut egui::Ui, canvas: Canvas) {
    let (scene_key, label, tally, aspect) = match canvas {
        Canvas::Preview => ("show.scene.preview", "PREVIEW", LedState::Armed, 16.0 / 9.0),
        Canvas::Tall => ("show.scene.program", "PROGRAM tall", LedState::Active, 9.0 / 16.0),
        _ => ("show.scene.program", "PROGRAM", LedState::Active, 16.0 / 9.0),
    };
    let scene = app.m.str(scene_key).to_string();
    let avail = ui.available_size();
    let h = (avail.x / aspect).min(avail.y).max(40.0);
    let hz = if canvas == Canvas::Preview { app.preview_hz() } else { app.program_hz() };
    monitor(app, ui, canvas, &scene, label, Vec2::new(h * aspect, h), tally, hz);
}

/// Multiview: every atlas tile (cameras, web, patches) with program/preview tally.
pub fn multiview(app: &mut App, ui: &mut egui::Ui, tile_h: f32) {
    let t = app.t.clone();
    let hz = app.atlas_hz();
    let atlas = texture(app, Canvas::Atlas, hz);
    let tiles = atlas_tiles(app);
    let on = |key: &str| -> Vec<String> {
        let scene = app.m.str(key);
        app.m
            .q_list("scenes")
            .iter()
            .find(|s| s.get_path("name").and_then(Value::as_str) == Some(scene))
            .and_then(|s| s.get_path("sources").and_then(Value::as_list))
            .map(|l| l.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default()
    };
    let (pgm, pvw) = (on("show.scene.program"), on("show.scene.preview"));
    if tiles.is_empty() {
        let msg = if atlas.is_some() { "atlas has no tiles yet" } else { "multiview: waiting for the engine's preview atlas (render.atlas.layout)" };
        ui.label(egui::RichText::new(msg).color(t.fg_dim));
        return;
    }
    let mut clicked = None;
    egui::ScrollArea::horizontal().id_salt("multiview").show(ui, |ui| {
        ui.horizontal(|ui| {
            for (src, r) in &tiles {
                let aspect = if r[3] > 0.0 { (r[2] / r[3]).clamp(0.3, 3.0) } else { 16.0 / 9.0 };
                let size = Vec2::new(tile_h * aspect, tile_h);
                let tally = if pgm.contains(src) {
                    Some(LedState::Active)
                } else if pvw.contains(src) {
                    Some(LedState::Armed)
                } else {
                    None
                };
                let tex = atlas.as_ref().map(|a| (a.id, uv_of(*r)));
                if se_ui_kit::widgets::thumbnail(ui, &t, size, tex, src, tally).clicked() {
                    clicked = Some(src.clone());
                }
            }
        });
    });
    if let Some(src) = clicked {
        app.select(format!("source.{src}"));
    }
}

/// Scene thumbnail: the scene's wide layout drawn with atlas tiles (or boxes).
pub fn scene_thumb(app: &App, ui: &egui::Ui, rect: Rect, scene: &str) {
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(4), app.t.bg_darker);
    schematic(app, &p, rect.shrink(2.0), scene, "wide");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crop_maps_into_the_tile_and_degenerate_crops_fall_back() {
        let uv = crop_uv([0.5, 0.0, 0.25, 0.5], [0.1, 0.2, 0.1, 0.0]);
        assert!((uv.min.x - 0.525).abs() < 1e-6 && (uv.min.y - 0.1).abs() < 1e-6);
        assert!((uv.width() - 0.2).abs() < 1e-6 && (uv.height() - 0.4).abs() < 1e-6);
        let full = crop_uv([0.0, 0.0, 0.5, 0.5], [0.6, 0.0, 0.6, 0.0]);
        assert_eq!(full, uv_of([0.0, 0.0, 0.5, 0.5]));
    }
}
