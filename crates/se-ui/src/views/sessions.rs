//! Session review (§15.6, §18): past sessions and markers. Clip review is added by the clips subsystem.

use crate::app::App;
use egui::RichText;
use se_proto::Value;
use se_ui_kit::widgets::{self, icon};

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::section(ui, &t, icon::SESSION, "Sessions");
    if app.m.q("sessions").is_none() || app.m.last_refresh.elapsed().as_secs() > 5 {
        app.m.query("sessions", Value::Null);
    }
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("sessions").striped(true).num_columns(4).show(ui, |ui| {
            ui.label(RichText::new("id").strong());
            ui.label(RichText::new("started").strong());
            ui.label(RichText::new("duration").strong());
            ui.label(RichText::new("").strong());
            ui.end_row();
            for s in app.m.q_list("sessions").to_vec() {
                let id = s.get_path("id").and_then(Value::as_str).unwrap_or("").to_string();
                let start = s.get_path("started_at").and_then(Value::as_i64).unwrap_or(0);
                let end = s.get_path("ended_at").and_then(Value::as_i64);
                ui.label(RichText::new(&id).monospace());
                ui.label(format!("{start}"));
                ui.label(match end {
                    Some(e) => format!("{}m", (e - start) / 60),
                    None => "open".into(),
                });
                ui.label(if id == app.m.session { RichText::new("current").color(t.green) } else { RichText::new("") });
                ui.end_row();
            }
        });
    });
}
