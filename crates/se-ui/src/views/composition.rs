//! Scenes → Scenes: the one three-pane scene editor (docs/ui-model.md).
//!
//! - Left: **Scenes** (pick one, `+` makes one) over **Layers** of the picked scene: the
//!   sources drawn above every scene ("On every scene", `[overlays.<id>]` in `project.toml`),
//!   then the scene's own layers front to back (eye, kind, name, effect/∿/⚡ marks; drag to
//!   reorder; `+` adds a layer).
//! - Center: the canvas editor for the Main and/or Vertical canvas.
//! - Right: the inspector of the selected layer (source, transform, crop, look, effects, when
//!   it shows, enter & exit, arrange) or, with no layer selected, of the scene (name and key,
//!   transition in, scene effects, what it does starting and ending, background, lights).
//!
//! Live numbers (transform, opacity, a layer effect's switch and settings) preview with `set`
//! while dragged and save with `set_base`, which the engine writes into the scene file.
//! Everything else edits the file text with `scene_edit` (comments kept) and `project.write`.

use crate::app::{App, ViewId};
use crate::views::live::nice;
use crate::views::scene_edit;
use crate::views::{canvas, links, media, patches, rules, sources};
use egui::{Align, Layout, RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::canvas as kc;
use se_ui_kit::theme::{font_medium, font_mono, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, Tone, icon};
use std::collections::BTreeSet;
use std::ops::RangeInclusive;

/// Width of the Scenes + Layers column.
const LEFT_W: f32 = 284.0;
/// Width of the inspector.
const RIGHT_W: f32 = 360.0;
/// Query key of `project.toml` (the "On every scene" settings).
const PROJECT_KEY: &str = "comp.file:project.toml";

#[derive(Default)]
pub struct CompositionState {
    pub rename: String,
    pub rename_for: String,
    pub new_open: bool,
    pub new_name: String,
    /// "Start from" choice (index into `scene_edit::START_LAYOUTS`).
    pub new_layout: usize,
    /// A source of the "On every scene" group picked in the Layers list (instead of a layer).
    pub pinned: Option<String>,
    /// Scene whose file text must be re-read (after a write), and when.
    reread: Option<(String, f64)>,
    asked: Option<(String, u64)>,
    /// Our latest edit of a scene file, shown until the engine's copy catches up.
    local: Option<LocalText>,
    /// "When this scene starts / ends" as edited, unfinished steps included.
    steps: Vec<StepsDraft>,
    /// The show condition as typed under Details: (layer, text).
    when_buf: Option<(String, String)>,
    patches_at: f64,
    /// The engine saved a live edit (`set_base`) into this scene's file: read it again.
    base_written: Option<String>,
    /// Addresses previewed with `set` while a slider is dragged (released on commit).
    held: BTreeSet<String>,
    project: ProjectText,
}

struct LocalText {
    scene: String,
    text: String,
    /// Debounced edits (a color being dragged, typing): when to send it.
    send_at: Option<f64>,
    /// Reply count of the file query when it was sent; a newer reply replaces this text.
    seq: u64,
}

/// `project.toml` as read for the "On every scene" group, and our latest edit of it.
#[derive(Default)]
struct ProjectText {
    asked: Option<u64>,
    reread_at: f64,
    local: Option<(String, u64)>,
}

struct StepsDraft {
    scene: String,
    key: &'static str,
    cmds: Vec<String>,
    /// The file's list when last read or written: a different one there is someone else's edit.
    seen: Vec<String>,
}

fn file_path(scene: &str) -> String {
    format!("scenes/{scene}.toml")
}

fn file_key(scene: &str) -> String {
    format!("scene.file:{scene}")
}

/// The scene file text (queried once per scene/connection and again after writes); our own
/// latest edit until the engine's copy catches up.
fn scene_text(app: &mut App, scene: &str, now: f64) -> Option<String> {
    let due = app.build.comp.reread.as_ref().is_some_and(|(s, at)| s == scene && now >= *at);
    let asked = app.build.comp.asked.as_ref().is_some_and(|(s, g)| s == scene && *g == app.m.conn_gen);
    if app.m.connected && (!asked || due) {
        app.m.query_as(&file_key(scene), "project.read", Value::map().with("path", file_path(scene)));
        app.build.comp.asked = Some((scene.to_string(), app.m.conn_gen));
        if due {
            app.build.comp.reread = None;
        }
    }
    let seq = app.m.q_seq(&file_key(scene));
    if let Some(l) = &app.build.comp.local
        && l.scene == scene
        && (l.send_at.is_some() || l.seq == seq)
    {
        return Some(l.text.clone());
    }
    app.m.q(&file_key(scene)).and_then(|v| v.get_path("text")).and_then(Value::as_str).map(String::from)
}

/// Write the scene file now and re-read it once the engine has reloaded.
fn write_scene(app: &mut App, scene: &str, text: anyhow::Result<String>, now: f64) {
    match text {
        Ok(text) => send_scene(app, scene, text, now),
        Err(e) => app.m.toast(format!("Couldn't change the scene: {e:#}"), true),
    }
}

/// Write the scene file after a short pause (edits that come in bursts: dragging a color,
/// typing). The page shows the new text right away.
fn write_scene_soon(app: &mut App, scene: &str, text: anyhow::Result<String>, now: f64) {
    match text {
        Ok(text) => {
            if app.build.comp.local.as_ref().is_some_and(|l| l.scene != scene && l.send_at.is_some()) {
                flush(app, now, true);
            }
            let seq = app.m.q_seq(&file_key(scene));
            app.build.comp.local = Some(LocalText { scene: scene.to_string(), text, send_at: Some(now + 0.6), seq });
        }
        Err(e) => app.m.toast(format!("Couldn't change the scene: {e:#}"), true),
    }
}

fn send_scene(app: &mut App, scene: &str, text: String, now: f64) {
    app.m.action("project.write", Value::map().with("path", file_path(scene)).with("text", text.as_str()));
    let seq = app.m.q_seq(&file_key(scene));
    app.build.comp.local = Some(LocalText { scene: scene.to_string(), text, send_at: None, seq });
    app.build.comp.reread = Some((scene.to_string(), now + 0.5));
    app.m.refresh_soon();
}

/// Another page wrote `scenes/<scene>.toml`: read it again, and drop our own copy of it unless
/// it holds an edit that isn't sent yet.
pub fn scene_file_changed(app: &mut App, scene: &str) {
    app.build.comp.reread = Some((scene.to_string(), 0.0));
    if app.build.comp.local.as_ref().is_some_and(|l| l.scene == scene && l.send_at.is_none()) {
        app.build.comp.local = None;
    }
}

/// A live edit was saved with `set_base`: the engine writes it into `scenes/<scene>.toml`, so
/// our copy of the file is read again (later file edits start from the saved value).
pub fn base_written(app: &mut App, scene: &str) {
    app.build.comp.base_written = Some(scene.to_string());
}

/// Send a debounced edit once it's due (`force`: now).
pub(crate) fn flush(app: &mut App, now: f64, force: bool) {
    let Some(l) = app.build.comp.local.as_ref() else { return };
    let Some(at) = l.send_at else { return };
    if force || now >= at {
        let (scene, text) = (l.scene.clone(), l.text.clone());
        send_scene(app, &scene, text, now);
    }
}

/// `project.toml` (read once per connection and every few seconds while shown).
fn project_text(app: &mut App, now: f64) -> Option<String> {
    let p = &mut app.build.comp.project;
    if app.m.connected && (p.asked != Some(app.m.conn_gen) || now >= p.reread_at) {
        p.asked = Some(app.m.conn_gen);
        p.reread_at = now + 5.0;
        app.m.query_as(PROJECT_KEY, "project.read", Value::map().with("path", "project.toml"));
    }
    let seq = app.m.q_seq(PROJECT_KEY);
    if let Some((text, s)) = &app.build.comp.project.local
        && *s == seq
    {
        return Some(text.clone());
    }
    app.m.q(PROJECT_KEY).and_then(|v| v.get_path("text")).and_then(Value::as_str).map(String::from)
}

fn write_project(app: &mut App, text: anyhow::Result<String>, now: f64) {
    match text {
        Ok(text) => {
            app.m.action("project.write", Value::map().with("path", "project.toml").with("text", text.as_str()));
            let seq = app.m.q_seq(PROJECT_KEY);
            app.build.comp.project.local = Some((text, seq));
            app.build.comp.project.reread_at = now + 0.5;
            app.m.refresh_soon();
        }
        Err(e) => app.m.toast(format!("Couldn't change the project settings: {e:#}"), true),
    }
}

/// Show `scene` (and its layer `layer`) in the scene editor: "Used in" rows on other pages.
pub fn open(app: &mut App, scene: &str, layer: Option<&str>) {
    select_scene(app, scene);
    app.build.canvas.selected = layer.map(String::from);
    app.open_view(ViewId::Composition);
}

fn select_scene(app: &mut App, scene: &str) {
    app.build.canvas.scene = Some(scene.to_string());
    app.build.canvas.selected = None;
    app.build.comp.pinned = None;
    app.build.comp.new_open = false;
    // edits apply to "Up next": bring the scene there
    app.m.command(Op::SceneGo { scene: scene.to_string() });
}

/// Friendly name for a source (`cam_kit` → its label or "Cam kit"; `patch.x` → its label;
/// `color:#…` → "Solid color").
pub fn source_label(app: &App, src: &str) -> String {
    if let Some(p) = src.strip_prefix("patch.") {
        return app
            .m
            .q_list("patches")
            .iter()
            .find(|e| e.get_path("id").and_then(Value::as_str) == Some(p))
            .and_then(|e| e.get_path("label").and_then(Value::as_str))
            .filter(|l| !l.is_empty() && *l != p)
            .map_or_else(|| nice(p), nice);
    }
    if src.starts_with("color:") || src.starts_with('#') {
        return "Solid color".into();
    }
    app.m
        .q_list("sources")
        .iter()
        .find(|s| s.get_path("name").and_then(Value::as_str) == Some(src))
        .and_then(|s| s.get_path("label").and_then(Value::as_str).filter(|l| !l.is_empty()).map(String::from))
        .unwrap_or_else(|| nice(src))
}

fn scenes(app: &App) -> Vec<(String, String, i64)> {
    app.m
        .q_list("scenes")
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let name = s.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
            let label = s.get_path("label").and_then(Value::as_str).unwrap_or(&name).to_string();
            (name, label, s.get_path("key").and_then(Value::as_i64).unwrap_or(i as i64 + 1))
        })
        .collect()
}

fn scene_label(app: &App, scene: &str) -> String {
    scenes(app).into_iter().find(|(n, _, _)| n == scene).map_or_else(|| nice(scene), |(_, l, _)| nice(&l))
}

