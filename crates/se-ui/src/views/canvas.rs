//! The canvas editor in the middle of Scenes → Scenes: drag, resize, crop and round the layers
//! of a scene on the Main and/or Vertical canvas. Drags preview live through a UI override
//! (`set`), and commit once on release with `set_base` (saved into `scenes/<scene>.toml` by the
//! engine, comments kept), then release the override.

use crate::app::App;
use crate::frames::Canvas;
use crate::views::monitor;
use egui::Vec2;
use se_proto::{Op, Value};
use se_ui_kit::canvas::{self as kc, CanvasEdit, CanvasNode, CanvasOpts};
use se_ui_kit::widgets;
use std::collections::{BTreeSet, HashMap};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Pick {
    Wide,
    Tall,
    Both,
}

pub struct CanvasState {
    /// Scene being edited (`None` = follow the preview scene).
    pub scene: Option<String>,
    pub pick: Pick,
    pub snap: bool,
    pub grid: Option<f32>,
    pub safe: bool,
    pub tiktok: bool,
    pub labels: bool,
    pub selected: Option<String>,
    /// In-progress drafts per (canvas, node).
    drafts: HashMap<(String, String), CanvasEdit>,
    /// Node geometry when a drag started (live overrides make the resolved value the draft).
    drag_start: HashMap<(String, String), CanvasNode>,
    /// Addresses the UI currently overrides for live drag preview (released on commit).
    overridden: BTreeSet<String>,
    last_live: Option<Instant>,
}

impl Default for CanvasState {
    fn default() -> Self {
        CanvasState {
            scene: None,
            pick: Pick::Both,
            snap: true,
            grid: None,
            safe: false,
            tiktok: false,
            labels: false,
            selected: None,
            drafts: HashMap::new(),
            drag_start: HashMap::new(),
            overridden: BTreeSet::new(),
            last_live: None,
        }
    }
}

/// Live overrides are sent at most this often while dragging.
const LIVE_MS: u128 = 33;

/// Layer names of `scene` as the engine loaded it.
pub fn node_ids(app: &App, scene: &str) -> Vec<String> {
    app.m
        .q_list("scenes")
        .iter()
        .find(|s| s.get_path("name").and_then(Value::as_str) == Some(scene))
        .and_then(|s| s.get_path("nodes").and_then(Value::as_list))
        .map(|l| l.iter().filter_map(|v| v.as_str().map(String::from)).collect())
        .unwrap_or_default()
}

/// Editable nodes of `scene` on `canvas` (resolved values, with drafts applied).
pub fn editor_nodes(app: &App, scene: &str, canvas: &str) -> Vec<CanvasNode> {
    let mut out = Vec::new();
    for id in node_ids(app, scene) {
        let p = format!("scene.{scene}.node.{id}");
        let Some(rect) = app.m.get(&format!("{p}.rect.{canvas}")).and_then(Value::as_vec4) else { continue };
        let crop = app.m.get(&format!("{p}.crop.{canvas}")).and_then(Value::as_vec4).unwrap_or([0.0; 4]);
        let radius = app.m.f(&format!("{p}.radius.{canvas}")) as f32;
        let mut n = CanvasNode {
            id: id.clone(),
            label: crate::views::composition::source_label(app, app.build.node_src(scene, &id).unwrap_or(&id)),
            rect,
            crop,
            radius,
            z: app.m.get(&format!("{p}.z.{canvas}")).and_then(Value::as_i64).unwrap_or(0) as i32,
            visible: app.m.get(&format!("{p}.visible")).is_none_or(Value::truthy),
            locked: false,
            modulated: ["offset_x", "offset_y", "scale", "rotation", "opacity"].iter().any(|k| app.is_modulated(&format!("{p}.{k}")))
                || app.is_modulated(&format!("{p}.rect.{canvas}")),
        };
        if let Some(d) = app.build.canvas.drafts.get(&(canvas.to_string(), id.clone())) {
            n.rect = d.rect;
            n.crop = d.crop;
            n.radius = d.radius;
        }
        out.push(n);
    }
    out
}

