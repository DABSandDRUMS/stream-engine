//! Scenes → Layout: the composition view (§15.5). Pick a scene on the left, arrange its layers
//! on the composite view in the middle, and tune the selected layer — or the scene's
//! transitions and effects — on the right. Geometry edits go through live state (`set_base`,
//! saved to the scene file by the engine); adding/removing/reordering layers, effects and
//! transitions edit the scene file with `scene_edit` (comments kept).

use crate::app::App;
use crate::views::live::nice;
use crate::views::{canvas, scene_edit};
use egui::{Align, Layout, RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::canvas as kc;
use se_ui_kit::theme::{font_medium, font_semibold, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};

#[derive(Default)]
pub struct CompositionState {
    /// Right panel: 0 = selected layer, 1 = scene settings.
    pub side: usize,
    pub rename: String,
    pub rename_for: String,
    pub new_open: bool,
    pub new_name: String,
    /// Scene whose file text must be re-read (after a write), and when.
    reread: Option<(String, f64)>,
    asked: Option<(String, u64)>,
}

fn file_path(scene: &str) -> String {
    format!("scenes/{scene}.toml")
}

fn file_key(scene: &str) -> String {
    format!("scene.file:{scene}")
}

/// The scene file text (queried once per scene/connection and again after writes).
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
    app.m.q(&file_key(scene)).and_then(|v| v.get_path("text")).and_then(Value::as_str).map(String::from)
}

/// Write the scene file and re-read it once the engine has reloaded.
fn write_scene(app: &mut App, scene: &str, text: anyhow::Result<String>, now: f64) {
    match text {
        Ok(text) => {
            app.m.action("project.write", Value::map().with("path", file_path(scene)).with("text", text));
            app.build.comp.reread = Some((scene.to_string(), now + 0.5));
            app.m.refresh_soon();
        }
        Err(e) => app.m.toast(format!("Couldn't change the scene: {e:#}"), true),
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
        ui.label(RichText::new("Your scenes").font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.add_space(spacing::S);
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
        if app.build.comp.new_open {
            ui.add(se_ui_kit::widgets::field(&mut app.build.comp.new_name).hint_text("Name, e.g. Chill cams").desired_width(ui.available_width()));
            ui.horizontal(|ui| {
                let name = scene_edit::slug(&app.build.comp.new_name);
                let taken = list.iter().any(|(n, _, _)| *n == name);
                let ok = !name.is_empty() && !taken;
                if widgets::button_ex(ui, &t, None, "Create", Kind::Primary, Size::Small, 0.0, ok).clicked() {
                    let label = app.build.comp.new_name.trim().to_string();
                    let key = (1..=9).find(|k| !list.iter().any(|(_, _, used)| used == k));
                    app.m.action("project.write", Value::map().with("path", file_path(&name)).with("text", scene_edit::new_scene(&label, key)));
                    app.build.canvas.scene = Some(name.clone());
                    app.build.comp.new_open = false;
                    app.build.comp.new_name.clear();
                    app.build.comp.reread = Some((name, now + 0.5));
                    app.m.refresh_soon();
                }
                if widgets::button_ex(ui, &t, None, "Cancel", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                    app.build.comp.new_open = false;
                }
                if taken {
                    widgets::hint(ui, &t, "That name is taken");
                }
            });
        } else if widgets::button_ex(ui, &t, Some(icon::PLUS), "New scene", Kind::Secondary, Size::Medium, ui.available_width(), true).clicked() {
            app.build.comp.new_open = true;
        }
    });
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
                        if widgets::list_row(ui, &t, ic, &label, &sub, if n.modulated { "moves with music" } else { "" }, selected).clicked() {
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

    widgets::section(ui, &t, "", "TRANSITIONS INTO THIS SCENE");
    widgets::hint(ui, &t, "Switching picks one of these at random. Pick \"More\" or \"Most\" to see one come up more often.");
    let pool = scene_edit::pool(&text);
    let mut all: Vec<String> = app.m.q_list("transitions").iter().filter_map(|v| v.as_str().map(String::from)).filter(|n| n != "cut").collect();
    all.sort();
    all.dedup();
    let mut new_pool = pool.clone();
    let mut changed = false;
    for name in &all {
        let pos = new_pool.iter().position(|(n, _)| n == name);
        let mut on = pos.is_some();
        ui.horizontal(|ui| {
            if widgets::toggle(ui, &t, &mut on).changed() {
                changed = true;
                match (on, pos) {
                    (true, None) => new_pool.push((name.clone(), 1.0)),
                    (false, Some(i)) => {
                        new_pool.remove(i);
                    }
                    _ => {}
                }
            }
            ui.label(RichText::new(nice(name)).color(if on { t.fg } else { t.text_dim }));
            if let Some(i) = new_pool.iter().position(|(n, _)| n == name) {
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let mut w = (new_pool[i].1.round().clamp(1.0, 3.0) as usize) - 1;
                    if widgets::segmented(ui, &t, &mut w, &["Normal", "More", "Most"]) {
                        new_pool[i].1 = [1.0, 2.0, 3.0][w];
                        changed = true;
                    }
                });
            }
        });
    }
    if changed {
        write_scene(app, scene, scene_edit::set_pool(&text, &new_pool), now);
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
        app.build.canvas.scene = None;
        app.build.canvas.selected = None;
        app.m.refresh_soon();
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
