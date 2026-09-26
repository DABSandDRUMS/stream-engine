//! Right rail: events feed (with trace on click) and chat.

use crate::app::App;
use egui::RichText;
use se_proto::Value;
use se_ui_kit::widgets::icon;

fn event_icon(ty: &str) -> &'static str {
    match ty.split('.').next().unwrap_or("") {
        "twitch" => icon::CHAT,
        "tip" | "kofi" => icon::ALERT,
        "scene" => icon::SCENE,
        "preset" => icon::PRESET,
        "mode" => icon::LIVE,
        "patch" => icon::PATCH,
        "audio" | "music" | "drums" => icon::MIX,
        "lights" => icon::LIGHT,
        _ => icon::TRACE,
    }
}

pub fn events(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let mut want_trace = None;
    egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        for e in app.m.events.iter().filter(|e| !e.ty.ends_with(".trigger") || e.actor.is_some()) {
            if e.ty.starts_with("twitch.chat") {
                continue;
            }
            let who = e.actor.as_ref().map(|a| format!(" · {}", a.name)).unwrap_or_default();
            let detail = match e.ty.as_str() {
                "twitch.cheer" => format!(" {} bits", e.payload.get_path("bits").map(|v| v.to_string()).unwrap_or_default()),
                "twitch.raid" => format!(" {} viewers", e.payload.get_path("viewers").map(|v| v.to_string()).unwrap_or_default()),
                "twitch.gift" => format!(" ×{}", e.payload.get_path("count").map(|v| v.to_string()).unwrap_or_default()),
                "tip" => format!(
                    " {} {}",
                    e.payload.get_path("amount").map(|v| v.to_string()).unwrap_or_default(),
                    e.payload.get_path("currency").map(|v| v.to_string()).unwrap_or_default()
                ),
                "scene.take" => format!(" → {}", e.payload.get_path("to").map(|v| v.to_string()).unwrap_or_default()),
                "mode.changed" => format!(" → {}", e.payload.get_path("to").map(|v| v.to_string()).unwrap_or_default()),
                _ => String::new(),
            };
            let r = ui.add(
                egui::Label::new(RichText::new(format!("{} {}{who}{detail}", event_icon(&e.ty), e.ty)).color(if e.actor.is_some() { t.fg } else { t.fg_dim }))
                    .sense(egui::Sense::click()),
            );
            if r.on_hover_text("click: show what this caused").clicked() {
                want_trace = Some(e.id);
            }
        }
    });
    if let Some(id) = want_trace {
        app.m.trace_of(id);
        app.mode = crate::app::Mode::Build;
        app.dock = crate::app::DockTab::Trace;
    }
}

pub fn chat(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
        let deleted: std::collections::HashSet<String> = app
            .m
            .events
            .iter()
            .filter(|e| e.ty == "twitch.chat.delete")
            .filter_map(|e| e.payload.get_path("message_id").and_then(Value::as_str).map(String::from))
            .collect();
        for e in app.m.events.iter().filter(|e| e.ty == "twitch.chat") {
            let id = e.payload.get_path("message_id").and_then(Value::as_str).unwrap_or("");
            if deleted.contains(id) {
                continue;
            }
            let name = e.actor.as_ref().map(|a| a.name.as_str()).unwrap_or("?");
            let msg = e.payload.get_path("message").map(|v| v.to_string()).unwrap_or_default();
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(name).strong().color(t.accent));
                ui.label(msg);
            });
        }
    });
}