/// Every source a layer can show: saved cameras and media (also while video isn't running),
/// then web and generative ones.
fn all_sources(app: &App) -> Vec<String> {
    let saved = app.m.q("project.sources").and_then(|v| v.get_path("sources")).and_then(Value::as_list).unwrap_or(&[]);
    let mut out: Vec<String> = Vec::new();
    for s in saved.iter().chain(app.m.q_list("sources")) {
        if let Some(n) = s.get_path("name").and_then(Value::as_str)
            && !out.iter().any(|o| o == n)
        {
            out.push(n.to_string());
        }
    }
    out.extend(
        app.m
            .q_list("patches")
            .iter()
            .filter(|p| matches!(p.get_path("kind").and_then(Value::as_str), Some("web" | "shader" | "particles" | "script")))
            .filter(|p| matches!(p.get_path("layer").and_then(Value::as_str), Some("source" | "overlay")))
            .filter_map(|p| p.get_path("id").and_then(Value::as_str).map(|id| format!("patch.{id}"))),
    );
    out
}

fn patch_entry(app: &App, id: &str) -> Option<Value> {
    app.m.q_list("patches").iter().find(|p| p.get_path("id").and_then(Value::as_str) == Some(id)).cloned()
}

/// The five groups of the Add layer menu, in order.
const GROUPS: [&str; 5] = ["Cameras & capture", "Media", "Web", "Generative", "Color"];

fn group_of(k: sources::SourceKind) -> usize {
    use sources::SourceKind::*;
    match k {
        Camera => 0,
        Video | Image => 1,
        Web => 2,
        Generative => 3,
        Color => 4,
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let now = ui.input(|i| i.time);
    for q in ["sources", "project.sources"] {
        if app.m.q_seq(q) == 0 && app.m.connected {
            app.m.query(q, Value::Null);
        }
    }
    crate::views::transitions::poll(app, now);
    links::keep_fresh(app);
    // source settings and show conditions follow the sources' live state
    if app.m.connected && now - app.build.comp.patches_at > 2.0 {
        app.build.comp.patches_at = now;
        app.m.query("patches", Value::Null);
    }
    if let Some(s) = app.build.comp.base_written.take() {
        app.build.comp.reread = Some((s, now + 0.8));
    }
    flush(app, now, false);
    if app.build.comp.local.as_ref().is_some_and(|l| l.send_at.is_some()) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
    let list = scenes(app);
    let scene = app.build.canvas.scene.clone().filter(|s| list.iter().any(|(n, _, _)| n == s)).unwrap_or_else(|| {
        let pv = app.m.str("show.scene.preview").to_string();
        if pv.is_empty() { list.first().map(|(n, _, _)| n.clone()).unwrap_or_default() } else { pv }
    });
    if app.build.comp.pinned.as_ref().is_some_and(|p| !patches::overlay_ids(app).contains(p)) {
        app.build.comp.pinned = None;
    }
    let h = ui.available_height();
    let pad = 2.0 * spacing::L + 2.0;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = spacing::L;
        ui.allocate_ui_with_layout(Vec2::new(LEFT_W, h), Layout::top_down(Align::Min), |ui| {
            ui.set_width(LEFT_W);
            left_column(app, ui, &list, &scene, h, now);
        });
        let rest = ui.available_width();
        ui.allocate_ui_with_layout(Vec2::new(rest, h), Layout::top_down(Align::Min), |ui| {
            ui.set_width(rest);
            if app.build.comp.new_open {
                widgets::panel(ui, &t, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(h - pad);
                    egui::ScrollArea::vertical().id_salt("comp-new").auto_shrink([false, false]).show(ui, |ui| new_scene_form(app, ui, &list, now));
                });
                return;
            }
            if scene.is_empty() {
                widgets::panel(ui, &t, |ui| {
                    ui.set_width(ui.available_width());
                    ui.set_min_height(h - pad);
                    ui.add_space((h * 0.18).max(0.0));
                    if widgets::empty_state(
                        ui,
                        &t,
                        icon::LAYERS,
                        "Scenes",
                        "A scene is one arrangement of your sources as layers, for the Main and Vertical canvases.",
                        Some("Create your first scene"),
                    ) {
                        start_new_scene(app);
                    }
                });
                return;
            }
            ui.horizontal_top(|ui| {
                ui.spacing_mut().item_spacing.x = spacing::L;
                let mid = (rest - RIGHT_W - spacing::L).max(240.0);
                ui.allocate_ui_with_layout(Vec2::new(mid, h), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(mid);
                    center(app, ui, &scene, now);
                });
                ui.allocate_ui_with_layout(Vec2::new(RIGHT_W, h), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(RIGHT_W);
                    widgets::panel(ui, &t, |ui| {
                        ui.set_width(ui.available_width());
                        ui.set_min_height(h - pad);
                        egui::ScrollArea::vertical().id_salt("comp-inspector").auto_shrink([false, false]).show(ui, |ui| inspector(app, ui, &scene, now));
                    });
                });
            });
        });
    });
}

fn start_new_scene(app: &mut App) {
    app.build.comp.new_open = true;
    app.build.comp.new_name.clear();
    app.build.comp.new_layout = 0;
}

// ---- left: scenes + layers ------------------------------------------------------------------------

fn left_column(app: &mut App, ui: &mut egui::Ui, list: &[(String, String, i64)], current: &str, h: f32, now: f64) {
    let t = app.t.clone();
    let pad = 2.0 * spacing::L + 2.0;
    let list_h = (list.len().max(1) as f32 * (SCENE_ROW_H + ui.spacing().item_spacing.y)).min((h * 0.4).max(SCENE_ROW_H + 8.0));
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        if widgets::pane_header(ui, &t, "Scenes", Some(list.len()), Some("New scene")) {
            start_new_scene(app);
        }
        egui::ScrollArea::vertical().id_salt("comp-scenes").max_height(list_h).auto_shrink([false, true]).show(ui, |ui| scene_rows(app, ui, list, current));
    });
    ui.add_space(spacing::M);
    let rest = (ui.available_height() - pad).max(160.0);
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(rest);
        if current.is_empty() {
            widgets::pane_header(ui, &t, "Layers", None, None);
            widgets::hint(ui, &t, "Layers of the picked scene show here.");
        } else {
            layers_pane(app, ui, current, rest, now);
        }
    });
}

/// Height of a scene row: a 16:9 thumbnail with the name, state and key beside it.
const SCENE_ROW_H: f32 = 60.0;

fn scene_rows(app: &mut App, ui: &mut egui::Ui, list: &[(String, String, i64)], current: &str) {
    let t = app.t.clone();
    let program = app.m.str("show.scene.program").to_string();
    let preview = app.m.str("show.scene.preview").to_string();
    let on_air = crate::views::status::on_air(app);
    // Live pictures for the thumbnails: the scene that's up next is the engine's preview
    // canvas; the others are drawn from their layers' live source tiles (the atlas).
    use crate::frames::Canvas;
    let hz = app.atlas_hz().min(10.0);
    let _ = crate::views::monitor::texture(app, Canvas::Atlas, hz);
    let pv_tex = crate::views::monitor::texture(app, Canvas::Preview, hz);
    if list.is_empty() {
        widgets::hint(ui, &t, "No scenes yet.");
    }
    for (name, label, key) in list {
        let (state, word) = if *name == program && on_air {
            (Some(t.tally_program()), "On air")
        } else if *name == preview {
            (Some(t.tally_preview()), "Up next")
        } else {
            (None, "")
        };
        let layers = app
            .m
            .q_list("scenes")
            .iter()
            .find(|s| s.get_path("name").and_then(Value::as_str) == Some(name.as_str()))
            .and_then(|s| s.get_path("nodes").and_then(Value::as_list))
            .map_or(0, |l| l.len());
        let count = match layers {
            0 => "No layers".to_string(),
            1 => "1 layer".to_string(),
            n => format!("{n} layers"),
        };
        let sub = if word.is_empty() { count } else { format!("{word} · {count}") };
        let title = nice(label);
        let selected = name == current;
        let (rect, r) = ui.allocate_exact_size(Vec2::new(ui.available_width(), SCENE_ROW_H), egui::Sense::click());
        r.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &title));
        if ui.is_rect_visible(rect) {
            let p = ui.painter();
            if selected || r.hovered() {
                let fill = if selected { t.selection } else { se_ui_kit::theme::mix(t.surface, t.fg, 0.04) };
                p.rect_filled(rect, egui::CornerRadius::same(radius::CONTROL), fill);
            }
            let th = SCENE_ROW_H - 12.0;
            let thumb = egui::Rect::from_min_size(rect.left_top() + Vec2::new(6.0, 6.0), Vec2::new(th * 16.0 / 9.0, th));
            match pv_tex.as_ref().filter(|_| *name == preview) {
                Some(ft) => {
                    let uv = egui::Rect::from_min_max(egui::Pos2::ZERO, egui::pos2(1.0, 1.0));
                    let shape = egui::epaint::RectShape::filled(thumb, egui::CornerRadius::same(radius::TILE), egui::Color32::WHITE).with_texture(ft.id, uv);
                    p.add(egui::Shape::Rect(shape));
                }
                None => crate::views::monitor::scene_thumb(app, ui, thumb, name),
            }
            let (edge, width) = match state {
                Some(c) => (c, 2.0),
                None => (t.border, 1.0),
            };
            p.rect_stroke(thumb, egui::CornerRadius::same(radius::TILE), egui::Stroke::new(width, edge), egui::StrokeKind::Outside);
            // key chip on the right, name and state between
            let cap = egui::Rect::from_center_size(egui::pos2(rect.right() - 20.0, rect.center().y), Vec2::new(22.0, 22.0));
            p.rect(cap, 5, t.inset, egui::Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
            p.text(cap.center(), egui::Align2::CENTER_CENTER, key.to_string(), font_mono(type_scale::SMALL), t.text_dim);
            let x = thumb.right() + 10.0;
            let w = (cap.left() - 8.0 - x).max(20.0);
            let line = |text: String, font: egui::FontId, color: egui::Color32| {
                let mut job = egui::text::LayoutJob::simple_singleline(text, font, color);
                job.wrap = egui::text::TextWrapping::truncate_at_width(w);
                ui.fonts_mut(|f| f.layout_job(job))
            };
            let g1 = line(title.clone(), font_medium(type_scale::BODY), t.fg);
            let g2 = line(sub, se_ui_kit::theme::font(type_scale::SMALL), state.unwrap_or(t.text_dim));
            let top = rect.center().y - (g1.size().y + 2.0 + g2.size().y) / 2.0;
            p.galley(egui::pos2(x, top), g1.clone(), t.fg);
            p.galley(egui::pos2(x, top + g1.size().y + 2.0), g2, t.text_dim);
        }
        let r = r.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(format!("Key {key}"));
        if r.clicked() {
            if selected {
                // the scene's own settings
                app.build.canvas.selected = None;
                app.build.comp.pinned = None;
                app.build.comp.new_open = false;
            } else {
                select_scene(app, name);
            }
        }
    }
}

