//! One persistent slot editor for every video-FX host, shared with the scene inspector.
use crate::app::App;
use crate::views::{composition, links, media, scene_edit};
use egui::RichText;
use se_proto::{Op, Value};
use se_ui_kit::theme::spacing;
use std::collections::BTreeSet;

#[derive(Clone, Debug, PartialEq)]
pub struct Target {
    pub args: Value,
    pub prefix: String,
    pub label: String,
    pub authored: Value,
}

fn text<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get_path(key).and_then(Value::as_str).unwrap_or("")
}
fn list<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get_path(key).and_then(Value::as_list).unwrap_or(&[])
}
fn target(args: Value, prefix: String, label: String, authored: &Value) -> Target {
    let mut fields = std::collections::BTreeMap::new();
    for key in ["fx", "fx_enabled"] {
        if let Some(value) = authored.get_path(key) { fields.insert(key.to_string(), value.clone()); }
    }
    if text(&args, "kind") == "group" {
        for key in ["nodes", "z"] {
            if let Some(value) = authored.get_path(key) { fields.insert(key.to_string(), value.clone()); }
        }
    }
    Target { args, prefix, label, authored: Value::Map(fields) }
}

/// Enumerate authored hosts, deduplicating nodes repeated on different layouts.
fn targets(scenes: &Value, sources: &Value, render: &Value) -> Vec<Target> {
    let mut out = Vec::new();
    if let Some(scenes) = scenes.as_map() {
        for (scene, def) in scenes {
            let scene_args = Value::map().with("scene", scene.as_str());
            out.push(target(scene_args.clone().with("kind", "scene"), format!("scene.{scene}"), format!("Scene: {scene}"), def));
            let mut seen = BTreeSet::new();
            if let Some(canvases) = def.get_path("canvas").and_then(Value::as_map) {
                // Match the inspector's main-canvas-first authored node selection.
                for (canvas, layout) in canvases.iter().filter(|(c, _)| c.as_str() == "wide").chain(canvases.iter().filter(|(c, _)| c.as_str() != "wide")) {
                    let args = scene_args.clone().with("canvas", canvas.as_str());
                    out.push(target(args.clone().with("kind", "layout"), format!("scene.{scene}.canvas.{canvas}"), format!("Layout: {scene} / {canvas}"), layout));
                    for node in list(layout, "nodes") {
                        let id = text(node, "id");
                        let id = if id.is_empty() { text(node, "src") } else { id };
                        if !id.is_empty() && seen.insert(id.to_string()) {
                            out.push(target(scene_args.clone().with("kind", "node").with("node", id), format!("scene.{scene}.node.{id}"), format!("Layer: {scene} / {id}"), node));
                        }
                    }
                    for group in list(layout, "groups") {
                        let id = text(group, "id");
                        out.push(target(args.clone().with("kind", "group").with("group", id), format!("scene.{scene}.canvas.{canvas}.group.{id}"), format!("Group: {scene} / {canvas} / {id}"), group));
                    }
                }
            }
        }
    }
    if let Some(sources) = sources.as_map() {
        for (name, def) in sources {
            out.push(target(Value::map().with("kind", "source").with("name", name.as_str()), format!("source.{name}"), format!("Source: {name}"), def));
        }
    }
    for kind in ["canvas", "output"] {
        for canvas in ["wide", "tall"] {
            let fx = render.get_path(&format!("{kind}_fx.{canvas}")).cloned().unwrap_or(Value::List(Vec::new()));
            let enabled = render.get_path(&format!("{kind}_fx_enabled.{canvas}")).cloned().unwrap_or(Value::Bool(true));
            out.push(target(Value::map().with("kind", kind).with("canvas", canvas), format!("render.{kind}.{canvas}"), format!("Master {kind}: {canvas}"), &Value::map().with("fx", fx).with("fx_enabled", enabled)));
        }
    }
    out
}

