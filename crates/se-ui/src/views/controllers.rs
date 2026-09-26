//! Controllers (§15.6, §10): Stream Deck page editor with live key images (drag presets and
//! scenes onto keys), MIDI devices with their controls, mappings, bindings, MCU banks and a
//! message monitor, MIDI learn, and voice push-to-talk status. Data: `controllers.*` queries
//! and state (se-input); edits go through `deck.assign`, `midi.*`, `voice.*` actions.

use crate::app::App;
use base64::Engine;
use egui::{Align2, Color32, CornerRadius, FontId, RichText, Sense, Stroke, StrokeKind, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Tab {
    #[default]
    Deck,
    Midi,
    Monitor,
}

/// What can be dropped on a key.
#[derive(Clone, Debug)]
enum Drop {
    Preset(String),
    Scene(String),
    Command(String),
}

#[derive(Default)]
struct State {
    tab: Tab,
    deck: Option<String>,
    /// Page shown in the editor (`None` = follow the device).
    page: Option<String>,
    selected: Option<i64>,
    preview_version: i64,
    preview_page: String,
    textures: HashMap<i64, egui::TextureHandle>,
    last_poll: Option<Instant>,
    last_monitor: Option<Instant>,
    learn_target: String,
    // key editor
    edit_key: Option<i64>,
    edit_kind: String,
    edit_target: String,
    edit_label: String,
    edit_icon: String,
    edit_color: String,
    edit_hold: String,
    edit_confirm: bool,
    ptt_down: bool,
    monitor_device: String,
}

type Shared = Arc<Mutex<State>>;

fn state(ui: &egui::Ui) -> Shared {
    ui.ctx().data_mut(|d| d.get_temp_mut_or_insert_with::<Shared>(egui::Id::new("se.controllers.view"), Shared::default).clone())
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}
fn i(v: &Value, k: &str) -> i64 {
    v.get_path(k).and_then(Value::as_i64).unwrap_or(0)
}
fn b(v: &Value, k: &str) -> bool {
    v.get_path(k).is_some_and(Value::truthy)
}

fn action(app: &mut App, name: &str, args: Value) {
    app.m.command(Op::Action { name: name.into(), args });
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let st = state(ui);
    let mut st = st.lock().unwrap_or_else(|p| p.into_inner());
    poll(app, &mut st);
    let t = app.t.clone();

    egui::Panel::left("controllers.devices").resizable(true).default_size(320.0).show(ui, |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            devices_panel(app, &mut st, ui);
        });
    });
    egui::CentralPanel::default().show(ui, |ui| {
        ui.horizontal(|ui| {
            for (tab, label) in [
                (Tab::Deck, format!("{} Stream Deck", icon::CONTROLLER)),
                (Tab::Midi, format!("{} MIDI", icon::MIX)),
                (Tab::Monitor, format!("{} Monitor", icon::CONSOLE)),
            ] {
                if ui.selectable_label(st.tab == tab, RichText::new(label).strong()).clicked() {
                    st.tab = tab;
                }
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.label(RichText::new(format!("page {}", app.m.str("controllers.page"))).color(t.fg_dim));
            });
        });
        ui.separator();
        match st.tab {
            Tab::Deck => deck_tab(app, &mut st, ui),
            Tab::Midi => midi_tab(app, &mut st, ui),
            Tab::Monitor => monitor_tab(app, &mut st, ui),
        }
    });
}