/// One row of the scene's layer list (front first).
struct LayerRow {
    id: String,
    src: String,
    name: String,
    sub: String,
    visible: bool,
    fx: usize,
    modulated: bool,
    triggered: bool,
    waiting: bool,
    on_canvas: bool,
}

fn layer_rows(app: &App, scene: &str, text: Option<&str>, canvas_name: &str) -> Vec<LayerRow> {
    // the file first (a layer just added isn't in the loaded config yet), else the config
    let layers = match text {
        Some(x) => scene_edit::layers(x, canvas_name),
        None => canvas::node_ids(app, scene)
            .into_iter()
            .map(|id| scene_edit::LayerInfo { src: app.build.node_src(scene, &id).unwrap_or(&id).to_string(), id, fx: 0 })
            .collect(),
    };
    let mut rows: Vec<(usize, i64, LayerRow)> = layers
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let p = format!("scene.{scene}.node.{}", l.id);
            let same_src = layers.iter().filter(|o| o.src == l.src).count() > 1;
            let row = LayerRow {
                id: l.id.clone(),
                src: l.src.clone(),
                name: source_label(app, &l.src),
                sub: if same_src { nice(&l.id) } else { String::new() },
                visible: app.m.get(&format!("{p}.visible")).is_none_or(Value::truthy),
                fx: l.fx,
                modulated: links::modulated(app, &format!("{p}.")),
                triggered: links::triggered(app, &format!("{p}.")),
                waiting: node_when(app, scene, &l.id).is_some_and(|w| !when_now(app, &w).unwrap_or(false)),
                on_canvas: app.m.get(&format!("{p}.rect.{canvas_name}")).is_some(),
            };
            (i, app.m.get(&format!("{p}.z.{canvas_name}")).and_then(Value::as_i64).unwrap_or(0), row)
        })
        .collect();
    // later entries (and higher z) draw on top: list them first
    rows.sort_by_key(|(i, z, _)| (std::cmp::Reverse(*z), std::cmp::Reverse(*i)));
    rows.into_iter().map(|(_, _, r)| r).collect()
}

fn add_popup_id(scene: &str) -> egui::Id {
    egui::Id::new(("comp-add-layer", scene))
}

fn layers_pane(app: &mut App, ui: &mut egui::Ui, scene: &str, h: f32, now: f64) {
    let t = app.t.clone();
    let text = scene_text(app, scene, now);
    let canvas_name = canvas::edit_canvas(app);
    let rows = layer_rows(app, scene, text.as_deref(), canvas_name);
    let header = ui.scope(|ui| widgets::pane_header(ui, &t, "Layers", Some(rows.len()), Some("Add layer")));
    egui::Popup::new(add_popup_id(scene), ui.ctx().clone(), header.response.rect, ui.layer_id())
        .kind(egui::PopupKind::Menu)
        .layout(Layout::top_down_justified(Align::Min))
        .align(egui::RectAlign::BOTTOM_END)
        .open_memory(header.inner.then_some(egui::SetOpenCommand::Toggle))
        .close_behavior(egui::PopupCloseBehavior::CloseOnClickOutside)
        .width(300.0)
        .show(|ui| add_layer_menu(app, ui, scene, text.as_deref(), now));
    let overlays = patches::overlay_ids(app);
    egui::ScrollArea::vertical().id_salt("comp-layers").max_height((h - 44.0).max(80.0)).auto_shrink([false, true]).show(ui, |ui| {
        if !overlays.is_empty() {
            widgets::group_label(ui, &t, "On every scene");
            let project = project_text(app, now);
            for id in &overlays {
                pinned_row(app, ui, id, project.as_deref(), now);
            }
        }
        widgets::group_label(ui, &t, &scene_label(app, scene));
        if rows.is_empty() {
            widgets::hint(ui, &t, "No layers yet.");
        }
        for row in &rows {
            layer_row(app, ui, scene, row, text.as_deref(), now);
        }
    });
}

fn pinned_row(app: &mut App, ui: &mut egui::Ui, id: &str, project: Option<&str>, now: f64) {
    let t = app.t.clone();
    let src = format!("patch.{id}");
    let shown = project.is_none_or(|p| patches::on_every_scene(p, id));
    let selected = app.build.comp.pinned.as_deref() == Some(id);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = spacing::XS;
        let tip = if shown { "Drawn above every scene: click to stop" } else { "Only where you place it: click to draw it above every scene" };
        if widgets::icon_button(ui, &t, if shown { icon::EYE } else { icon::EYE_OFF }, tip).clicked() {
            match project {
                Some(p) => write_project(app, patches::set_on_every_scene(p, id, !shown), now),
                None => app.m.toast("The project settings aren't loaded yet. Try again in a second.", true),
            }
        }
        let kind = sources::source_kind(app, &src);
        let trailing = if shown { "" } else { "off" };
        if widgets::list_row(ui, &t, kind.icon(), &source_label(app, &src), "", trailing, selected).clicked() {
            app.build.comp.pinned = Some(id.to_string());
            app.build.canvas.selected = None;
        }
    });
}

fn layer_row(app: &mut App, ui: &mut egui::Ui, scene: &str, row: &LayerRow, text: Option<&str>, now: f64) {
    let t = app.t.clone();
    let selected = app.build.comp.pinned.is_none() && app.build.canvas.selected.as_deref() == Some(row.id.as_str());
    let mut marks = Vec::new();
    let mut words = Vec::new();
    if row.fx > 0 {
        marks.push(format!("{} {}", icon::WAND, row.fx));
        words.push(if row.fx == 1 { "1 effect".to_string() } else { format!("{} effects", row.fx) });
    }
    if row.modulated {
        marks.push(links::SINE.to_string());
        words.push("a setting follows a signal".into());
    }
    if row.triggered {
        marks.push(icon::BOLT.to_string());
        words.push("a trigger changes it".into());
    }
    if row.waiting {
        marks.push(icon::CLOCK.to_string());
        words.push("hidden right now by its show rule".into());
    }
    if !row.on_canvas {
        words.push(format!("not on the {} canvas", if canvas::edit_canvas(app) == "tall" { "vertical" } else { "main" }));
    }
    let resp = ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = spacing::XS;
        let tip = if row.visible { "Shown: click to hide" } else { "Hidden: click to show" };
        if widgets::icon_button(ui, &t, if row.visible { icon::EYE } else { icon::EYE_OFF }, tip).clicked() {
            app.m.command(Op::SetBase { address: format!("scene.{scene}.node.{}.visible", row.id), value: Value::Bool(!row.visible) });
            base_written(app, scene);
        }
        let kind = sources::source_kind(app, &row.src);
        let inner = widgets::draggable_list_row(ui, &t, kind.icon(), &row.name, &row.sub, &marks.join("  "), selected);
        inner.dnd_set_drag_payload(row.id.clone());
        inner
    });
    let (inner, resp) = (resp.inner, resp.response);
    let inner = if words.is_empty() { inner } else { inner.on_hover_text(words.join(" · ")) };
    if inner.clicked() {
        app.build.canvas.selected = Some(row.id.clone());
        app.build.comp.pinned = None;
        app.select(format!("scene.{scene}.node.{}", row.id));
    }
    // dropping another layer here puts it just in front of (upper half) or behind this one
    if let Some(p) = resp.dnd_hover_payload::<String>()
        && *p != row.id
        && let Some(pos) = ui.ctx().pointer_interact_pos()
    {
        let y = if pos.y < resp.rect.center().y { resp.rect.top() } else { resp.rect.bottom() };
        ui.painter().hline(resp.rect.x_range(), y, egui::Stroke::new(2.0, t.accent));
    }
    if let Some(p) = resp.dnd_release_payload::<String>()
        && *p != row.id
        && let Some(text) = text
    {
        let in_front = ui.ctx().pointer_interact_pos().is_some_and(|pos| pos.y < resp.rect.center().y);
        write_scene(app, scene, scene_edit::place_layer(text, &p, &row.id, in_front), now);
    }
}

fn add_layer_menu(app: &mut App, ui: &mut egui::Ui, scene: &str, text: Option<&str>, now: f64) {
    let t = app.t.clone();
    let mut groups: [Vec<String>; 5] = Default::default();
    for s in all_sources(app) {
        let k = sources::source_kind(app, &s);
        groups[group_of(k)].push(s);
    }
    let mut pick = None;
    egui::ScrollArea::vertical().max_height(440.0).show(ui, |ui| {
        for (g, srcs) in groups.iter().enumerate() {
            if g == 4 {
                widgets::group_label(ui, &t, GROUPS[g]);
                if widgets::list_row(ui, &t, icon::PALETTE, "Solid color", "Fills the canvas; pick the color after", "", false).clicked() {
                    pick = Some("color:#000000".to_string());
                }
                continue;
            }
            if srcs.is_empty() {
                continue;
            }
            widgets::group_label(ui, &t, GROUPS[g]);
            for s in srcs {
                let k = sources::source_kind(app, s);
                if widgets::list_row(ui, &t, k.icon(), &source_label(app, s), "", "", false).clicked() {
                    pick = Some(s.clone());
                }
            }
        }
    });
    ui.separator();
    if widgets::button_ex(ui, &t, Some(icon::PLUS), "New source…", Kind::Ghost, Size::Small, 0.0, true).clicked() {
        app.open_view(ViewId::Sources);
        ui.close();
    }
    if let Some(src) = pick {
        match text {
            Some(text) => match scene_edit::add_layer(text, &src) {
                Ok((text, id)) => {
                    write_scene(app, scene, Ok(text), now);
                    app.build.canvas.selected = Some(id);
                    app.build.comp.pinned = None;
                }
                Err(e) => write_scene(app, scene, Err(e), now),
            },
            None => app.m.toast("The scene file isn't loaded yet. Try again in a second.", true),
        }
        ui.close();
    }
}

/// Cameras to fill a starting layout with, best first: (name, picture width / height). Cameras
/// with a picture come first; without the camera service, every camera file in the project.
pub fn cameras(app: &App) -> Vec<(String, f64)> {
    let live = app.m.q_list("sources");
    if !live.is_empty() {
        let mut cams: Vec<(bool, String, f64)> = live
            .iter()
            .filter(|s| s.get_path("kind").and_then(Value::as_str) == Some("camera"))
            .filter_map(|s| {
                let name = s.get_path("name").and_then(Value::as_str)?.to_string();
                let size = s.get_path("size").and_then(Value::as_list).map(|l| l.iter().filter_map(Value::as_f64).collect::<Vec<_>>()).unwrap_or_default();
                let (w, h) = match size.as_slice() {
                    [w, h] if *w > 0.0 && *h > 0.0 => (*w, *h),
                    _ => (s.get_path("width").and_then(Value::as_f64).unwrap_or(0.0), s.get_path("height").and_then(Value::as_f64).unwrap_or(0.0)),
                };
                let aspect = if w > 0.0 && h > 0.0 { w / h } else { 16.0 / 9.0 };
                Some((!s.get_path("signal").is_some_and(Value::truthy), name, aspect))
            })
            .collect();
        cams.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        return cams.into_iter().map(|(_, n, a)| (n, a)).collect();
    }
    let mut names: Vec<String> = app
        .m
        .q_list("project.files")
        .iter()
        .filter(|f| f.get_path("kind").and_then(Value::as_str) == Some("sources"))
        .filter_map(|f| f.get_path("name").and_then(Value::as_str).map(String::from))
        .collect();
    names.sort();
    names.into_iter().map(|n| (n, 16.0 / 9.0)).collect()
}

