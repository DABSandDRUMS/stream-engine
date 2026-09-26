//! Patches (§6, §15.5): every patch with its kind, layer, status, and last error
//! (`file:line`), script CPU/memory, enable/disable/reload/edit/trigger, and "New patch" from
//! the shipped templates.

use crate::app::App;
use egui::RichText;
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};

const KINDS: &[&str] = &["script", "shader", "particles", "web", "dsp"];

#[derive(Clone, Default)]
struct NewForm {
    id: String,
    kind: usize,
    template: String,
    open: bool,
}

fn state_led(st: &str) -> LedState {
    match st {
        "loaded" => LedState::Healthy,
        "error" => LedState::Error,
        "suspended" => LedState::Armed,
        _ => LedState::Idle,
    }
}

fn action(app: &mut App, name: &str, args: Value) {
    app.m.command(Op::Action { name: name.into(), args });
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let poll = egui::Id::new("patches-view-poll");
    let now = ui.input(|i| i.time);
    let last: f64 = ui.data_mut(|d| d.get_temp(poll)).unwrap_or(-10.0);
    if now - last > 1.0 {
        ui.data_mut(|d| d.insert_temp(poll, now));
        app.m.query("patches", Value::Null);
        if app.m.q("patch.templates").is_none() || now - last > 30.0 {
            app.m.query("patch.templates", Value::Null);
        }
    }
    let form_id = egui::Id::new("patches-view-new");
    let mut form: NewForm = ui.data_mut(|d| d.get_temp(form_id)).unwrap_or_default();

    widgets::section(ui, &t, icon::PATCH, "Patches");
    ui.add_space(6.0);

    // --- New patch ------------------------------------------------------------------------
    widgets::card(ui, &t, icon::PATCH, "New patch", LedState::Idle, |ui| {
        ui.horizontal(|ui| {
            ui.label(RichText::new("id").color(t.fg_dim));
            ui.add(egui::TextEdit::singleline(&mut form.id).hint_text("my_overlay").desired_width(160.0));
            egui::ComboBox::from_id_salt("patch-new-kind").selected_text(KINDS[form.kind]).show_ui(ui, |ui| {
                for (i, k) in KINDS.iter().enumerate() {
                    ui.selectable_value(&mut form.kind, i, *k);
                }
            });
            let kind = KINDS[form.kind];
            let templates: Vec<(String, String)> = app
                .m
                .q_list("patch.templates")
                .iter()
                .filter(|v| v.get_path("kind").and_then(Value::as_str) == Some(kind))
                .map(|v| {
                    (
                        v.get_path("name").and_then(Value::as_str).unwrap_or("").to_string(),
                        v.get_path("description").and_then(Value::as_str).unwrap_or("").to_string(),
                    )
                })
                .collect();
            if !templates.iter().any(|(n, _)| *n == form.template) {
                form.template = templates.first().map(|(n, _)| n.clone()).unwrap_or_else(|| "default".into());
            }
            egui::ComboBox::from_id_salt("patch-new-template").selected_text(form.template.clone()).show_ui(ui, |ui| {
                for (n, d) in &templates {
                    ui.selectable_value(&mut form.template, n.clone(), n).on_hover_text(d);
                }
            });
            ui.checkbox(&mut form.open, "open in editor");
            let valid = !form.id.is_empty() && form.id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
            if ui.add_enabled(valid, egui::Button::new(format!("{} create", icon::CHECK))).clicked() {
                action(
                    app,
                    "patch.new",
                    Value::map().with("id", form.id.clone()).with("kind", kind).with("template", form.template.clone()).with("open", form.open),
                );
                app.m.toast(format!("creating patch {}", form.id), false);
                form.id.clear();
                app.m.refresh_soon();
            }
        });
        if let Some(d) = app
            .m
            .q_list("patch.templates")
            .iter()
            .find(|v| {
                v.get_path("kind").and_then(Value::as_str) == Some(KINDS[form.kind])
                    && v.get_path("name").and_then(Value::as_str) == Some(form.template.as_str())
            })
            .and_then(|v| v.get_path("description").and_then(Value::as_str))
        {
            ui.label(RichText::new(d).small().color(t.fg_dim));
        }
    });
    ui.data_mut(|d| d.insert_temp(form_id, form));
    ui.add_space(6.0);

    // --- Library ------------------------------------------------------------------------------
    let patches = app.m.q_list("patches").to_vec();
    if patches.is_empty() {
        ui.label(
            RichText::new(if app.m.query_errors.contains_key("patches") { "patch loader not running" } else { "no patches in patches/ yet" }).color(t.fg_dim),
        );
        return;
    }
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        for p in patches {
            let s = |k: &str| p.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
            let id = s("id");
            let st = app.m.get(&format!("patch.{id}.state")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| s("state"));
            let err = {
                let live = app.m.get(&format!("patch.{id}.error")).and_then(Value::as_str).unwrap_or("").to_string();
                if live.is_empty() { s("error") } else { live }
            };
            let enabled = p.get_path("enabled").is_none_or(Value::truthy);
            let title = format!("{}  ·  {} / {}", if s("label").is_empty() { id.clone() } else { s("label") }, s("kind"), s("layer"));
            widgets::card(ui, &t, icon::PATCH, &title, state_led(&st), |ui| {
                ui.horizontal(|ui| {
                    widgets::pill(ui, &t, icon::PATCH, &st, state_led(&st));
                    ui.label(RichText::new(format!("patch.{id}")).monospace().small().color(t.fg_dim));
                    let env = app.m.f(&format!("patch.{id}.env"));
                    if env > 0.0 {
                        ui.label(RichText::new(format!("env {:.2}", env)).small().color(t.accent));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button(icon::CONSOLE).on_hover_text("open in editor").clicked() {
                            action(app, "patch.open", Value::map().with("id", id.clone()));
                        }
                        if ui.small_button("\u{f021}").on_hover_text("reload from disk").clicked() {
                            action(app, "patch.reload", Value::map().with("id", id.clone()));
                        }
                        let (label, name) = if enabled { (icon::STOP, "patch.disable") } else { (icon::PLAY, "patch.enable") };
                        let hint = if st == "suspended" {
                            "resume"
                        } else if enabled {
                            "disable"
                        } else {
                            "enable"
                        };
                        let name = if st == "suspended" { "patch.enable" } else { name };
                        if ui.small_button(if st == "suspended" { icon::PLAY } else { label }).on_hover_text(hint).clicked() {
                            action(app, name, Value::map().with("id", id.clone()));
                        }
                        if p.get_path("trigger").is_some_and(Value::truthy) && ui.small_button(icon::PRESET).on_hover_text("trigger").clicked() {
                            app.m.command(Op::Trigger { address: format!("patch.{id}"), payload: Value::Null });
                        }
                    });
                });
                if !s("description").is_empty() {
                    ui.label(RichText::new(s("description")).small().color(t.fg_dim));
                }
                if !err.is_empty() {
                    let loc = match (p.get_path("error_file").and_then(Value::as_str), p.get_path("error_line").and_then(Value::as_i64)) {
                        (Some(f), Some(l)) if l > 0 => format!("patches/{id}/{f}:{l}"),
                        (Some(f), _) => format!("patches/{id}/{f}"),
                        _ => format!("patches/{id}/"),
                    };
                    ui.label(RichText::new(format!("{} {loc}", icon::WARN)).small().color(t.red));
                    ui.label(RichText::new(&err).monospace().small().color(t.fg));
                    if p.get_path("live").is_some_and(Value::truthy) && st == "error" {
                        ui.label(RichText::new("the last good version is still running").small().color(t.fg_dim));
                    }
                }
                if let Some(sc) = p.get_path("script") {
                    let n = |k: &str| sc.get_path(k).and_then(Value::as_f64).unwrap_or(0.0);
                    let budget = p.get_path("budget.cpu_ms").and_then(Value::as_f64).unwrap_or(2.0);
                    ui.label(
                        RichText::new(format!(
                            "cpu {:.3} ms avg · {:.3} ms peak (budget {budget} ms) · {} KB · {} handlers · {} draw ops · {} events{}",
                            n("cpu_ms_avg"),
                            n("cpu_ms_peak"),
                            n("memory_kb") as u64,
                            n("handlers") as u64,
                            n("draw_ops") as u64,
                            n("events") as u64,
                            match n("dropped_events") as u64 {
                                0 => String::new(),
                                d => format!(" ({d} dropped)"),
                            }
                        ))
                        .small()
                        .color(t.fg_dim),
                    );
                }
                let params = p.get_path("params").and_then(Value::as_list).unwrap_or(&[]);
                if !params.is_empty() {
                    let names: Vec<String> = params
                        .iter()
                        .map(|q| {
                            let n = q.get_path("name").and_then(Value::as_str).unwrap_or("");
                            match q.get_path("value") {
                                Some(v) => format!("{n}={v}"),
                                None => n.to_string(),
                            }
                        })
                        .collect();
                    ui.label(RichText::new(names.join("  ")).monospace().small().color(t.fg_dim));
                }
            });
            ui.add_space(4.0);
        }
    });
}
