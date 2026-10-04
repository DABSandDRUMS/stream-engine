//! Source-level mixer controls shared by Overview and the mixer popout.
//! Names come from the operator's input names, devices, and the engine — never invented here.

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

/// Bus names as the engine reports them (derived from the inputs routed into each bus).
static BUS_LABELS: std::sync::RwLock<std::collections::BTreeMap<String, String>> = std::sync::RwLock::new(std::collections::BTreeMap::new());

/// Keep [`bus_label`] in step with the engine's `audio.mix` / `project.audio.inputs` replies.
pub fn sync_bus_labels(app: &App) {
    let reported = || {
        ["project.audio.inputs", "audio.mix"]
            .into_iter()
            .filter_map(|query| app.m.q(query)?.get_path("buses")?.as_list())
            .flatten()
            .filter_map(|bus| Some((bus.get_path("name")?.as_str()?, bus.get_path("label")?.as_str()?)))
    };
    let stale = {
        let known = BUS_LABELS.read().unwrap_or_else(|e| e.into_inner());
        reported().any(|(name, label)| known.get(name).map(String::as_str) != Some(label))
    };
    if stale {
        let mut known = BUS_LABELS.write().unwrap_or_else(|e| e.into_inner());
        known.extend(reported().map(|(name, label)| (name.to_string(), label.to_string())));
    }
}

#[cfg(test)]
pub fn remember_bus_label(name: &str, label: &str) {
    BUS_LABELS.write().unwrap_or_else(|e| e.into_inner()).insert(name.into(), label.into());
}

/// A routing bus by the names of the inputs routed into it (as the engine reports), else what
/// the app plays into it, else its identifier unchanged.
pub fn bus_label(b: &str) -> String {
    if let Some(label) = BUS_LABELS.read().unwrap_or_else(|e| e.into_inner()).get(b) {
        return label.clone();
    }
    match b {
        "music" => "Music".into(),
        "sfx" => "Sound effects".into(),
        "tts" => "Read-out voice".into(),
        "program" => "Everything".into(),
        other => other.into(),
    }
}

/// One mixer control: a capture input, or one group of app audio (all its producers together).
pub struct SourceChannel {
    pub label: String,
    /// Gain/mute address prefix (`audio.input.<id>` or `audio.bus.<bus>`).
    pub address: String,
    /// Level/peak signal prefix (`audio.input.<id>` or `audio.<bus>`).
    pub meter: String,
}

/// A capture input by the name the operator gave it, else its device.
pub fn input_label(input: &Value, devices: &Value) -> String {
    if let Some(label) = input.get_path("label").and_then(Value::as_str).filter(|label| !label.trim().is_empty()) {
        return label.to_string();
    }
    let target = input.get_path("target").and_then(Value::as_str).unwrap_or("");
    let description = devices
        .as_list()
        .unwrap_or(&[])
        .iter()
        .find(|device| device.get_path("name").and_then(Value::as_str) == Some(target))
        .and_then(|device| device.get_path("description"))
        .and_then(Value::as_str)
        .filter(|description| !description.is_empty())
        .unwrap_or(target);
    if description.is_empty() { "Capture source needs a device".into() } else { description.into() }
}

pub fn refresh_sources(app: &mut App, ui: &egui::Ui) {
    let id = egui::Id::new("mixer_source_refresh");
    let last = ui.data(|data| data.get_temp::<(u64, std::time::Instant)>(id));
    if app.m.connected && last.is_none_or(|(generation, at)| generation != app.m.conn_gen || at.elapsed() >= std::time::Duration::from_secs(1)) {
        for query in ["audio.mix", "project.audio.inputs", "audio.devices"] {
            app.m.query(query, Value::Null);
        }
        ui.data_mut(|data| data.insert_temp(id, (app.m.conn_gen, std::time::Instant::now())));
    }
}

