//! Scenes → Scenes: the composition view (§15.5). Pick a scene on the left (or make one from a
//! starting layout), arrange its layers on the composite view in the middle, and tune the
//! selected layer (place, look, an overlay's own settings, when it shows and why it's hidden)
//! — or the scene's transitions, effects, what it does coming on and going off, and its
//! background — on the right. Geometry edits go through live state (`set_base`, saved to the
//! scene file by the engine); everything else edits the scene file with `scene_edit`
//! (comments kept).

use crate::app::App;
use crate::views::live::nice;
use crate::views::{canvas, rules, scene_edit};
use egui::{Align, Layout, RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::canvas as kc;
use se_ui_kit::theme::{font_medium, font_semibold, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, Tone, icon};

#[derive(Default)]
pub struct CompositionState {
    /// Right panel: 0 = selected layer, 1 = scene settings.
    pub side: usize,
    pub rename: String,
    pub rename_for: String,
    pub new_open: bool,
    pub new_name: String,
    /// "Start from" choice (index into `scene_edit::START_LAYOUTS`).
    pub new_layout: usize,
    /// Scene whose file text must be re-read (after a write), and when.
    reread: Option<(String, f64)>,
    asked: Option<(String, u64)>,
    /// Our latest edit of a scene file, shown until the engine's copy catches up.
    local: Option<LocalText>,
    /// "When this scene comes on / goes off" as edited, unfinished steps included.
    steps: Vec<StepsDraft>,
    /// The show condition as typed under Details: (layer, text).
    when_buf: Option<(String, String)>,
    patches_at: f64,
}

struct LocalText {
    scene: String,
    text: String,
    /// Debounced edits (a color being dragged, typing): when to send it.
    send_at: Option<f64>,
    /// Reply count of the file query when it was sent; a newer reply replaces this text.
    seq: u64,
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

/// Send a debounced edit once it's due (`force`: now).
fn flush(app: &mut App, now: f64, force: bool) {
    let Some(l) = app.build.comp.local.as_ref() else { return };
    let Some(at) = l.send_at else { return };
    if force || now >= at {
        let (scene, text) = (l.scene.clone(), l.text.clone());
        send_scene(app, &scene, text, now);
    }
}

/// Friendly name for a source (`cam_kit` → its device label or "Cam kit"; `patch.x` → "X").
pub fn source_label(app: &App, src: &str) -> String {
    if let Some(p) = src.strip_prefix("patch.") {
        return nice(p);
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

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let now = ui.input(|i| i.time);
    if app.m.q_seq("sources") == 0 {
        app.m.query("sources", Value::Null);
    }
    crate::views::transitions::poll(app, now);
    // overlay settings and show conditions follow the overlays' live state
    if app.m.connected && now - app.build.comp.patches_at > 2.0 {
        app.build.comp.patches_at = now;
        app.m.query("patches", Value::Null);
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
    let h = ui.available_height();
    let w = ui.available_width();
    let left = 230.0;
    let right = (w * 0.24).clamp(320.0, 400.0);
    let mid = w - left - right - 2.0 * spacing::L;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = spacing::L;
        ui.allocate_ui_with_layout(Vec2::new(left, h), Layout::top_down(Align::Min), |ui| scene_list(app, ui, &list, &scene, now));
        ui.allocate_ui_with_layout(Vec2::new(mid, h), Layout::top_down(Align::Min), |ui| {
            if scene.is_empty() {
                widgets::panel(ui, &t, |ui| {
                    ui.set_width(ui.available_width());
                    if widgets::empty_state(
                        ui,
                        &t,
                        icon::LAYERS,
                        "No scenes yet",
                        "A scene is one arrangement of your cameras and overlays.",
                        Some("Make your first scene"),
                    ) {
                        app.build.comp.new_open = true;
                    }
                });
                return;
            }
            middle(app, ui, &scene, h, now);
        });
        ui.allocate_ui_with_layout(Vec2::new(right, h), Layout::top_down(Align::Min), |ui| {
            if !scene.is_empty() {
                side_panel(app, ui, &scene, h, now);
            }
        });
    });
}

// ---- left: scenes -------------------------------------------------------------------------------

fn scene_list(app: &mut App, ui: &mut egui::Ui, list: &[(String, String, i64)], current: &str, now: f64) {
    let t = app.t.clone();
    let program = app.m.str("show.scene.program").to_string();
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(ui.available_height() - 36.0);
        let creating = app.build.comp.new_open;
        ui.label(RichText::new(if creating { "New scene" } else { "Your scenes" }).font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.add_space(spacing::S);
        if creating {
            // the form takes the list's place (it doesn't fit under a long list)
            egui::ScrollArea::vertical().id_salt("comp-new").show(ui, |ui| new_scene_form(app, ui, list, now));
            return;
        }
        egui::ScrollArea::vertical().id_salt("comp-scenes").max_height(ui.available_height() - 60.0).show(ui, |ui| {
            for (name, label, key) in list {
                let r = widgets::list_row(ui, &t, icon::LAYERS, &nice(label), if *name == program { "On air" } else { "" }, "", name == current)
                    .on_hover_text(format!("Press {key} to put it up next"));
                // number key as a keycap on the right
                let kc = egui::Rect::from_center_size(egui::pos2(r.rect.right() - 22.0, r.rect.center().y), Vec2::new(22.0, 22.0));
                ui.painter().rect(kc, 5, t.inset, egui::Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
                ui.painter().text(kc.center(), egui::Align2::CENTER_CENTER, key.to_string(), se_ui_kit::theme::font_mono(type_scale::SMALL), t.text_dim);
                if *name == program {
                    ui.painter().circle_filled(egui::pos2(r.rect.right() - 44.0, r.rect.center().y), 3.5, t.tally_program());
                }
                if r.clicked() && name != current {
                    app.build.canvas.scene = Some(name.clone());
                    app.build.canvas.selected = None;
                    // edits apply to "Up next": bring the scene there
                    app.m.command(Op::SceneGo { scene: name.clone() });
                }
            }
        });
        ui.add_space(spacing::S);
        if widgets::button_ex(ui, &t, Some(icon::PLUS), "New scene", Kind::Secondary, Size::Medium, ui.available_width(), true).clicked() {
            app.build.comp.new_open = true;
        }
    });
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

/// Name + "Start from" layouts + Create.
fn new_scene_form(app: &mut App, ui: &mut egui::Ui, list: &[(String, String, i64)], now: f64) {
    let t = app.t.clone();
    ui.add(se_ui_kit::widgets::field(&mut app.build.comp.new_name).hint_text("Name, e.g. Chill cams").desired_width(ui.available_width()));
    ui.add_space(spacing::S);
    ui.label(RichText::new("Start from").font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
    let cams = cameras(app);
    let gap = spacing::S;
    let tile_w = ((ui.available_width() - gap) / 2.0).floor();
    let tile = Vec2::new(tile_w, tile_w * 9.0 / 16.0 + 24.0);
    for row in scene_edit::START_LAYOUTS.iter().enumerate().collect::<Vec<_>>().chunks(2) {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            for (i, l) in row {
                let usable = l.slots() == 0 || !cams.is_empty();
                let r = layout_tile(ui, &t, tile, l, app.build.comp.new_layout == *i, usable);
                let r = if usable { r } else { r.on_hover_text("Needs a camera: set one up under Settings → Devices.") };
                if r.clicked() && usable {
                    app.build.comp.new_layout = *i;
                }
            }
        });
        ui.add_space(gap);
    }
    let layout = scene_edit::START_LAYOUTS.get(app.build.comp.new_layout).unwrap_or(&scene_edit::START_LAYOUTS[0]);
    if layout.slots() > 0 {
        if cams.is_empty() {
            widgets::hint(ui, &t, "No cameras yet. Set them up under Settings → Devices, or start from Blank.");
        } else {
            let shown: Vec<String> = (0..layout.slots()).map(|i| source_label(app, &cams[i % cams.len()].0)).collect();
            widgets::hint(ui, &t, &format!("Shows {}. Swap cameras after.", join_words(&shown)));
        }
    }
    ui.add_space(spacing::S);
    ui.horizontal(|ui| {
        let name = scene_edit::slug(&app.build.comp.new_name);
        let taken = list.iter().any(|(n, _, _)| *n == name);
        let ok = !name.is_empty() && !taken && (layout.slots() == 0 || !cams.is_empty());
        if widgets::button_ex(ui, &t, None, "Create", Kind::Primary, Size::Small, 0.0, ok).clicked() {
            let label = app.build.comp.new_name.trim().to_string();
            let key = (1..=9).find(|k| !list.iter().any(|(_, _, used)| used == k));
            let sizes = [crate::views::monitor::canvas_size("wide"), crate::views::monitor::canvas_size("tall")].map(|[w, h]| [w as f64, h as f64]);
            match scene_edit::new_scene(&label, key, layout, &cams, sizes) {
                Ok(text) => {
                    app.m.action("project.write", Value::map().with("path", file_path(&name)).with("text", text));
                    app.build.canvas.scene = Some(name.clone());
                    app.build.canvas.selected = None;
                    app.build.comp.new_open = false;
                    app.build.comp.new_name.clear();
                    app.build.comp.reread = Some((name, now + 0.5));
                    app.m.refresh_soon();
                }
                Err(e) => app.m.toast(format!("Couldn't make the scene: {e:#}"), true),
            }
        }
        if widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Small, 0.0, true).clicked() {
            app.build.comp.new_open = false;
        }
        if taken {
            widgets::hint(ui, &t, "That name is taken");
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
    p.rect(rect, se_ui_kit::theme::radius::CONTROL, bg, egui::Stroke::new(1.0, if selected { t.accent } else { t.border }), egui::StrokeKind::Inside);
    let screen = egui::Rect::from_min_size(rect.min + Vec2::splat(6.0), Vec2::new(size.x - 12.0, (size.x - 12.0) * 9.0 / 16.0));
    p.rect_filled(screen, 4, egui::Color32::from_rgb(6, 7, 9));
    let fill = if usable { if selected { t.accent } else { t.text_dim } } else { t.text_faint };
    for r in l.wide {
        let b = egui::Rect::from_min_size(
            screen.min + Vec2::new(r[0] as f32 * screen.width(), r[1] as f32 * screen.height()),
            Vec2::new(r[2] as f32 * screen.width(), r[3] as f32 * screen.height()),
        )
        .shrink(1.0);
        p.rect(b, 2, se_ui_kit::theme::mix(egui::Color32::from_rgb(6, 7, 9), fill, 0.55), egui::Stroke::new(1.0, fill), egui::StrokeKind::Inside);
    }
    let text = if usable { t.fg } else { t.text_faint };
    p.text(egui::pos2(rect.center().x, rect.bottom() - 11.0), egui::Align2::CENTER_CENTER, l.label, font_medium(type_scale::SMALL), text);
    if usable { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

// ---- middle: composite view + layers --------------------------------------------------------------

fn middle(app: &mut App, ui: &mut egui::Ui, scene: &str, h: f32, now: f64) {
    let t = app.t.clone();
    let live = app.m.str("show.scene.program") == scene;
    canvas::toolbar(app, ui);
    ui.add_space(spacing::S);
    if live {
        widgets::callout(ui, &t, widgets::Tone::Danger, icon::LIVE, "This scene is on air", "Your viewers see every change right away.", None);
        ui.add_space(spacing::S);
    }
    let layers_h = 250.0;
    let avail = Vec2::new(ui.available_width(), (h - layers_h - 90.0 - if live { 80.0 } else { 0.0 }).max(200.0));
    canvas::editors(app, ui, scene, avail);
    ui.add_space(spacing::M);
    layers(app, ui, scene, now);
}

fn layers(app: &mut App, ui: &mut egui::Ui, scene: &str, now: f64) {
    let t = app.t.clone();
    let canvas_name = canvas::edit_canvas(app);
    let mut nodes = canvas::editor_nodes(app, scene, canvas_name);
    nodes.sort_by_key(|n| std::cmp::Reverse(n.z));
    let mut add = None;
    widgets::titled(
        ui,
        &t,
        "Layers",
        "Top of the list is in front. Click one to change it.",
        |ui| {
            add = Some(widgets::button_ex(ui, &t, Some(icon::PLUS), "Add layer", Kind::Primary, Size::Small, 0.0, true));
        },
        |ui| {
            if nodes.is_empty() {
                widgets::hint(ui, &t, "This scene is empty. Add a camera or an overlay.");
                return;
            }
            egui::ScrollArea::vertical().id_salt("comp-layers").max_height(ui.available_height().max(120.0)).show(ui, |ui| {
                for n in &nodes {
                    let src = app.build.node_src(scene, &n.id).unwrap_or(&n.id).to_string();
                    let label = source_label(app, &src);
                    let sub = if n.id != src { n.id.replace('_', " ") } else { String::new() };
                    let selected = app.build.canvas.selected.as_deref() == Some(n.id.as_str());
                    ui.horizontal_top(|ui| {
                        let vis_a = format!("scene.{scene}.node.{}.visible", n.id);
                        let mut vis = n.visible;
                        let row_h = if sub.is_empty() { 40.0 } else { 54.0 };
                        let toggled = ui
                            .vertical(|ui| {
                                ui.add_space((row_h - 22.0) / 2.0);
                                widgets::toggle(ui, &t, &mut vis).on_hover_text(if vis { "Showing: click to hide" } else { "Hidden: click to show" }).changed()
                            })
                            .inner;
                        if toggled {
                            app.m.command(Op::SetBase { address: vis_a, value: Value::Bool(vis) });
                        }
                        let ic = if src.starts_with("patch.") { icon::SPARKLE } else { icon::CAMERA };
                        let waiting = node_when(app, scene, &n.id).is_some_and(|w| !when_now(app, &w).unwrap_or(false));
                        let trailing = if waiting {
                            "hidden right now"
                        } else if n.modulated {
                            "moves with music"
                        } else {
                            ""
                        };
                        if widgets::list_row(ui, &t, ic, &label, &sub, trailing, selected).clicked() {
                            app.build.canvas.selected = Some(n.id.clone());
                            app.build.comp.side = 0;
                        }
                    });
                }
            });
        },
    );
    if let Some(r) = add {
        egui::Popup::menu(&r).show(|ui| add_layer_menu(app, ui, scene, now));
    }
}

fn add_layer_menu(app: &mut App, ui: &mut egui::Ui, scene: &str, now: f64) {
    let t = app.t.clone();
    ui.set_min_width(260.0);
    let cams: Vec<String> = app.m.q_list("sources").iter().filter_map(|s| s.get_path("name").and_then(Value::as_str).map(String::from)).collect();
    let overlays: Vec<String> = app
        .m
        .q_list("patches")
        .iter()
        .filter(|p| matches!(p.get_path("kind").and_then(Value::as_str), Some("web" | "shader" | "particles" | "script")))
        .filter_map(|p| p.get_path("id").or_else(|| p.get_path("name")).and_then(Value::as_str).map(|id| format!("patch.{id}")))
        .collect();
    let mut pick = None;
    ui.label(RichText::new("Cameras & media").font(font_medium(type_scale::SMALL)).color(t.text_dim));
    for c in &cams {
        if ui.button(format!("{}  {}", icon::CAMERA, source_label(app, c))).clicked() {
            pick = Some(c.clone());
        }
    }
    if !overlays.is_empty() {
        ui.add_space(spacing::S);
        ui.label(RichText::new("Overlays").font(font_medium(type_scale::SMALL)).color(t.text_dim));
        for o in &overlays {
            if ui.button(format!("{}  {}", icon::SPARKLE, source_label(app, o))).clicked() {
                pick = Some(o.clone());
            }
        }
    }
    if let Some(src) = pick {
        let now_text = scene_text(app, scene, now);
        match now_text {
            Some(text) => write_scene(app, scene, scene_edit::add_layer(&text, &src), now),
            None => app.m.toast("The scene file isn't loaded yet. Try again in a second.", true),
        }
        ui.close();
    }
}

// ---- right: selected layer / scene settings ---------------------------------------------------------

fn side_panel(app: &mut App, ui: &mut egui::Ui, scene: &str, h: f32, now: f64) {
    let t = app.t.clone();
    widgets::panel(ui, &t, |ui| {
        ui.set_width(ui.available_width());
        ui.set_min_height(h - 36.0);
        let mut side = app.build.comp.side;
        if widgets::segmented(ui, &t, &mut side, &["Selected layer", "Scene settings"]) {
            app.build.comp.side = side;
        }
        ui.add_space(spacing::M);
        egui::ScrollArea::vertical().id_salt("comp-side").auto_shrink([false, false]).show(ui, |ui| {
            if app.build.comp.side == 0 {
                layer_panel(app, ui, scene, now);
            } else {
                scene_panel(app, ui, scene, now);
            }
        });
    });
}

fn layer_panel(app: &mut App, ui: &mut egui::Ui, scene: &str, now: f64) {
    let t = app.t.clone();
    let canvas_name = canvas::edit_canvas(app);
    let Some(sel) = app.build.canvas.selected.clone() else {
        widgets::empty_state(ui, &t, icon::HAND, "Pick a layer", "Click a layer on the picture or in the list to change it.", None);
        return;
    };
    let Some(node) = canvas::editor_nodes(app, scene, canvas_name).into_iter().find(|n| n.id == sel) else {
        widgets::hint(ui, &t, "That layer isn't on this screen.");
        return;
    };
    let src = app.build.node_src(scene, &sel).unwrap_or(&sel).to_string();
    let p = format!("scene.{scene}.node.{sel}");
    ui.label(RichText::new(source_label(app, &src)).font(font_semibold(type_scale::HEADING)).color(t.fg));
    widgets::hint(
        ui,
        &t,
        match app.build.canvas.pick {
            canvas::Pick::Tall => "Changes apply to the vertical video.",
            canvas::Pick::Wide => "Changes apply to the main video.",
            canvas::Pick::Both => "Editing the main video. Pick Vertical above to change the vertical one.",
        },
    );
    // a camera layer can show another camera in the same place
    let cams = cameras(app);
    if cams.len() > 1 && cams.iter().any(|(c, _)| *c == src) {
        ui.add_space(spacing::S);
        let mut pick = None;
        ui.horizontal(|ui| {
            ui.label(RichText::new("Camera").color(t.text_dim));
            egui::ComboBox::from_id_salt(("comp-swap", &sel)).selected_text(source_label(app, &src)).width(ui.available_width() - 8.0).show_ui(ui, |ui| {
                for (c, _) in &cams {
                    if ui.selectable_label(*c == src, source_label(app, c)).clicked() && *c != src {
                        pick = Some(c.clone());
                    }
                }
            });
        });
        if let Some(c) = pick
            && let Some(text) = scene_text(app, scene, now)
        {
            match scene_edit::set_layer_source(&text, &sel, &c) {
                Ok((text, id)) => {
                    write_scene(app, scene, Ok(text), now);
                    app.build.canvas.selected = Some(id);
                }
                Err(e) => write_scene(app, scene, Err(e), now),
            }
        }
    }
    let text = scene_text(app, scene, now);
    let when = text.as_deref().and_then(|x| scene_edit::layer_when(x, &sel));
    show_status(app, ui, node.visible, when.as_deref());
    // an overlay's own settings (its text, colors, …)
    if let Some(id) = src.strip_prefix("patch.") {
        overlay_settings(app, ui, id);
    }
    ui.add_space(spacing::M);
    show_when(app, ui, scene, &sel, &src, text.as_deref(), when.as_deref(), now);
    ui.add_space(spacing::M);

    // quick placements
    widgets::section(ui, &t, "", "PLACE IT");
    let [cw, ch] = crate::views::monitor::canvas_size(canvas_name);
    let mut place = None;
    ui.horizontal_wrapped(|ui| {
        let [_, _, nw, nh] = node.rect;
        for (label, r) in [
            ("Full screen", kc::fill_canvas()),
            ("Left half", [0.0, 0.0, 0.5, 1.0]),
            ("Right half", [0.5, 0.0, 0.5, 1.0]),
            ("Corner", [0.68, 0.66, 0.3, 0.3]),
            ("Center", [(1.0 - nw) / 2.0, (1.0 - nh) / 2.0, nw, nh]),
        ] {
            if widgets::button_ex(ui, &t, None, label, Kind::Secondary, Size::Small, 0.0, true).clicked() {
                place = Some(r);
            }
        }
        if widgets::button_ex(ui, &t, None, "Fit camera shape", Kind::Secondary, Size::Small, 0.0, true)
            .on_hover_text("Make the box 16:9 so the camera isn't squashed")
            .clicked()
        {
            place = Some(kc::fit_aspect(node.rect, 16.0 / 9.0, [cw, ch]));
        }
    });
    if let Some(r) = place {
        canvas::apply_rect(app, scene, canvas_name, &sel, r);
    }
    ui.add_space(spacing::M);

    // position & size in %
    widgets::section(ui, &t, "", "POSITION & SIZE");
    let mut r = node.rect;
    let mut changed = false;
    egui::Grid::new("comp-rect").num_columns(4).spacing([10.0, 6.0]).show(ui, |ui| {
        for (i, label) in ["Left", "Top", "Width", "Height"].iter().enumerate() {
            ui.label(RichText::new(*label).color(t.text_dim));
            let mut pct = r[i] * 100.0;
            if ui.add(egui::DragValue::new(&mut pct).range(if i < 2 { -50.0..=100.0 } else { 1.0..=200.0 }).speed(0.2).suffix("%").fixed_decimals(0)).changed()
            {
                r[i] = pct / 100.0;
                changed = true;
            }
            if i % 2 == 1 {
                ui.end_row();
            }
        }
    });
    if changed {
        canvas::apply_rect(app, scene, canvas_name, &sel, r);
    }
    ui.add_space(spacing::M);

    // look
    widgets::section(ui, &t, "", "LOOK");
    let mut rad = node.radius;
    let rr = rounded_corners(ui, &t, &mut rad);
    if rr.changed() || rr.drag_stopped() {
        canvas::apply_shape(app, scene, canvas_name, &sel, node.crop, rad, !rr.dragged());
    }
    let op_a = format!("{p}.opacity");
    let mut op = app.m.get(&op_a).and_then(Value::as_f64).unwrap_or(1.0) as f32;
    let r2 = widgets::labeled_slider(ui, &t, "Opacity", &mut op, 0.0..=1.0, "");
    if r2.changed() {
        app.m.command(Op::SetBase { address: op_a.clone(), value: Value::Float((op as f64 * 100.0).round() / 100.0) });
    }
    ui.add_space(spacing::S);
    widgets::details(ui, &t, ("crop", &sel), "Crop the edges", |ui| {
        let mut crop = node.crop;
        let mut changed = false;
        let mut done = false;
        for (i, label) in ["Left", "Top", "Right", "Bottom"].iter().enumerate() {
            let r = widgets::labeled_slider(ui, &t, label, &mut crop[i], 0.0..=0.45, "");
            changed |= r.changed();
            done |= r.drag_stopped() || (r.changed() && !r.dragged());
        }
        if changed || done {
            canvas::apply_shape(app, scene, canvas_name, &sel, crop, node.radius, done);
        }
    });
    ui.add_space(spacing::M);

    // effects on the layer
    widgets::section(ui, &t, "", "EFFECTS ON THIS LAYER");
    if let Some(text) = scene_text(app, scene, now) {
        let on = scene_edit::layer_effects(&text, &sel);
        effect_toggles(app, ui, &on, |fx, want| scene_edit::set_layer_effect(&text, &sel, fx, want), scene, now);
    }
    ui.add_space(spacing::M);

    // order + remove
    widgets::section(ui, &t, "", "ARRANGE");
    ui.horizontal(|ui| {
        let mut mv = None;
        if widgets::button_ex(ui, &t, Some(icon::UP), "Bring forward", Kind::Secondary, Size::Small, 0.0, true).clicked() {
            mv = Some(true);
        }
        if widgets::button_ex(ui, &t, Some(icon::DOWN), "Send back", Kind::Secondary, Size::Small, 0.0, true).clicked() {
            mv = Some(false);
        }
        if let Some(fwd) = mv
            && let Some(text) = scene_text(app, scene, now)
        {
            write_scene(app, scene, scene_edit::move_layer(&text, &sel, fwd), now);
        }
    });
    ui.add_space(spacing::S);
    if widgets::hold_button(ui, &t, "Remove layer", t.bright_red, 0.8)
        && let Some(text) = scene_text(app, scene, now)
    {
        write_scene(app, scene, scene_edit::remove_layer(&text, &sel), now);
        app.build.canvas.selected = None;
    }
}

// ---- when a layer shows ------------------------------------------------------------------------------

/// A layer's show condition from the loaded scene definition (first video that has the layer).
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

/// Is a show condition true right now? `Err` (its mistake) when it can't be read: the video
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
        Overlay { id, playing } => {
            format!("the {} overlay {}", source_label(app, &format!("patch.{id}")), if *playing { "is playing" } else { "isn't playing" })
        }
    }
}

/// Why the selected layer isn't on the video right now (nothing when it is).
fn show_status(app: &App, ui: &mut egui::Ui, visible: bool, when: Option<&str>) {
    let t = &app.t;
    if !visible {
        ui.add_space(spacing::S);
        widgets::callout(ui, t, Tone::Info, icon::EYE_OFF, "Switched off", "You switched it off in the Layers list. Turn it back on there.", None);
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
                None => "It only shows when its own rule is true (see Details under Show this layer), and right now it isn't.".into(),
            };
            ui.add_space(spacing::S);
            widgets::callout(ui, t, Tone::Warn, icon::EYE_OFF, "Hidden right now", &body, None);
        }
        Err(_) => {
            ui.add_space(spacing::S);
            widgets::callout(
                ui,
                t,
                Tone::Danger,
                icon::WARN,
                "Its show rule has a mistake",
                "So it never shows. Fix it under Details below, or pick Always.",
                None,
            );
        }
    }
}

/// The overlay's own settings (the same controls as on the Overlays tab).
fn overlay_settings(app: &mut App, ui: &mut egui::Ui, id: &str) {
    let Some(p) = app.m.q_list("patches").iter().find(|p| p.get_path("id").and_then(Value::as_str) == Some(id)).cloned() else { return };
    if p.get_path("params").and_then(Value::as_list).is_none_or(|l| l.is_empty()) {
        return;
    }
    let t = app.t.clone();
    ui.add_space(spacing::M);
    widgets::section(ui, &t, "", "OVERLAY SETTINGS");
    widgets::hint(ui, &t, "The same overlay looks like this in every scene. Changes show right away.");
    crate::views::patches::params_ui(app, ui, &p);
}

const CHECK_KINDS: [&str; 3] = ["The show mode", "A song request", "An overlay"];

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
            // another overlay in this scene first (like "while the intro plays")
            let ids: Vec<&str> = app.m.q_list("patches").iter().filter_map(|p| p.get_path("id").and_then(Value::as_str)).collect();
            let pick = ids
                .iter()
                .find(|id| {
                    let s = format!("patch.{id}");
                    s != src && app.build.node_src(scene, &s).is_some()
                })
                .or(ids.first());
            Overlay { id: pick.map_or_else(String::new, |s| s.to_string()), playing: true }
        }
    }
}