pub fn scene_target(app: &App, scene: &str, node: Option<&str>) -> Option<Target> {
    let def = app.m.q("config.scenes")?.as_map()?.get(scene)?;
    let args = Value::map().with("scene", scene);
    match node {
        None => Some(target(args.with("kind", "scene"), format!("scene.{scene}"), format!("Scene: {scene}"), def)),
        Some(id) => {
            let canvases = def.get_path("canvas")?.as_map()?;
            let authored = canvases.iter().filter(|(c, _)| c.as_str() == "wide").chain(canvases.iter().filter(|(c, _)| c.as_str() != "wide"))
                .flat_map(|(_, c)| list(c, "nodes"))
                .find(|n| { let node_id = text(n, "id"); if node_id.is_empty() { text(n, "src") == id } else { node_id == id } })?;
            Some(target(args.with("kind", "node").with("node", id), format!("scene.{scene}.node.{id}"), format!("Layer: {scene} / {id}"), authored))
        }
    }
}

fn action(app: &mut App, name: &str, args: Value) {
    // A pending whole-scene edit must land first, or it could overwrite this chain action.
    composition::flush(app, 0.0, true);
    let mut scenes = BTreeSet::new();
    for path in ["target.scene", "scene"] {
        if let Some(scene) = args.get_path(path).and_then(Value::as_str) { scenes.insert(scene.to_string()); }
    }
    for t in list(&args, "targets") {
        if let Some(scene) = t.get_path("scene").and_then(Value::as_str) { scenes.insert(scene.to_string()); }
    }
    for scene in scenes { composition::scene_file_changed(app, &scene); }
    app.m.action(name, args);
    app.m.refresh_soon();
}
fn slot_set(app: &mut App, t: &Target, id: &str, key: &str, value: Value) {
    action(app, "fx.slot.set", Value::map().with("target", t.args.clone()).with("id", id).with("key", key).with("value", value));
}

#[derive(Clone, Default)]
struct State {
    active: String,
    selected: BTreeSet<String>,
    preset: String,
    replace: bool,
    save_name: String,
    save_label: String,
    group_id: String,
    group_nodes: String,
    group_z: i32,
    group_host: String,
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let state_id = egui::Id::new("video-fx-target-rack");
    let mut st = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<State>(state_id)));
    let all = targets(app.m.q("config.scenes").unwrap_or(&Value::Null), app.m.q("config.sources").unwrap_or(&Value::Null), app.m.q("config.render").unwrap_or(&Value::Null));
    st.selected.retain(|p| all.iter().any(|t| &t.prefix == p));
    if !all.iter().any(|t| t.prefix == st.active) {
        st.active = all.first().map(|t| t.prefix.clone()).unwrap_or_default();
    }
    egui::ScrollArea::vertical().id_salt("fx-target-rack-scroll").show(ui, |ui| {
        ui.heading("Video effect chains");
        ui.label("Each target has its own ordered slots. Apply a saved chain to checked targets to make independent copies.");
        egui::CollapsingHeader::new("Targets — select a rack or check several for a preset").default_open(true).show(ui, |ui| {
            for t in &all {
                ui.horizontal(|ui| {
                    let mut selected = st.selected.contains(&t.prefix);
                    if ui.checkbox(&mut selected, "").changed() {
                        if selected { st.selected.insert(t.prefix.clone()); } else { st.selected.remove(&t.prefix); }
                    }
                    if ui.selectable_label(st.active == t.prefix, &t.label).clicked() { st.active = t.prefix.clone(); }
                });
            }
        });
        let chains = app.m.q_list("fx.chains").to_vec();
        ui.horizontal_wrapped(|ui| {
            egui::ComboBox::from_id_salt("fx-chain-preset").selected_text(if st.preset.is_empty() { "Saved chain" } else { &st.preset }).show_ui(ui, |ui| {
                for c in &chains {
                    let name = text(c, "name");
                    let label = text(c, "label");
                    ui.selectable_value(&mut st.preset, name.to_string(), if label.is_empty() { name } else { label });
                }
            });
            ui.checkbox(&mut st.replace, "Replace existing slots");
            if ui.add_enabled(!st.selected.is_empty() && chains.iter().any(|c| text(c, "name") == st.preset), egui::Button::new("Apply to checked targets")).clicked() {
                let selected: Vec<Value> = all.iter().filter(|t| st.selected.contains(&t.prefix)).map(|t| t.args.clone()).collect();
                action(app, "fx.chain.apply", Value::map().with("name", st.preset.as_str()).with("targets", Value::List(selected)).with("replace", st.replace));
            }
        });
        if let Some(t) = all.iter().find(|t| t.prefix == st.active) {
            ui.separator();
            ui.heading(&t.label);
            chain(app, ui, t);
            ui.add_space(spacing::S);
            ui.horizontal_wrapped(|ui| {
                ui.add(egui::TextEdit::singleline(&mut st.save_name).hint_text("Chain name").desired_width(150.0));
                ui.add(egui::TextEdit::singleline(&mut st.save_label).hint_text("Label (optional)").desired_width(150.0));
                if ui.add_enabled(!st.save_name.trim().is_empty(), egui::Button::new("Save this chain")).clicked() {
                    let mut args = Value::map().with("name", st.save_name.trim()).with("target", t.args.clone());
                    if !st.save_label.trim().is_empty() { args = args.with("label", st.save_label.trim()); }
                    action(app, "fx.chain.save", args);
                }
            });
            if matches!(text(&t.args, "kind"), "layout" | "group") { groups(app, ui, t, &mut st); }
        }
    });
    ui.data_mut(|d| d.insert_temp(state_id, st));
}