fn poll(app: &mut App, st: &mut State) {
    if !app.m.connected {
        return;
    }
    let due = st.last_poll.is_none_or(|t| t.elapsed().as_millis() > 300);
    if due {
        st.last_poll = Some(Instant::now());
        app.m.query("controllers.deck", Value::Null);
        let mut args = Value::map().with("since", st.preview_version);
        if let Some(d) = &st.deck {
            args = args.with("deck", d.clone());
        }
        app.m.query("controllers.deck.preview", args);
        app.m.query("controllers.midi", Value::Null);
        app.m.query("controllers.voice", Value::Null);
        if app.m.q("presets").is_none() {
            app.m.query("presets", Value::Null);
        }
        if app.m.q("scenes").is_none() {
            app.m.query("scenes", Value::Null);
        }
        if st.tab == Tab::Midi {
            app.m.query("config.bindings", Value::Null);
        }
    }
    if st.tab == Tab::Monitor && st.last_monitor.is_none_or(|t| t.elapsed().as_millis() > 250) {
        st.last_monitor = Some(Instant::now());
        let mut args = Value::map().with("n", 200);
        if !st.monitor_device.is_empty() {
            args = args.with("device", st.monitor_device.clone());
        }
        app.m.query("controllers.midi.monitor", args);
    }
}

fn led(ok: bool) -> LedState {
    if ok { LedState::Healthy } else { LedState::Error }
}

fn devices_panel(app: &mut App, st: &mut State, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::section(ui, &t, icon::DEVICE, "Devices");
    let decks = app.m.q_list("controllers.deck").to_vec();
    if decks.is_empty() {
        ui.label(RichText::new("No Stream Deck configured (controllers/deck.toml).").color(t.fg_dim));
    }
    for d in &decks {
        let id = s(d, "id").to_string();
        let title = format!("Deck `{id}`{}", if b(d, "primary") { " (primary)" } else { "" });
        widgets::card(ui, &t, icon::CONTROLLER, &title, led(b(d, "connected")), |ui| {
            if b(d, "connected") {
                ui.label(format!("{} · {} · fw {}", s(d, "model"), s(d, "serial"), s(d, "firmware")));
                ui.label(RichText::new(format!("{} ({})", s(d, "devnode"), s(d, "usb"))).small().color(t.fg_dim));
            } else {
                ui.label(RichText::new(s(d, "error")).color(t.bright_red));
            }
            ui.label(format!("page `{}` · {} images sent", s(d, "page"), i(d, "images_sent")));
            let addr = format!("controllers.{id}.brightness");
            let mut bright =
                app.m.get(&addr).and_then(Value::as_f64).unwrap_or_else(|| d.get_path("brightness").and_then(Value::as_f64).unwrap_or(70.0)) as f32;
            ui.horizontal(|ui| {
                ui.label(format!("{} brightness", render_icon("sun")));
                if ui.add(egui::Slider::new(&mut bright, 0.0..=100.0).integer()).changed() {
                    app.m.command(Op::Set { address: addr.clone(), value: Value::Int(bright as i64) });
                }
            });
            ui.horizontal(|ui| {
                if ui.button("re-send keys").clicked() {
                    action(app, "deck.refresh", Value::Null);
                }
                if ui.selectable_label(st.deck.as_deref() == Some(id.as_str()), "edit").clicked() {
                    st.deck = Some(id.clone());
                    st.preview_version = -1;
                    st.textures.clear();
                }
            });
        });
    }
    ui.add_space(6.0);
    for d in app.m.q_list("controllers.midi").to_vec() {
        let id = s(&d, "id").to_string();
        let configured = b(&d, "configured");
        let title = format!("MIDI `{id}`{}", if configured { "" } else { " (auto)" });
        widgets::card(
            ui,
            &t,
            icon::MIX,
            &title,
            if b(&d, "online") {
                LedState::Healthy
            } else if configured {
                LedState::Error
            } else {
                LedState::Idle
            },
            |ui| {
                if b(&d, "online") {
                    ui.label(format!("{} · {}", s(&d, "client"), s(&d, "profile")));
                    ui.label(RichText::new(d.get_path("usb").and_then(Value::as_str).unwrap_or("no USB path")).small().color(t.fg_dim));
                    ui.label(RichText::new(format!("in {} · out {}", i(&d, "msgs_in"), i(&d, "msgs_out"))).small().color(t.fg_dim));
                } else {
                    ui.label(RichText::new(s(&d, "error")).color(t.bright_red));
                }
                if configured {
                    ui.label(RichText::new(s(&d, "file")).small().color(t.fg_dim));
                }
                let count = i(&d, "bank_count");
                if count > 0 {
                    ui.horizontal(|ui| {
                        ui.label(format!("bank {}–{} of {count}", i(&d, "bank") + 1, (i(&d, "bank") + 8).min(count)));
                        if ui.button("◀").clicked() {
                            action(app, "midi.bank", Value::map().with("device", id.clone()).with("delta", -8));
                        }
                        if ui.button("▶").clicked() {
                            action(app, "midi.bank", Value::map().with("device", id.clone()).with("delta", 8));
                        }
                    });
                }
            },
        );
    }
    ui.add_space(6.0);
    // learn
    let learning = app.m.b("controllers.learn.active");
    widgets::card(ui, &t, icon::BINDING, "MIDI learn", if learning { LedState::Armed } else { LedState::Idle }, |ui| {
        if learning {
            ui.label(RichText::new(format!("move a control or press a deck key for `{}`…", app.m.str("controllers.learn.target"))).color(t.yellow));
            if ui.button("cancel").clicked() {
                action(app, "midi.learn.cancel", Value::Null);
            }
        } else {
            ui.add(
                egui::TextEdit::singleline(&mut st.learn_target)
                    .hint_text("fx.rgb_split.amount · preset.hype · scene.duo · do:scene.take")
                    .desired_width(f32::INFINITY),
            );
            if let Some(sel) = app.selected.clone()
                && st.learn_target.is_empty()
                && ui.small_button(format!("use selection `{sel}`")).clicked()
            {
                st.learn_target = sel;
            }
            if ui.add_enabled(!st.learn_target.trim().is_empty(), egui::Button::new(RichText::new("LEARN").strong())).clicked() {
                action(app, "midi.learn", Value::map().with("target", st.learn_target.trim().to_string()));
            }
        }
        if let Some(e) = app.m.events.iter().rev().find(|e| e.ty == "midi.learned") {
            ui.label(
                RichText::new(format!(
                    "last: {} → {} ({})",
                    e.payload.get_path("control").map(|v| v.to_string()).unwrap_or_default(),
                    e.payload.get_path("target").map(|v| v.to_string()).unwrap_or_default(),
                    e.payload.get_path("file").map(|v| v.to_string()).unwrap_or_default()
                ))
                .small()
                .color(t.fg_dim),
            );
        }
    });
    ui.add_space(6.0);
    voice_card(app, st, ui);
}

