//! Editing state shared by the Scenes page and the tools (§15.5): scene definitions from the
//! project, the canvas editor, inspector, bindings, rules, and the project library.

use crate::app::{App, ViewId};
use crate::panels::Panel;
use crate::views::{canvas, inspector, modulate, rules, tools};
use egui::RichText;
use se_proto::Value;
use se_ui_kit::widgets::{self, LedState, icon};
use std::collections::{BTreeMap, HashMap};

#[derive(Default)]
pub struct BuildState {
    pub library_filter: String,
    pub library_focus: bool,
    /// `config.scenes` by scene name (full scene definitions from the project).
    scene_cfg: BTreeMap<String, Value>,
    /// (scene, node id) → source.
    node_src: HashMap<(String, String), String>,
    scenes_seq: u64,
    pub canvas: canvas::CanvasState,
    pub inspector: inspector::InspectorState,
    pub modulate: modulate::ModulateState,
    pub rules: rules::RulesEditor,
    pub tools: tools::ToolsState,
    pub comp: crate::views::composition::CompositionState,
    pub transitions: crate::views::transitions::TransitionsState,
}

impl BuildState {
    pub fn node_src(&self, scene: &str, id: &str) -> Option<&str> {
        self.node_src.get(&(scene.to_string(), id.to_string())).map(String::as_str)
    }
    pub fn scene_config(&self, scene: &str) -> Option<&Value> {
        self.scene_cfg.get(scene)
    }
    pub fn scene_configs(&self) -> &BTreeMap<String, Value> {
        &self.scene_cfg
    }
}

/// Rebuild caches derived from query replies (runs every frame; cheap unless a reply arrived).
pub fn sync(app: &mut App) {
    let seq = app.m.q_seq("config.scenes");
    if seq == app.build.scenes_seq {
        return;
    }
    app.build.scenes_seq = seq;
    let Some(Value::Map(scenes)) = app.m.q("config.scenes").cloned() else { return };
    app.build.node_src.clear();
    for (name, s) in &scenes {
        if let Some(Value::Map(canvases)) = s.get_path("canvas") {
            for c in canvases.values() {
                for n in c.get_path("nodes").and_then(Value::as_list).unwrap_or(&[]) {
                    let src = n.get_path("src").and_then(Value::as_str).unwrap_or("");
                    let id = n.get_path("id").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(src);
                    app.build.node_src.insert((name.clone(), id.to_string()), src.to_string());
                }
            }
        }
    }
    app.build.scene_cfg = scenes;
}

/// Library kinds, in display order: (kind id, icon, title, `project.files` kind).
const KINDS: &[(&str, &str, &str, &str)] = &[
    ("lights", icon::LIGHT, "Lights", "lights"),
    ("timelines", icon::TIMELINE, "Timelines", "timelines"),
    ("mixes", icon::MIX, "Mixes", "mixes"),
    ("controllers", icon::CONTROLLER, "Controllers", "controllers"),
    ("commands", icon::BOT, "Chatbot", "commands"),
    ("alerts", icon::ALERT, "Alerts", "alerts"),
    ("rewards", icon::PRESET, "Rewards", "rewards"),
    ("layouts", icon::SETTINGS, "Layouts", "layouts"),
];

/// The full view that edits a project kind (views owned by other subsystems), if one exists.
pub fn view_for_kind(kind: &str) -> Option<ViewId> {
    Some(match kind.split('/').next().unwrap_or(kind) {
        "scenes" => ViewId::Composition,
        "transitions" => ViewId::Transitions,
        "sources" | "patches" => ViewId::Sources,
        "assets" => ViewId::Media,
        "alerts" => ViewId::Alerts,
        "rules" => ViewId::Reactions,
        "controllers" => ViewId::Controllers,
        "commands" => ViewId::Chatbot,
        "presets" => ViewId::Actions,
        "bindings" => ViewId::Modulation,
        "timelines" => ViewId::Timeline,
        "mixes" => ViewId::Mixer,
        "lights" => ViewId::Lights,
        "rewards" => ViewId::Twitch,
        _ => return None,
    })
}

fn matches(filter: &str, s: &str) -> bool {
    filter.is_empty() || s.to_lowercase().contains(filter)
}

