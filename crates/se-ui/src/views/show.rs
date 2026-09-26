//! Show mode (§15.4): scenes → preview/program with Take, preset pads, active list, right rail.

use crate::app::{App, RailTab};
use egui::{RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    right_rail(app, ui);
    egui::CentralPanel::default().show(ui, |ui| {
        crate::views::program::preview_program_row(app, ui);
        ui.add_space(6.0);
        scenes(app, ui);
        ui.add_space(6.0);
        ui.columns(2, |cols| {
            presets(app, &mut cols[0]);
            active(app, &mut cols[1]);
        });
    });
}

fn scenes(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::section(ui, &t, icon::SCENE, "Scenes");
    let program = app.m.str("show.scene.program").to_string();
    let preview = app.m.str("show.scene.preview").to_string();
    let scenes: Vec<Value> = app.m.q_list("scenes").to_vec();
    ui.horizontal_wrapped(|ui| {
        for (i, s) in scenes.iter().enumerate() {
            let name = s.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
            let label = s.get_path("label").and_then(Value::as_str).unwrap_or(&name).to_string();
            let key = s.get_path("key").and_then(Value::as_i64).unwrap_or(i as i64 + 1);
            let st = if name == program {
                LedState::Active
            } else if name == preview {
                LedState::Armed
            } else {
                LedState::Idle
            };
            let r = widgets::pad(ui, &t, Vec2::new(110.0, 64.0), icon::SCENE, &label, None, st, None, Some(&key.to_string()));
            if r.double_clicked() {
                app.m.command(Op::SceneCut { scene: name.clone(), transition: app.transition_choice.clone() });
            } else if r.clicked() {
                app.m.command(Op::SceneGo { scene: name.clone() });
            }
        }
    });
    ui.horizontal(|ui| {
        ui.label(RichText::new("TRANSITION").small().color(t.fg_dim));
        let cur = app.transition_choice.clone().unwrap_or_else(|| "scene pool (random)".into());
        egui::ComboBox::from_id_salt("transition").selected_text(cur).show_ui(ui, |ui| {
            if ui.selectable_label(app.transition_choice.is_none(), "scene pool (random)").clicked() {
                app.transition_choice = None;
            }
            for n in app.m.q_list("transitions").iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>() {
                if ui.selectable_label(app.transition_choice.as_deref() == Some(&n), &n).clicked() {
                    app.transition_choice = Some(n);
                }
            }
        });
        let active = app.m.b("show.transition.active");
        if active {
            ui.add(egui::ProgressBar::new(app.m.f("show.transition.progress") as f32).desired_width(120.0).text(app.m.str("show.transition.name").to_string()));
        } else {
            ui.label(RichText::new(format!("last: {} {}ms", app.m.str("show.transition.name"), app.m.f("show.transition.ms") as i64)).small().color(t.fg_dim));
        }
        let mut direct = app.m.b("show.direct");
        if ui.checkbox(&mut direct, "direct").on_hover_text("Scene clicks go straight to program").changed() {
            app.m.command(Op::Set { address: "show.direct".into(), value: Value::Bool(direct) });
        }
        if ui
            .add(egui::Button::new(RichText::new("TAKE ⏎").strong().color(t.fg_bright)).fill(t.red.gamma_multiply(0.7)).min_size(Vec2::new(110.0, 30.0)))
            .clicked()
        {
            app.m.command(Op::SceneTake { transition: app.transition_choice.clone(), ms: None });
        }
    });
}

fn presets(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::section(ui, &t, icon::PRESET, "Presets");
    let presets: Vec<Value> = app.m.q_list("presets").to_vec();
    ui.horizontal_wrapped(|ui| {
        for (i, p) in presets.iter().enumerate() {
            let name = p.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
            let label = p.get_path("label").and_then(Value::as_str).unwrap_or(&name).to_string();
            let active = p.get_path("active").is_some_and(Value::truthy);
            let color = p.get_path("color").and_then(Value::as_str).and_then(se_ui_kit::theme::hex);
            let rem = p.get_path("remaining_ms").and_then(Value::as_f64);
            let confirm = p.get_path("confirm").is_some_and(Value::truthy);
            let chat = p.get_path("chat").is_none_or(Value::truthy);
            let hint = if i < 12 { format!("F{}", i + 1) } else { String::new() };
            let label = if confirm || !chat { format!("{label}*") } else { label };
            let r = widgets::pad(
                ui,
                &t,
                Vec2::new(96.0, 64.0),
                icon::PRESET,
                &label,
                color,
                if active { LedState::Active } else { LedState::Idle },
                rem.map(|r| (r / 8000.0).min(1.0) as f32),
                Some(&hint),
            );
            if r.clicked() {
                app.m.command(Op::PresetFire { name: name.clone(), payload: Value::Null });
            }
            if r.secondary_clicked() && active {
                app.m.command(Op::PresetRelease { name: name.clone() });
            }
        }
    });
}

fn active(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::section(ui, &t, icon::EFFECT, "Active");
    let items: Vec<Value> = app.m.q_list("active").to_vec();
    if items.is_empty() {
        ui.label(RichText::new("nothing running").color(t.fg_dim));
    }
    egui::Grid::new("active").striped(true).show(ui, |ui| {
        for it in items {
            let kind = it.get_path("kind").and_then(Value::as_str).unwrap_or("");
            let name = it.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
            let chat = it.get_path("chat").is_some_and(Value::truthy);
            ui.label(format!("{} {name}{}", if kind == "preset" { icon::PRESET } else { icon::EFFECT }, if chat { " (chat)" } else { "" }));
            if let Some(l) = it.get_path("level").and_then(Value::as_f64) {
                ui.add(egui::ProgressBar::new(l as f32).desired_width(80.0));
            } else {
                ui.label("");
            }
            match it.get_path("remaining_ms").and_then(Value::as_f64) {
                Some(ms) => ui.label(format!("{:.1}s", ms / 1000.0)),
                None => ui.label("latched"),
            };
            if ui.button(icon::CROSS).on_hover_text("kill").clicked() {
                if kind == "preset" {
                    app.m.command(Op::PresetRelease { name });
                } else {
                    app.m.command(Op::Release { address: name });
                }
            }
            ui.end_row();
        }
    });
}

fn right_rail(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    egui::Panel::right("rail").default_size(340.0).show(ui, |ui| {
        ui.horizontal(|ui| {
            for (tab, label) in [(RailTab::Events, "EVENTS"), (RailTab::Chat, "CHAT")] {
                if ui.selectable_label(app.rail == tab, RichText::new(label).strong()).clicked() {
                    app.rail = tab;
                }
            }
        });
        ui.separator();
        match app.rail {
            RailTab::Events => crate::views::rail::events(app, ui),
            RailTab::Chat => crate::views::rail::chat(app, ui),
        }
        let _ = &t;
    });
}