fn render_icon(name: &str) -> &'static str {
    match name {
        "sun" => "\u{f185}",
        "mic" => "\u{f130}",
        _ => icon::PLAY,
    }
}

fn voice_card(app: &mut App, st: &mut State, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let v = app.m.q("controllers.voice").cloned().unwrap_or_default();
    let state = app.m.str("controllers.voice.state").to_string();
    let led = match state.as_str() {
        "listening" => LedState::Active,
        "transcribing" | "confirm" | "loading" | "downloading" => LedState::Armed,
        "error" => LedState::Error,
        "disabled" => LedState::Idle,
        _ => LedState::Healthy,
    };
    widgets::card(ui, &t, render_icon("mic"), "Voice", led, |ui| {
        ui.label(format!("{} · whisper {} · capture `{}`", if state.is_empty() { "…" } else { &state }, s(&v, "model"), s(&v, "device")));
        if let Some(e) = v.get_path("error").and_then(Value::as_str) {
            ui.label(RichText::new(e).color(t.bright_red));
        }
        if let Some(p) = v.get_path("download").and_then(Value::as_f64) {
            ui.add(egui::ProgressBar::new(p as f32).text("downloading model"));
        }
        // hold to talk
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 34.0), Sense::click_and_drag());
        let down = resp.is_pointer_button_down_on();
        let fill = if down { t.bright_red } else { t.bg_light };
        ui.painter().rect_filled(rect, CornerRadius::same(6), fill);
        ui.painter().text(
            rect.center(),
            Align2::CENTER_CENTER,
            format!("{} hold to talk", render_icon("mic")),
            FontId::proportional(15.0),
            widgets::contrast(fill),
        );
        if down != st.ptt_down {
            st.ptt_down = down;
            action(app, "voice.ptt", Value::map().with("args", vec![Value::from(if down { "start" } else { "stop" })]));
        }
        let pending = app.m.str("controllers.voice.pending").to_string();
        if !pending.is_empty() {
            ui.label(RichText::new(format!("confirm `{pending}`?")).color(t.yellow).strong());
            ui.horizontal(|ui| {
                if ui.button(format!("{} yes", icon::CHECK)).clicked() {
                    action(app, "voice.confirm", Value::Null);
                }
                if ui.button(format!("{} no", icon::CROSS)).clicked() {
                    action(app, "voice.cancel", Value::Null);
                }
            });
        }
        for h in v.get_path("history").and_then(Value::as_list).unwrap_or(&[]).iter().take(5) {
            let intent = h.get_path("intent").and_then(Value::as_str).unwrap_or("—");
            let col = if b(h, "executed") { t.green } else { t.fg_dim };
            ui.label(RichText::new(format!("“{}” → {intent} ({} ms)", s(h, "text"), i(h, "ms"))).small().color(col));
        }
    });
}