pub fn library(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let hint = format!("{} search library ({})", icon::SEARCH, app.keys_label("search"));
    let r = ui.add(se_ui_kit::widgets::field(&mut app.build.library_filter).hint_text(hint).desired_width(f32::INFINITY));
    if std::mem::take(&mut app.build.library_focus) {
        r.request_focus();
    }
    let f = app.build.library_filter.to_lowercase();
    let files = app.m.q_list("project.files").to_vec();
    let sel = app.selected.clone();
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        // Sources: every source used by any scene.
        let mut sources: Vec<String> = app
            .m
            .q_list("scenes")
            .iter()
            .flat_map(|s| s.get_path("sources").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str().map(String::from)))
            .collect();
        sources.sort();
        sources.dedup();
        egui::CollapsingHeader::new(format!("{} Sources ({})", icon::DEVICE, sources.len())).default_open(true).show(ui, |ui| {
            for s in sources.iter().filter(|s| matches(&f, s)) {
                let a = format!("source.{s}");
                if ui.selectable_label(sel.as_deref() == Some(&a), format!("{} {s}", icon::DEVICE)).clicked() {
                    app.select(a);
                }
            }
        });
        let scenes = app.m.q_list("scenes").to_vec();
        egui::CollapsingHeader::new(format!("{} Scenes ({})", icon::SCENE, scenes.len())).default_open(true).show(ui, |ui| {
            for s in scenes {
                let n = s.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
                let nodes: Vec<String> =
                    s.get_path("nodes").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str().map(String::from)).collect();
                if !matches(&f, &n) && !nodes.iter().any(|x| matches(&f, x)) {
                    continue;
                }
                let prog = app.m.str("show.scene.program") == n;
                let prev = app.m.str("show.scene.preview") == n;
                let head = format!(
                    "{}{n}",
                    if prog {
                        "● "
                    } else if prev {
                        "○ "
                    } else {
                        ""
                    }
                );
                let resp = egui::CollapsingHeader::new(RichText::new(head).color(if prog {
                    t.tally_program()
                } else if prev {
                    t.tally_preview()
                } else {
                    t.fg
                }))
                .id_salt(("scene", &n))
                .show(ui, |ui| {
                    for id in &nodes {
                        let a = format!("scene.{n}.node.{id}");
                        if ui.selectable_label(sel.as_deref() == Some(&a), format!("{} {id}", icon::DEVICE)).clicked() {
                            app.select(a);
                            app.build.canvas.scene = Some(n.clone());
                            app.build.canvas.selected = Some(id.clone());
                        }
                    }
                });
                resp.header_response.context_menu(|ui| {
                    if ui.button(format!("{} edit on canvas (to preview)", icon::SCENE)).clicked() {
                        app.build.canvas.scene = Some(n.clone());
                        app.m.command(se_proto::Op::SceneGo { scene: n.clone() });
                        ui.close();
                    }
                    if ui.button(format!("{} open scenes/{n}.toml", icon::CONSOLE)).clicked() {
                        app.open_in_editor(&format!("scenes/{n}.toml"), None);
                        ui.close();
                    }
                });
            }
        });
        let patches = app.m.q_list("patches").to_vec();
        egui::CollapsingHeader::new(format!("{} Patches ({})", icon::PATCH, patches.len())).show(ui, |ui| {
            if patches.is_empty() {
                ui.label(
                    RichText::new(app.m.query_errors.get("patches").map(|_| "patch runtime not running").unwrap_or("no patches in patches/"))
                        .small()
                        .color(t.fg_dim),
                );
            }
            for p in patches {
                let id = p.get_path("id").or_else(|| p.get_path("name")).and_then(Value::as_str).unwrap_or("").to_string();
                if !matches(&f, &id) {
                    continue;
                }
                let err = p.get_path("error").is_some_and(|e| !e.is_null() && e.as_str() != Some(""))
                    || app.m.get(&format!("patch.{id}.error")).and_then(Value::as_str).is_some_and(|s| !s.is_empty());
                let kind = p.get_path("kind").and_then(Value::as_str).unwrap_or("");
                ui.horizontal(|ui| {
                    widgets::led(
                        ui,
                        &t,
                        if err {
                            LedState::Error
                        } else if p.get_path("active").is_some_and(Value::truthy) {
                            LedState::Active
                        } else {
                            LedState::Idle
                        },
                    );
                    let a = format!("patch.{id}");
                    if ui.selectable_label(sel.as_deref() == Some(&a), format!("{id} ({kind})")).clicked() {
                        app.select(a);
                    }
                    if ui.small_button(icon::PLAY).on_hover_text("trigger on preview/test").clicked() {
                        app.m.command(se_proto::Op::Trigger { address: format!("patch.{id}"), payload: Value::Null });
                    }
                });
            }
        });
        let fx: Vec<String> = {
            let mut v: Vec<String> = app.m.under("fx").filter_map(|(a, _)| a.strip_prefix("fx.")?.split('.').next().map(String::from)).collect();
            v.dedup();
            v
        };
        egui::CollapsingHeader::new(format!("{} Effects ({})", icon::EFFECT, fx.len())).show(ui, |ui| {
            for n in fx.iter().filter(|n| matches(&f, n)) {
                let a = format!("fx.{n}");
                if ui.selectable_label(sel.as_deref() == Some(&a), format!("{} {n}", icon::EFFECT)).clicked() {
                    app.select(a);
                }
            }
        });
        let presets = app.m.q_list("presets").to_vec();
        egui::CollapsingHeader::new(format!("{} Presets ({})", icon::PRESET, presets.len())).show(ui, |ui| {
            for p in presets {
                let n = p.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
                if !matches(&f, &n) {
                    continue;
                }
                ui.horizontal(|ui| {
                    widgets::led(ui, &t, if p.get_path("active").is_some_and(Value::truthy) { LedState::Active } else { LedState::Idle });
                    let a = format!("preset.{n}");
                    if ui.selectable_label(sel.as_deref() == Some(&a), &n).clicked() {
                        app.select(a);
                    }
                });
            }
        });
        let rules_list = app.m.q_list("rules").to_vec();
        egui::CollapsingHeader::new(format!("{} Rules ({})", icon::RULE, rules_list.len())).show(ui, |ui| {
            if ui.small_button(format!("{} new rule", icon::RULE)).clicked() {
                app.build.rules.new_rule();
                app.focus_panel(Panel::Rules);
            }
            for r in rules_list {
                let n = r.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
                if !matches(&f, &n) && !matches(&f, r.get_path("when").and_then(Value::as_str).unwrap_or("")) {
                    continue;
                }
                ui.horizontal(|ui| {
                    widgets::led(ui, &t, if r.get_path("enabled").is_some_and(Value::truthy) { LedState::Healthy } else { LedState::Idle });
                    if ui.selectable_label(app.build.rules.editing_name() == Some(n.as_str()), &n).clicked() {
                        app.build.rules.open(&r);
                        app.focus_panel(Panel::Rules);
                    }
                });
            }
        });
        let bindings = app.m.q_list("bindings").to_vec();
        egui::CollapsingHeader::new(format!("{} Bindings ({})", icon::BINDING, bindings.len())).show(ui, |ui| {
            for b in bindings {
                let n = b.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
                let target = b.get_path("target").and_then(Value::as_str).unwrap_or("").to_string();
                let signal = b.get_path("signal").and_then(Value::as_str).unwrap_or("").to_string();
                if !matches(&f, &n) && !matches(&f, &target) && !matches(&f, &signal) {
                    continue;
                }
                ui.horizontal(|ui| {
                    widgets::led(ui, &t, if b.get_path("active").is_some_and(Value::truthy) { LedState::Modulated } else { LedState::Idle });
                    let r = ui.selectable_label(false, format!("{n}: {target} ← {signal}"));
                    if r.clicked() {
                        app.build.modulate.edit_existing(&b);
                        app.focus_panel(Panel::Modulate);
                    }
                });
            }
        });
        for (kind, ic, title, fkind) in KINDS {
            let items: Vec<&Value> = files
                .iter()
                .filter(|v| v.get_path("kind").and_then(Value::as_str).is_some_and(|k| k == *fkind || k.starts_with(&format!("{fkind}/"))))
                .collect();
            let view = view_for_kind(kind);
            egui::CollapsingHeader::new(format!("{ic} {title} ({})", items.len())).id_salt(("kind", kind)).show(ui, |ui| {
                if let Some(v) = view
                    && ui.small_button(format!("{ic} open {title} view")).clicked()
                {
                    app.open_view(v);
                }
                if items.is_empty() {
                    ui.label(RichText::new(format!("no {kind} files yet")).small().color(t.fg_dim));
                }
                for it in items {
                    let name = it.get_path("name").and_then(Value::as_str).unwrap_or("");
                    let k = it.get_path("kind").and_then(Value::as_str).unwrap_or("");
                    let path = it.get_path("path").and_then(Value::as_str).unwrap_or("").to_string();
                    let label = if k.contains('/') { format!("{} / {name}", k.split_once('/').map(|x| x.1).unwrap_or("")) } else { name.to_string() };
                    if !matches(&f, &label) {
                        continue;
                    }
                    let r =
                        ui.selectable_label(false, label).on_hover_text(format!("{path}\nclick: open the {title} view (or $EDITOR) · right-click: $EDITOR"));
                    if r.clicked() {
                        match view {
                            Some(v) => app.open_view(v),
                            None => app.open_in_editor(&path, None),
                        }
                    }
                    if r.secondary_clicked() {
                        app.open_in_editor(&path, None);
                    }
                }
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn library_filter_is_case_insensitive_substring() {
        assert!(matches("", "anything"));
        assert!(matches("kit", "cam_Kit"));
        assert!(!matches("wide", "cam_kit"));
    }
}