/// New scene: a starting layout (drawn as tiles), a name, then Create.
fn new_scene_form(app: &mut App, ui: &mut egui::Ui, list: &[(String, String, i64)], now: f64) {
    let t = app.t.clone();
    widgets::detail_header(ui, &t, icon::LAYERS, "New scene", "Pick how it starts, name it, then create it.", |_| {});
    widgets::group_label(ui, &t, "Start from");
    let cams = cameras(app);
    let tile_w = 188.0;
    let tile = Vec2::new(tile_w, tile_w * 9.0 / 16.0 + 24.0);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(spacing::S);
        for (i, l) in scene_edit::START_LAYOUTS.iter().enumerate() {
            let usable = l.slots() == 0 || !cams.is_empty();
            let r = layout_tile(ui, &t, tile, l, app.build.comp.new_layout == i, usable);
            let r = if usable { r } else { r.on_hover_text("Needs a camera source. Add one in Sources.") };
            if r.clicked() && usable {
                app.build.comp.new_layout = i;
            }
        }
    });
    let layout = scene_edit::START_LAYOUTS.get(app.build.comp.new_layout).unwrap_or(&scene_edit::START_LAYOUTS[0]);
    ui.add_space(spacing::S);
    if layout.slots() > 0 {
        if cams.is_empty() {
            widgets::hint(ui, &t, "No camera sources yet. Add one in Sources, or start from Blank.");
        } else {
            let shown: Vec<String> = (0..layout.slots()).map(|i| source_label(app, &cams[i % cams.len()].0)).collect();
            widgets::hint(ui, &t, &format!("Shows {}. You can swap sources after.", join_words(&shown)));
        }
    } else {
        widgets::hint(ui, &t, "An empty canvas. Add layers after.");
    }
    ui.add_space(spacing::M);
    widgets::prop_row(ui, &t, "Name", |ui| {
        ui.add(widgets::field(&mut app.build.comp.new_name).hint_text("e.g. Chill cams").desired_width(ui.available_width().min(320.0)));
    });
    ui.add_space(spacing::M);
    ui.horizontal(|ui| {
        let name = scene_edit::slug(&app.build.comp.new_name);
        let taken = list.iter().any(|(n, _, _)| *n == name);
        let ok = !name.is_empty() && !taken && (layout.slots() == 0 || !cams.is_empty());
        if widgets::button_ex(ui, &t, None, "Create", Kind::Primary, Size::Medium, 0.0, ok).clicked() {
            let label = app.build.comp.new_name.trim().to_string();
            let key = (1..=9).find(|k| !list.iter().any(|(_, _, used)| used == k));
            let sizes = [crate::views::monitor::canvas_size("wide"), crate::views::monitor::canvas_size("tall")].map(|[w, h]| [w as f64, h as f64]);
            match scene_edit::new_scene(&label, key, layout, &cams, sizes) {
                Ok(text) => {
                    app.m.action("project.write", Value::map().with("path", file_path(&name)).with("text", text));
                    app.build.canvas.scene = Some(name.clone());
                    app.build.canvas.selected = None;
                    app.build.comp.pinned = None;
                    app.build.comp.new_open = false;
                    app.build.comp.new_name.clear();
                    app.build.comp.reread = Some((name, now + 0.5));
                    app.m.refresh_soon();
                }
                Err(e) => app.m.toast(format!("Couldn't make the scene: {e:#}"), true),
            }
        }
        if widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
            app.build.comp.new_open = false;
        }
        if taken {
            widgets::hint(ui, &t, "That name is taken.");
        }
    });
}

