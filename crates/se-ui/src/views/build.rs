//! Build mode (§15.5): library, inspector with provenance, and the dock (rules, scopes, trace,
//! simulator, console).

use crate::app::{App, DockTab};
use egui::RichText;
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    egui::Panel::left("library").default_size(260.0).show(ui, |ui| library(app, ui));
    egui::Panel::right("inspector").default_size(320.0).show(ui, |ui| inspector(app, ui));
    egui::Panel::bottom("dock").resizable(true).default_size(300.0).show(ui, |ui| dock(app, ui));
    egui::CentralPanel::default().show(ui, |ui| {
        crate::views::program::preview_program_row(app, ui);
    });
}

fn library(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.add(egui::TextEdit::singleline(&mut app.library_filter).hint_text(format!("{} search", icon::SEARCH)));
    let f = app.library_filter.to_lowercase();
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::CollapsingHeader::new(format!("{} Scenes", icon::SCENE)).default_open(true).show(ui, |ui| {
            for s in app.m.q_list("scenes").to_vec() {
                let n = s.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
                if !f.is_empty() && !n.contains(&f) {
                    continue;
                }
                egui::CollapsingHeader::new(&n).id_salt(("scene", &n)).show(ui, |ui| {
                    for node in s.get_path("nodes").and_then(Value::as_list).unwrap_or(&[]) {
                        let id = node.as_str().unwrap_or("");
                        if ui.selectable_label(false, format!("{} {id}", icon::DEVICE)).clicked() {
                            app.selected = Some(format!("scene.{n}.node.{id}"));
                        }
                    }
                });
            }
        });
        egui::CollapsingHeader::new(format!("{} Presets", icon::PRESET)).default_open(true).show(ui, |ui| {
            for p in app.m.q_list("presets").to_vec() {
                let n = p.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
                if !f.is_empty() && !n.contains(&f) {
                    continue;
                }
                ui.horizontal(|ui| {
                    widgets::led(ui, &t, if p.get_path("active").is_some_and(Value::truthy) { LedState::Active } else { LedState::Idle });
                    if ui.selectable_label(app.selected.as_deref() == Some(&format!("preset.{n}")), &n).clicked() {
                        app.selected = Some(format!("preset.{n}"));
                    }
                });
            }
        });
        egui::CollapsingHeader::new(format!("{} Rules", icon::RULE)).show(ui, |ui| {
            for r in app.m.q_list("rules").to_vec() {
                let n = r.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
                if !f.is_empty() && !n.to_lowercase().contains(&f) {
                    continue;
                }
                ui.horizontal(|ui| {
                    widgets::led(ui, &t, if r.get_path("enabled").is_some_and(Value::truthy) { LedState::Healthy } else { LedState::Idle });
                    ui.label(&n);
                });
            }
        });
        egui::CollapsingHeader::new(format!("{} Bindings", icon::BINDING)).show(ui, |ui| {
            for b in app.m.q_list("bindings").to_vec() {
                let n = b.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
                ui.horizontal(|ui| {
                    widgets::led(ui, &t, if b.get_path("active").is_some_and(Value::truthy) { LedState::Modulated } else { LedState::Idle });
                    ui.label(format!(
                        "{n}: {} ← {}",
                        b.get_path("target").map(|v| v.to_string()).unwrap_or_default(),
                        b.get_path("signal").map(|v| v.to_string()).unwrap_or_default()
                    ));
                });
            }
        });
    });
}

