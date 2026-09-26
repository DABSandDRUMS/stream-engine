//! Mix strip (§15.4): our buses (`audio.bus.<b>.*`, meters from `audio.<b>.level|peak`
//! signals) plus the StudioLive 16R channels (`mixer.16r.ch.<n>.*`, meters
//! `mixer.16r.meter.ch.<n>`). Faders follow MIDI and the mixer live (they only read state).

use crate::app::App;
use egui::{RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, icon};

/// Bus order in the strip; unknown buses follow alphabetically.
const BUS_ORDER: &[&str] = &["mic", "band", "drums", "music", "game", "sfx", "tts", "program"];

pub fn buses(app: &App) -> Vec<String> {
    let mut v: Vec<String> = app
        .m
        .under("audio.bus")
        .filter_map(|(a, _)| a.strip_prefix("audio.bus.")?.strip_suffix(".gain").filter(|b| !b.contains('.')).map(String::from))
        .collect();
    v.sort_by_key(|b| (BUS_ORDER.iter().position(|o| o == b).unwrap_or(usize::MAX), b.clone()));
    v
}

/// Fader position (0–1) ↔ gain in dB over the address range (default −60…+12 dB).
pub fn db_to_pos(db: f64, [lo, hi]: [f64; 2]) -> f32 {
    ((db - lo) / (hi - lo).max(1e-6)).clamp(0.0, 1.0) as f32
}
pub fn pos_to_db(p: f32, [lo, hi]: [f64; 2]) -> f64 {
    let db = lo + p.clamp(0.0, 1.0) as f64 * (hi - lo);
    (db * 10.0).round() / 10.0
}

pub fn strip(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let h = (ui.available_height() - 58.0).clamp(40.0, 400.0);
    let bus_list = buses(app);
    let ch16 = app.m.get("mixer.16r.channels").and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
    let connected16 = app.m.b("mixer.16r.connected");
    egui::ScrollArea::horizontal().id_salt("mix").show(ui, |ui| {
        ui.horizontal(|ui| {
            widgets::section(ui, &t, icon::MIX, "Mix");
            if bus_list.is_empty() {
                ui.label(RichText::new("no audio buses (audio engine not running)").color(t.fg_dim));
            }
            for b in &bus_list {
                bus_strip(app, ui, b, h);
            }
            ui.separator();
            widgets::section(ui, &t, icon::MIX, "16R");
            if !connected16 || ch16 == 0 {
                ui.label(RichText::new(if app.m.has("mixer.16r.connected") { "16R disconnected" } else { "16R not connected" }).color(t.fg_dim));
            } else {
                for n in 1..=ch16 {
                    ch_strip(app, ui, n, h);
                }
            }
        });
    });
}

fn bus_strip(app: &mut App, ui: &mut egui::Ui, b: &str, h: f32) {
    let t = app.t.clone();
    let gain_a = format!("audio.bus.{b}.gain");
    let range = app.m.meta.get(&gain_a).and_then(|m| m.range).unwrap_or([-60.0, 12.0]);
    let db = app.m.f(&gain_a);
    let mute_a = format!("audio.bus.{b}.mute");
    let muted = app.m.b(&mute_a);
    let level = app.m.sig(&format!("audio.{b}.level")).unwrap_or(0.0);
    let peak = app.m.sig(&format!("audio.{b}.peak")).unwrap_or(level);
    let ducked = app.m.f(&format!("audio.bus.{b}.ducked"));
    let modulated = app.is_modulated(&gain_a);
    ui.vertical(|ui| {
        ui.set_width(54.0);
        let name = if ducked < -0.5 { format!("{b} (duck)") } else { b.to_string() };
        ui.label(RichText::new(name).small().strong().color(if muted { t.fg_dim } else { t.fg }));
        ui.horizontal(|ui| {
            widgets::meter(ui, &t, Vec2::new(10.0, h), level, peak);
            let mut pos = db_to_pos(db, range);
            let r = widgets::fader(ui, &t, Vec2::new(22.0, h), &mut pos, modulated);
            if r.changed() {
                app.m.command(Op::Set { address: gain_a.clone(), value: Value::Float(pos_to_db(pos, range)) });
            }
            if r.double_clicked() {
                app.m.command(Op::Set { address: gain_a.clone(), value: Value::Float(0.0) });
            }
            r.on_hover_text(format!("{b}: {db:+.1} dB · double-click = 0 dB · right-click: inspect"));
        });
        ui.horizontal(|ui| {
            let m = ui.selectable_label(muted, RichText::new("M").small().color(if muted { t.bright_red } else { t.fg_dim }));
            if m.clicked() {
                app.m.command(Op::Set { address: mute_a.clone(), value: Value::Bool(!muted) });
            }
            ui.label(RichText::new(format!("{db:+.0}")).small().monospace().color(t.fg_dim));
        });
    });
    if !app.m.meta.contains_key(&gain_a) && app.m.connected {
        app.fetch_meta_once(&gain_a);
    }
}

fn ch_strip(app: &mut App, ui: &mut egui::Ui, n: usize, h: f32) {
    let t = app.t.clone();
    let p = format!("mixer.16r.ch.{n}");
    let name = app.m.str(&format!("{p}.name")).to_string();
    let fader_a = format!("{p}.fader");
    let mute_a = format!("{p}.mute");
    let muted = app.m.b(&mute_a);
    let level = app.m.sig(&format!("mixer.16r.meter.ch.{n}")).unwrap_or(0.0);
    let db = app.m.get(&format!("{p}.db")).and_then(Value::as_f64);
    let modulated = app.is_modulated(&fader_a);
    ui.vertical(|ui| {
        ui.set_width(44.0);
        let label = if name.is_empty() { format!("{n}") } else { name.chars().take(6).collect() };
        ui.label(RichText::new(label).small().strong().color(if muted { t.fg_dim } else { t.fg })).on_hover_text(format!("16R ch {n} {name}"));
        ui.horizontal(|ui| {
            widgets::meter(ui, &t, Vec2::new(8.0, h), level, level);
            let mut pos = app.m.f(&fader_a) as f32;
            let r = widgets::fader(ui, &t, Vec2::new(18.0, h), &mut pos, modulated);
            if r.changed() {
                app.m.command(Op::Set { address: fader_a.clone(), value: Value::Float(pos as f64) });
            }
        });
        ui.horizontal(|ui| {
            if ui.selectable_label(muted, RichText::new("M").small().color(if muted { t.bright_red } else { t.fg_dim })).clicked() {
                app.m.command(Op::Set { address: mute_a.clone(), value: Value::Bool(!muted) });
            }
            if let Some(db) = db {
                ui.label(RichText::new(if db <= -80.0 { "-∞".into() } else { format!("{db:+.0}") }).small().monospace().color(t.fg_dim));
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fader_taper_round_trips_over_the_meta_range() {
        let r = [-60.0, 12.0];
        assert_eq!(db_to_pos(-60.0, r), 0.0);
        assert_eq!(db_to_pos(12.0, r), 1.0);
        assert_eq!(db_to_pos(40.0, r), 1.0);
        for db in [-60.0, -12.5, 0.0, 6.0, 12.0] {
            assert!((pos_to_db(db_to_pos(db, r), r) - db).abs() < 0.11, "{db}");
        }
    }
}