pub fn source_channels(app: &App) -> Vec<SourceChannel> {
    let empty = Value::Null;
    let mix = app.m.q("audio.mix").unwrap_or(&empty);
    let configured = app.m.q("project.audio.inputs").unwrap_or(&empty);
    let devices = app.m.q("audio.devices").unwrap_or(&empty);
    collect_sources(mix, configured, devices)
}

fn rows<'a>(value: &'a Value, key: &str) -> &'a [Value] {
    value.get_path(key).and_then(Value::as_list).unwrap_or(&[])
}

fn collect_sources(mix: &Value, configured: &Value, devices: &Value) -> Vec<SourceChannel> {
    let saved = rows(configured, "inputs");
    let mut channels = Vec::new();
    for input in rows(mix, "inputs") {
        if let Some(saved) = saved.iter().find(|saved| saved.get_path("name") == input.get_path("name")) {
            let address = input.get_path("address").and_then(Value::as_str).unwrap_or("").to_string();
            channels.push(SourceChannel { label: input_label(saved, devices), meter: address.clone(), address });
        }
    }
    channels.extend(app_audio_groups(mix).into_iter().map(|(_, group)| group));
    channels.retain(|source| !source.address.is_empty());
    channels
}

/// App audio as mixer controls: one per group it plays into (all scene layers together), never
/// one per layer or producer. Returns (bus, control); producers stay adjustable under Advanced.
pub fn app_audio_groups(mix: &Value) -> Vec<(String, SourceChannel)> {
    let mut groups: Vec<&str> = Vec::new();
    for bus in rows(mix, "slots").iter().filter_map(|slot| slot.get_path("bus")?.as_str()).filter(|bus| !bus.is_empty() && *bus != "program") {
        if !groups.contains(&bus) {
            groups.push(bus);
        }
    }
    groups.sort_by_key(|bus| (bus_rank(bus), *bus));
    groups
        .into_iter()
        .filter_map(|name| {
            let bus = rows(mix, "buses").iter().find(|bus| bus.get_path("name").and_then(Value::as_str) == Some(name))?;
            let control = SourceChannel {
                label: bus.get_path("label").and_then(Value::as_str).map_or_else(|| bus_label(name), str::to_string),
                address: bus.get_path("address").and_then(Value::as_str)?.to_string(),
                meter: format!("audio.{name}"),
            };
            Some((name.to_string(), control))
        })
        .collect()
}

/// Running app-audio producers: `tts` → "Read-out voice", `patch.win31_chatting` → "Win31 chatting".
pub fn app_audio_label(name: &str) -> String {
    match name {
        "tts" => "Read-out voice".into(),
        "youtube" => "YouTube songs".into(),
        n if n.starts_with("timecode") => "Show sync signal".into(),
        n => crate::views::live::nice(&n.strip_prefix("patch.").unwrap_or(n).replace('.', " ")),
    }
}