fn hex_color(s: &str) -> Option<Color32> {
    se_ui_kit::theme::hex(s)
}

fn deck_tab(app: &mut App, st: &mut State, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let decks = app.m.q_list("controllers.deck").to_vec();
    let Some(deck) =
        st.deck.as_ref().and_then(|id| decks.iter().find(|d| s(d, "id") == id)).or_else(|| decks.iter().find(|d| b(d, "primary"))).or(decks.first()).cloned()
    else {
        ui.label(RichText::new("Add controllers/deck.toml (kind = \"deck\") to configure the Stream Deck.").color(t.fg_dim));
        return;
    };
    let deck_id = s(&deck, "id").to_string();
    let device_page = s(&deck, "page").to_string();
    let page_name = st.page.clone().unwrap_or_else(|| device_page.clone());
    // textures from the preview (the device's current page)
    if let Some(pv) = app.m.q("controllers.deck.preview").cloned()
        && !b(&pv, "unchanged")
        && i(&pv, "version") != st.preview_version
        && s(&pv, "deck") == deck_id
    {
        st.preview_version = i(&pv, "version");
        st.preview_page = s(&pv, "page").to_string();
        let size = i(&pv, "size").max(1) as usize;
        let b64 = base64::engine::general_purpose::STANDARD;
        for k in pv.get_path("keys").and_then(Value::as_list).unwrap_or(&[]) {
            let key = i(k, "key");
            match k.get_path("jpeg").and_then(Value::as_str).and_then(|x| b64.decode(x).ok()) {
                Some(bytes) => {
                    if let Ok(img) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg) {
                        let rgba = img.to_rgba8();
                        let ci = egui::ColorImage::from_rgba_unmultiplied([size, size], rgba.as_raw());
                        let tex = ui.ctx().load_texture(format!("deck.{deck_id}.{key}"), ci, egui::TextureOptions::LINEAR);
                        st.textures.insert(key, tex);
                    }
                }
                None => {
                    st.textures.remove(&key);
                }
            }
        }
    }
    let pages: Vec<Value> = deck.get_path("pages").and_then(Value::as_list).map(|l| l.to_vec()).unwrap_or_default();
    ui.horizontal_wrapped(|ui| {
        ui.label(RichText::new("pages").color(t.fg_dim));
        for p in &pages {
            let name = s(p, "page").to_string();
            let label = format!("{}{}", s(p, "label"), if name == device_page { " ●" } else { "" });
            if ui.selectable_label(page_name == name, label).on_hover_text("edit this page (● = on the deck now)").clicked() {
                st.page = if name == device_page { None } else { Some(name) };
                st.selected = None;
                st.edit_key = None;
            }
        }
        if page_name != device_page && ui.button(format!("{} show on deck", icon::PLAY)).clicked() {
            action(app, "deck.page", Value::map().with("deck", deck_id.clone()).with("args", vec![Value::from(page_name.as_str())]));
            st.page = None;
        }
    });
    let Some(page) = pages.iter().find(|p| s(p, "page") == page_name).cloned() else {
        return;
    };
    let keys: HashMap<i64, Value> = page.get_path("keys").and_then(Value::as_list).unwrap_or(&[]).iter().map(|k| (i(k, "key"), k.clone())).collect();
    let nkeys = i(&deck, "keys").max(15);
    let cols = i(&deck, "cols").max(5);
    let live = page_name == st.preview_page;
    ui.add_space(6.0);
    let mut dropped: Option<(i64, Drop)> = None;
    egui::Panel::right("controllers.deck.palette").resizable(true).default_size(220.0).show(ui, |ui| {
        palette(app, ui);
    });
    egui::ScrollArea::vertical().show(ui, |ui| {
        let size = 86.0;
        egui::Grid::new("deck.grid").spacing([8.0, 8.0]).show(ui, |ui| {
            for k in 0..nkeys {
                let def = keys.get(&k);
                let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
                let p = ui.painter_at(rect.expand(3.0));
                p.rect_filled(rect, CornerRadius::same(8), Color32::BLACK);
                match (live, st.textures.get(&k)) {
                    (true, Some(tex)) => {
                        p.image(tex.id(), rect.shrink(3.0), egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
                    }
                    _ => {
                        if let Some(d) = def {
                            let fill = hex_color(s(d, "color")).unwrap_or(t.bg_light);
                            p.rect_filled(rect.shrink(3.0), CornerRadius::same(6), fill.gamma_multiply(if b(d, "active") { 1.0 } else { 0.45 }));
                            let fg = widgets::contrast(fill);
                            p.text(rect.center() - Vec2::new(0.0, 12.0), Align2::CENTER_CENTER, s(d, "icon"), FontId::proportional(24.0), fg);
                            p.text(rect.center() + Vec2::new(0.0, 22.0), Align2::CENTER_CENTER, s(d, "label"), FontId::proportional(12.0), fg);
                        }
                    }
                }
                let edge = if st.selected == Some(k) { Stroke::new(2.5, t.accent) } else { Stroke::new(1.0, t.muted) };
                p.rect_stroke(rect, CornerRadius::same(8), edge, StrokeKind::Outside);
                p.text(rect.left_top() + Vec2::new(4.0, 2.0), Align2::LEFT_TOP, k.to_string(), FontId::proportional(9.0), t.fg_dim);
                if let Some(payload) = resp.dnd_release_payload::<Drop>() {
                    dropped = Some((k, (*payload).clone()));
                }
                if resp.dnd_hover_payload::<Drop>().is_some() {
                    p.rect_stroke(rect, CornerRadius::same(8), Stroke::new(3.0, t.yellow), StrokeKind::Outside);
                }
                let hover = match def {
                    Some(d) => format!("{} · {}\nclick: edit · right-click: press on the deck", s(d, "label"), s(d, "action")),
                    None => "empty — drop a preset or scene here".into(),
                };
                let resp = resp.on_hover_text(hover);
                if resp.clicked() {
                    st.selected = Some(k);
                    load_editor(st, k, def);
                }
                if resp.secondary_clicked() && def.is_some() {
                    action(app, "deck.press", Value::map().with("deck", deck_id.clone()).with("key", k).with("page", page_name.clone()));
                    action(app, "deck.release", Value::map().with("deck", deck_id.clone()).with("key", k).with("page", page_name.clone()));
                }
                if (k + 1) % cols == 0 {
                    ui.end_row();
                }
            }
        });
        if !live {
            ui.label(
                RichText::new("editing a page that is not on the device: showing a sketch (double-click the page to show it on the deck)")
                    .small()
                    .color(t.fg_dim),
            );
        }
        ui.add_space(8.0);
        key_editor(app, st, ui, &deck_id, &page_name);
    });
    if let Some((k, d)) = dropped {
        let mut args = Value::map().with("deck", deck_id.clone()).with("page", page_name.clone()).with("key", k);
        args = match d {
            Drop::Preset(p) => args.with("preset", p),
            Drop::Scene(sc) => args.with("scene", sc),
            Drop::Command(c) => args.with("do", vec![Value::Str(c)]),
        };
        action(app, "deck.assign", args);
        st.selected = Some(k);
    }
}

