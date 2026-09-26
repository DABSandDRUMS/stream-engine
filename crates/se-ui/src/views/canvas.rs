//! Build-mode canvas editor (§15.5): edits node rect/crop/radius of the preview scene on the
//! wide and/or tall canvas. Drags preview live through a UI override (`set`), and commit once
//! on release with `set_base` (persisted to `scenes/<scene>.toml` by the engine, comments kept),
//! then release the override.

use crate::app::App;
use crate::frames::Canvas;
use crate::views::monitor;
use egui::{RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::canvas::{self as kc, CanvasEdit, CanvasNode, CanvasOpts};
use se_ui_kit::widgets::{self, LedState, icon};
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
            grid: Some(1.0 / 48.0),
            safe: true,
            tiktok: true,
            labels: true,
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

fn node_ids(app: &App, scene: &str) -> Vec<String> {
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
            label: app.build.node_src(scene, &id).filter(|s| *s != id).map(|s| format!("{id} ({s})")).unwrap_or_else(|| id.clone()),
            rect,
            crop,
            radius,
            z: app.m.get(&format!("{p}.z.{canvas}")).and_then(Value::as_i64).unwrap_or(0) as i32,
            visible: app.m.get(&format!("{p}.visible")).is_none_or(Value::truthy),
            locked: false,
            modulated: app.is_modulated(&format!("{p}.rect.{canvas}"))
                || app.is_modulated(&format!("{p}.offset_x"))
                || app.is_modulated(&format!("{p}.offset_y")),
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

fn toolbar(app: &mut App, ui: &mut egui::Ui, scene: &str) {
    let t = app.t.clone();
    ui.horizontal_wrapped(|ui| {
        widgets::section(ui, &t, icon::SCENE, "Canvas editor");
        let scenes: Vec<String> = app.m.q_list("scenes").iter().filter_map(|s| s.get_path("name").and_then(Value::as_str).map(String::from)).collect();
        egui::ComboBox::from_id_salt("canvas-scene").selected_text(if scene.is_empty() { "(no scene)".to_string() } else { scene.to_string() }).show_ui(
            ui,
            |ui| {
                if ui.selectable_label(app.build.canvas.scene.is_none(), "follow preview").clicked() {
                    app.build.canvas.scene = None;
                }
                for s in scenes {
                    if ui.selectable_label(app.build.canvas.scene.as_deref() == Some(&s), &s).clicked() {
                        app.build.canvas.scene = Some(s.clone());
                        // edits apply to preview: bring the edited scene there
                        app.m.command(Op::SceneGo { scene: s });
                    }
                }
            },
        );
        for (p, l) in [(Pick::Wide, "wide"), (Pick::Tall, "tall"), (Pick::Both, "both")] {
            if ui.selectable_label(app.build.canvas.pick == p, l).clicked() {
                app.build.canvas.pick = p;
            }
        }
        ui.separator();
        ui.checkbox(&mut app.build.canvas.snap, "snap").on_hover_text("Snap to edges, centers, other nodes and the grid (hold Ctrl to disable)");
        let grid_txt = match app.build.canvas.grid {
            None => "grid off".to_string(),
            Some(g) => format!("grid 1/{:.0}", 1.0 / g),
        };
        egui::ComboBox::from_id_salt("canvas-grid").selected_text(grid_txt).show_ui(ui, |ui| {
            for g in [None, Some(1.0 / 12.0), Some(1.0 / 24.0), Some(1.0 / 48.0), Some(1.0 / 96.0)] {
                let l = g.map(|g: f32| format!("1/{:.0}", 1.0 / g)).unwrap_or_else(|| "off".into());
                if ui.selectable_label(app.build.canvas.grid == g, l).clicked() {
                    app.build.canvas.grid = g;
                }
            }
        });
        ui.checkbox(&mut app.build.canvas.safe, "safe areas");
        ui.checkbox(&mut app.build.canvas.tiktok, "TikTok guides").on_hover_text("TikTok UI overlay guides on the tall canvas");
        ui.checkbox(&mut app.build.canvas.labels, "labels");
        if app.m.str("show.scene.program") == scene && !scene.is_empty() {
            ui.label(RichText::new(format!("{} on PROGRAM — edits are live", icon::LIVE)).strong().color(t.tally_program()));
        }
    });
}

fn align_bar(app: &mut App, ui: &mut egui::Ui, scene: &str, canvas: &str) {
    let Some(sel) = app.build.canvas.selected.clone() else { return };
    let nodes = editor_nodes(app, scene, canvas);
    let Some(node) = nodes.iter().find(|n| n.id == sel).cloned() else { return };
    let [cw, ch] = monitor::canvas_size(canvas);
    let mut new_rect = None;
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{sel} on {canvas}")).small().strong());
        for (a, l) in [
            (kc::Align::Left, "⇤"),
            (kc::Align::HCenter, "↔"),
            (kc::Align::Right, "⇥"),
            (kc::Align::Top, "⤒"),
            (kc::Align::VCenter, "↕"),
            (kc::Align::Bottom, "⤓"),
        ] {
            if ui.small_button(l).on_hover_text(format!("align {a:?} to the canvas")).clicked() {
                new_rect = kc::align(&[node.rect], a).into_iter().next();
            }
        }
        if ui.small_button("fill").clicked() {
            new_rect = Some(kc::fill_canvas());
        }
        if ui.small_button("16:9").on_hover_text("match a 16:9 source").clicked() {
            new_rect = Some(kc::fit_aspect(node.rect, 16.0 / 9.0, [cw, ch]));
        }
        if ui.small_button("9:16").clicked() {
            new_rect = Some(kc::fit_aspect(node.rect, 9.0 / 16.0, [cw, ch]));
        }
        let px = kc::rect_px(node.rect, [cw, ch]);
        ui.label(RichText::new(format!("{:.0},{:.0}  {:.0}×{:.0}px  r{:.0}", px[0], px[1], px[2], px[3], node.radius)).small().monospace());
    });
    if let Some(r) = new_rect {
        let e = CanvasEdit { node: sel, kind: kc::EditKind::Move, rect: r, crop: node.crop, radius: node.radius, finished: true };
        apply_edit(app, scene, canvas, e);
    }
}

