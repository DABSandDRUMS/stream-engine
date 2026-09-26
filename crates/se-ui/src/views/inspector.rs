//! Inspector (§15.5): generated from metadata for any selected thing. Each row shows the
//! resolved value with a control matching its `Meta` type, its provenance (base → scene →
//! bindings → overrides → clamps), and has Modulate (create a binding) and MIDI learn.

use crate::app::App;
use crate::panels::Panel;
use egui::RichText;
use se_proto::{Meta, Op, Value, ValueType};
use se_ui_kit::widgets::{self, icon};
use std::collections::HashMap;
use std::time::Instant;

#[derive(Default)]
pub struct InspectorState {
    fetched_for: Option<String>,
    last_fetch: Option<Instant>,
    /// Address whose provenance is expanded.
    pub explain: Option<String>,
    /// Text being edited for string/list fields.
    text: HashMap<String, String>,
    pub filter: String,
}

/// Refetch the selected subtree this often (values outside the subscription + metadata).
const FETCH_MS: u128 = 1000;

fn short<'a>(sel: &str, a: &'a str) -> &'a str {
    a.strip_prefix(sel).map(|r| r.trim_start_matches('.')).filter(|r| !r.is_empty()).unwrap_or(a)
}

/// Does this address reach program? (scene-bound addresses only when that scene is on program)
pub fn affects_program(app: &App, a: &str) -> bool {
    match a.strip_prefix("scene.") {
        Some(rest) => rest.split('.').next() == Some(app.m.str("show.scene.program")),
        None => !a.starts_with("show.") && !a.starts_with("project."),
    }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::section(ui, &t, icon::SEARCH, "Inspector");
    let Some(sel) = app.selected.clone() else {
        ui.label(RichText::new("select something in the library, the canvas, or the multiview").color(t.fg_dim));
        return;
    };
    let st = &mut app.build.inspector;
    if st.fetched_for.as_deref() != Some(&sel) || st.last_fetch.is_none_or(|t| t.elapsed().as_millis() > FETCH_MS) {
        st.fetched_for = Some(sel.clone());
        st.last_fetch = Some(Instant::now());
        app.m.fetch(&format!("{sel}.**"));
        app.m.fetch(&sel);
    }
    ui.horizontal(|ui| {
        ui.label(RichText::new(&sel).strong().monospace());
        if ui.small_button(icon::CROSS).on_hover_text("clear selection").clicked() {
            app.selected = None;
        }
    });
    actions_for(app, ui, &sel);
    ui.add(se_ui_kit::widgets::field(&mut app.build.inspector.filter).hint_text(format!("{} filter parameters", icon::SEARCH)).desired_width(f32::INFINITY));
    ui.separator();
    let f = app.build.inspector.filter.to_lowercase();
    let mut rows: Vec<(String, Value)> = Vec::new();
    let pre = format!("{sel}.");
    let inside = |a: &String| *a == sel || a.starts_with(&pre);
    for (a, v) in app.m.state.range(sel.clone()..).take_while(|(a, _)| a.starts_with(&sel)).filter(|(a, _)| inside(a)) {
        rows.push((a.clone(), v.clone()));
    }
    for (a, v) in app.m.fetched.range(sel.clone()..).take_while(|(a, _)| a.starts_with(&sel)).filter(|(a, _)| inside(a)) {
        if !app.m.state.contains_key(a) {
            rows.push((a.clone(), v.clone()));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows.retain(|(a, _)| f.is_empty() || a.to_lowercase().contains(&f));
    if rows.is_empty() {
        ui.label(RichText::new(if app.m.connected { "no parameters under this address" } else { "engine offline" }).color(t.fg_dim));
        return;
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for (a, v) in rows {
            row(app, ui, &sel, &a, &v);
        }
    });
}

fn actions_for(app: &mut App, ui: &mut egui::Ui, sel: &str) {
    ui.horizontal_wrapped(|ui| {
        if let Some(name) = sel.strip_prefix("preset.") {
            if ui.button(format!("{} fire", icon::PLAY)).clicked() {
                app.m.command(Op::PresetFire { name: name.into(), payload: Value::Null });
            }
            if ui.button(format!("{} release", icon::STOP)).clicked() {
                app.m.command(Op::PresetRelease { name: name.into() });
            }
            if ui.button(format!("{} presets/{name}.toml", icon::CONSOLE)).clicked() {
                app.open_in_editor(&format!("presets/{name}.toml"), None);
            }
        }
        if sel.starts_with("patch.") || sel.starts_with("fx.") {
            if ui.button(format!("{} trigger", icon::PLAY)).on_hover_text("fire on preview/test").clicked() {
                app.m.command(Op::Trigger { address: sel.into(), payload: Value::Null });
            }
            if ui.button(format!("{} release", icon::STOP)).clicked() {
                app.m.command(Op::Release { address: sel.into() });
            }
        }
        if let Some(rest) = sel.strip_prefix("scene.")
            && let Some((scene, _)) = rest.split_once(".node.")
        {
            if ui.button(format!("{} scenes/{scene}.toml", icon::CONSOLE)).clicked() {
                app.open_in_editor(&format!("scenes/{scene}.toml"), None);
            }
            if app.m.str("show.scene.preview") != scene && ui.button("to preview").clicked() {
                app.m.command(Op::SceneGo { scene: scene.into() });
            }
        }
    });
}

fn meta_of(app: &App, a: &str) -> Meta {
    app.m.meta.get(a).cloned().unwrap_or_default()
}

fn row(app: &mut App, ui: &mut egui::Ui, sel: &str, a: &str, v: &Value) {
    let t = app.t.clone();
    let meta = meta_of(app, a);
    let live = affects_program(app, a);
    let modulated = app.is_modulated(a);
    let learning = app.m.b("controllers.learn.active") && app.m.str("controllers.learn.target") == a;
    widgets::live_edge(ui, &t, live, |ui| {
        ui.horizontal(|ui| {
            let name = RichText::new(short(sel, a)).color(if modulated { t.modulated() } else { t.fg });
            let lbl = ui.add(egui::Label::new(name).sense(egui::Sense::click()));
            let desc = meta.description.clone().unwrap_or_default();
            let lbl = lbl.on_hover_text(format!("{a}\n{desc}\nclick: provenance"));
            if lbl.clicked() {
                if app.build.inspector.explain.as_deref() == Some(a) {
                    app.build.inspector.explain = None;
                } else {
                    app.build.inspector.explain = Some(a.to_string());
                    app.m.explain_of(a);
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if !meta.readonly
                    && matches!(meta.ty, ValueType::Float | ValueType::Int | ValueType::Bool | ValueType::Any | ValueType::Vec4 | ValueType::Color)
                {
                    let midi = if learning { RichText::new("MIDI…").color(t.yellow) } else { RichText::new("MIDI").small() };
                    let r = ui.small_button(midi).on_hover_text(if learning {
                        "move a control on a MIDI device (click to cancel)"
                    } else {
                        "MIDI learn: move a hardware control to bind it"
                    });
                    if r.clicked() {
                        if learning {
                            app.m.action("midi.learn.cancel", Value::Null);
                        } else {
                            app.m.action("midi.learn", Value::map().with("target", a));
                        }
                    }
                    if ui.small_button(RichText::new("~").color(t.modulated())).on_hover_text("Modulate: drive this from a signal").clicked() {
                        app.build.modulate.start(a, &meta, v);
                        app.focus_panel(Panel::Modulate);
                    }
                }
                control(app, ui, a, v, &meta);
            });
        });
        if app.build.inspector.explain.as_deref() == Some(a) {
            provenance(app, ui, a);
        }
    });
}

fn unit(m: &Meta) -> String {
    m.unit.as_deref().map(|u| format!(" {u}")).unwrap_or_default()
}

fn control(app: &mut App, ui: &mut egui::Ui, a: &str, v: &Value, meta: &Meta) {
    let t = app.t.clone();
    let set = |app: &mut App, value: Value| app.m.command(Op::SetBase { address: a.to_string(), value });
    if meta.readonly {
        ui.label(RichText::new(format!("{v}{}", unit(meta))).monospace().color(t.fg_dim));
        return;
    }
    match (meta.ty, v) {
        (ValueType::Trigger, _) => {
            if ui.small_button(format!("{} fire", icon::PLAY)).clicked() {
                let target = a.trim_end_matches(".trigger");
                app.m.command(Op::Trigger { address: target.into(), payload: Value::Null });
            }
            ui.add(egui::ProgressBar::new(v.as_f64().unwrap_or(0.0) as f32).desired_width(60.0));
        }
        (ValueType::Bool, _) | (ValueType::Any, Value::Bool(_)) => {
            let mut b = v.truthy();
            if ui.checkbox(&mut b, "").changed() {
                set(app, Value::Bool(b));
            }
        }
        (ValueType::Enum, _) => {
            let cur = v.as_str().unwrap_or("").to_string();
            egui::ComboBox::from_id_salt(("enum", a)).selected_text(&cur).show_ui(ui, |ui| {
                for o in &meta.options {
                    if ui.selectable_label(*o == cur, o).clicked() {
                        set(app, Value::Str(o.clone()));
                    }
                }
            });
        }
        (ValueType::Int, _) | (ValueType::Any, Value::Int(_)) => {
            let mut i = v.as_i64().unwrap_or(0);
            let mut dv = egui::DragValue::new(&mut i).suffix(unit(meta));
            if let Some([lo, hi]) = meta.range {
                dv = dv.range(lo as i64..=hi as i64);
            }
            if ui.add(dv).changed() {
                set(app, Value::Int(i));
            }
        }
        (ValueType::Float, _) | (ValueType::Any, Value::Float(_)) => {
            let mut f = v.as_f64().unwrap_or(0.0);
            let r = match meta.range {
                Some([lo, hi]) if hi - lo <= 10.0 => ui.add(egui::Slider::new(&mut f, lo..=hi).suffix(unit(meta)).max_decimals(3)),
                Some([lo, hi]) => ui.add(egui::DragValue::new(&mut f).range(lo..=hi).speed((hi - lo) / 500.0).suffix(unit(meta))),
                None => ui.add(egui::DragValue::new(&mut f).speed(0.01).suffix(unit(meta))),
            };
            if r.changed() {
                set(app, Value::Float(f));
            }
        }
        (ValueType::Color, _) => {
            let c = v.as_vec4().unwrap_or([1.0; 4]);
            let mut rgba = egui::Rgba::from_rgba_unmultiplied(c[0], c[1], c[2], c[3]);
            if egui::color_picker::color_edit_button_rgba(ui, &mut rgba, egui::color_picker::Alpha::OnlyBlend).changed() {
                set(app, Value::from([rgba.r(), rgba.g(), rgba.b(), rgba.a()]));
            }
        }
        (ValueType::Vec4 | ValueType::Vec2, _) | (ValueType::Any, Value::List(_)) if v.as_list().is_some_and(|l| l.iter().all(|x| x.as_f64().is_some())) => {
            let mut xs: Vec<f64> = v.as_list().unwrap_or(&[]).iter().filter_map(Value::as_f64).collect();
            let mut changed = false;
            for x in xs.iter_mut().rev() {
                changed |= ui.add(egui::DragValue::new(x).speed(0.002).max_decimals(4)).changed();
            }
            if changed {
                set(app, Value::List(xs.into_iter().map(Value::Float).collect()));
            }
        }
        _ => {
            let buf = app.build.inspector.text.entry(a.to_string()).or_insert_with(|| v.as_str().map(String::from).unwrap_or_else(|| v.to_string()));
            let r = ui.add(se_ui_kit::widgets::field(buf).desired_width(160.0));
            if r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                let text = buf.clone();
                let value = if v.as_str().is_some() || meta.ty == ValueType::String { Value::Str(text) } else { Value::parse_text(&text) };
                set(app, value);
                app.build.inspector.text.remove(a);
            } else if !r.has_focus() {
                app.build.inspector.text.remove(a);
            }
        }
    }
}