pub(crate) fn framed_window(app: &App, scene: &str, canvas: &str, id: &str) -> bool {
    let nodes = app.build.scene_config(scene)
        .and_then(|s| s.get_path("canvas"))
        .and_then(|c| c.get_path(canvas))
        .and_then(|c| c.get_path("nodes"))
        .and_then(Value::as_list)
        .unwrap_or(&[]);
    nodes.iter().any(|n| {
        let src = n.get_path("src").and_then(Value::as_str).unwrap_or("");
        let node_id = n.get_path("id").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(src);
        node_id == id && (src == "youtube" || n.get_path("fx").and_then(Value::as_list).unwrap_or(&[]).iter()
            .any(|fx| fx.get_path("name").and_then(Value::as_str) == Some("patch.win31_video")))
    })
}

fn vec4(v: [f32; 4]) -> Value {
    Value::from(v.map(|x| (x * 10000.0).round() / 10000.0))
}

/// The (address, value) pairs an edit changes relative to the node's current geometry.
pub fn edit_changes(scene: &str, canvas: &str, before: &CanvasNode, e: &CanvasEdit) -> Vec<(String, Value)> {
    let p = format!("scene.{scene}.node.{}", e.node);
    let mut out = Vec::new();
    let differs = |a: [f32; 4], b: [f32; 4]| a.iter().zip(b).any(|(x, y)| (x - y).abs() > 1e-5);
    if differs(before.rect, e.rect) {
        out.push((format!("{p}.rect.{canvas}"), vec4(e.rect)));
    }
    if differs(before.crop, e.crop) {
        out.push((format!("{p}.crop.{canvas}"), vec4(e.crop)));
    }
    if (before.radius - e.radius).abs() > 0.01 {
        out.push((format!("{p}.radius.{canvas}"), Value::Float((e.radius as f64 * 10.0).round() / 10.0)));
    }
    out
}

fn apply_edit(app: &mut App, scene: &str, canvas: &str, e: CanvasEdit) {
    let key = (canvas.to_string(), e.node.clone());
    // geometry before the drag (resolved state without drafts)
    app.build.canvas.drafts.remove(&key);
    let before = match app.build.canvas.drag_start.get(&key) {
        Some(n) => Some(n.clone()),
        None => editor_nodes(app, scene, canvas).into_iter().find(|n| n.id == e.node),
    };
    let Some(before) = before else { return };
    if e.finished {
        app.build.canvas.drag_start.remove(&key);
    } else {
        app.build.canvas.drag_start.entry(key.clone()).or_insert_with(|| before.clone());
    }
    let changes = edit_changes(scene, canvas, &before, &e);
    if e.finished {
        for (a, v) in &changes {
            app.m.command(Op::SetBase { address: a.clone(), value: v.clone() });
        }
        let prefix = format!("scene.{scene}.node.{}.", e.node);
        let held: Vec<String> = app.build.canvas.overridden.iter().filter(|a| a.starts_with(&prefix)).cloned().collect();
        for a in held {
            app.build.canvas.overridden.remove(&a);
            app.m.command(Op::Release { address: a });
        }
        app.build.canvas.last_live = None;
        if !changes.is_empty() {
            crate::views::composition::base_written(app, scene);
        }
    } else {
        let due = app.build.canvas.last_live.is_none_or(|t| t.elapsed().as_millis() >= LIVE_MS);
        if due && !changes.is_empty() {
            app.build.canvas.last_live = Some(Instant::now());
            for (a, v) in changes {
                app.build.canvas.overridden.insert(a.clone());
                app.m.command(Op::Set { address: a, value: v });
            }
        }
        app.build.canvas.drafts.insert(key, e);
    }
}

