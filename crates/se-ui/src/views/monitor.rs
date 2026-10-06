//! Canvas monitors (preview / program wide / program tall), scene thumbnails, and the
//! multiview — real engine textures from frames.sock (§4.5), with a live schematic of the
//! scene layout when no frames are arriving.

use crate::app::App;
use crate::frames::{Canvas, FrameTexture};
use egui::{Color32, CornerRadius, Pos2, Rect, Sense, Stroke, StrokeKind, Vec2};
use se_core::config::NodeFit;
use se_proto::Value;
use se_ui_kit::theme::{font_bold, font_medium, radius, type_scale};
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
    if c == Canvas::Tall && app.m.get("render.vertical.enabled").is_some_and(|v| !v.truthy()) {
        return None;
    }
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

/// One monitor: texture when available, the scene's layout otherwise, a tally outline, and a
/// badge (`label`, e.g. "ON AIR") with the scene name.
#[allow(clippy::too_many_arguments)]
pub fn monitor(app: &mut App, ui: &mut egui::Ui, canvas: Canvas, scene: &str, label: &str, size: Vec2, tally: LedState, hz: f32) -> egui::Response {
    let t = app.t.clone();
    let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    if canvas == Canvas::Tall {
        let disabled = app.m.get("render.vertical.enabled").is_some_and(|v| !v.truthy());
        let restoring = app.m.b("obs.vertical.enabled") && app.m.has("obs.vertical.ready") && !app.m.b("obs.vertical.ready");
        if disabled || !app.m.connected || restoring {
            let text = if !app.m.connected {
                "Vertical state unknown\nEngine disconnected"
            } else if disabled {
                if app.m.b("obs.vertical.enabled") { "Restoring vertical video…" } else { "Vertical video off\nEncoder and canvas stopped" }
            } else {
                "Restoring vertical video…\nWaiting for OBS"
            };
            let p = ui.painter_at(rect);
            p.rect(rect, CornerRadius::same(radius::TILE), t.inset, Stroke::new(1.0, t.border), StrokeKind::Inside);
            let g = p.layout(text.into(), font_medium(type_scale::BODY), t.text_dim, (rect.width() - 24.0).max(16.0));
            p.galley(rect.center() - g.size() / 2.0, g, t.text_dim);
            return resp;
        }
    }
    let tex = texture(app, canvas, hz);
    let p = ui.painter_at(rect.expand(4.0));
    let r = CornerRadius::same(radius::TILE);
    p.rect_filled(rect, r, Color32::from_rgb(6, 7, 9));
    let canvas_name = match canvas {
        Canvas::Tall => "tall",
        _ => "wide",
    };
    match &tex {
        Some(ft) => {
            let uv = Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0));
            p.add(egui::Shape::Rect(egui::epaint::RectShape::filled(rect, r, Color32::WHITE).with_texture(ft.id, uv)));
        }
        None => schematic(app, &p, rect, scene, canvas_name),
    }
    let c = led_color(&t, tally);
    let lit = matches!(tally, LedState::Active | LedState::Armed);
    p.rect_stroke(rect, r, Stroke::new(if lit { 3.0 } else { 1.0 }, if lit { c } else { t.border }), StrokeKind::Outside);
    if !label.is_empty() {
        let fid = font_bold(type_scale::SMALL);
        let g = p.layout_no_wrap(label.to_string(), fid, Color32::WHITE);
        let chip = Rect::from_min_size(rect.left_top() + Vec2::new(10.0, 10.0), Vec2::new(g.size().x + 16.0, 24.0));
        p.rect_filled(chip, CornerRadius::same(6), if lit { c } else { Color32::from_black_alpha(170) });
        p.galley(chip.center() - g.size() / 2.0, g, Color32::WHITE);
        if !scene.is_empty() {
            let label = app
                .m
                .q_list("scenes")
                .iter()
                .find(|s| s.get_path("name").and_then(Value::as_str) == Some(scene))
                .and_then(|s| s.get_path("label").and_then(Value::as_str))
                .unwrap_or(scene)
                .to_string();
            let sg = p.layout_no_wrap(crate::views::live::nice(&label), font_medium(type_scale::SMALL), Color32::WHITE);
            let sc = Rect::from_min_size(Pos2::new(chip.right() + 6.0, chip.top()), Vec2::new(sg.size().x + 16.0, 24.0));
            p.rect_filled(sc, CornerRadius::same(6), Color32::from_black_alpha(170));
            p.galley(sc.center() - sg.size() / 2.0, sg, Color32::WHITE);
        }
    }
    match &tex {
        Some(ft) if ft.age.as_millis() > STALE_MS => {
            badge(&p, rect, &format!("Frozen for {:.1}s", ft.age.as_secs_f32()), t.yellow);
        }
        None if app.m.connected => badge(&p, rect, "Layout preview (no video yet)", t.text_dim),
        _ => {}
    }
    resp
}