/// A saved capture input by its device, falling back to its ID only when it is not saved.
pub fn saved_input_label(configured: &Value, devices: &Value, name: &str) -> String {
    configured
        .get_path("inputs")
        .and_then(Value::as_list)
        .and_then(|inputs| inputs.iter().find(|input| input.get_path("name").and_then(Value::as_str) == Some(name)))
        .map(|input| input_label(input, devices))
        .unwrap_or_else(|| name.to_string())
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
    refresh_sources(app, ui);
    let sources = source_channels(app);
    let ch16 = app.m.get("mixer.16r.channels").and_then(Value::as_i64).unwrap_or(0).max(0) as usize;
    let connected16 = app.m.b("mixer.16r.connected");
    egui::ScrollArea::horizontal().id_salt("mix").show(ui, |ui| {
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = spacing::S;
            if sources.is_empty() {
                widgets::hint(ui, &t, if app.m.connected { "No audio sources. Add an input in Sound." } else { "Waiting for the engine…" });
            }
            for source in &sources {
                source_strip(app, ui, source, h);
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

fn source_strip(app: &mut App, ui: &mut egui::Ui, source: &SourceChannel, h: f32) {
    let t = app.t.clone();
    let gain_a = format!("{}.gain", source.address);
    let range = app.m.meta.get(&gain_a).and_then(|m| m.range).unwrap_or([-60.0, 12.0]);
    let db = app.m.f(&gain_a);
    let mute_a = format!("{}.mute", source.address);
    let muted = app.m.b(&mute_a);
    let level = app.m.sig(&format!("{}.level", source.meter)).unwrap_or(0.0);
    let peak = app.m.sig(&format!("{}.peak", source.meter)).unwrap_or(level);
    let modulated = app.is_modulated(&gain_a);
    let label = &source.label;
    ui.vertical(|ui| {
        ui.set_width(64.0);
        ui.add(egui::Label::new(RichText::new(label).font(font_semibold(type_scale::SMALL)).color(if muted { t.text_faint } else { t.fg })).truncate())
            .on_hover_text(label);
        ui.horizontal(|ui| {
            widgets::meter(ui, &t, Vec2::new(8.0, h), if muted { 0.0 } else { level }, peak);
            let mut pos = db_to_pos(db, range);
            let r = widgets::fader(ui, &t, Vec2::new(28.0, h), &mut pos, modulated);
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
    fn routing_buses_do_not_create_source_controls() {
        let mix = Value::map().with(
            "buses",
            vec![Value::map().with("name", "band").with("address", "audio.bus.band"), Value::map().with("name", "game").with("address", "audio.bus.game")],
        );
        let configured = Value::map()
            .with("buses", vec![Value::map().with("name", "band").with("configured", true), Value::map().with("name", "game").with("configured", true)]);
        assert!(collect_sources(&mix, &configured, &Value::Null).is_empty());
    }

    #[test]
    fn source_controls_use_device_identity_and_input_addresses_not_legacy_bus_names() {
        let mix = Value::map()
            .with(
                "inputs",
                vec![
                    Value::map().with("name", "band").with("address", "audio.input.band"),
                    Value::map().with("name", "ghost").with("address", "audio.input.ghost"),
                ],
            )
            .with("buses", vec![Value::map().with("name", "game")]);
        let configured = Value::map().with("inputs", vec![Value::map().with("name", "band").with("target", "alsa_input.usb.interface").with("bus", "band")]);
        let devices = Value::List(vec![Value::map().with("name", "alsa_input.usb.interface").with("description", "USB Audio Interface")]);
        let channels = collect_sources(&mix, &configured, &devices);
        assert_eq!(
            channels.iter().map(|source| (source.label.as_str(), source.address.as_str())).collect::<Vec<_>>(),
            vec![("USB Audio Interface", "audio.input.band")]
        );
        let disconnected = collect_sources(&mix, &configured, &Value::Null);
        assert_eq!(disconnected[0].label, "alsa_input.usb.interface");
    }

    #[test]
    fn scene_layers_share_one_mixer_control_instead_of_one_per_layer() {
        let slot = |name: &str, bus: &str| Value::map().with("name", name).with("bus", bus).with("address", format!("audio.slot.{name}"));
        let bus = |name: &str, label: &str| Value::map().with("name", name).with("label", label).with("address", format!("audio.bus.{name}"));
        let mix = Value::map()
            .with(
                "slots",
                vec![slot("patch.terminal_boot", "scene"), slot("patch.terminal_title", "scene"), slot("youtube", "music"), slot("timecode.ltc", "")],
            )
            .with("buses", vec![bus("music", "Music"), bus("scene", "Scene audio"), bus("program", "Everything")]);
        let channels = collect_sources(&mix, &Value::Null, &Value::Null);
        assert_eq!(
            channels.iter().map(|c| (c.label.as_str(), c.address.as_str(), c.meter.as_str())).collect::<Vec<_>>(),
            vec![("Music", "audio.bus.music", "audio.music"), ("Scene audio", "audio.bus.scene", "audio.scene")]
        );
    }

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