/// Editing options above the canvas: which canvas(es), snapping, and a "Guides" menu.
pub fn toolbar(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        let mut i = match app.build.canvas.pick {
            Pick::Wide => 0,
            Pick::Tall => 1,
            Pick::Both => 2,
        };
        if widgets::segmented(ui, &t, &mut i, &["Main", "Vertical", "Both"]) {
            app.build.canvas.pick = [Pick::Wide, Pick::Tall, Pick::Both][i];
        }
        ui.add_space(se_ui_kit::theme::spacing::M);
        let tip = "Snap to edges, centers and other layers (hold Ctrl to place freely)";
        widgets::toggle(ui, &t, &mut app.build.canvas.snap).on_hover_text(tip);
        ui.label(egui::RichText::new("Snap").color(t.text_dim)).on_hover_text(tip);
        ui.add_space(se_ui_kit::theme::spacing::S);
        let r = widgets::button_ex(ui, &t, Some(se_ui_kit::widgets::icon::GRID), "Guides  \u{f078}", widgets::Kind::Secondary, widgets::Size::Small, 0.0, true);
        egui::Popup::menu(&r).show(|ui| {
            ui.set_min_width(240.0);
            for (label, v) in [
                ("Safe areas (TV and phone)", &mut app.build.canvas.safe),
                ("TikTok button areas", &mut app.build.canvas.tiktok),
                ("Layer names", &mut app.build.canvas.labels),
            ] {
                ui.horizontal(|ui| {
                    widgets::toggle(ui, &t, v);
                    ui.label(label);
                });
            }
            let mut g = match app.build.canvas.grid {
                None => 0,
                Some(x) if x > 1.0 / 30.0 => 1,
                _ => 2,
            };
            ui.horizontal(|ui| {
                ui.label("Grid");
                if widgets::segmented(ui, &t, &mut g, &["Off", "Coarse", "Fine"]) {
                    app.build.canvas.grid = [None, Some(1.0 / 24.0), Some(1.0 / 48.0)][g];
                }
            });
        });
    });
}

/// Replace a layer's rectangle on `canvas` (placements and typed values from the inspector).
pub fn apply_rect(app: &mut App, scene: &str, canvas: &str, node: &str, rect: [f32; 4]) {
    let Some(n) = editor_nodes(app, scene, canvas).into_iter().find(|n| n.id == node) else { return };
    let e = CanvasEdit { node: node.to_string(), kind: kc::EditKind::Move, rect, crop: n.crop, radius: n.radius, finished: true };
    apply_edit(app, scene, canvas, e);
}

/// Replace a layer's crop / corner radius on `canvas`. `finished = false` previews live while a
/// slider is dragged; `true` saves.
pub fn apply_shape(app: &mut App, scene: &str, canvas: &str, node: &str, crop: [f32; 4], radius: f32, finished: bool) {
    let Some(n) = editor_nodes(app, scene, canvas).into_iter().find(|n| n.id == node) else { return };
    let e = CanvasEdit { node: node.to_string(), kind: kc::EditKind::Move, rect: n.rect, crop, radius, finished };
    apply_edit(app, scene, canvas, e);
}