fn badge(p: &egui::Painter, rect: Rect, text: &str, color: Color32) {
    let g = p.layout_no_wrap(text.to_string(), font_medium(type_scale::SMALL), color);
    let r = Rect::from_min_size(rect.right_bottom() - g.size() - Vec2::new(26.0, 18.0), g.size() + Vec2::new(16.0, 8.0));
    p.rect_filled(r, CornerRadius::same(6), Color32::from_black_alpha(170));
    p.galley(r.min + Vec2::new(8.0, 4.0), g, color);
}

/// Node boxes of `scene` on `canvas` from the live scene addresses (bindings and edits show),
/// filled with the node's source tile from the atlas when available.
pub fn schematic(app: &App, p: &egui::Painter, rect: Rect, scene: &str, canvas: &str) {
    schematic_ex(app, p, rect, scene, canvas, false);
}

/// [`schematic`], or (`thumbnail`) a small picture of what the scene shows right now: no layer
/// names, and layers whose show condition is off right now are left out.
pub fn schematic_ex(app: &App, p: &egui::Painter, rect: Rect, scene: &str, canvas: &str, thumbnail: bool) {
    let labels = !thumbnail;
    let t = &app.t;
    let atlas = app.frames.texture(Canvas::Atlas).filter(|f| f.age.as_millis() < DEAD_MS);
    let tiles = if atlas.is_some() { atlas_tiles(app) } else { Vec::new() };
    let [cw, ch] = canvas_size(canvas);
    for n in scene_nodes(app, scene, canvas) {
        if thumbnail && !crate::views::composition::node_showing(app, scene, &n.id) {
            continue;
        }
        let r = Rect::from_min_size(
            Pos2::new(rect.left() + (n.rect[0] + n.offset[0] / cw) * rect.width(), rect.top() + (n.rect[1] + n.offset[1] / ch) * rect.height()),
            Vec2::new(n.rect[2] * rect.width(), n.rect[3] * rect.height()),
        );
        let radius = CornerRadius::same(((n.radius / cw) * rect.width()).clamp(0.0, 40.0) as u8);
        let source_size = if n.fit == NodeFit::Native { source_native_size(app, &n.src) } else { None };
        let tile = tiles.iter().find(|(s, _)| *s == n.src)
            .filter(|_| n.fit == NodeFit::Stretch || source_size.is_some())
            .map(|(_, r)| *r);
        match (atlas.as_ref(), tile) {
            (Some(a), Some(tr)) => {
                let (dst, uv) = source_placement(n.fit, r, n.crop, tr, source_size.unwrap_or([0.0; 2]), a.size, rect.width() / cw);
                p.add(egui::Shape::Rect(egui::epaint::RectShape::filled(dst, radius, Color32::WHITE.gamma_multiply(n.opacity)).with_texture(a.id, uv)));
            }
            _ => {
                p.rect_filled(r, radius, se_ui_kit::theme::mix(t.surface_hi, Color32::BLACK, 0.2 * (1.0 - n.opacity)));
                p.rect_stroke(r, radius, Stroke::new(1.0, t.border), StrokeKind::Inside);
                if labels {
                    let name = crate::views::composition::source_label(app, &n.src);
                    let g = p.layout(name, font_medium(type_scale::SMALL), t.text_dim, (r.width() - 8.0).max(10.0));
                    if g.size().x <= r.width() - 4.0 && g.size().y <= r.height() - 4.0 {
                        p.galley(r.center() - g.size() / 2.0, g, t.text_dim);
                    }
                }
            }
        }
    }
}

/// Current texture dimensions, sharing the model's existing query cache.
fn source_native_size(app: &App, src: &str) -> Option<[f32; 2]> {
    let valid = |size: [f32; 2]| size.iter().all(|v| v.is_finite() && *v > 0.0).then_some(size);
    let list_size = |v: &Value| {
        let list = v.as_list()?;
        valid([list.first()?.as_f64()? as f32, list.get(1)?.as_f64()? as f32])
    };
    if let Some(web) = app.m.q("web").and_then(|v| v.get_path("sources")).and_then(Value::as_list)
        .unwrap_or(&[]).iter().find(|s| s.get_path("slot").and_then(Value::as_str) == Some(src))
    {
        return web.get_path("frame_size").and_then(list_size).or_else(|| web.get_path("size").and_then(list_size));
    }
    if let Some(source) = app.m.q_list("sources").iter().find(|s| s.get_path("name").and_then(Value::as_str) == Some(src)) {
        let measured = source.get_path("width").and_then(Value::as_f64)
            .zip(source.get_path("height").and_then(Value::as_f64))
            .and_then(|(w, h)| valid([w as f32, h as f32]));
        return measured.or_else(|| source.get_path("size").and_then(list_size));
    }
    let id = src.strip_prefix("patch.")?;
    app.m.q_list("patches").iter().find(|p| p.get_path("id").and_then(Value::as_str) == Some(id))
        .filter(|p| p.get_path("kind").and_then(Value::as_str) != Some("web"))
        .and_then(|p| p.get_path("size")).and_then(list_size)
}