/// "Show this layer: Always / Only when …" with plain checks; anything else as text under Details.
#[allow(clippy::too_many_arguments)]
fn show_when(app: &mut App, ui: &mut egui::Ui, scene: &str, sel: &str, src: &str, text: Option<&str>, when: Option<&str>, now: f64) {
    use scene_edit::LayerCheck::*;
    let t = app.t.clone();
    widgets::section(ui, &t, "", "SHOW THIS LAYER");
    let Some(text) = text else {
        widgets::hint(ui, &t, "Loading…");
        return;
    };
    let mut write: Option<Option<String>> = None;
    let mut only = usize::from(when.is_some());
    if widgets::segmented(ui, &t, &mut only, &["Always", "Only when…"]) {
        write = Some(if only == 0 {
            None
        } else {
            let k = if app.m.q_list("modes").is_empty() { 1 } else { 0 };
            new_check(app, scene, src, k).text()
        });
    }
    if let Some(w) = when {
        ui.add_space(spacing::S);
        match scene_edit::parse_layer_when(w) {
            Some(mut checks) => {
                let modes: Vec<(String, String)> =
                    app.m.q_list("modes").iter().filter_map(Value::as_str).map(|m| (m.to_string(), rules::nice_name(m))).collect();
                let overlays: Vec<(String, String)> = app
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
                    egui::Frame::new().fill(t.surface_hi).corner_radius(radius::CONTROL).inner_margin(egui::Margin::symmetric(10, 8)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            let k = match c {
                                Mode { .. } => 0,
                                Song { .. } => 1,
                                Overlay { .. } => 2,
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
                            Overlay { id, playing } => {
                                if let Some(o) = rules::pick_name(
                                    ui,
                                    ("layer-check-overlay", sel, i),
                                    id,
                                    &overlays,
                                    "Pick an overlay",
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
                widgets::callout(
                    ui,
                    &t,
                    Tone::Info,
                    icon::EDIT,
                    "It has its own rule",
                    "Change it under Details below, or pick Always to show it all the time.",
                    None,
                );
            }
        }
        ui.add_space(spacing::XS);
        widgets::details(ui, &t, ("layer-when-raw", sel), "Details", |ui| {
            widgets::hint(ui, &t, "The rule as Stream Engine reads it. It shows the layer while this is true.");
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

/// Toggle rows for the built-in effects (and effect overlays), writing the scene file on change.
fn effect_toggles(app: &mut App, ui: &mut egui::Ui, on: &[String], edit: impl Fn(&str, bool) -> anyhow::Result<String>, scene: &str, now: f64) {
    let t = app.t.clone();
    let mut change = None;
    for (id, name) in scene_edit::EFFECTS {
        let mut v = on.iter().any(|n| n == id);
        ui.horizontal(|ui| {
            if widgets::toggle(ui, &t, &mut v).changed() {
                change = Some((id.to_string(), v));
            }
            ui.label(RichText::new(*name).color(if v { t.fg } else { t.text_dim }));
        });
    }
    for extra in on.iter().filter(|n| !scene_edit::EFFECTS.iter().any(|(id, _)| id == n)) {
        let mut v = true;
        ui.horizontal(|ui| {
            if widgets::toggle(ui, &t, &mut v).changed() {
                change = Some((extra.clone(), false));
            }
            ui.label(RichText::new(scene_edit::effect_name(extra)).color(t.fg));
        });
    }
    if let Some((fx, want)) = change {
        let r = edit(&fx, want);
        write_scene(app, scene, r, now);
    }
}

fn scene_panel(app: &mut App, ui: &mut egui::Ui, scene: &str, now: f64) {
    let t = app.t.clone();
    let Some(text) = scene_text(app, scene, now) else {
        widgets::hint(ui, &t, "Loading…");
        return;
    };
    let label = app
        .m
        .q_list("scenes")
        .iter()
        .find(|s| s.get_path("name").and_then(Value::as_str) == Some(scene))
        .and_then(|s| s.get_path("label").and_then(Value::as_str))
        .unwrap_or(scene)
        .to_string();
    let label = nice(&label);
    if app.build.comp.rename_for != scene {
        app.build.comp.rename_for = scene.to_string();
        app.build.comp.rename = label.clone();
    }
    widgets::section(ui, &t, "", "NAME");
    ui.horizontal(|ui| {
        ui.add(se_ui_kit::widgets::field(&mut app.build.comp.rename).desired_width(ui.available_width() - 96.0));
        let new = app.build.comp.rename.trim().to_string();
        if widgets::button_ex(ui, &t, None, "Rename", Kind::Secondary, Size::Small, 0.0, !new.is_empty() && new != label).clicked() {
            write_scene(app, scene, scene_edit::set_label(&text, &new), now);
        }
    });
    ui.add_space(spacing::M);

    widgets::section(ui, &t, "", "SWITCHING TO THIS SCENE");
    if let Some(r) = crate::views::transitions::scene_choice(app, ui, scene, &text) {
        write_scene(app, scene, r, now);
    }
    ui.add_space(spacing::S);
    let speeds: [(&str, Option<[i64; 2]>); 4] = [("Auto", None), ("Fast", Some([250, 450])), ("Normal", Some([500, 900])), ("Slow", Some([1000, 1600]))];
    let cur = scene_edit::speed(&text);
    let mut i = speeds.iter().position(|(_, s)| *s == cur).unwrap_or(0);
    ui.horizontal(|ui| {
        ui.label(RichText::new("Speed").color(t.text_dim));
        if widgets::segmented(ui, &t, &mut i, &["Auto", "Fast", "Normal", "Slow"]) {
            write_scene(app, scene, scene_edit::set_speed(&text, speeds[i].1), now);
        }
    });
    if cur.is_some() && !speeds.iter().any(|(_, s)| *s == cur) {
        let [lo, hi] = cur.unwrap_or_default();
        widgets::hint(ui, &t, &format!("Custom: {:.1}–{:.1} seconds", lo as f64 / 1000.0, hi as f64 / 1000.0));
    }
    ui.add_space(spacing::M);

    widgets::section(ui, &t, "", "EFFECTS ON THE WHOLE SCENE");
    let on = scene_edit::scene_effects(&text);
    effect_toggles(app, ui, &on, |fx, want| scene_edit::set_scene_effect(&text, fx, want), scene, now);
    ui.add_space(spacing::M);

    widgets::section(ui, &t, "", "WHEN THIS SCENE COMES ON");
    widgets::hint(ui, &t, "Stream Engine does these, top to bottom, every time you switch to this scene.");
    steps_block(app, ui, scene, &text, "on_enter", now);
    ui.add_space(spacing::M);
    widgets::section(ui, &t, "", "WHEN IT GOES OFF");
    widgets::hint(ui, &t, "Done when you switch away from it.");
    steps_block(app, ui, scene, &text, "on_exit", now);
    ui.add_space(spacing::M);

    widgets::section(ui, &t, "", "BACKGROUND");
    let bg = scene_edit::background(&text);
    let mut c = bg.as_deref().and_then(se_ui_kit::theme::hex).unwrap_or(egui::Color32::BLACK);
    ui.horizontal(|ui| {
        let r = egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::Opaque).on_hover_text("Pick the background color");
        if r.changed() {
            let hex = format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b());
            write_scene_soon(app, scene, scene_edit::set_background(&text, Some(&hex)), now);
        }
        ui.label(RichText::new(if bg.is_some() { "Your color" } else { "Black (the default)" }).color(t.fg));
        if bg.is_some() {
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::button_ex(ui, &t, None, "Use black", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                    write_scene(app, scene, scene_edit::set_background(&text, None), now);
                }
            });
        }
    });
    widgets::hint(ui, &t, "Shows wherever no layer covers the video.");
    ui.add_space(spacing::M);

    widgets::section(ui, &t, "", "MORE");
    ui.horizontal(|ui| {
        if widgets::button_ex(ui, &t, Some(icon::COPY), "Duplicate", Kind::Secondary, Size::Small, 0.0, true).clicked() {
            let base = format!("{} copy", label);
            let used: Vec<String> = scenes(app).into_iter().map(|(n, _, _)| n).collect();
            let mut name = scene_edit::slug(&base);
            let mut n = 2;
            while used.contains(&name) {
                name = format!("{}_{n}", scene_edit::slug(&base));
                n += 1;
            }
            match scene_edit::duplicate(&text, &base, None) {
                Ok(copy) => {
                    app.m.action("project.write", Value::map().with("path", file_path(&name)).with("text", copy));
                    app.build.canvas.scene = Some(name.clone());
                    app.build.comp.reread = Some((name, now + 0.5));
                    app.m.refresh_soon();
                }
                Err(e) => app.m.toast(format!("Couldn't copy the scene: {e:#}"), true),
            }
        }
    });
    ui.add_space(spacing::S);
    let on_air = app.m.str("show.scene.program") == scene;
    if on_air {
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
}

/// A scene's "when it comes on / goes off" steps (`key`: `on_enter` / `on_exit`). Unfinished
/// steps stay on the page; the file gets the finished ones.
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
    if cmds.is_empty() {
        widgets::hint(ui, &t, "Nothing yet.");
    }
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

/// "Rounded corners" as a Square … Round slider (0–120 px on the canvas, no units shown).
fn rounded_corners(ui: &mut egui::Ui, t: &se_ui_kit::Theme, rad: &mut f32) -> egui::Response {
    ui.vertical(|ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("Rounded corners").font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let word = match *rad {
                    r if r < 1.0 => "Square",
                    r if r < 30.0 => "A little",
                    r if r < 70.0 => "Rounded",
                    _ => "Very round",
                };
                ui.label(RichText::new(word).color(t.fg));
            });
        });
        ui.spacing_mut().slider_width = ui.available_width();
        ui.spacing_mut().interact_size.y = 20.0;
        ui.add(egui::Slider::new(rad, 0.0..=120.0).show_value(false))
    })
    .inner
}