/// "a, b and c".
fn join_words(words: &[String]) -> String {
    match words {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// A "Start from" choice: the layout drawn as boxes on a little screen, its name below.
fn layout_tile(ui: &mut egui::Ui, t: &se_ui_kit::Theme, size: Vec2, l: &scene_edit::StartLayout, selected: bool, usable: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, usable, selected, l.label));
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let p = ui.painter();
    let bg = if selected {
        se_ui_kit::theme::mix(t.surface, t.accent, 0.14)
    } else if resp.hovered() && usable {
        t.surface_hi
    } else {
        t.surface
    };
    p.rect(rect, radius::CONTROL, bg, egui::Stroke::new(1.0, if selected { t.accent } else { t.border }), egui::StrokeKind::Inside);
    let screen = egui::Rect::from_min_size(rect.min + Vec2::splat(6.0), Vec2::new(size.x - 12.0, (size.x - 12.0) * 9.0 / 16.0));
    p.rect_filled(screen, 4, t.inset);
    let fill = if usable { if selected { t.accent } else { t.text_dim } } else { t.text_faint };
    for r in l.wide {
        let b = egui::Rect::from_min_size(
            screen.min + Vec2::new(r[0] as f32 * screen.width(), r[1] as f32 * screen.height()),
            Vec2::new(r[2] as f32 * screen.width(), r[3] as f32 * screen.height()),
        )
        .shrink(1.0);
        p.rect(b, 2, se_ui_kit::theme::mix(t.inset, fill, 0.55), egui::Stroke::new(1.0, fill), egui::StrokeKind::Inside);
    }
    let text = if usable { t.fg } else { t.text_faint };
    p.text(egui::pos2(rect.center().x, rect.bottom() - 11.0), egui::Align2::CENTER_CENTER, l.label, font_medium(type_scale::SMALL), text);
    if usable { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

// ---- center: the canvas -----------------------------------------------------------------------------

fn center(app: &mut App, ui: &mut egui::Ui, scene: &str, now: f64) {
    let t = app.t.clone();
    canvas::toolbar(app, ui);
    ui.add_space(spacing::S);
    if crate::views::status::on_air(app) && app.m.str("show.scene.program") == scene {
        widgets::callout(ui, &t, Tone::Danger, icon::LIVE, "On air: viewers see every change right away.", "", None);
        ui.add_space(spacing::S);
    }
    let avail = Vec2::new(ui.available_width(), (ui.available_height() - spacing::S).max(200.0));
    let area = canvas::editors(app, ui, scene, avail);
    let empty = scene_text(app, scene, now).is_some_and(|x| scene_edit::layer_order(&x, "wide").is_empty());
    if empty {
        ui.scope_builder(egui::UiBuilder::new().max_rect(area), |ui| {
            ui.vertical_centered(|ui| {
                ui.add_space((area.height() * 0.5 - 44.0).max(0.0));
                ui.label(RichText::new("This scene has no layers yet.").font(font_medium(type_scale::BODY)).color(t.fg));
                ui.add_space(spacing::S);
                if widgets::button_ex(ui, &t, Some(icon::PLUS), "Add layer", Kind::Primary, Size::Medium, 0.0, true).clicked() {
                    egui::Popup::open_id(ui.ctx(), add_popup_id(scene));
                }
            });
        });
    }
}

// ---- right: the inspector ------------------------------------------------------------------------------

fn inspector(app: &mut App, ui: &mut egui::Ui, scene: &str, now: f64) {
    if let Some(id) = app.build.comp.pinned.clone() {
        pinned_inspector(app, ui, &id, now);
        return;
    }
    let text = scene_text(app, scene, now);
    let exists = |app: &App, id: &str| match text.as_deref() {
        Some(x) => scene_edit::layer_order(x, "wide").iter().any(|l| l == id),
        None => canvas::node_ids(app, scene).iter().any(|l| l == id),
    };
    match app.build.canvas.selected.clone() {
        Some(sel) if exists(app, &sel) => layer_inspector(app, ui, scene, &sel, text.as_deref(), now),
        Some(_) if text.is_some() => {
            app.build.canvas.selected = None;
            scene_inspector(app, ui, scene, text.as_deref(), now);
        }
        _ => scene_inspector(app, ui, scene, text.as_deref(), now),
    }
}

fn canvas_word(canvas_name: &str) -> &'static str {
    if canvas_name == "tall" { "Vertical" } else { "Main" }
}

/// A numeric setting on a slider (`%` shows 0–1 as percent).
fn slider(ui: &mut egui::Ui, v: &mut f64, range: RangeInclusive<f64>, suffix: &str, badge: bool) -> egui::Response {
    let span = range.end() - range.start();
    let decimals = if span <= 0.2 {
        3
    } else if span <= 4.0 {
        2
    } else if span <= 10.0 {
        1
    } else {
        0
    };
    // fixed value box and ∿ slot, so rows line up and never run past the inspector
    const VALUE_W: f32 = 64.0;
    let gap = ui.spacing().item_spacing.x;
    let badge_w = if badge { 28.0 + gap } else { 0.0 };
    ui.spacing_mut().slider_width = (ui.available_width() - VALUE_W - gap - badge_w - 2.0).max(40.0);
    ui.spacing_mut().interact_size.x = VALUE_W;
    let s = egui::Slider::new(v, range);
    let s = if suffix == "%" {
        s.custom_formatter(|v, _| format!("{:.0}%", v * 100.0)).custom_parser(|s| s.trim().trim_end_matches('%').trim().parse::<f64>().ok().map(|x| x / 100.0))
    } else {
        s.suffix(suffix).fixed_decimals(decimals)
    };
    ui.add(s)
}

/// Preview a dragged live setting with `set`; save it with `set_base` (the engine writes it into
/// the scene file) when the drag ends or a value is typed.
fn commit_live(app: &mut App, scene: &str, addr: &str, r: &egui::Response, v: f64) {
    let value = Value::Float((v * 10000.0).round() / 10000.0);
    if r.drag_stopped() || (r.changed() && !r.dragged()) {
        app.m.command(Op::SetBase { address: addr.to_string(), value });
        if app.build.comp.held.remove(addr) {
            app.m.command(Op::Release { address: addr.to_string() });
        }
        base_written(app, scene);
    } else if r.dragged() && r.changed() {
        app.m.command(Op::Set { address: addr.to_string(), value });
        app.build.comp.held.insert(addr.to_string());
    }
}

/// A live layer number: label, slider, and ∿ (`follow`: the range a signal moves it over).
#[allow(clippy::too_many_arguments)]
fn live_row(
    app: &mut App,
    ui: &mut egui::Ui,
    scene: &str,
    label: &str,
    addr: &str,
    default: f64,
    range: RangeInclusive<f64>,
    suffix: &str,
    follow: Option<(f64, f64)>,
) {
    let t = app.t.clone();
    let cur = app.m.get(addr).and_then(Value::as_f64).unwrap_or(default);
    widgets::prop_row(ui, &t, label, |ui| {
        let mut v = cur;
        let r = slider(ui, &mut v, range, suffix, follow.is_some());
        commit_live(app, scene, addr, &r, v);
        if let Some(rg) = follow {
            links::modulate_button(app, ui, addr, label, rg);
        }
    });
}

fn layer_title(app: &App, row_id: &str, src: &str) -> String {
    let label = source_label(app, src);
    let plain = src.strip_prefix("patch.").unwrap_or(src);
    if row_id == src || row_id == plain || src.starts_with("color:") || src.starts_with('#') { label } else { nice(row_id) }
}

fn layer_inspector(app: &mut App, ui: &mut egui::Ui, scene: &str, sel: &str, text: Option<&str>, now: f64) {
    let t = app.t.clone();
    let canvas_name = canvas::edit_canvas(app);
    let src = app
        .build
        .node_src(scene, sel)
        .map(String::from)
        .or_else(|| text.and_then(|x| scene_edit::layer_prop(x, sel, "src")))
        .unwrap_or_else(|| sel.to_string());
    let kind = sources::source_kind(app, &src);
    let p = format!("scene.{scene}.node.{sel}");
    let title = layer_title(app, sel, &src);
    let mut edit_source = false;
    let color = src.starts_with("color:") || src.starts_with('#');
    widgets::detail_header(ui, &t, kind.icon(), &title, &format!("{} · {}", source_label(app, &src), kind.label()), |ui| {
        if !color {
            edit_source = widgets::button_ex(ui, &t, None, "Edit source", Kind::Secondary, Size::Small, 0.0, true).clicked();
        }
    });
    if edit_source {
        sources::open(app, ui.ctx(), &src);
    }
    let node = canvas::editor_nodes(app, scene, canvas_name).into_iter().find(|n| n.id == sel);
    let visible = app.m.get(&format!("{p}.visible")).is_none_or(Value::truthy);
    let when = text.and_then(|x| scene_edit::layer_when(x, sel));
    show_status(app, ui, visible, when.as_deref());
    let Some(text) = text else {
        widgets::hint(ui, &t, "Loading the scene…");
        return;
    };

    // what it shows
    widgets::inspector_section(ui, &t, ("layer-source", scene), "Source", true, |_| {}, |ui| source_section(app, ui, scene, sel, &src, kind, text, now));

    // place and motion
    let cw = canvas_word(canvas_name);
    widgets::inspector_section(
        ui,
        &t,
        ("layer-transform", scene),
        "Transform",
        true,
        |ui| {
            widgets::hint(ui, &t, cw);
        },
        |ui| match &node {
            Some(n) => transform_section(app, ui, scene, sel, &src, n, canvas_name),
            None => {
                widgets::hint(ui, &t, &format!("This layer isn't on the {} canvas.", cw.to_lowercase()));
            }
        },
    );

    if let Some(n) = &node {
        widgets::inspector_section(
            ui,
            &t,
            ("layer-crop", scene),
            "Crop",
            false,
            |ui| {
                widgets::hint(ui, &t, cw);
            },
            |ui| {
                let mut crop = n.crop;
                let mut changed = false;
                let mut done = false;
                for (i, label) in ["Left", "Top", "Right", "Bottom"].iter().enumerate() {
                    widgets::prop_row(ui, &t, label, |ui| {
                        let mut v = crop[i] as f64;
                        let r = slider(ui, &mut v, 0.0..=0.45, "%", false);
                        crop[i] = v as f32;
                        changed |= r.changed();
                        done |= r.drag_stopped() || (r.changed() && !r.dragged());
                    });
                }
                if changed || done {
                    canvas::apply_shape(app, scene, canvas_name, sel, crop, n.radius, done);
                }
            },
        );
    }

    widgets::inspector_section(
        ui,
        &t,
        ("layer-look", scene),
        "Look",
        true,
        |_| {},
        |ui| look_section(app, ui, scene, sel, node.as_ref(), canvas_name, text, now),
    );

    widgets::inspector_section(
        ui,
        &t,
        ("layer-fx", scene),
        "Effects",
        true,
        |_| {},
        |ui| effect_cards(app, ui, scene, Some(sel)),
    );

    widgets::inspector_section(
        ui,
        &t,
        ("layer-visibility", scene),
        "Visibility",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, &t, "Shown", |ui| {
                let mut v = visible;
                if widgets::toggle(ui, &t, &mut v).changed() {
                    app.m.command(Op::SetBase { address: format!("{p}.visible"), value: Value::Bool(v) });
                    base_written(app, scene);
                }
                links::trigger_badge(app, ui, &format!("{p}.visible"), "Shown");
            });
            show_when(app, ui, scene, sel, &src, text, when.as_deref(), now);
        },
    );

    widgets::inspector_section(
        ui,
        &t,
        ("layer-motion", scene),
        "Enter & exit",
        false,
        |_| {},
        |ui| {
            widgets::hint(ui, &t, "How it comes in and goes out when the scene changes with a glide.");
            ui.add_space(spacing::XS);
            for (key, label) in [("enter", "Enter"), ("exit", "Exit")] {
                let cur = scene_edit::layer_prop(text, sel, key);
                widgets::prop_row(ui, &t, label, |ui| {
                    let shown = cur.as_deref().map_or("The transition's", |c| style_label(c));
                    let mut pick = None;
                    egui::ComboBox::from_id_salt(("layer-anim", key, sel)).width(ui.available_width() - 8.0).selected_text(shown).show_ui(ui, |ui| {
                        if ui.selectable_label(cur.is_none(), "The transition's").clicked() {
                            pick = Some(None);
                        }
                        for s in STYLES {
                            if ui.selectable_label(cur.as_deref() == Some(s), style_label(s)).clicked() {
                                pick = Some(Some(s));
                            }
                        }
                    });
                    if let Some(v) = pick
                        && v != cur.as_deref()
                    {
                        write_scene(app, scene, scene_edit::set_layer_prop(text, sel, key, v), now);
                    }
                });
            }
        },
    );

    widgets::inspector_section(
        ui,
        &t,
        ("layer-arrange", scene),
        "Arrange",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, &t, "Order", |ui| {
                let mut mv = None;
                if widgets::button_ex(ui, &t, Some(icon::UP), "Forward", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    mv = Some(true);
                }
                if widgets::button_ex(ui, &t, Some(icon::DOWN), "Back", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    mv = Some(false);
                }
                if let Some(fwd) = mv {
                    write_scene(app, scene, scene_edit::move_layer(text, sel, fwd), now);
                }
            });
            widgets::prop_row(ui, &t, "Copy", |ui| {
                if widgets::button_ex(ui, &t, Some(icon::COPY), "Duplicate layer", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    match scene_edit::duplicate_layer(text, sel) {
                        Ok((text, id)) => {
                            write_scene(app, scene, Ok(text), now);
                            app.build.canvas.selected = Some(id);
                        }
                        Err(e) => write_scene(app, scene, Err(e), now),
                    }
                }
            });
            ui.add_space(spacing::S);
            if widgets::hold_button(ui, &t, "Remove layer", t.bright_red, 0.8) {
                write_scene(app, scene, scene_edit::remove_layer(text, sel), now);
                app.build.canvas.selected = None;
            }
        },
    );
}

/// Enter/exit styles a layer can use (`none`: it just appears).
const STYLES: [&str; 7] = ["fade", "scale", "slide_left", "slide_right", "slide_up", "slide_down", "none"];

fn style_label(s: &str) -> &str {
    match s {
        "fade" => "Fade",
        "scale" => "Grow / shrink",
        "slide_left" => "Slide left",
        "slide_right" => "Slide right",
        "slide_up" => "Slide up",
        "slide_down" => "Slide down",
        "none" | "cut" => "None (cut)",
        other => other,
    }
}