fn palette(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::section(ui, &t, icon::PRESET, "Drag onto a key");
    egui::ScrollArea::vertical().show(ui, |ui| {
        ui.label(RichText::new("presets").small().color(t.fg_dim));
        for p in app.m.q_list("presets").to_vec() {
            let name = s(&p, "name").to_string();
            let label = format!("{} {}", icon::PRESET, s(&p, "label"));
            ui.dnd_drag_source(egui::Id::new(("deck.drag.preset", &name)), Drop::Preset(name.clone()), |ui| {
                ui.label(RichText::new(label).color(hex_color(s(&p, "color")).unwrap_or(t.fg)));
            });
        }
        ui.add_space(6.0);
        ui.label(RichText::new("scenes").small().color(t.fg_dim));
        for sc in app.m.q_list("scenes").to_vec() {
            let name = s(&sc, "name").to_string();
            ui.dnd_drag_source(egui::Id::new(("deck.drag.scene", &name)), Drop::Scene(name.clone()), |ui| {
                ui.label(format!("{} {}", icon::SCENE, s(&sc, "label")));
            });
        }
        ui.add_space(6.0);
        ui.label(RichText::new("actions").small().color(t.fg_dim));
        for c in ["scene.take", "clean", "panic", "scene.next", "scene.prev", "session.marker", "queue.skip", "voice.confirm", "voice.cancel"] {
            ui.dnd_drag_source(egui::Id::new(("deck.drag.cmd", c)), Drop::Command(c.to_string()), |ui| {
                ui.label(format!("{} {c}", icon::PLAY));
            });
        }
    });
}