fn groups(app: &mut App, ui: &mut egui::Ui, t: &Target, st: &mut State) {
    let scene = text(&t.args, "scene");
    let canvas = text(&t.args, "canvas");
    if st.group_host != t.prefix {
        st.group_host = t.prefix.clone();
        st.group_id = text(&t.args, "group").to_string();
        st.group_nodes = list(&t.authored, "nodes").iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
        st.group_z = t.authored.get_path("z").and_then(Value::as_i64).unwrap_or(0) as i32;
    }
    let layout = app.m.q("config.scenes").and_then(|v| v.get_path(&format!("{scene}.canvas.{canvas}"))).cloned().unwrap_or(Value::Null);
    ui.separator();
    ui.heading("Composited groups");
    ui.label("Members are composited together at the group's z. This is separate from FX exclusivity. Nodes may belong to only one group in this layout.");
    ui.label(format!("Available layer IDs: {}", list(&layout, "nodes").iter().map(|n| { let id = text(n, "id"); if id.is_empty() { text(n, "src") } else { id } }).collect::<Vec<_>>().join(", ")));
    for g in list(&layout, "groups") {
        if ui.selectable_label(st.group_id == text(g, "id"), format!("{} · z {} · {}", text(g, "id"), g.get_path("z").and_then(Value::as_i64).unwrap_or(0), list(g, "nodes").iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "))).clicked() {
            st.group_id = text(g, "id").to_string();
            st.group_nodes = list(g, "nodes").iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
            st.group_z = g.get_path("z").and_then(Value::as_i64).unwrap_or(0) as i32;
        }
    }
    ui.horizontal(|ui| {
        ui.label("Group ID");
        ui.text_edit_singleline(&mut st.group_id);
        ui.label("z");
        ui.add(egui::DragValue::new(&mut st.group_z));
    });
    ui.add(egui::TextEdit::singleline(&mut st.group_nodes).hint_text("Member layer IDs, comma-separated").desired_width(f32::INFINITY));
    ui.horizontal(|ui| {
        let args = Value::map().with("scene", scene).with("canvas", canvas).with("id", st.group_id.trim());
        if ui.add_enabled(!st.group_id.trim().is_empty(), egui::Button::new("Save membership / z")).clicked() {
            let nodes = st.group_nodes.split(',').map(str::trim).filter(|s| !s.is_empty()).map(Value::from).collect();
            action(app, "fx.group.set", args.clone().with("nodes", Value::List(nodes)).with("z", st.group_z as i64));
        }
        if ui.add_enabled(list(&layout, "groups").iter().any(|g| text(g, "id") == st.group_id), egui::Button::new("Delete group")).clicked() {
            action(app, "fx.group.set", args.with("nodes", Value::List(Vec::new())).with("delete", true));
        }
    });
}

/// Persistent controls: neither disabling a slot nor disabling its chain opts into trigger mode.
pub fn chain(app: &mut App, ui: &mut egui::Ui, t: &Target) {
    ui.push_id(&t.prefix, |ui| {
        let mut enabled = app.m.get(&format!("{}.fx_enabled", t.prefix)).map(Value::truthy).unwrap_or_else(|| t.authored.get_path("fx_enabled").is_none_or(Value::truthy));
        ui.horizontal(|ui| {
            if ui.checkbox(&mut enabled, "Chain enabled").changed() {
                action(app, "fx.chain.set", Value::map().with("target", t.args.clone()).with("enabled", enabled));
            }
            ui.menu_button("Add effect", |ui| add_menu(app, ui, t));
        });
        let entries = scene_edit::fx_values(list(&t.authored, "fx"));
        if entries.is_empty() { ui.label("No slots on this target."); }
        for (index, e) in entries.iter().enumerate() {
            let base = format!("{}.fx.{}", t.prefix, e.id);
            ui.push_id(&e.id, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let mut on = app.m.get(&format!("{base}.enabled")).map(Value::truthy).unwrap_or(e.enabled);
                    if ui.checkbox(&mut on, "Enabled").changed() { slot_set(app, t, &e.id, "enabled", Value::Bool(on)); }
                    ui.label(RichText::new(scene_edit::effect_name(&e.name)).strong());
                    ui.weak(format!("[{}]", e.id));
                    let mut triggered = app.m.get(&format!("{base}.triggered")).map(Value::truthy).unwrap_or(e.triggered);
                    if ui.checkbox(&mut triggered, "Only on trigger").changed() { slot_set(app, t, &e.id, "triggered", Value::Bool(triggered)); }
                    if ui.button("Trigger kind").on_hover_text("Fires the shared effect-kind envelope; only trigger-mode slots follow it.").clicked() {
                        let address = if e.name.starts_with("patch.") { e.name.clone() } else { format!("fx.{}", e.name) };
                        app.m.command(Op::Trigger { address, payload: Value::Null });
                    }
                    links::trigger_badge(app, ui, &format!("{base}.enabled"), &scene_edit::effect_name(&e.name));
                    if ui.add_enabled(index > 0, egui::Button::new("Earlier")).clicked() { action(app, "fx.slot.move", Value::map().with("target", t.args.clone()).with("id", e.id.as_str()).with("forward", false)); }
                    if ui.add_enabled(index + 1 < entries.len(), egui::Button::new("Later")).clicked() { action(app, "fx.slot.move", Value::map().with("target", t.args.clone()).with("id", e.id.as_str()).with("forward", true)); }
                    if ui.button("Remove").clicked() { action(app, "fx.slot.remove", Value::map().with("target", t.args.clone()).with("id", e.id.as_str())); }
                });
                egui::CollapsingHeader::new("Settings").id_salt("settings").show(ui, |ui| settings(app, ui, t, e, &base));
                ui.separator();
            });
        }
    });
}