fn inspector(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::section(ui, &t, icon::SEARCH, "Inspector");
    let Some(sel) = app.selected.clone() else {
        ui.label(RichText::new("select something in the library").color(t.fg_dim));
        return;
    };
    ui.label(RichText::new(&sel).strong());
    ui.separator();
    let rows: Vec<(String, Value)> =
        app.m.state.iter().filter(|(a, _)| a.starts_with(&format!("{sel}.")) || **a == sel).map(|(a, v)| (a.clone(), v.clone())).collect();
    if sel.starts_with("preset.") {
        let name = sel.trim_start_matches("preset.").to_string();
        ui.horizontal(|ui| {
            if ui.button(format!("{} fire", icon::PLAY)).clicked() {
                app.m.command(Op::PresetFire { name: name.clone(), payload: Value::Null });
            }
            if ui.button(format!("{} release", icon::STOP)).clicked() {
                app.m.command(Op::PresetRelease { name: name.clone() });
            }
        });
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("insp").striped(true).num_columns(3).show(ui, |ui| {
            for (a, v) in rows {
                let short = a.strip_prefix(&format!("{sel}.")).unwrap_or(&a).to_string();
                ui.label(&short);
                match &v {
                    Value::Float(x) => {
                        let mut f = *x;
                        if ui.add(egui::DragValue::new(&mut f).speed(0.01)).changed() {
                            app.m.command(Op::SetBase { address: a.clone(), value: Value::Float(f) });
                        }
                    }
                    Value::Bool(b) => {
                        let mut bb = *b;
                        if ui.checkbox(&mut bb, "").changed() {
                            app.m.command(Op::SetBase { address: a.clone(), value: Value::Bool(bb) });
                        }
                    }
                    other => {
                        ui.label(other.to_string());
                    }
                }
                if ui.small_button("why?").on_hover_text("provenance").clicked() {
                    app.m.explain_of(&a);
                }
                ui.end_row();
            }
        });
        if let Some(p) = app.m.explain.clone() {
            ui.separator();
            widgets::section(ui, &t, icon::TRACE, "Provenance");
            ui.label(RichText::new(format!("{} = {}", p.address, p.value)).strong());
            for l in &p.layers {
                let c = match l.kind.as_str() {
                    "binding" => t.modulated(),
                    "override" | "animation" if l.active => t.tally_preview(),
                    "clamp" => t.bright_red,
                    _ => t.fg_dim,
                };
                let prio = l.priority.map(|p| format!(" ({p})")).unwrap_or_default();
                ui.label(RichText::new(format!("{} {} {}{prio}: {}", if l.active { "▸" } else { " " }, l.kind, l.source, l.value)).color(c));
            }
        }
    });
}

fn dock(app: &mut App, ui: &mut egui::Ui) {
    ui.horizontal(|ui| {
        for (tab, label) in [
            (DockTab::Rules, "Rules"),
            (DockTab::Scopes, "Signal scopes"),
            (DockTab::Trace, "Trace"),
            (DockTab::Simulator, "Simulator"),
            (DockTab::Console, "Console"),
        ] {
            if ui.selectable_label(app.dock == tab, label).clicked() {
                app.dock = tab;
            }
        }
    });
    ui.separator();
    match app.dock {
        DockTab::Rules => rules(app, ui),
        DockTab::Scopes => scopes(app, ui),
        DockTab::Trace => trace(app, ui),
        DockTab::Simulator => simulator(app, ui),
        DockTab::Console => console(app, ui),
    }
}

fn rules(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("rules").striped(true).num_columns(5).show(ui, |ui| {
            for r in app.m.q_list("rules").to_vec() {
                let name = r.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
                let enabled = r.get_path("enabled").is_some_and(Value::truthy);
                let mut en = enabled;
                if ui.checkbox(&mut en, &name).changed() {
                    app.m.text(&format!("{} '{name}'", if en { "rule.enable" } else { "rule.disable" }));
                }
                ui.label(RichText::new(format!("when {}", r.get_path("when").map(|v| v.to_string()).unwrap_or_default())).color(t.cyan));
                ui.label(
                    RichText::new(format!("if {}", r.get_path("if").filter(|v| !v.is_null()).map(|v| v.to_string()).unwrap_or_else(|| "–".into())))
                        .color(t.yellow),
                );
                ui.label(RichText::new(format!("do {}", r.get_path("do").map(|v| v.to_string()).unwrap_or_default())).color(t.green));
                if ui.small_button(format!("{} test", icon::SIM)).clicked() {
                    let when = r.get_path("when").and_then(Value::as_str).unwrap_or("").replace('*', "test");
                    app.m.text(&format!("emit {when} {}", app.sim_args));
                }
                ui.end_row();
            }
        });
    });
}