fn load_editor(st: &mut State, k: i64, def: Option<&Value>) {
    st.edit_key = Some(k);
    let d = def.cloned().unwrap_or_default();
    st.edit_kind = match s(&d, "kind") {
        "" | "empty" => "preset".into(),
        other => other.into(),
    };
    st.edit_target = match st.edit_kind.as_str() {
        "preset" => s(&d, "preset").into(),
        "scene" => s(&d, "scene").into(),
        "toggle" | "momentary" => s(&d, "address").into(),
        "page" => s(&d, "page").into(),
        "action" => s(&d, "action").into(),
        _ => String::new(),
    };
    st.edit_label.clear();
    st.edit_icon.clear();
    st.edit_color.clear();
    st.edit_hold = if i(&d, "hold_ms") > 0 { format!("{}ms", i(&d, "hold_ms")) } else { String::new() };
    st.edit_confirm = b(&d, "confirm");
}

fn key_editor(app: &mut App, st: &mut State, ui: &mut egui::Ui, deck: &str, page: &str) {
    let t = app.t.clone();
    let Some(k) = st.edit_key else {
        ui.label(RichText::new("Click a key to edit it; drag presets, scenes, or actions from the right onto keys.").color(t.fg_dim));
        return;
    };
    widgets::card(ui, &t, icon::SETTINGS, &format!("key {k} on page `{page}`"), LedState::Idle, |ui| {
        egui::Grid::new("deck.key.editor").num_columns(2).show(ui, |ui| {
            ui.label("does");
            egui::ComboBox::from_id_salt("deck.key.kind").selected_text(st.edit_kind.clone()).show_ui(ui, |ui| {
                for kind in ["preset", "scene", "toggle", "momentary", "action", "page", "ptt"] {
                    ui.selectable_value(&mut st.edit_kind, kind.to_string(), kind);
                }
            });
            ui.end_row();
            if st.edit_kind != "ptt" {
                ui.label(match st.edit_kind.as_str() {
                    "preset" => "preset",
                    "scene" => "scene",
                    "page" => "page (name/next/prev)",
                    "action" => "commands (; separated)",
                    _ => "address",
                });
                ui.text_edit_singleline(&mut st.edit_target);
                ui.end_row();
            }
            ui.label("label");
            ui.add(egui::TextEdit::singleline(&mut st.edit_label).hint_text("from the target"));
            ui.end_row();
            ui.label("icon");
            ui.add(egui::TextEdit::singleline(&mut st.edit_icon).hint_text("preset, scene, mic, fire, … or a glyph"));
            ui.end_row();
            ui.label("color");
            ui.add(egui::TextEdit::singleline(&mut st.edit_color).hint_text("#rrggbb or red, accent, …"));
            ui.end_row();
            ui.label("hold to fire");
            ui.add(egui::TextEdit::singleline(&mut st.edit_hold).hint_text("e.g. 1s"));
            ui.end_row();
            ui.label("confirm");
            ui.checkbox(&mut st.edit_confirm, "press twice");
            ui.end_row();
        });
        ui.horizontal(|ui| {
            if ui.button(RichText::new(format!("{} apply", icon::CHECK)).strong()).clicked() {
                let mut args = Value::map().with("deck", deck.to_string()).with("page", page.to_string()).with("key", k);
                let target = st.edit_target.trim().to_string();
                args = match st.edit_kind.as_str() {
                    "action" => {
                        args.with("do", target.split(';').map(|c| Value::Str(c.trim().to_string())).filter(|v| v.as_str() != Some("")).collect::<Vec<_>>())
                    }
                    "ptt" => args.with("ptt", true),
                    kind => args.with(kind, target),
                };
                for (key, v) in [("label", &st.edit_label), ("icon", &st.edit_icon), ("color", &st.edit_color), ("hold", &st.edit_hold)] {
                    if !v.trim().is_empty() {
                        args = args.with(key, v.trim().to_string());
                    }
                }
                if st.edit_confirm {
                    args = args.with("confirm", true);
                }
                action(app, "deck.assign", args);
            }
            if ui.button(format!("{} clear", icon::CROSS)).clicked() {
                action(app, "deck.assign", Value::map().with("deck", deck.to_string()).with("page", page.to_string()).with("key", k).with("clear", true));
            }
            if ui.button(format!("{} press", icon::PLAY)).on_hover_text("run it as if pressed on the deck").clicked() {
                action(app, "deck.press", Value::map().with("deck", deck.to_string()).with("key", k).with("page", page.to_string()));
                action(app, "deck.release", Value::map().with("deck", deck.to_string()).with("key", k).with("page", page.to_string()));
            }
        });
    });
}