/// Native placement operates in source UVs, not atlas UVs: the atlas letterboxes each source.
fn source_placement(fit: NodeFit, window: Rect, crop: [f32; 4], tile: [f32; 4], source_size: [f32; 2], atlas_size: [u32; 2], canvas_scale: f32) -> (Rect, Rect) {
    if fit == NodeFit::Stretch {
        return (window, crop_uv(tile, crop));
    }
    let source_uv = crop_uv([0.0, 0.0, 1.0, 1.0], crop);
    let (dst, uv) = fit.placement(
        [window.left(), window.top(), window.width(), window.height()],
        [source_uv.min.x, source_uv.min.y, source_uv.max.x, source_uv.max.y],
        source_size,
        canvas_scale,
    );
    let tile_px = [tile[2] * atlas_size[0] as f32, tile[3] * atlas_size[1] as f32];
    let scale = (tile_px[0] / source_size[0]).min(tile_px[1] / source_size[1]);
    let content = [
        source_size[0] * scale / atlas_size[0] as f32,
        source_size[1] * scale / atlas_size[1] as f32,
    ];
    let origin = [tile[0] + (tile[2] - content[0]) * 0.5, tile[1] + (tile[3] - content[1]) * 0.5];
    (
        uv_of(dst),
        Rect::from_min_max(
            Pos2::new(origin[0] + uv[0] * content[0], origin[1] + uv[1] * content[1]),
            Pos2::new(origin[0] + uv[2] * content[0], origin[1] + uv[3] * content[1]),
        ),
    )
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
    pub fit: NodeFit,
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
    let config_nodes = app.build.scene_config(scene)
        .and_then(|s| s.get_path("canvas")).and_then(|c| c.get_path(canvas))
        .and_then(|c| c.get_path("nodes")).and_then(Value::as_list).unwrap_or(&[]);
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
        let config = config_nodes.iter().find(|n| {
            let src = n.get_path("src").and_then(Value::as_str).unwrap_or("");
            n.get_path("id").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(src) == id
        });
        out.push(NodeView {
            id: id.to_string(),
            src: config.and_then(|n| n.get_path("src")).and_then(Value::as_str)
                .or_else(|| app.build.node_src(scene, id)).unwrap_or(id).to_string(),
            rect,
            crop: g(&format!("crop.{canvas}")).and_then(Value::as_vec4).unwrap_or([0.0; 4]),
            fit: if config.and_then(|n| n.get_path("fit")).and_then(Value::as_str) == Some("native") { NodeFit::Native } else { NodeFit::Stretch },
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
        let msg = if atlas.is_some() { "No cameras or sources to show yet." } else { "Waiting for video from the engine…" };
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
                let name = crate::views::composition::source_label(app, src);
                if se_ui_kit::widgets::thumbnail(ui, &t, size, tex, &name, tally).on_hover_text(&name).clicked() {
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
    p.rect_filled(rect, CornerRadius::same(radius::TILE), Color32::from_rgb(6, 7, 9));
    schematic_ex(app, &p, rect.shrink(3.0), scene, "wide", true);
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

    fn assert_rect(actual: Rect, expected: [f32; 4]) {
        for (a, e) in [actual.left(), actual.top(), actual.width(), actual.height()].into_iter().zip(expected) {
            assert!((a - e).abs() < 1e-5, "{actual:?} != {expected:?}");
        }
    }

    #[test]
    fn native_atlas_resize_clips_top_right_and_pads_without_scaling_pixels() {
        let placement = |window| source_placement(
            NodeFit::Native, uv_of(window), [0.0; 4], [0.0, 0.0, 1.0, 1.0], [800.0, 800.0], [1600, 900], 0.25,
        );
        let (small, small_uv) = placement([10.0, 20.0, 100.0, 100.0]);
        assert_rect(small, [10.0, 20.0, 100.0, 100.0]);
        assert_rect(small_uv, [0.21875, 0.5, 0.28125, 0.5]);
        let (large, large_uv) = placement([10.0, 20.0, 300.0, 300.0]);
        assert_rect(large, [10.0, 120.0, 200.0, 200.0]);
        assert_rect(large_uv, [0.21875, 0.0, 0.5625, 1.0]);
        assert!((small.width() / small_uv.width() - large.width() / large_uv.width()).abs() < 1e-4);
        assert!((small.height() / small_uv.height() - large.height() / large_uv.height()).abs() < 1e-4);
    }

    #[test]
    fn native_atlas_crop_preserves_source_pixels_at_preview_scale() {
        let (dst, uv) = source_placement(
            NodeFit::Native, uv_of([10.0, 20.0, 100.0, 80.0]), [0.125, 0.25, 0.25, 0.125],
            [0.0, 0.0, 1.0, 1.0], [800.0, 800.0], [1600, 900], 0.25,
        );
        assert_rect(dst, [10.0, 20.0, 100.0, 80.0]);
        assert_rect(uv, [0.2890625, 0.475, 0.28125, 0.4]);
        let (dst, uv) = source_placement(
            NodeFit::Native, uv_of([10.0, 20.0, 50.0, 40.0]), [0.125, 0.25, 0.25, 0.125],
            [0.0, 0.0, 1.0, 1.0], [800.0, 800.0], [1600, 900], 0.125,
        );
        assert_rect(dst, [10.0, 20.0, 50.0, 40.0]);
        assert_rect(uv, [0.2890625, 0.475, 0.28125, 0.4]);
    }
}