#[allow(clippy::too_many_arguments)]
fn source_section(app: &mut App, ui: &mut egui::Ui, scene: &str, sel: &str, src: &str, kind: sources::SourceKind, text: &str, now: f64) {
    let t = app.t.clone();
    let mut swap: Option<(String, bool)> = None;
    if matches!(kind, sources::SourceKind::Color) {
        let hex = src.strip_prefix("color:").unwrap_or(src);
        let mut c = se_ui_kit::theme::hex(hex).unwrap_or(egui::Color32::BLACK);
        widgets::prop_row(ui, &t, "Color", |ui| {
            if egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::Opaque).changed() {
                swap = Some((format!("color:#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b()), true));
            }
        });
    } else {
        let group = group_of(kind);
        let options: Vec<String> = all_sources(app).into_iter().filter(|s| group_of(sources::source_kind(app, s)) == group).collect();
        widgets::prop_row(ui, &t, "Shows", |ui| {
            egui::ComboBox::from_id_salt(("layer-src", sel)).width(ui.available_width() - 8.0).selected_text(source_label(app, src)).show_ui(ui, |ui| {
                for s in &options {
                    if ui.selectable_label(s == src, source_label(app, s)).clicked() && s != src {
                        swap = Some((s.clone(), false));
                    }
                }
            });
        });
    }
    if let Some((new, soon)) = swap {
        match scene_edit::set_layer_source(text, sel, &new) {
            Ok((text, id)) => {
                if soon {
                    write_scene_soon(app, scene, Ok(text), now);
                } else {
                    write_scene(app, scene, Ok(text), now);
                }
                app.build.canvas.selected = Some(id);
            }
            Err(e) => write_scene(app, scene, Err(e), now),
        }
    }
    // a web or generative source's own settings (the same everywhere it shows)
    if let Some(id) = src.strip_prefix("patch.")
        && let Some(p) = patch_entry(app, id)
        && p.get_path("params").and_then(Value::as_list).is_some_and(|l| !l.is_empty())
    {
        ui.add_space(spacing::XS);
        widgets::details(ui, &t, ("layer-src-settings", sel), "Source settings", |ui| {
            widgets::hint(ui, &t, "Shared by every scene that shows this source.");
            patches::params_ui(app, ui, &p);
        });
    }
}

/// Where a quick placement puts the layer.
fn place_rect(i: usize, r: [f32; 4]) -> [f32; 4] {
    let [_, _, w, h] = r;
    match i {
        0 => kc::fill_canvas(),
        1 => [0.0, 0.0, 0.5, 1.0],
        2 => [0.5, 0.0, 0.5, 1.0],
        3 => [0.0, 0.0, 1.0, 0.5],
        4 => [0.0, 0.5, 1.0, 0.5],
        5 => [0.02, 0.03, 0.3, 0.3],
        6 => [0.68, 0.03, 0.3, 0.3],
        7 => [0.02, 0.67, 0.3, 0.3],
        8 => [0.68, 0.67, 0.3, 0.3],
        _ => [(1.0 - w) / 2.0, (1.0 - h) / 2.0, w, h],
    }
}

const PLACES: [&str; 10] =
    ["Fill the canvas", "Left half", "Right half", "Top half", "Bottom half", "Top left", "Top right", "Bottom left", "Bottom right", "Center"];

fn transform_section(app: &mut App, ui: &mut egui::Ui, scene: &str, sel: &str, src: &str, n: &kc::CanvasNode, canvas_name: &str) {
    let t = app.t.clone();
    let p = format!("scene.{scene}.node.{sel}");
    let mut place = None;
    widgets::prop_row(ui, &t, "Place", |ui| {
        let r = widgets::button_ex(ui, &t, Some(icon::GRID), "Place  \u{f078}", Kind::Secondary, Size::Small, 0.0, true);
        egui::Popup::menu(&r).show(|ui| {
            for (i, label) in PLACES.iter().enumerate() {
                if ui.button(*label).clicked() {
                    place = Some(place_rect(i, n.rect));
                }
            }
            ui.separator();
            if ui.button("Fit the picture's shape").on_hover_text("Resize the box so the picture isn't squashed").clicked() {
                let aspect = cameras(app).into_iter().find(|(c, _)| c == src).map_or(16.0 / 9.0, |(_, a)| a) as f32;
                place = Some(kc::fit_aspect(n.rect, aspect, crate::views::monitor::canvas_size(canvas_name)));
            }
        });
    });
    if let Some(r) = place {
        canvas::apply_rect(app, scene, canvas_name, sel, r);
    }
    let mut r = n.rect;
    let aspect_locked = canvas::framed_window(app, scene, canvas_name, sel) && n.rect[2] > 0.0 && n.rect[3] > 0.0;
    let mut changed = false;
    for (label, a, b) in [("Position", 0, 1), ("Size", 2, 3)] {
        widgets::prop_row(ui, &t, label, |ui| {
            for (i, axis) in [(a, if a == 0 { "X " } else { "W " }), (b, if b == 1 { "Y " } else { "H " })] {
                let mut pct = r[i] * 100.0;
                let range = if i < 2 { -50.0..=100.0 } else { 1.0..=200.0 };
                if ui.add(egui::DragValue::new(&mut pct).range(range).speed(0.2).prefix(axis).suffix("%").fixed_decimals(1)).changed() {
                    r[i] = pct / 100.0;
                    if aspect_locked && i >= 2 {
                        let other = if i == 2 { 3 } else { 2 };
                        r[other] = r[i] * n.rect[other] / n.rect[i];
                    }
                    changed = true;
                }
            }
        });
    }
    if changed {
        canvas::apply_rect(app, scene, canvas_name, sel, r);
    }
    if aspect_locked {
        widgets::hint(ui, &t, "Aspect ratio locked for this video window.");
    }
    ui.add_space(spacing::XS);
    widgets::hint(ui, &t, &format!("Scale, rotation and offset move the picture without changing its box. {} lets one follow the music.", links::SINE));
    live_row(app, ui, scene, "Scale", &format!("{p}.scale"), 1.0, 0.1..=4.0, "×", Some((0.8, 1.3)));
    live_row(app, ui, scene, "Rotation", &format!("{p}.rotation"), 0.0, -180.0..=180.0, "°", Some((-15.0, 15.0)));
    live_row(app, ui, scene, "Offset X", &format!("{p}.offset_x"), 0.0, -1000.0..=1000.0, " px", Some((-60.0, 60.0)));
    live_row(app, ui, scene, "Offset Y", &format!("{p}.offset_y"), 0.0, -1000.0..=1000.0, " px", Some((-60.0, 60.0)));
}

#[allow(clippy::too_many_arguments)]
fn look_section(app: &mut App, ui: &mut egui::Ui, scene: &str, sel: &str, n: Option<&kc::CanvasNode>, canvas_name: &str, text: &str, now: f64) {
    let t = app.t.clone();
    let p = format!("scene.{scene}.node.{sel}");
    live_row(app, ui, scene, "Opacity", &format!("{p}.opacity"), 1.0, 0.0..=1.0, "%", Some((0.0, 1.0)));
    if let Some(n) = n {
        let addr = format!("{p}.radius.{canvas_name}");
        widgets::prop_row(ui, &t, "Corners", |ui| {
            let mut v = n.radius as f64;
            let r = slider(ui, &mut v, 0.0..=120.0, " px", true);
            if r.changed() || r.drag_stopped() {
                canvas::apply_shape(app, scene, canvas_name, sel, n.crop, v as f32, !r.dragged());
            }
            links::modulate_button(app, ui, &addr, "Corners", (0.0, 60.0));
        });
    }
    let blend = scene_edit::layer_prop(text, sel, "blend").unwrap_or_else(|| "normal".into());
    widgets::prop_row(ui, &t, "Blend", |ui| {
        let modes = [("normal", "Normal"), ("add", "Add (brighten)"), ("screen", "Screen (lighten)"), ("multiply", "Multiply (darken)")];
        let shown = modes.iter().find(|(m, _)| *m == blend || (blend == "additive" && *m == "add")).map_or(blend.as_str(), |(_, l)| *l);
        let mut pick = None;
        egui::ComboBox::from_id_salt(("layer-blend", sel)).width(ui.available_width() - 8.0).selected_text(shown).show_ui(ui, |ui| {
            for (m, l) in modes {
                if ui.selectable_label(blend == m, l).clicked() && blend != m {
                    pick = Some(m);
                }
            }
        });
        if let Some(m) = pick {
            write_scene(app, scene, scene_edit::set_layer_prop(text, sel, "blend", (m != "normal").then_some(m)), now);
        }
    });
    let mut mask = scene_edit::layer_prop(text, sel, "mask").unwrap_or_default();
    widgets::prop_row(ui, &t, "Mask", |ui| {
        widgets::hint(ui, &t, "White shows the layer, black hides it.");
    });
    if media::picker(app, ui, ("layer-mask", scene, sel), media::MediaKind::Image, &mut mask) {
        write_scene(app, scene, scene_edit::set_layer_prop(text, sel, "mask", Some(mask.as_str())), now);
    }
}

// ---- effects ----------------------------------------------------------------------------------------------


/// Scene and layer FX use the same action-based slot editor as the Effects target racks.
fn effect_cards(app: &mut App, ui: &mut egui::Ui, scene: &str, node: Option<&str>) {
    if let Some(target) = crate::views::fx_rack::scene_target(app, scene, node) {
        crate::views::fx_rack::chain(app, ui, &target);
    }
}


// ---- when a layer shows ------------------------------------------------------------------------------

/// Whether a layer's show condition lets it show right now (no condition, or one that can't be
/// read, counts as showing).
pub(crate) fn node_showing(app: &App, scene: &str, id: &str) -> bool {
    node_when(app, scene, id).is_none_or(|w| when_now(app, &w).unwrap_or(true))
}

/// A layer's show condition from the loaded scene definition (first canvas that has the layer).
fn node_when(app: &App, scene: &str, id: &str) -> Option<String> {
    let Some(Value::Map(canvases)) = app.build.scene_config(scene)?.get_path("canvas") else { return None };
    canvases
        .values()
        .flat_map(|c| c.get_path("nodes").and_then(Value::as_list).unwrap_or(&[]))
        .find(|n| {
            let src = n.get_path("src").and_then(Value::as_str).unwrap_or("");
            n.get_path("id").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(src) == id
        })
        .and_then(|n| n.get_path("when").and_then(Value::as_str))
        .filter(|w| !w.trim().is_empty())
        .map(String::from)
}

/// Is a show condition true right now? `Err` (its mistake) when it can't be read: the canvas
/// then never shows the layer.
fn when_now(app: &App, when: &str) -> Result<bool, String> {
    let e = se_expr::Expr::parse(when).map_err(|e| e.msg)?;
    // the engine's short names in conditions
    fn alias(p: &str) -> &str {
        match p {
            "mode" => "show.mode",
            "scene" => "show.scene.program",
            "preview" => "show.scene.preview",
            p => p,
        }
    }
    Ok(e.eval_bool(&|p: &str| app.m.get(alias(p)).cloned().unwrap_or(Value::Null)))
}

/// A check in words, for "It only shows when …".
fn check_words(app: &App, c: &scene_edit::LayerCheck) -> String {
    use scene_edit::LayerCheck::*;
    match c {
        Mode { mode, not } => format!("the show mode {} {}", if *not { "isn't" } else { "is" }, rules::nice_name(mode)),
        Song { playing: true } => "a song request is playing".into(),
        Song { playing: false } => "no song request is playing".into(),
        Source { id, playing } => format!("{} {}", source_label(app, &format!("patch.{id}")), if *playing { "is playing" } else { "isn't playing" }),
    }
}

/// Why the selected layer isn't on the canvas right now (nothing when it is).
fn show_status(app: &App, ui: &mut egui::Ui, visible: bool, when: Option<&str>) {
    let t = &app.t;
    if !visible {
        widgets::callout(ui, t, Tone::Info, icon::EYE_OFF, "Hidden", "Its eye is off. Turn it on in the Layers list or under Visibility.", None);
        ui.add_space(spacing::S);
        return;
    }
    let Some(when) = when else { return };
    match when_now(app, when) {
        Ok(true) => {}
        Ok(false) => {
            let body = match scene_edit::parse_layer_when(when).filter(|c| !c.is_empty()) {
                Some(checks) => {
                    let all: Vec<String> = checks.iter().map(|c| check_words(app, c)).collect();
                    let not: Vec<String> =
                        checks.iter().filter(|c| c.text().is_some_and(|x| !when_now(app, &x).unwrap_or(false))).map(|c| check_words(app, c)).collect();
                    if not.len() == all.len() {
                        format!("It only shows when {}, and right now that isn't so.", all.join(" and "))
                    } else {
                        format!("It only shows when {}. Not true right now: {}.", all.join(" and "), not.join(", "))
                    }
                }
                None => "It only shows when its own rule is true (see Visibility), and right now it isn't.".into(),
            };
            widgets::callout(ui, t, Tone::Warn, icon::EYE_OFF, "Hidden right now", &body, None);
            ui.add_space(spacing::S);
        }
        Err(_) => {
            widgets::callout(
                ui,
                t,
                Tone::Danger,
                icon::WARN,
                "Its show rule has a mistake",
                "So it never shows. Fix it under Visibility, or pick Always.",
                None,
            );
            ui.add_space(spacing::S);
        }
    }
}

const CHECK_KINDS: [&str; 3] = ["The show mode", "A song request", "A source playing"];

/// A new check of kind `k` (index into [`CHECK_KINDS`]) with a sensible first choice.
fn new_check(app: &App, scene: &str, src: &str, k: usize) -> scene_edit::LayerCheck {
    use scene_edit::LayerCheck::*;
    match k {
        0 => {
            let now = app.m.str("show.mode");
            let mode = if now.is_empty() { app.m.q_list("modes").first().and_then(Value::as_str).unwrap_or("").to_string() } else { now.to_string() };
            Mode { mode, not: false }
        }
        1 => Song { playing: true },
        _ => {
            // another source in this scene first (like "while the intro plays")
            let ids: Vec<&str> = app.m.q_list("patches").iter().filter_map(|p| p.get_path("id").and_then(Value::as_str)).collect();
            let pick = ids
                .iter()
                .find(|id| {
                    let s = format!("patch.{id}");
                    s != src && app.build.node_src(scene, &s).is_some()
                })
                .or(ids.first());
            Source { id: pick.map_or_else(String::new, |s| s.to_string()), playing: true }
        }
    }
}

/// "Shows: Always / Only when …" with plain checks; anything else as text under Details.
#[allow(clippy::too_many_arguments)]
fn show_when(app: &mut App, ui: &mut egui::Ui, scene: &str, sel: &str, src: &str, text: &str, when: Option<&str>, now: f64) {
    use scene_edit::LayerCheck::*;
    let t = app.t.clone();
    let mut write: Option<Option<String>> = None;
    let mut only = usize::from(when.is_some());
    widgets::prop_row(ui, &t, "When", |ui| {
        if widgets::segmented(ui, &t, &mut only, &["Always", "Only when…"]) {
            write = Some(if only == 0 {
                None
            } else {
                let k = if app.m.q_list("modes").is_empty() { 1 } else { 0 };
                new_check(app, scene, src, k).text()
            });
        }
    });
    if let Some(w) = when {
        ui.add_space(spacing::S);
        match scene_edit::parse_layer_when(w) {
            Some(mut checks) => {
                let modes: Vec<(String, String)> =
                    app.m.q_list("modes").iter().filter_map(Value::as_str).map(|m| (m.to_string(), rules::nice_name(m))).collect();
                let playable: Vec<(String, String)> = app
                    .m
                    .q_list("patches")
                    .iter()
                    .filter_map(|p| p.get_path("id").and_then(Value::as_str))
                    .map(|id| (id.to_string(), source_label(app, &format!("patch.{id}"))))
                    .collect();
                let mut changed = false;
                let mut remove = None;
                for (i, c) in checks.iter_mut().enumerate() {
                    if i > 0 {
                        ui.label(RichText::new("and").color(t.text_dim));
                    }
                    egui::Frame::new().fill(t.inset).corner_radius(radius::CONTROL).inner_margin(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let k = match c {
                                Mode { .. } => 0,
                                Song { .. } => 1,
                                Source { .. } => 2,
                            };
                            let mut pick = k;
                            egui::ComboBox::from_id_salt(("layer-check-kind", sel, i))
                                .width(ui.available_width() - 44.0)
                                .selected_text(CHECK_KINDS[k])
                                .show_ui(ui, |ui| {
                                    for (j, l) in CHECK_KINDS.iter().enumerate() {
                                        ui.selectable_value(&mut pick, j, *l);
                                    }
                                });
                            if pick != k {
                                *c = new_check(app, scene, src, pick);
                                changed = true;
                            }
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                if widgets::icon_button(ui, &t, icon::CROSS, "Remove this check").clicked() {
                                    remove = Some(i);
                                }
                            });
                        });
                        ui.horizontal_wrapped(|ui| match c {
                            Mode { mode, not } => {
                                let mut n = usize::from(*not);
                                if widgets::segmented(ui, &t, &mut n, &["is", "isn't"]) {
                                    *not = n == 1;
                                    changed = true;
                                }
                                if let Some(m) =
                                    rules::pick_name(ui, ("layer-check-mode", sel, i), mode, &modes, "Pick a mode", (ui.available_width() - 8.0).max(120.0))
                                {
                                    *mode = m;
                                    changed = true;
                                }
                            }
                            Song { playing } => {
                                let mut n = usize::from(!*playing);
                                if widgets::segmented(ui, &t, &mut n, &["is playing", "isn't playing"]) {
                                    *playing = n == 0;
                                    changed = true;
                                }
                            }
                            Source { id, playing } => {
                                if let Some(o) = rules::pick_name(
                                    ui,
                                    ("layer-check-source", sel, i),
                                    id,
                                    &playable,
                                    "Pick a source",
                                    (ui.available_width() - 8.0).max(120.0),
                                ) {
                                    *id = o;
                                    changed = true;
                                }
                                let mut n = usize::from(!*playing);
                                if widgets::segmented(ui, &t, &mut n, &["is playing", "isn't playing"]) {
                                    *playing = n == 0;
                                    changed = true;
                                }
                            }
                        });
                    });
                }
                if let Some(i) = remove {
                    checks.remove(i);
                    changed = true;
                }
                ui.add_space(spacing::XS);
                if widgets::button_ex(ui, &t, Some(icon::PLUS), "Add another check", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    checks.push(new_check(app, scene, src, if modes.is_empty() { 1 } else { 0 }));
                    changed = true;
                }
                if changed {
                    let w = scene_edit::layer_when_text(&checks);
                    write = Some((!w.is_empty()).then_some(w));
                }
            }
            None => {
                widgets::callout(ui, &t, Tone::Info, icon::EDIT, "It has its own rule", "Change it under Details below, or pick Always.", None);
            }
        }
        ui.add_space(spacing::XS);
        widgets::details(ui, &t, ("layer-when-raw", sel), "Details", |ui| {
            widgets::hint(ui, &t, "The rule as Stream Engine reads it. The layer shows while this is true.");
            let mut buf = match &app.build.comp.when_buf {
                Some((n, b)) if n == sel => b.clone(),
                _ => w.to_string(),
            };
            rules::text_field(app, ui, &format!("cond:{sel}"), "mode == 'live' && queue.now.id", &mut buf);
            let typed = buf.trim().to_string();
            if typed != w {
                ui.horizontal(|ui| match se_expr::Expr::parse(&typed) {
                    Err(e) if !typed.is_empty() => {
                        widgets::hint(ui, &t, &format!("That has a mistake: {}", e.msg));
                    }
                    _ => {
                        if widgets::button_ex(ui, &t, Some(icon::CHECK), "Use this", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                            write = Some((!typed.is_empty()).then(|| typed.clone()));
                        }
                        if widgets::button_ex(ui, &t, None, "Undo", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                            buf = w.to_string();
                        }
                    }
                });
            }
            app.build.comp.when_buf = (buf.trim() != w).then(|| (sel.to_string(), buf));
        });
    }
    if let Some(w) = write {
        app.build.comp.when_buf = None;
        write_scene(app, scene, scene_edit::set_layer_when(text, sel, w.as_deref()), now);
    }
}