fn midi_tab(app: &mut App, _st: &mut State, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let devices = app.m.q_list("controllers.midi").to_vec();
    if devices.is_empty() {
        ui.label(RichText::new("No MIDI devices.").color(t.fg_dim));
        return;
    }
    let bindings = app.m.q("config.bindings").and_then(Value::as_list).map(|l| l.to_vec()).unwrap_or_default();
    egui::ScrollArea::vertical().show(ui, |ui| {
        for d in devices {
            let id = s(&d, "id").to_string();
            let header = format!("{} {id} — {} ({})", icon::MIX, s(&d, "client"), s(&d, "profile"));
            egui::CollapsingHeader::new(RichText::new(header).strong()).id_salt(("midi.dev", &id)).default_open(b(&d, "configured")).show(ui, |ui| {
                // mappings
                let maps = d.get_path("maps").and_then(Value::as_list).unwrap_or(&[]).to_vec();
                let prefix = format!("midi.{id}.");
                let dev_bindings: Vec<&Value> = bindings.iter().filter(|b| s(b, "signal").starts_with(&prefix)).collect();
                if !maps.is_empty() || !dev_bindings.is_empty() {
                    widgets::section(ui, &t, icon::BINDING, "Mappings");
                    egui::Grid::new(("midi.maps", &id)).striped(true).num_columns(4).show(ui, |ui| {
                        for m in &maps {
                            ui.label(RichText::new(s(m, "control")).monospace());
                            ui.label(s(m, "action"));
                            ui.label(if b(m, "feedback") { RichText::new("feedback").color(t.green) } else { RichText::new("") });
                            if ui.small_button("unmap").clicked() {
                                action(app, "midi.unmap", Value::map().with("device", id.clone()).with("control", s(m, "control")));
                            }
                            ui.end_row();
                        }
                        for bnd in &dev_bindings {
                            let control = s(bnd, "signal").trim_start_matches(&prefix).to_string();
                            ui.label(RichText::new(&control).monospace());
                            let takeover = bnd.get_path("takeover").and_then(Value::as_str).unwrap_or("—");
                            ui.label(format!("binding → {} (takeover {takeover}, curve {})", s(bnd, "target"), s(bnd, "curve")));
                            ui.label(if b(bnd, "feedback") { RichText::new("feedback").color(t.green) } else { RichText::new("") });
                            if ui.small_button("unmap").clicked() {
                                action(app, "midi.unmap", Value::map().with("device", id.clone()).with("control", control.clone()));
                            }
                            ui.end_row();
                        }
                    });
                }
                // controls
                widgets::section(ui, &t, icon::CONTROLLER, "Controls");
                egui::Grid::new(("midi.controls", &id)).striped(true).num_columns(4).show(ui, |ui| {
                    for c in d.get_path("controls").and_then(Value::as_list).unwrap_or(&[]) {
                        let name = s(c, "name");
                        ui.label(RichText::new(name).monospace());
                        ui.label(RichText::new(s(c, "kind")).small().color(t.fg_dim));
                        let v = c.get_path("value").and_then(Value::as_f64).unwrap_or(0.0) as f32;
                        ui.add(egui::ProgressBar::new(v).desired_width(120.0).text(format!("{v:.2}")));
                        let mut flags = Vec::new();
                        if b(c, "relative") {
                            flags.push("relative");
                        }
                        if b(c, "button") {
                            flags.push("button");
                        }
                        if b(c, "feedback") {
                            flags.push("feedback");
                        }
                        if b(c, "touched") {
                            flags.push("TOUCHED");
                        }
                        ui.label(RichText::new(flags.join(" · ")).small().color(t.fg_dim)).on_hover_text(format!("signal midi.{id}.{name}"));
                        ui.end_row();
                    }
                });
            });
        }
    });
}