fn provenance(app: &mut App, ui: &mut egui::Ui, a: &str) {
    let t = app.t.clone();
    let Some(p) = app.m.explain.clone().filter(|p| p.address == a) else {
        ui.label(RichText::new("explaining…").small().color(t.fg_dim));
        return;
    };
    egui::Frame::new().fill(t.bg_darker).corner_radius(4).inner_margin(6).show(ui, |ui| {
        ui.label(RichText::new(format!("{} = {}", short(a, &p.address), p.value)).strong().monospace());
        for l in &p.layers {
            let c = match l.kind.as_str() {
                "binding" => t.modulated(),
                "override" | "animation" | "envelope" if l.active => t.tally_preview(),
                "clamp" => t.bright_red,
                "scene" if l.active => t.cyan,
                _ if l.active => t.fg,
                _ => t.fg_dim,
            };
            let prio = l.priority.map(|p| format!(", {p}")).unwrap_or_default();
            let mark = if l.active { "▸" } else { " " };
            let src = if l.source.is_empty() { String::new() } else { format!(" ({}{prio})", l.source) };
            ui.label(RichText::new(format!("{mark} {:<9} {}{src}", l.kind, l.value)).monospace().color(c));
        }
        ui.horizontal(|ui| {
            if ui.small_button("refresh").clicked() {
                app.m.explain_of(a);
            }
            if ui.small_button(format!("{} trace last change", icon::TRACE)).clicked()
                && let Some(r) = app.m.trace.iter().rev().find(|r| r.kind == "change" && r.label.starts_with(a))
            {
                let id = r.id;
                app.m.trace_of(id);
                app.focus_panel(Panel::Trace);
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_names_strip_the_selection_prefix() {
        assert_eq!(short("scene.duo.node.cam_kit", "scene.duo.node.cam_kit.rect.wide"), "rect.wide");
        assert_eq!(short("fx.vhs", "fx.vhs"), "fx.vhs");
    }
}