// ---- the scene --------------------------------------------------------------------------------------------

fn scene_inspector(app: &mut App, ui: &mut egui::Ui, scene: &str, text: Option<&str>, now: f64) {
    let t = app.t.clone();
    let list = scenes(app);
    let label = scene_label(app, scene);
    let program = app.m.str("show.scene.program") == scene;
    let preview = app.m.str("show.scene.preview") == scene;
    let key = text.and_then(scene_edit::key);
    let state = if program {
        if crate::views::status::on_air(app) { "On air" } else { "Program (off air)" }
    } else if preview {
        "Up next"
    } else {
        "Not showing"
    };
    let sub = match key {
        Some(k) => format!("Scene · key {k} · {state}"),
        None => format!("Scene · {state}"),
    };
    widgets::detail_header(ui, &t, icon::LAYERS, &label, &sub, |_| {});
    let Some(text) = text else {
        widgets::hint(ui, &t, "Loading the scene…");
        return;
    };
    if app.build.comp.rename_for != scene {
        app.build.comp.rename_for = scene.to_string();
        app.build.comp.rename = label.clone();
    }

    widgets::inspector_section(
        ui,
        &t,
        "scene-main",
        "Scene",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, &t, "Name", |ui| {
                let new = app.build.comp.rename.trim().to_string();
                let can = !new.is_empty() && new != label;
                ui.add(widgets::field(&mut app.build.comp.rename).desired_width((ui.available_width() - if can { 84.0 } else { 0.0 }).max(80.0)));
                if can && widgets::button_ex(ui, &t, None, "Rename", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    write_scene(app, scene, scene_edit::set_label(text, &new), now);
                }
            });
            widgets::prop_row(ui, &t, "Key", |ui| {
                let mut pick = None;
                let shown = key.map_or_else(|| "None".to_string(), |k| k.to_string());
                let combo = egui::ComboBox::from_id_salt(("scene-key", scene)).width(ui.available_width() - 8.0).selected_text(shown).show_ui(ui, |ui| {
                    if ui.selectable_label(key.is_none(), "None").clicked() {
                        pick = Some(None);
                    }
                    for k in 1..=9i64 {
                        let other = list.iter().find(|(n, _, used)| n != scene && *used == k).map(|(_, l, _)| nice(l));
                        let words = match other {
                            Some(o) => format!("{k}  (also {o})"),
                            None => k.to_string(),
                        };
                        if ui.selectable_label(key == Some(k), words).clicked() {
                            pick = Some(Some(k));
                        }
                    }
                });
                combo.response.on_hover_text("Press this number key to put the scene up next.");
                if let Some(k) = pick
                    && k != key
                {
                    write_scene(app, scene, scene_edit::set_key(text, k), now);
                }
            });
        },
    );

    widgets::inspector_section(
        ui,
        &t,
        "scene-transition",
        "Transition in",
        true,
        |_| {},
        |ui| {
            if let Some(r) = crate::views::transitions::scene_choice(app, ui, scene, text) {
                write_scene(app, scene, r, now);
            }
            ui.add_space(spacing::S);
            let speeds: [(&str, Option<[i64; 2]>); 4] =
                [("Auto", None), ("Fast", Some([250, 450])), ("Normal", Some([500, 900])), ("Slow", Some([1000, 1600]))];
            let cur = scene_edit::speed(text);
            let i = speeds.iter().position(|(_, s)| *s == cur).unwrap_or(0);
            widgets::prop_row(ui, &t, "Speed", |ui| {
                let names = ["Auto", "Fast", "Normal", "Slow"];
                let shown = if cur.is_some() && !speeds.iter().any(|(_, s)| *s == cur) { "Custom" } else { names[i] };
                let mut pick = None;
                egui::ComboBox::from_id_salt(("scene-speed", scene)).width(ui.available_width() - 8.0).selected_text(shown).show_ui(ui, |ui| {
                    for (j, n) in names.iter().enumerate() {
                        if ui.selectable_label(shown == *n, *n).clicked() {
                            pick = Some(j);
                        }
                    }
                });
                if let Some(j) = pick
                    && speeds[j].1 != cur
                {
                    write_scene(app, scene, scene_edit::set_speed(text, speeds[j].1), now);
                }
            });
            if cur.is_some() && !speeds.iter().any(|(_, s)| *s == cur) {
                let [lo, hi] = cur.unwrap_or_default();
                widgets::hint(ui, &t, &format!("Custom: {:.1}–{:.1} seconds", lo as f64 / 1000.0, hi as f64 / 1000.0));
            }
        },
    );

    widgets::inspector_section(
        ui,
        &t,
        "scene-fx",
        "Scene effects",
        true,
        |_| {},
        |ui| effect_cards(app, ui, scene, None),
    );

    widgets::inspector_section(
        ui,
        &t,
        "scene-enter",
        "When this scene starts",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, &t, "Done top to bottom every time this scene goes on air.");
            steps_block(app, ui, scene, text, "on_enter", now);
        },
    );
    widgets::inspector_section(
        ui,
        &t,
        "scene-exit",
        "When it ends",
        false,
        |_| {},
        |ui| {
            widgets::hint(ui, &t, "Done when another scene takes over.");
            steps_block(app, ui, scene, text, "on_exit", now);
        },
    );

    widgets::inspector_section(
        ui,
        &t,
        "scene-bg",
        "Background",
        false,
        |_| {},
        |ui| {
            let bg = scene_edit::background(text);
            let mut c = bg.as_deref().and_then(se_ui_kit::theme::hex).unwrap_or(egui::Color32::BLACK);
            widgets::prop_row(ui, &t, "Color", |ui| {
                let r = egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::Opaque)
                    .on_hover_text("Shows wherever no layer covers the canvas");
                if r.changed() {
                    let hex = format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
                    write_scene_soon(app, scene, scene_edit::set_background(text, Some(&hex)), now);
                }
                if bg.is_some() {
                    if widgets::button_ex(ui, &t, None, "Use black", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                        write_scene(app, scene, scene_edit::set_background(text, None), now);
                    }
                } else {
                    widgets::hint(ui, &t, "Black");
                }
            });
        },
    );

    lights_section(app, ui, scene, text, now);

    widgets::inspector_section(
        ui,
        &t,
        "scene-danger",
        "Duplicate or delete",
        false,
        |_| {},
        |ui| {
            if widgets::button_ex(ui, &t, Some(icon::COPY), "Duplicate scene", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                let base = format!("{label} copy");
                let used: Vec<String> = list.iter().map(|(n, _, _)| n.clone()).collect();
                let mut name = scene_edit::slug(&base);
                let mut n = 2;
                while used.contains(&name) {
                    name = format!("{}_{n}", scene_edit::slug(&base));
                    n += 1;
                }
                match scene_edit::duplicate(text, &base, None) {
                    Ok(copy) => {
                        app.m.action("project.write", Value::map().with("path", file_path(&name)).with("text", copy));
                        app.build.canvas.scene = Some(name.clone());
                        app.build.canvas.selected = None;
                        app.build.comp.reread = Some((name, now + 0.5));
                        app.m.refresh_soon();
                    }
                    Err(e) => app.m.toast(format!("Couldn't copy the scene: {e:#}"), true),
                }
            }
            ui.add_space(spacing::S);
            if program {
                widgets::hint(ui, &t, "Switch to another scene before deleting this one.");
            } else if widgets::hold_button(ui, &t, "Delete scene", t.bright_red, 1.0) {
                app.m.action("project.write", Value::map().with("path", file_path(scene)).with("delete", true));
                // an edit still waiting to be saved would bring it back
                if app.build.comp.local.as_ref().is_some_and(|l| l.scene == scene) {
                    app.build.comp.local = None;
                }
                app.build.canvas.scene = None;
                app.build.canvas.selected = None;
                app.m.refresh_soon();
            }
        },
    );
}

