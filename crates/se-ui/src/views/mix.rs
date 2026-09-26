//! Mix strip (§15.4): our sound channels (`audio.bus.<b>.*`, meters from `audio.<b>.level|peak`
//! signals) plus the StudioLive 16R channels (`mixer.16r.ch.<n>.*`, meters
//! `mixer.16r.meter.ch.<n>`). Faders follow MIDI and the mixer live (they only read state).
//! Also the shared friendly channel names and order used by the Live page and Sound → Mix.

use crate::app::App;
use egui::{RichText, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::theme::{font_medium, font_mono, font_semibold, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};

/// Bus order in the strip; unknown buses follow alphabetically.
const BUS_ORDER: &[&str] = &["mic", "band", "drums", "music", "game", "sfx", "tts", "program"];

/// Sort position of a bus (known buses first, in mixer order).
pub fn bus_rank(b: &str) -> usize {
    BUS_ORDER.iter().position(|o| *o == b).unwrap_or(usize::MAX)
}

/// Friendly channel name: `sfx` → "Sound effects", `program` → "Everything".
pub fn bus_label(b: &str) -> String {
    match b {
        "mic" => "Mic".into(),
        "band" => "Band".into(),
        "drums" => "Drums".into(),
        "music" => "Music".into(),
        "game" => "Game".into(),
        "sfx" => "Sound effects".into(),
        "tts" => "Read-out voice".into(),
        "program" => "Everything".into(),
        other => crate::views::live::nice(other),
    }
}

pub fn buses(app: &App) -> Vec<String> {
    let mut v: Vec<String> = app
        .m
        .under("audio.bus")
        .filter_map(|(a, _)| a.strip_prefix("audio.bus.")?.strip_suffix(".gain").filter(|b| !b.contains('.')).map(String::from))
        .collect();
    v.sort_by_key(|b| (bus_rank(b), b.clone()));
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

/// A level as "+1.5 dB" (never "-0.0 dB").
pub fn db_text(db: f64) -> String {
    let r = (db * 10.0).round() / 10.0;
    format!("{:+.1} dB", if r == 0.0 { 0.0 } else { r })
}

pub fn strip(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let h = (ui.available_height() - 72.0).clamp(40.0, 400.0);
    let bus_list = buses(app);
    let ch16 = app.m.get("mixer.16r.channels").and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
    let connected16 = app.m.b("mixer.16r.connected");
    egui::ScrollArea::horizontal().id_salt("mix").show(ui, |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = spacing::S;
            if bus_list.is_empty() {
                widgets::hint(ui, &t, if app.m.connected { "No sound channels yet." } else { "Waiting for the engine…" });
            }
            for b in &bus_list {
                bus_strip(app, ui, b, h);
            }
            ui.add_space(spacing::M);
            ui.separator();
            ui.add_space(spacing::M);
            if !connected16 || ch16 == 0 {
                widgets::hint(ui, &t, "Mixing desk not connected");
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
    let lowered = app.m.f(&format!("audio.bus.{b}.ducked")) < -0.5;
    let modulated = app.is_modulated(&gain_a);
    let label = bus_label(b);
    ui.vertical(|ui| {
        ui.set_width(64.0);
        ui.add(egui::Label::new(RichText::new(&label).font(font_semibold(type_scale::SMALL)).color(if muted { t.text_faint } else { t.fg })).truncate())
            .on_hover_text(if lowered { format!("{label} · lowered while you talk") } else { label.clone() });
        ui.horizontal(|ui| {
            widgets::meter(ui, &t, Vec2::new(8.0, h), if muted { 0.0 } else { level }, peak);
            let mut pos = db_to_pos(db, range);
            let r = widgets::fader(ui, &t, Vec2::new(28.0, h), &mut pos, modulated || lowered);
            if r.changed() {
                app.m.command(Op::Set { address: gain_a.clone(), value: Value::Float(pos_to_db(pos, range)) });
            }
            if r.double_clicked() {
                app.m.command(Op::Set { address: gain_a.clone(), value: Value::Float(0.0) });
            }
            r.on_hover_text(format!("{label}: {} · double-click for 0 dB", db_text(db)));
        });
        ui.horizontal(|ui| {
            let (ic, tip) = if muted { (icon::MUTE, "Unmute") } else { (icon::VOLUME, "Mute") };
            if widgets::button_ex(ui, &t, Some(ic), "", if muted { Kind::Danger } else { Kind::Ghost }, Size::Small, 28.0, true).on_hover_text(tip).clicked() {
                app.m.command(Op::Set { address: mute_a.clone(), value: Value::Bool(!muted) });
            }
            ui.label(RichText::new(format!("{:+}", db.round() as i64)).font(font_mono(type_scale::SMALL)).color(t.text_dim));
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
        ui.set_width(56.0);
        let label = if name.is_empty() { format!("Ch. {n}") } else { name.clone() };
        ui.add(egui::Label::new(RichText::new(&label).font(font_medium(type_scale::SMALL)).color(if muted { t.text_faint } else { t.fg })).truncate())
            .on_hover_text(format!("Desk channel {n}: {label}"));
        ui.horizontal(|ui| {
            widgets::meter(ui, &t, Vec2::new(8.0, h), if muted { 0.0 } else { level }, level);
            let mut pos = app.m.f(&fader_a) as f32;
            let r = widgets::fader(ui, &t, Vec2::new(24.0, h), &mut pos, modulated);
            if r.changed() {
                app.m.command(Op::Set { address: fader_a.clone(), value: Value::Float(pos as f64) });
            }
        });
        ui.horizontal(|ui| {
            let (ic, tip) = if muted { (icon::MUTE, "Unmute") } else { (icon::VOLUME, "Mute") };
            if widgets::button_ex(ui, &t, Some(ic), "", if muted { Kind::Danger } else { Kind::Ghost }, Size::Small, 28.0, true).on_hover_text(tip).clicked() {
                app.m.command(Op::Set { address: mute_a.clone(), value: Value::Bool(!muted) });
            }
            if let Some(db) = db {
                ui.label(
                    RichText::new(if db <= -80.0 { "-∞".into() } else { format!("{:+}", db.round() as i64) })
                        .font(font_mono(type_scale::SMALL))
                        .color(t.text_dim),
                );
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