pub fn add_menu(app: &mut App, ui: &mut egui::Ui, t: &Target) {
    let mut chosen = None;
    egui::ScrollArea::vertical().max_height(420.0).show(ui, |ui| {
        for e in scene_edit::EFFECTS {
            if ui.button(e.label).on_hover_text(e.about).clicked() { chosen = Some(e.id.to_string()); }
        }
        for p in app.m.q_list("patches") {
            if text(p, "layer") == "effect" && ui.button(text(p, "id")).clicked() { chosen = Some(format!("patch.{}", text(p, "id"))); }
        }
    });
    if let Some(name) = chosen {
        action(app, "fx.slot.add", Value::map().with("target", t.args.clone()).with("name", name));
        ui.close();
    }
}

/// Preview with the existing live override, persist through the slot action, then release it.
fn commit_number(app: &mut App, ui: &mut egui::Ui, t: &Target, e: &scene_edit::FxEntry, key: &str, address: &str, r: &egui::Response, value: Value) {
    let held_id = egui::Id::new("fx-rack-held");
    if r.drag_stopped() || (r.changed() && !r.dragged()) {
        slot_set(app, t, &e.id, key, value);
        if ui.data_mut(|d| d.get_temp_mut_or_default::<BTreeSet<String>>(held_id).remove(address)) {
            app.m.command(Op::Release { address: address.to_string() });
        }
    } else if r.changed() && r.dragged() {
        app.m.command(Op::Set { address: address.to_string(), value });
        ui.data_mut(|d| d.get_temp_mut_or_default::<BTreeSet<String>>(held_id).insert(address.to_string()));
    }
}