fn monitor_tab(app: &mut App, st: &mut State, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        ui.label("device");
        egui::ComboBox::from_id_salt("midi.monitor.dev")
            .selected_text(if st.monitor_device.is_empty() { "all".to_string() } else { st.monitor_device.clone() })
            .show_ui(ui, |ui| {
                ui.selectable_value(&mut st.monitor_device, String::new(), "all");
                for d in app.m.q_list("controllers.midi").to_vec() {
                    let id = s(&d, "id").to_string();
                    ui.selectable_value(&mut st.monitor_device, id.clone(), id);
                }
            });
        ui.label(RichText::new("newest first · watching keeps the monitor on").small().color(t.fg_dim));
    });
    egui::ScrollArea::vertical().show(ui, |ui| {
        egui::Grid::new("midi.monitor").striped(true).num_columns(5).show(ui, |ui| {
            for e in app.m.q_list("controllers.midi.monitor").to_vec() {
                ui.label(RichText::new(format!("{}", i(&e, "ts_ms"))).small().color(t.fg_dim));
                ui.label(s(&e, "device"));
                ui.label(if s(&e, "dir") == "in" { RichText::new("in").color(t.green) } else { RichText::new("out").color(t.blue) });
                ui.label(RichText::new(s(&e, "hex")).monospace());
                ui.label(s(&e, "desc"));
                ui.end_row();
            }
        });
    });
}