fn scopes(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        ui.label("signals:");
        ui.text_edit_singleline(&mut app.scope_pattern);
    });
    let pat = app.scope_pattern.clone();
    let mut names: Vec<&String> = app.m.signals.keys().filter(|n| se_proto::address::matches(&pat, n)).collect();
    names.sort();
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            for n in names.into_iter().take(24) {
                let v: Vec<f32> = app.m.signals[n].iter().copied().collect();
                ui.vertical(|ui| {
                    ui.label(RichText::new(format!("{n} {:.3}", v.last().copied().unwrap_or(0.0))).small());
                    widgets::scope(ui, &t, egui::vec2(220.0, 60.0), &v, None, None);
                });
            }
        });
    });
}

fn trace(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.columns(2, |cols| {
        cols[0].label(RichText::new("recent (click to follow)").small().color(t.fg_dim));
        let mut follow = None;
        egui::ScrollArea::vertical().id_salt("trace-recent").stick_to_bottom(true).show(&mut cols[0], |ui| {
            for r in app.m.trace.iter().filter(|r| r.parent.is_none()).rev().take(300).collect::<Vec<_>>().into_iter().rev() {
                if ui.selectable_label(false, format!("{} {}", r.kind, r.label)).clicked() {
                    follow = Some(r.id);
                }
            }
        });
        if let Some(id) = follow {
            app.m.trace_of(id);
        }
        cols[1].label(RichText::new("cause chain").small().color(t.fg_dim));
        egui::ScrollArea::vertical().id_salt("trace-view").show(&mut cols[1], |ui| {
            let view = app.m.trace_view.clone();
            let mut depth: std::collections::HashMap<u64, usize> = Default::default();
            for r in &view {
                let d = r.parent.and_then(|p| depth.get(&p).copied()).map(|d| d + 1).unwrap_or(0);
                depth.insert(r.id, d);
                let c = match r.kind.as_str() {
                    "event" => t.cyan,
                    "rule" => t.yellow,
                    "command" => t.blue,
                    "preset" => t.magenta,
                    "change" => t.green,
                    "error" => t.bright_red,
                    _ => t.fg,
                };
                ui.label(RichText::new(format!("{}{} {}", "  ".repeat(d), r.kind, r.label)).color(c).monospace());
            }
        });
    });
}

fn simulator(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        ui.label("args:");
        ui.add(egui::TextEdit::singleline(&mut app.sim_args).hint_text("bits=1000 message='hi' user=drumfan").desired_width(400.0));
    });
    ui.horizontal_wrapped(|ui| {
        for p in app.m.q_list("sim.presets").to_vec() {
            let n = p.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
            let hint = p.get_path("args").map(|v| v.to_string()).unwrap_or_default();
            if ui.button(format!("{} {n}", icon::SIM)).on_hover_text(hint).clicked() {
                app.m.text(&format!("sim.{n} {}", app.sim_args));
            }
        }
    });
    ui.horizontal_wrapped(|ui| {
        for (label, cmd) in [
            ("gift bomb 50", "sim.gift_bomb count=50"),
            ("raid 300", "sim.raid viewers=300"),
            ("1000 bits with message", "sim.cheer bits=1000 message='This stream rocks!'"),
        ] {
            if ui.button(RichText::new(label).color(t.accent)).clicked() {
                app.m.text(cmd);
            }
        }
    });
}

fn console(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        ui.label(format!("{} filter", icon::CONSOLE));
        ui.text_edit_singleline(&mut app.console_filter);
    });
    for e in app.m.q_list("errors").to_vec() {
        ui.label(
            RichText::new(format!(
                "{} {}: {}",
                icon::WARN,
                e.get_path("file").map(|v| v.to_string()).unwrap_or_default(),
                e.get_path("msg").map(|v| v.to_string()).unwrap_or_default()
            ))
            .color(t.bright_red),
        );
    }
    let f = app.console_filter.to_lowercase();
    egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        for l in app.m.logs.iter().filter(|l| f.is_empty() || l.msg.to_lowercase().contains(&f) || l.target.contains(&f)) {
            let c = match l.level.as_str() {
                "error" => t.bright_red,
                "warn" => t.yellow,
                _ => t.fg_dim,
            };
            ui.label(RichText::new(format!("[{}] {}: {}", l.level, l.target, l.msg)).color(c).monospace());
        }
    });
}