fn number(app: &mut App, ui: &mut egui::Ui, t: &Target, e: &scene_edit::FxEntry, base: &str, p: &scene_edit::FxParam) {
    let address = format!("{base}.{}", p.name);
    let mut value = app.m.get(&address).and_then(Value::as_f64).or_else(|| e.num(p.name)).or_else(|| (p.name != "amount").then(|| app.m.get(&format!("fx.{}.{}", e.name, p.name)).and_then(Value::as_f64)).flatten()).unwrap_or(p.default);
    ui.horizontal(|ui| {
        ui.label(p.label);
        let r = ui.add(egui::Slider::new(&mut value, p.min..=p.max).suffix(p.unit));
        commit_number(app, ui, t, e, p.name, &address, &r, Value::Float(value));
        links::modulate_button(app, ui, &address, p.label, (p.min, p.max));
    });
}

fn settings(app: &mut App, ui: &mut egui::Ui, t: &Target, e: &scene_edit::FxEntry, base: &str) {
    number(app, ui, t, e, base, &scene_edit::STRENGTH);
    if let Some(def) = scene_edit::effect_def(&e.name) {
        for p in def.params { number(app, ui, t, e, base, p); }
        if e.name == "lut" {
            let mut file = app.m.get(&format!("{base}.file")).and_then(Value::as_str).or_else(|| e.text("file")).unwrap_or("").to_string();
            if media::picker(app, ui, ("rack-lut", &t.prefix, &e.id), media::MediaKind::Lut, &mut file) { slot_set(app, t, &e.id, "file", Value::from(file)); }
        }
        return;
    }
    let Some(id) = e.name.strip_prefix("patch.") else { return };
    let patch = app.m.q_list("patches").iter().find(|p| text(p, "id") == id).cloned().unwrap_or(Value::Null);
    for p in list(&patch, "params") {
        let key = text(p, "name");
        let address = format!("{base}.{key}");
        let default = p.get_path("default").cloned().unwrap_or(Value::Null);
        let current = app.m.get(&address).or_else(|| e.params.get(key)).or_else(|| app.m.get(text(p, "address"))).cloned().unwrap_or(default.clone());
        match text(p, "type") {
            "float" | "int" => {
                let label = crate::views::live::nice(key);
                let range = list(p, "range");
                let min = range.first().and_then(Value::as_f64).unwrap_or(0.0);
                let max = range.get(1).and_then(Value::as_f64).unwrap_or(1.0);
                let label: &str = &label;
                // Custom manifests are dynamic; the control uses their range and inherited value.
                let mut value = current.as_f64().or_else(|| e.num(key)).unwrap_or(0.0);
                ui.horizontal(|ui| {
                    ui.label(label);
                    let r = ui.add(egui::DragValue::new(&mut value).range(min..=max));
                    let value = if text(p, "type") == "int" { Value::Int(value.round() as i64) } else { Value::Float(value) };
                    commit_number(app, ui, t, e, key, &address, &r, value);
                    links::modulate_button(app, ui, &address, label, (min, max));
                });
            }
            "bool" => {
                let mut v = current.truthy();
                if ui.checkbox(&mut v, crate::views::live::nice(key)).changed() { slot_set(app, t, &e.id, key, Value::Bool(v)); }
            }
            "color" => {
                let mut color = crate::views::patches::rgba(&current).unwrap_or(egui::Color32::WHITE);
                let source = e.params.get(key).and_then(Value::as_str).and_then(se_proto::palette::Reference::parse).filter(|reference| reference.component().is_none());
                ui.horizontal(|ui| {
                    ui.label(crate::views::live::nice(key));
                    egui::ComboBox::from_id_salt(("fx-slot-color-source", &address))
                        .selected_text(source.map(|reference| format!("Palette: {}", crate::views::live::nice(se_proto::palette::SLOTS[reference.slot()]))).unwrap_or_else(|| "Fixed color".into()))
                        .show_ui(ui, |ui| {
                            if ui.selectable_label(source.is_none(), "Fixed color").clicked() && source.is_some() {
                                slot_set(app, t, &e.id, key, crate::views::patches::color_value(color, &default));
                            }
                            for (slot, name) in se_proto::palette::SLOTS.iter().enumerate() {
                                let selected = source.is_some_and(|reference| reference.slot() == slot);
                                if ui.selectable_label(selected, format!("Palette: {}", crate::views::live::nice(name))).clicked() && !selected {
                                    slot_set(app, t, &e.id, key, Value::from(format!("stream:{name}")));
                                }
                            }
                        });
                    ui.add_enabled_ui(source.is_none(), |ui| {
                        if egui::color_picker::color_edit_button_srgba(ui, &mut color, egui::color_picker::Alpha::OnlyBlend).changed() {
                            slot_set(app, t, &e.id, key, crate::views::patches::color_value(color, &default));
                        }
                    });
                });
            }
            "enum" => {
                egui::ComboBox::from_id_salt(("fx-slot-enum", &address)).selected_text(current.as_str().unwrap_or("")).show_ui(ui, |ui| {
                    for option in list(p, "options").iter().filter_map(Value::as_str) {
                        if ui.selectable_label(current.as_str() == Some(option), option).clicked() {
                            slot_set(app, t, &e.id, key, Value::from(option));
                        }
                    }
                });
            }
            _ => {
                let draft_id = egui::Id::new(("fx-rack-text", &address));
                let mut v = ui.data(|d| d.get_temp::<String>(draft_id)).unwrap_or_else(|| current.as_str().unwrap_or("").to_string());
                ui.horizontal(|ui| {
                    ui.label(crate::views::live::nice(key));
                    let r = ui.text_edit_singleline(&mut v);
                    if r.changed() { ui.data_mut(|d| d.insert_temp(draft_id, v.clone())); }
                    if r.lost_focus() {
                        if v != current.as_str().unwrap_or("") { slot_set(app, t, &e.id, key, Value::from(v.clone())); }
                        ui.data_mut(|d| d.remove::<String>(draft_id));
                    }
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn targets_keep_group_membership_and_canvas_hosts_distinct() {
        let scenes = Value::map().with("duo", Value::map().with("canvas", Value::map()
            .with("wide", Value::map().with("nodes", Value::List(vec![Value::map().with("id", "cam").with("src", "camera")])).with("groups", Value::List(vec![Value::map().with("id", "pair").with("nodes", Value::List(vec!["cam".into()]))])))
            .with("tall", Value::map().with("nodes", Value::List(vec![Value::map().with("id", "cam").with("src", "camera")])))));
        let render = Value::map().with("output_fx", Value::map().with("tall", Value::List(vec![Value::map().with("id", "blur_2").with("name", "blur")]))).with("output_fx_enabled", Value::map().with("tall", false));
        let all = targets(&scenes, &Value::map().with("camera", Value::map()), &render);
        assert_eq!(all.iter().filter(|t| text(&t.args, "kind") == "node").count(), 1);
        let group = all.iter().find(|t| text(&t.args, "kind") == "group").unwrap();
        assert_eq!(group.prefix, "scene.duo.canvas.wide.group.pair");
        assert_eq!(list(&group.authored, "nodes"), &[Value::from("cam")]);
        let output = all.iter().find(|t| t.prefix == "render.output.tall").unwrap();
        assert!(!output.authored.get_path("fx_enabled").unwrap().truthy());
        assert_eq!(text(&list(&output.authored, "fx")[0], "id"), "blur_2");
        assert!(list(&all.iter().find(|t| t.prefix == "render.canvas.tall").unwrap().authored, "fx").is_empty());
    }
}