fn editor(app: &mut App, ui: &mut egui::Ui, scene: &str, canvas: &str, size: Vec2) {
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
    let live = app.m.str("show.scene.program") == scene;
    let opts = CanvasOpts {
        canvas_px: monitor::canvas_size(canvas),
        grid: app.build.canvas.grid,
        snap: app.build.canvas.snap,
        snap_px: 6.0,
        safe_area: app.build.canvas.safe,
        tiktok_guides: app.build.canvas.tiktok && canvas == "tall",
        show_labels: app.build.canvas.labels,
        tally: Some(if live { LedState::Active } else { LedState::Armed }),
        background: None,
        live,
        transparent: true,
    };
    let sel = app.build.canvas.selected.clone();
    let r = kc::canvas_editor(&mut child, &t, egui::Id::new(("canvas-editor", scene, canvas)), size, &nodes, sel.as_deref(), &opts);
    if r.selected != sel {
        app.build.canvas.selected = r.selected.clone();
        if let Some(id) = &r.selected {
            app.select(format!("scene.{scene}.node.{id}"));
        }
    }
    if let Some(e) = r.edit {
        app.build.canvas.selected = Some(e.node.clone());
        apply_edit(app, scene, canvas, e);
        ui.ctx().request_repaint();
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let scene = app.build.canvas.scene.clone().unwrap_or_else(|| app.m.str("show.scene.preview").to_string());
    toolbar(app, ui, &scene);
    if scene.is_empty() {
        ui.label(RichText::new("no preview scene — pick a scene above or press a number key").color(t.fg_dim));
        return;
    }
    let avail = ui.available_size() - Vec2::new(0.0, 30.0);
    let gap = 12.0;
    match app.build.canvas.pick {
        Pick::Wide => {
            let w = avail.x.min(avail.y * 16.0 / 9.0).max(160.0);
            editor(app, ui, &scene, "wide", Vec2::new(w, w * 9.0 / 16.0));
            align_bar(app, ui, &scene, "wide");
        }
        Pick::Tall => {
            let h = avail.y.min(avail.x * 16.0 / 9.0).max(160.0);
            editor(app, ui, &scene, "tall", Vec2::new(h * 9.0 / 16.0, h));
            align_bar(app, ui, &scene, "tall");
        }
        Pick::Both => {
            // wide + tall of equal height side by side: w_wide = h·16/9, w_tall = h·9/16
            let h = ((avail.x - gap) / (16.0 / 9.0 + 9.0 / 16.0)).min(avail.y).max(120.0);
            ui.horizontal_top(|ui| {
                ui.vertical(|ui| {
                    editor(app, ui, &scene, "wide", Vec2::new(h * 16.0 / 9.0, h));
                    align_bar(app, ui, &scene, "wide");
                });
                ui.add_space(gap);
                ui.vertical(|ui| {
                    editor(app, ui, &scene, "tall", Vec2::new(h * 9.0 / 16.0, h));
                    align_bar(app, ui, &scene, "tall");
                });
            });
        }
    }
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