fn editor(app: &mut App, ui: &mut egui::Ui, scene: &str, canvas: &str, size: Vec2) -> egui::Rect {
    let t = app.t.clone();
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let mut child = ui.new_child(egui::UiBuilder::new().max_rect(rect));
    // canvas contents first: the engine's preview for wide, else the live schematic
    let preview_scene = app.m.str("show.scene.preview").to_string();
    let bg = if canvas == "wide" && preview_scene == scene { monitor::texture(app, Canvas::Preview, app.preview_hz()) } else { None };
    {
        let p = child.painter_at(rect);
        p.rect_filled(rect, egui::CornerRadius::same(3), egui::Color32::BLACK);
        match bg {
            Some(ft) => {
                p.image(ft.id, rect, egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
            }
            None => monitor::schematic(app, &p, rect, scene, canvas),
        }
    }
    let nodes = editor_nodes(app, scene, canvas);
    let live = crate::views::status::on_air(app) && app.m.str("show.scene.program") == scene;
    let opts = CanvasOpts {
        canvas_px: monitor::canvas_size(canvas),
        grid: app.build.canvas.grid,
        snap: app.build.canvas.snap,
        keep_aspect: app.build.canvas.selected.as_deref().is_some_and(|id| framed_window(app, scene, canvas, id)),
        snap_px: 6.0,
        safe_area: app.build.canvas.safe,
        tiktok_guides: app.build.canvas.tiktok && canvas == "tall",
        show_labels: app.build.canvas.labels,
        tally: None,
        background: None,
        live,
        transparent: true,
    };
    let sel = app.build.canvas.selected.clone();
    let r = kc::canvas_editor(&mut child, &t, egui::Id::new(("canvas-editor", scene, canvas)), size, &nodes, sel.as_deref(), &opts);
    if r.selected != sel {
        app.build.canvas.selected = r.selected.clone();
        app.build.comp.pinned = None;
        if let Some(id) = &r.selected {
            app.select(format!("scene.{scene}.node.{id}"));
        }
    }
    if let Some(e) = r.edit {
        app.build.canvas.selected = Some(e.node.clone());
        app.build.comp.pinned = None;
        apply_edit(app, scene, canvas, e);
        ui.ctx().request_repaint();
    }
    rect
}

/// The canvas editor(s) of `scene`, sized to fit `avail`. Returns the area they cover.
pub fn editors(app: &mut App, ui: &mut egui::Ui, scene: &str, avail: Vec2) -> egui::Rect {
    let gap = se_ui_kit::theme::spacing::L;
    match app.build.canvas.pick {
        Pick::Wide => {
            let w = avail.x.min(avail.y * 16.0 / 9.0).max(160.0);
            editor(app, ui, scene, "wide", Vec2::new(w, w * 9.0 / 16.0))
        }
        Pick::Tall => {
            let h = avail.y.min(avail.x * 16.0 / 9.0).max(160.0);
            ui.horizontal(|ui| {
                ui.add_space(((avail.x - h * 9.0 / 16.0) / 2.0).max(0.0));
                editor(app, ui, scene, "tall", Vec2::new(h * 9.0 / 16.0, h))
            })
            .inner
        }
        Pick::Both => {
            // main + vertical of equal height side by side: w_main = h·16/9, w_vert = h·9/16
            let h = ((avail.x - gap) / (16.0 / 9.0 + 9.0 / 16.0)).min(avail.y).max(120.0);
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                let a = editor(app, ui, scene, "wide", Vec2::new(h * 16.0 / 9.0, h));
                let b = editor(app, ui, scene, "tall", Vec2::new(h * 9.0 / 16.0, h));
                a.union(b)
            })
            .inner
        }
    }
}

/// The canvas the inspector edits: the one shown, or the main one when both are.
pub fn edit_canvas(app: &App) -> &'static str {
    if app.build.canvas.pick == Pick::Tall { "tall" } else { "wide" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(rect: [f32; 4]) -> CanvasNode {
        CanvasNode { id: "cam_kit".into(), label: "cam_kit".into(), rect, crop: [0.0; 4], radius: 24.0, z: 0, visible: true, locked: false, modulated: false }
    }

    #[test]
    fn only_changed_properties_are_written() {
        let before = node([0.02, 0.05, 0.47, 0.90]);
        let e = CanvasEdit { node: "cam_kit".into(), kind: kc::EditKind::Move, rect: [0.1, 0.05, 0.47, 0.90], crop: [0.0; 4], radius: 24.0, finished: true };
        let ch = edit_changes("duo", "wide", &before, &e);
        assert_eq!(ch.len(), 1);
        assert_eq!(ch[0].0, "scene.duo.node.cam_kit.rect.wide");
        assert_eq!(ch[0].1.as_vec4(), Some([0.1, 0.05, 0.47, 0.9]));
        let e2 = CanvasEdit { rect: before.rect, crop: [0.1, 0.0, 0.0, 0.0], radius: 30.0, ..e };
        let ch2 = edit_changes("duo", "tall", &before, &e2);
        let addrs: Vec<&str> = ch2.iter().map(|(a, _)| a.as_str()).collect();
        assert_eq!(addrs, vec!["scene.duo.node.cam_kit.crop.tall", "scene.duo.node.cam_kit.radius.tall"]);
    }
}