/// Lights on this scene (`[lights]`): a look held and/or a cue list run while it's on air.
fn lights_section(app: &mut App, ui: &mut egui::Ui, scene: &str, text: &str, now: f64) {
    for q in ["lights.palettes", "lights.cuelists"] {
        if app.m.q_seq(q) == 0 && app.m.connected {
            app.m.query(q, Value::Null);
        }
    }
    let pairs = |l: &[Value]| -> Vec<(String, String)> {
        l.iter()
            .filter_map(|v| {
                let n = v.get_path("name").and_then(Value::as_str)?.to_string();
                let label = v.get_path("label").and_then(Value::as_str).filter(|s| !s.is_empty()).map_or_else(|| nice(&n), nice);
                Some((n, label))
            })
            .collect()
    };
    let looks = pairs(app.m.q_list("lights.palettes"));
    let cues = pairs(app.m.q_list("lights.cuelists"));
    let (look, cue) = scene_edit::lights(text);
    if looks.is_empty() && cues.is_empty() && look.is_none() && cue.is_none() {
        return;
    }
    let t = app.t.clone();
    widgets::inspector_section(
        ui,
        &t,
        "scene-lights",
        "Lights",
        false,
        |_| {},
        |ui| {
            for (key, label, opts, cur) in [("look", "Look", &looks, &look), ("cue", "Cue list", &cues, &cue)] {
                widgets::prop_row(ui, &t, label, |ui| {
                    let shown =
                        cur.as_deref().map_or_else(|| "None".to_string(), |c| opts.iter().find(|(n, _)| n == c).map_or_else(|| nice(c), |(_, l)| l.clone()));
                    let mut pick = None;
                    egui::ComboBox::from_id_salt(("scene-lights", key, scene)).width(ui.available_width() - 8.0).selected_text(shown).show_ui(ui, |ui| {
                        if ui.selectable_label(cur.is_none(), "None").clicked() {
                            pick = Some(None);
                        }
                        for (n, l) in opts {
                            if ui.selectable_label(cur.as_deref() == Some(n), l).clicked() {
                                pick = Some(Some(n.clone()));
                            }
                        }
                    });
                    if let Some(v) = pick
                        && v != *cur
                    {
                        write_scene(app, scene, scene_edit::set_lights(text, key, v.as_deref()), now);
                    }
                });
            }
            widgets::hint(ui, &t, "Held or run while this scene is on air.");
        },
    );
}

/// A scene's "when it starts / ends" steps (`key`: `on_enter` / `on_exit`). Unfinished steps
/// stay on the page; the file gets the finished ones.
fn steps_block(app: &mut App, ui: &mut egui::Ui, scene: &str, text: &str, key: &'static str, now: f64) {
    let t = app.t.clone();
    let file = scene_edit::scene_commands(text, key);
    let steps = &mut app.build.comp.steps;
    let idx = match steps.iter().position(|d| d.scene == scene && d.key == key) {
        Some(i) => i,
        None => {
            steps.push(StepsDraft { scene: scene.to_string(), key, cmds: file.clone(), seen: file.clone() });
            steps.len() - 1
        }
    };
    let d = &mut steps[idx];
    if d.seen != file {
        // changed in the file by someone else (another window, a restore)
        d.cmds = file.clone();
        d.seen = file.clone();
    }
    let mut cmds = d.cmds.clone();
    let changed = rules::steps_editor(app, ui, &format!("scene-{key}-"), &mut cmds, "");
    for (i, c) in cmds.iter().enumerate() {
        if let Some(what) = rules::step_missing(c) {
            widgets::hint(ui, &t, &format!("Step {} isn't saved yet: {what}.", i + 1));
        }
    }
    if changed {
        let ready: Vec<String> = cmds.iter().filter(|c| rules::step_missing(c).is_none()).cloned().collect();
        let d = &mut app.build.comp.steps[idx];
        d.cmds = cmds;
        if ready != file {
            d.seen = ready.clone();
            write_scene_soon(app, scene, scene_edit::set_scene_commands(text, key, &ready), now);
        }
    }
}

// ---- On every scene --------------------------------------------------------------------------------------

fn pinned_inspector(app: &mut App, ui: &mut egui::Ui, id: &str, now: f64) {
    let t = app.t.clone();
    let src = format!("patch.{id}");
    let kind = sources::source_kind(app, &src);
    let mut edit_source = false;
    widgets::detail_header(ui, &t, kind.icon(), &source_label(app, &src), &format!("On every scene · {}", kind.label()), |ui| {
        edit_source = widgets::button_ex(ui, &t, None, "Edit source", Kind::Secondary, Size::Small, 0.0, true).clicked();
    });
    if edit_source {
        sources::open(app, ui.ctx(), &src);
    }
    let Some(project) = project_text(app, now) else {
        widgets::hint(ui, &t, "Loading the project settings…");
        return;
    };
    let s = patches::overlay_settings(&project, id);
    widgets::inspector_section(
        ui,
        &t,
        "pinned-where",
        "Drawn on",
        true,
        |_| {},
        |ui| {
            let on = |c: &str| s.canvases.as_ref().is_none_or(|l| l.iter().any(|x| x == c));
            let mut pick: Option<Vec<&str>> = None;
            for (c, label) in [("wide", "Main"), ("tall", "Vertical")] {
                widgets::prop_row(ui, &t, label, |ui| {
                    let mut v = on(c);
                    if widgets::toggle(ui, &t, &mut v).changed() {
                        let mut next: Vec<&str> = ["wide", "tall"].into_iter().filter(|x| if *x == c { v } else { on(x) }).collect();
                        next.sort();
                        pick = Some(next);
                    }
                });
            }
            if let Some(list) = pick {
                let both = list.len() == 2;
                write_project(app, patches::set_overlay_canvases(&project, id, if both { None } else { Some(list.as_slice()) }), now);
            }
            widgets::hint(ui, &t, "Drawn above every scene. To show it in some scenes only, switch it off here and add it as a layer where you want it.");
        },
    );
    widgets::inspector_section(
        ui,
        &t,
        "pinned-place",
        "Placement",
        false,
        |_| {},
        |ui| {
            for (c, label) in [("wide", "Main"), ("tall", "Vertical")] {
                let words = match s.rect.iter().find(|(k, _)| k == c) {
                    Some((_, r)) => format!("{:.0}%, {:.0}% · {:.0} × {:.0}%", r[0] * 100.0, r[1] * 100.0, r[2] * 100.0, r[3] * 100.0),
                    None => "Fits the whole canvas".into(),
                };
                widgets::prop_row(ui, &t, label, |ui| {
                    ui.label(RichText::new(words).color(t.fg));
                });
            }
            widgets::prop_row(ui, &t, "Order", |ui| {
                ui.label(RichText::new(if s.z == 0 { "Default".to_string() } else { s.z.to_string() }).color(t.fg));
            });
            if let Some(w) = &s.when {
                widgets::prop_row(ui, &t, "Shows when", |ui| {
                    ui.label(RichText::new(w).font(font_mono(type_scale::SMALL)).color(t.fg));
                });
            }
            widgets::hint(ui, &t, "Placement and order are set in project.toml.");
        },
    );
    if let Some(p) = patch_entry(app, id)
        && p.get_path("params").and_then(Value::as_list).is_some_and(|l| !l.is_empty())
    {
        widgets::inspector_section(
            ui,
            &t,
            "pinned-settings",
            "Source settings",
            true,
            |_| {},
            |ui| {
                patches::params_ui(app, ui, &p);
            },
        );
    }
}
