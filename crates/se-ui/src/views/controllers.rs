//! Automation → Buttons & pedals (§15.6, §10). Devices on the left (Stream Decks, MIDI devices
//! like the X-TOUCH or the FBV pedal, voice control), the selected device on the right. A Stream
//! Deck shows its keys as a grid you click (or drop saved actions and scenes onto); the key's
//! inspector says what it does, how it looks and how safe it is. MIDI devices list "this control
//! → that" under a Learn box; voice has push-to-talk and its history. Action lists use the shared
//! step editor (`rules::steps_editor`). Live control values, the raw MIDI message log and file
//! names sit behind a sub-view or "Details". Data: `controllers.*` queries and state (se-input);
//! edits go through `deck.assign|page|press|release|refresh`, `midi.learn|learn.cancel|unmap|bank`,
//! `voice.*`.

use crate::app::App;
use crate::views::live::nice;
use crate::views::rules::{Words, nice_name, setting_name, setting_words, steps_editor};
use base64::Engine;
use egui::{Align, Align2, Color32, CornerRadius, Layout, RichText, Sense, Stroke, StrokeKind, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_medium, font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// Width of the device list.
const LIST_W: f32 = 290.0;

/// The device whose editor is open.
#[derive(Clone, PartialEq, Eq, Default, Debug)]
enum Device {
    #[default]
    Auto,
    Deck(String),
    Midi(String),
    Voice,
}

/// What can be dropped on a key.
#[derive(Clone, Debug)]
enum Drop {
    Preset(String),
    Scene(String),
    Command(String),
}

/// What a key can do: (config kind, words).
const KEY_KINDS: [(&str, &str); 7] = [
    ("preset", "Run a saved action"),
    ("scene", "Switch scene"),
    ("toggle", "Toggle a setting"),
    ("momentary", "Hold a setting"),
    ("action", "Run actions"),
    ("page", "Go to page"),
    ("ptt", "Push to talk"),
];

/// Show controls offered for dragging: (command, words).
const DECK_ACTIONS: [(&str, &str); 9] = [
    ("scene.take", "Switch to up next"),
    ("scene.next", "Next scene"),
    ("scene.prev", "Previous scene"),
    ("clean", "Clear effects"),
    ("panic", "Panic"),
    ("session.marker", "Marker"),
    ("queue.skip", "Skip song"),
    ("voice.confirm", "OK a voice command"),
    ("voice.cancel", "Cancel a voice command"),
];

/// Theme colors a key can use (the deck config accepts these names).
const KEY_COLORS: [&str; 8] = ["red", "orange", "yellow", "green", "cyan", "blue", "magenta", "accent"];

/// What a control can be taught (the Learn box).
const LEARN_KINDS: [&str; 4] = ["Run a saved action", "Switch scene", "Change a setting", "Run actions"];
const MIDI_VIEWS: [&str; 3] = ["What each control does", "See controls move", "Message log"];

#[derive(Default)]
struct State {
    device: Device,
    /// Page shown in the deck editor (`None` = follow the device).
    page: Option<String>,
    selected: Option<i64>,
    preview_version: i64,
    preview_page: String,
    textures: HashMap<i64, egui::TextureHandle>,
    last_poll: Option<Instant>,
    last_monitor: Option<Instant>,
    midi_view: usize,
    learn_kind: usize,
    /// Saved action, scene or setting address to learn.
    learn_pick: String,
    /// Action lines to learn ("Run actions").
    learn_cmds: Vec<String>,
    // key editor
    edit_key: Option<i64>,
    edit_kind: String,
    /// Saved action, scene, setting address or page (by kind).
    edit_target: String,
    /// "Switch scene": straight to air instead of up next.
    edit_cut: bool,
    /// "Run actions": lines on press and on let go.
    edit_do: Vec<String>,
    edit_release: Vec<String>,
    edit_label: String,
    edit_icon: String,
    edit_color: String,
    edit_hold: String,
    edit_confirm: bool,
    ptt_down: bool,
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
fn strings(v: &Value, k: &str) -> Vec<String> {
    v.get_path(k).and_then(Value::as_list).map(|l| l.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
}

/// Action lines as saved: trimmed, empty steps dropped.
fn clean(cmds: &[String]) -> Vec<String> {
    cmds.iter().map(|c| c.trim()).filter(|c| !c.is_empty()).map(String::from).collect()
}

fn action(app: &mut App, name: &str, args: Value) {
    app.m.command(Op::Action { name: name.into(), args });
}

use widgets::chip;

/// `fs_a` → "Footswitch A", `enc.3` → "Knob 3", `push.3` → "Knob 3 (press)".
fn control_name(n: &str) -> String {
    let parts: Vec<&str> = n.split(['.', '_']).collect();
    match parts.as_slice() {
        ["fs", x] => format!("Footswitch {}", x.to_uppercase()),
        ["enc", x] => format!("Knob {x}"),
        ["push", x] => format!("Knob {x} (press)"),
        ["btn", x] => format!("Button {x}"),
        ["fader", x] => format!("Fader {x}"),
        ["layer", x] => format!("Layer {}", x.to_uppercase()),
        ["cc", x] => format!("Control {x}"),
        ["note", x] => format!("Key {x}"),
        _ => nice(&n.replace('.', " ")),
    }
}

/// A theme color name (`red`, `accent`) or `#rrggbb`.
fn key_color(t: &Theme, c: &str) -> Option<Color32> {
    Some(match c {
        "red" => t.red,
        "orange" => t.orange,
        "yellow" => t.yellow,
        "green" => t.green,
        "cyan" => t.cyan,
        "blue" => t.blue,
        "magenta" => t.magenta,
        "accent" => t.accent,
        other => return se_ui_kit::theme::hex(other),
    })
}

fn device_name(d: &Value, fallback: &str) -> String {
    let name = [s(d, "client"), s(d, "model")].into_iter().find(|x| !x.is_empty()).map(String::from);
    name.unwrap_or_else(|| nice(fallback))
}

fn midi_glyph(d: &Value) -> &'static str {
    if s(d, "client").to_lowercase().contains("fbv") || s(d, "id").contains("fbv") { icon::HAND } else { icon::SLIDERS }
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let st = state(ui);
    let mut st = st.lock().unwrap_or_else(|p| p.into_inner());
    let decks = app.m.q_list("controllers.deck").to_vec();
    let midis = app.m.q_list("controllers.midi").to_vec();
    let dev = resolve(&st.device, &decks, &midis);
    poll(app, &mut st, &dev);
    let t = app.t.clone();
    let voice_state = app.m.str("controllers.voice.state").to_string();
    let connected = app.m.connected;
    let (picked, _) = widgets::split(
        ui,
        LIST_W,
        |ui| {
            egui::ScrollArea::vertical()
                .id_salt("controllers-list")
                .auto_shrink([false, false])
                .show(ui, |ui| device_list(ui, &t, &decks, &midis, &dev, &voice_state, connected))
                .inner
        },
        |ui| {
            egui::ScrollArea::vertical().id_salt("controllers-detail").auto_shrink([false, false]).show(ui, |ui| {
                match &dev {
                    Device::Deck(id) => {
                        if let Some(d) = decks.iter().find(|d| s(d, "id") == id) {
                            deck_editor(app, &mut st, ui, &t, d);
                        }
                    }
                    Device::Midi(id) => {
                        if let Some(d) = midis.iter().find(|d| s(d, "id") == id) {
                            midi_editor(app, &mut st, ui, &t, d);
                        }
                    }
                    Device::Voice => voice_editor(app, &mut st, ui, &t),
                    Device::Auto => {}
                }
                ui.add_space(spacing::XL);
            });
        },
    );
    if let Some(d) = picked {
        if matches!(d, Device::Deck(_)) {
            st.page = None;
            st.edit_key = None;
            st.selected = None;
            st.preview_version = -1;
            st.textures.clear();
        }
        st.device = d;
        ui.ctx().request_repaint();
    }
}

/// The selected device, falling back to the first one present.
fn resolve(sel: &Device, decks: &[Value], midis: &[Value]) -> Device {
    let present = match sel {
        Device::Deck(id) => decks.iter().any(|d| s(d, "id") == id),
        Device::Midi(id) => midis.iter().any(|d| s(d, "id") == id),
        Device::Voice => true,
        Device::Auto => false,
    };
    if present {
        return sel.clone();
    }
    decks
        .iter()
        .find(|d| b(d, "primary"))
        .or(decks.first())
        .map(|d| Device::Deck(s(d, "id").to_string()))
        .or_else(|| midis.first().map(|d| Device::Midi(s(d, "id").to_string())))
        .unwrap_or(Device::Voice)
}

fn poll(app: &mut App, st: &mut State, dev: &Device) {
    if !app.m.connected {
        return;
    }
    let due = st.last_poll.is_none_or(|t| t.elapsed().as_millis() > 300);
    if due {
        st.last_poll = Some(Instant::now());
        app.m.query("controllers.deck", Value::Null);
        let mut args = Value::map().with("since", st.preview_version);
        if let Device::Deck(d) = dev {
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
        if matches!(dev, Device::Midi(_)) {
            app.m.query("config.bindings", Value::Null);
        }
    }
    if let Device::Midi(id) = dev
        && st.midi_view == 2
        && st.last_monitor.is_none_or(|t| t.elapsed().as_millis() > 250)
    {
        st.last_monitor = Some(Instant::now());
        app.m.query("controllers.midi.monitor", Value::map().with("n", 200).with("device", id.clone()));
    }
}

// ---- device list ---------------------------------------------------------------------------------

fn voice_words(t: &Theme, state: &str) -> (&'static str, Color32) {
    match state {
        "listening" => ("Listening", t.bright_red),
        "transcribing" => ("Thinking…", t.yellow),
        "confirm" => ("Needs your OK", t.yellow),
        "loading" | "downloading" => ("Getting ready", t.yellow),
        "error" => ("Needs a look", t.bright_red),
        "disabled" => ("Off", t.text_dim),
        "" => ("Off", t.text_dim),
        _ => ("Working", t.green),
    }
}

/// Every device, grouped by kind. Returns the device clicked.
fn device_list(ui: &mut egui::Ui, t: &Theme, decks: &[Value], midis: &[Value], dev: &Device, voice_state: &str, connected: bool) -> Option<Device> {
    let mut picked = None;
    widgets::pane_header(ui, t, "Devices", Some(decks.len() + midis.len() + 1), None);
    if !decks.is_empty() {
        widgets::group_label(ui, t, "Stream Deck");
    }
    for d in decks {
        let id = s(d, "id");
        let sub = if b(d, "connected") {
            format!("Working · {} keys · “{}”", i(d, "keys").max(1), page_label(d, s(d, "page")))
        } else {
            "Not connected".into()
        };
        let sel = *dev == Device::Deck(id.to_string());
        if widgets::list_row(ui, t, icon::GRID, &device_name(d, "Stream Deck"), &sub, "", sel).clicked() && !sel {
            picked = Some(Device::Deck(id.to_string()));
        }
    }
    if !midis.is_empty() {
        widgets::group_label(ui, t, "MIDI");
    }
    for d in midis {
        let id = s(d, "id");
        let status = if b(d, "online") {
            "Working"
        } else if b(d, "configured") {
            "Not connected"
        } else {
            "Off"
        };
        let maps = d.get_path("maps").and_then(Value::as_list).map_or(0, <[Value]>::len);
        let controls = d.get_path("controls").and_then(Value::as_list).map_or(0, <[Value]>::len);
        let sub = match (controls, maps) {
            (0, 0) => status.to_string(),
            (c, 0) => format!("{status} · {c} controls"),
            (c, m) => format!("{status} · {c} controls, {m} with a job"),
        };
        let sel = *dev == Device::Midi(id.to_string());
        if widgets::list_row(ui, t, midi_glyph(d), &device_name(d, id), &sub, "", sel).clicked() && !sel {
            picked = Some(Device::Midi(id.to_string()));
        }
    }
    widgets::group_label(ui, t, "Voice");
    let sel = *dev == Device::Voice;
    let sub = format!("{} · hold to talk", voice_words(t, voice_state).0);
    if widgets::list_row(ui, t, icon::MIC, "Voice control", &sub, "", sel).clicked() && !sel {
        picked = Some(Device::Voice);
    }
    if decks.is_empty() && midis.is_empty() && connected {
        ui.add_space(spacing::S);
        widgets::hint(ui, t, "No Stream Deck, pedal or controller yet. Set one up under Settings → Devices and it shows up here.");
    }
    picked
}

/// A heading inside the detail pane, one step below the device's: title, one line, actions on
/// the right.
fn sub_header(ui: &mut egui::Ui, t: &Theme, title: &str, subtitle: &str, actions: impl FnOnce(&mut egui::Ui)) {
    ui.horizontal(|ui| {
        ui.vertical(|ui| {
            ui.label(RichText::new(title).font(font_semibold(type_scale::LARGE)).color(t.fg));
            if !subtitle.is_empty() {
                ui.label(RichText::new(subtitle).size(type_scale::SMALL + 0.5).color(t.text_dim));
            }
        });
        ui.with_layout(Layout::right_to_left(Align::Center), actions);
    });
    ui.add_space(spacing::S);
}

// ---- Stream Deck ---------------------------------------------------------------------------------

fn page_label(deck: &Value, page: &str) -> String {
    let label = deck.get_path("pages").and_then(Value::as_list).and_then(|l| l.iter().find(|p| s(p, "page") == page)).map(|p| s(p, "label")).unwrap_or("");
    nice_name(if label.is_empty() { page } else { label })
}

/// Decode the device's current key images (only the page it shows).
fn update_textures(ui: &egui::Ui, app: &App, st: &mut State, deck_id: &str) {
    let Some(pv) = app.m.q("controllers.deck.preview") else { return };
    if b(pv, "unchanged") || i(pv, "version") == st.preview_version || s(pv, "deck") != deck_id {
        return;
    }
    st.preview_version = i(pv, "version");
    st.preview_page = s(pv, "page").to_string();
    let size = i(pv, "size").max(1) as usize;
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

fn deck_editor(app: &mut App, st: &mut State, ui: &mut egui::Ui, t: &Theme, deck: &Value) {
    let deck_id = s(deck, "id").to_string();
    let device_page = s(deck, "page").to_string();
    let page_name = st.page.clone().unwrap_or_else(|| device_page.clone());
    update_textures(ui, app, st, &deck_id);
    let pages: Vec<Value> = deck.get_path("pages").and_then(Value::as_list).map(|l| l.to_vec()).unwrap_or_default();
    let words = Words::new(app);
    let sub = if b(deck, "connected") {
        format!("{} keys · “{}” is on the deck", i(deck, "keys").max(1), page_label(deck, &device_page))
    } else if s(deck, "error").is_empty() {
        "Not connected. Plug it in and it connects by itself.".into()
    } else {
        format!("Not connected: {}", s(deck, "error"))
    };
    let mut refresh = false;
    widgets::detail_header(ui, t, icon::GRID, &device_name(deck, "Stream Deck"), &sub, |ui| {
        refresh = widgets::icon_button(ui, t, icon::UNDO, "Send all key pictures to the deck again").clicked();
    });
    if refresh {
        action(app, "deck.refresh", Value::Null);
    }
    if pages.is_empty() {
        widgets::empty_state(ui, t, icon::GRID, "No pages yet", "Your Stream Deck's pages show up here once it's set up.", None);
        return;
    }
    // pages + brightness
    ui.horizontal(|ui| {
        let labels: Vec<String> = pages.iter().map(|p| page_label(deck, s(p, "page"))).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut cur = pages.iter().position(|p| s(p, "page") == page_name).unwrap_or(0);
        if widgets::segmented(ui, t, &mut cur, &refs) {
            let name = s(&pages[cur], "page").to_string();
            st.page = if name == device_page { None } else { Some(name) };
            st.selected = None;
            st.edit_key = None;
        }
        if page_name == device_page {
            widgets::hint(ui, t, "On the deck now");
        } else if widgets::button_ex(ui, t, Some(icon::PLAY), "Show on the deck", Kind::Secondary, Size::Small, 0.0, true).clicked() {
            action(app, "deck.page", Value::map().with("deck", deck_id.clone()).with("args", vec![Value::from(page_name.as_str())]));
            st.page = None;
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let addr = format!("controllers.{deck_id}.brightness");
            let mut bright =
                app.m.get(&addr).and_then(Value::as_f64).unwrap_or_else(|| deck.get_path("brightness").and_then(Value::as_f64).unwrap_or(70.0)) as f32;
            ui.spacing_mut().slider_width = 120.0;
            if ui.add(egui::Slider::new(&mut bright, 0.0..=100.0).show_value(false)).on_hover_text(format!("{bright:.0}%")).changed() {
                app.m.command(Op::Set { address: addr, value: Value::Int(bright as i64) });
            }
            ui.label(RichText::new("Brightness").size(type_scale::SMALL + 0.5).color(t.text_dim));
        });
    });
    ui.add_space(spacing::M);
    let Some(page) = pages.iter().find(|p| s(p, "page") == page_name).cloned() else { return };
    let keys: HashMap<i64, Value> = page.get_path("keys").and_then(Value::as_list).unwrap_or(&[]).iter().map(|k| (i(k, "key"), k.clone())).collect();
    let grid =
        Grid { keys: &keys, nkeys: i(deck, "keys").max(1), cols: i(deck, "cols").max(1), live: page_name == st.preview_page, deck: &deck_id, page: &page_name };
    // wide: keys (+ drag palette) | key inspector; narrow: stacked
    let w = ui.available_width();
    let side = w >= 1000.0;
    let insp_w = if side { (w * 0.46).clamp(420.0, 640.0) } else { w };
    let grid_w = if side { w - insp_w - spacing::XL } else { w };
    let size = ((grid_w - KEY_GAP * (grid.cols - 1) as f32) / grid.cols as f32).clamp(56.0, 132.0);
    let mut dropped: Option<(i64, Drop)> = None;
    if side {
        ui.horizontal_top(|ui| {
            ui.allocate_ui_with_layout(Vec2::new(grid_w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(grid_w);
                dropped = key_grid(app, st, ui, t, &words, &grid, size);
                ui.add_space(spacing::L);
                palette(ui, t, &words);
            });
            ui.add_space(spacing::XL - ui.spacing().item_spacing.x);
            ui.allocate_ui_with_layout(Vec2::new(insp_w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(insp_w);
                key_editor(app, st, ui, t, &words, &grid, &pages, deck);
            });
        });
    } else {
        dropped = key_grid(app, st, ui, t, &words, &grid, size);
        ui.add_space(spacing::L);
        key_editor(app, st, ui, t, &words, &grid, &pages, deck);
        ui.add_space(spacing::L);
        palette(ui, t, &words);
    }
    if let Some((k, d)) = dropped {
        let args = Value::map().with("deck", deck_id.clone()).with("page", page_name.clone()).with("key", k);
        // the key gets exactly this job; the editor shows it before the file comes back
        load_editor(st, k, None);
        let args = match d {
            Drop::Preset(p) => {
                st.edit_target = p.clone();
                args.with("preset", p)
            }
            Drop::Scene(sc) => {
                (st.edit_kind, st.edit_target) = ("scene".into(), sc.clone());
                args.with("scene", sc)
            }
            Drop::Command(c) => {
                (st.edit_kind, st.edit_do) = ("action".into(), vec![c.clone()]);
                args.with("do", vec![c])
            }
        };
        action(app, "deck.assign", args);
        st.selected = Some(k);
    }
    ui.add_space(spacing::M);
    widgets::details(ui, t, ("deck-details", deck_id.as_str()), "Details", |ui| {
        widgets::fact(ui, t, "Model", s(deck, "model"));
        widgets::fact(ui, t, "Serial number", s(deck, "serial"));
        widgets::fact(ui, t, "Firmware", s(deck, "firmware"));
        widgets::fact(ui, t, "Connection", &format!("{} ({})", s(deck, "devnode"), s(deck, "usb")));
        widgets::fact(ui, t, "Pictures sent", &i(deck, "images_sent").to_string());
        widgets::fact(ui, t, "Saved in", s(deck, "file"));
        if !s(deck, "error").is_empty() {
            widgets::fact(ui, t, "Last problem", s(deck, "error"));
        }
    });
}

const KEY_GAP: f32 = 10.0;

/// The page being edited.
struct Grid<'a> {
    keys: &'a HashMap<i64, Value>,
    nkeys: i64,
    cols: i64,
    /// The deck shows this page, so keys have their real pictures.
    live: bool,
    deck: &'a str,
    page: &'a str,
}

/// The key grid. Returns something dropped on a key.
fn key_grid(app: &mut App, st: &mut State, ui: &mut egui::Ui, t: &Theme, words: &Words, g: &Grid, size: f32) -> Option<(i64, Drop)> {
    let mut dropped = None;
    ui.spacing_mut().item_spacing = Vec2::splat(KEY_GAP);
    for row in 0..(g.nkeys as usize).div_ceil(g.cols as usize) as i64 {
        ui.horizontal(|ui| {
            for c in 0..g.cols {
                let k = row * g.cols + c;
                if k >= g.nkeys {
                    break;
                }
                if let Some(d) = deck_key(app, st, ui, t, words, k, g.keys.get(&k), g, size) {
                    dropped = Some((k, d));
                }
            }
        });
    }
    ui.spacing_mut().item_spacing = Vec2::splat(spacing::S);
    if !g.live {
        widgets::hint(ui, t, "This page isn't on the deck right now, so keys show a sketch instead of their real pictures.");
    }
    dropped
}

/// One key of the grid. Returns something dropped on it.
#[allow(clippy::too_many_arguments)]
fn deck_key(app: &mut App, st: &mut State, ui: &mut egui::Ui, t: &Theme, words: &Words, k: i64, def: Option<&Value>, g: &Grid, size: f32) -> Option<Drop> {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let p = ui.painter_at(rect.expand(4.0));
    let r = CornerRadius::same(radius::TILE);
    // the key face is a small screen: black like video
    p.rect_filled(rect, r, Color32::BLACK);
    match (g.live, st.textures.get(&k)) {
        (true, Some(tex)) => {
            p.image(tex.id(), rect.shrink(3.0), egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), Color32::WHITE);
        }
        _ => match def {
            Some(d) if s(d, "kind") != "empty" => {
                let fill = key_color(t, s(d, "color")).unwrap_or(t.surface_hi);
                p.rect_filled(rect.shrink(3.0), CornerRadius::same(radius::TILE - 2), fill.gamma_multiply(if b(d, "active") { 1.0 } else { 0.5 }));
                let fg = widgets::contrast(fill);
                p.text(rect.center() - Vec2::new(0.0, size * 0.12), Align2::CENTER_CENTER, s(d, "icon"), font(size * 0.26), fg);
                p.text(rect.center() + Vec2::new(0.0, size * 0.24), Align2::CENTER_CENTER, nice_name(s(d, "label")), font_medium(12.0), fg);
            }
            _ => {
                p.text(rect.center(), Align2::CENTER_CENTER, icon::PLUS, font(size * 0.2), t.text_faint);
            }
        },
    }
    let hovering_drop = resp.dnd_hover_payload::<Drop>().is_some();
    let edge = if hovering_drop {
        Stroke::new(3.0, t.yellow)
    } else if st.selected == Some(k) {
        Stroke::new(3.0, t.accent)
    } else if resp.hovered() {
        Stroke::new(1.5, t.text_dim)
    } else {
        Stroke::new(1.0, t.border)
    };
    p.rect_stroke(rect, r, edge, StrokeKind::Outside);
    // key number: a small chip inside the corner, readable on any key picture
    let num = ui.painter().layout_no_wrap((k + 1).to_string(), font_mono(10.5), Color32::WHITE);
    let chip_r = egui::Rect::from_min_size(rect.left_top() + Vec2::new(7.0, 7.0), num.size() + Vec2::new(8.0, 2.0));
    p.rect_filled(chip_r, CornerRadius::same(4), Color32::from_black_alpha(150));
    p.galley_with_override_text_color(chip_r.min + Vec2::new(4.0, 1.0), num, Color32::from_gray(210));
    let dropped = resp.dnd_release_payload::<Drop>().map(|d| (*d).clone());
    let hover = match def {
        Some(d) if s(d, "kind") != "empty" => format!("{}\nClick to change · right-click to press it", words.command(s(d, "action"))),
        _ => "Empty. Click to give it a job, or drop a saved action or scene here.".into(),
    };
    let resp = resp.on_hover_text(hover).on_hover_cursor(egui::CursorIcon::PointingHand);
    if resp.clicked() {
        st.selected = Some(k);
        load_editor(st, k, def);
    }
    if resp.secondary_clicked() && def.is_some() {
        let args = Value::map().with("deck", g.deck.to_string()).with("key", k).with("page", g.page.to_string());
        action(app, "deck.press", args.clone());
        action(app, "deck.release", args);
    }
    dropped
}

/// Width of a [`widgets::chip`] with an icon (for [`flow`]).
fn drag_chip_width(ui: &egui::Ui, glyph: &str, label: &str) -> f32 {
    let f = font_medium(type_scale::SMALL + 0.5);
    let w = |x: &str| ui.painter().layout_no_wrap(x.to_string(), f.clone(), Color32::WHITE).size().x;
    w(glyph) + 7.0 + w(label) + 24.0 + ui.spacing().item_spacing.x
}

/// Lay out items of known widths in rows that fit the width (`horizontal_wrapped` doesn't wrap
/// child uis such as drag sources).
fn flow(ui: &mut egui::Ui, widths: &[f32], mut draw: impl FnMut(&mut egui::Ui, usize)) {
    let avail = ui.available_width();
    let gap = ui.spacing().item_spacing.x;
    let mut start = 0;
    while start < widths.len() {
        let (mut end, mut w) = (start + 1, widths[start]);
        while end < widths.len() && w + gap + widths[end] <= avail {
            w += gap + widths[end];
            end += 1;
        }
        ui.horizontal(|ui| (start..end).for_each(|i| draw(ui, i)));
        start = end;
    }
}

/// Things to drag onto a key: saved actions, scenes and show controls.
fn palette(ui: &mut egui::Ui, t: &Theme, words: &Words) {
    widgets::inspector_section(
        ui,
        t,
        "deck-palette",
        "Drag onto a key",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, t, "Drop one on a key to give it that job.");
            let presets = words.presets();
            if !presets.is_empty() {
                widgets::group_label(ui, t, "Saved actions");
                let widths: Vec<f32> = presets.iter().map(|(_, l)| drag_chip_width(ui, icon::BOLT, l)).collect();
                flow(ui, &widths, |ui, n| {
                    let (name, label) = &presets[n];
                    ui.dnd_drag_source(egui::Id::new(("deck.drag.preset", name)), Drop::Preset(name.clone()), |ui| chip(ui, t, icon::BOLT, label, false));
                });
            }
            let scenes = words.scenes();
            if !scenes.is_empty() {
                widgets::group_label(ui, t, "Scenes");
                let widths: Vec<f32> = scenes.iter().map(|(_, l)| drag_chip_width(ui, icon::LAYERS, l)).collect();
                flow(ui, &widths, |ui, n| {
                    let (name, label) = &scenes[n];
                    ui.dnd_drag_source(egui::Id::new(("deck.drag.scene", name)), Drop::Scene(name.clone()), |ui| chip(ui, t, icon::LAYERS, label, false));
                });
            }
            widgets::group_label(ui, t, "Show controls");
            let widths: Vec<f32> = DECK_ACTIONS.iter().map(|(_, l)| drag_chip_width(ui, icon::PLAY, l)).collect();
            flow(ui, &widths, |ui, n| {
                let (c, label) = DECK_ACTIONS[n];
                ui.dnd_drag_source(egui::Id::new(("deck.drag.cmd", c)), Drop::Command(c.to_string()), |ui| chip(ui, t, icon::PLAY, label, false));
            });
        },
    );
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
        _ => String::new(),
    };
    st.edit_cut = s(&d, "action").starts_with("scene.cut ");
    st.edit_do = strings(&d, "do");
    st.edit_release = strings(&d, "release");
    st.edit_label.clear();
    st.edit_icon.clear();
    st.edit_color.clear();
    st.edit_hold = if i(&d, "hold_ms") > 0 { format!("{}ms", i(&d, "hold_ms")) } else { String::new() };
    st.edit_confirm = b(&d, "confirm");
}

/// The key has everything it needs to be saved.
fn key_ready(st: &State) -> bool {
    match st.edit_kind.as_str() {
        "ptt" => true,
        "action" => !clean(&st.edit_do).is_empty() || !clean(&st.edit_release).is_empty(),
        _ => !st.edit_target.trim().is_empty(),
    }
}

/// `deck.assign` arguments for the key being edited.
fn assign_args(st: &State, base: Value) -> Value {
    let target = st.edit_target.trim().to_string();
    let mut args = match st.edit_kind.as_str() {
        "action" => {
            let mut a = base;
            for (k, cmds) in [("do", clean(&st.edit_do)), ("release", clean(&st.edit_release))] {
                if !cmds.is_empty() {
                    a = a.with(k, cmds);
                }
            }
            a
        }
        "ptt" => base.with("ptt", true),
        "scene" if st.edit_cut => base.with("scene", target).with("cut", true),
        "page" => base.with("target_page", target),
        kind => base.with(kind, target),
    };
    for (key, v) in [("label", &st.edit_label), ("icon", &st.edit_icon), ("color", &st.edit_color), ("hold", &st.edit_hold)] {
        if !v.trim().is_empty() {
            args = args.with(key, v.trim().to_string());
        }
    }
    if st.edit_confirm {
        args = args.with("confirm", true);
    }
    args
}

/// Widest a picker in an inspector row gets.
const PICK_W: f32 = 300.0;

/// A name picker over (name, label) pairs. Returns a new pick.
fn pick(ui: &mut egui::Ui, id: &str, cur: &str, opts: &[(String, String)], placeholder: &str) -> Option<String> {
    let text = if cur.is_empty() { placeholder.to_string() } else { opts.iter().find(|(n, _)| n == cur).map_or_else(|| nice(cur), |(_, l)| l.clone()) };
    let mut out = None;
    egui::ComboBox::from_id_salt(id).width(ui.available_width().min(PICK_W)).selected_text(text).show_ui(ui, |ui| {
        for (name, label) in opts {
            if ui.selectable_label(name == cur, label).clicked() {
                out = Some(name.clone());
            }
        }
    });
    out
}

/// Most settings a picker lists at once (typing narrows it down).
const SETTING_ROWS: usize = 150;

/// A setting picker: the live settings in words, searchable (only on/off ones for `switches`),
/// plus "Use …" for an address typed in the search. Returns a new pick.
fn setting_picker(app: &App, ui: &mut egui::Ui, t: &Theme, id: &str, cur: &str, switches: bool) -> Option<String> {
    let text = if cur.is_empty() { "Pick a setting".to_string() } else { setting_name(cur) };
    let search_id = egui::Id::new((id, "search"));
    let mut q: String = ui.data(|d| d.get_temp(search_id)).unwrap_or_default();
    let mut out = None;
    let r = egui::ComboBox::from_id_salt(id).width(ui.available_width().min(PICK_W)).selected_text(text).show_ui(ui, |ui| {
        ui.add(widgets::field(&mut q).hint_text("Search settings").desired_width(f32::INFINITY));
        let ql = q.trim().to_lowercase();
        let mut n = 0;
        for (addr, v) in &app.m.state {
            let fits = if switches { matches!(v, Value::Bool(_)) } else { matches!(v, Value::Bool(_) | Value::Int(_) | Value::Float(_)) };
            if !fits {
                continue;
            }
            let name = setting_name(addr);
            if !ql.is_empty() && !addr.to_lowercase().contains(&ql) && !name.to_lowercase().contains(&ql) {
                continue;
            }
            if n == SETTING_ROWS {
                widgets::hint(ui, t, "Keep typing to find more.");
                break;
            }
            n += 1;
            ui.horizontal(|ui| {
                if ui.selectable_label(addr == cur, name).clicked() {
                    out = Some(addr.clone());
                }
                ui.label(RichText::new(addr).font(font_mono(type_scale::SMALL)).color(t.text_faint));
            });
        }
        if n == 0 {
            widgets::hint(ui, t, "No setting matches.");
        }
        let typed = q.trim();
        if typed.contains('.') && !typed.contains(char::is_whitespace) && !app.m.state.contains_key(typed) && ui.button(format!("Use “{typed}”")).clicked()
        {
            out = Some(typed.to_string());
        }
    });
    if !cur.is_empty() {
        r.response.on_hover_text(cur);
    }
    ui.data_mut(|d| d.insert_temp(search_id, q));
    out
}

/// The selected key: what it does, how it looks, how safe it is.
#[allow(clippy::too_many_arguments)]
fn key_editor(app: &mut App, st: &mut State, ui: &mut egui::Ui, t: &Theme, words: &Words, g: &Grid, pages: &[Value], deck_v: &Value) {
    let Some(k) = st.edit_key else {
        widgets::empty_state(ui, t, icon::HAND, "Pick a key", "Click a key to choose what it does, or drag a saved action or scene onto it.", None);
        return;
    };
    let (mut save, mut clear, mut press) = (false, false, false);
    let ready = key_ready(st);
    sub_header(ui, t, &format!("Key {}", k + 1), &format!("On the “{}” page", page_label(deck_v, g.page)), |ui| {
        save = widgets::button_ex(ui, t, Some(icon::CHECK), "Save key", Kind::Primary, Size::Medium, 0.0, ready).clicked();
        press = widgets::button_ex(ui, t, Some(icon::PLAY), "Try it", Kind::Secondary, Size::Medium, 0.0, true)
            .on_hover_text("Runs what's saved, as if you pressed the key")
            .clicked();
    });
    widgets::inspector_section(
        ui,
        t,
        "deck-key-does",
        "What it does",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Does", |ui| {
                let cur = KEY_KINDS.iter().find(|(kind, _)| *kind == st.edit_kind).map_or("Pick one", |(_, l)| *l);
                egui::ComboBox::from_id_salt("deck-key-kind").width(ui.available_width().min(PICK_W)).selected_text(cur).show_ui(ui, |ui| {
                    for (kind, label) in KEY_KINDS {
                        if ui.selectable_label(st.edit_kind == kind, label).clicked() && st.edit_kind != kind {
                            st.edit_kind = kind.to_string();
                            st.edit_target.clear();
                        }
                    }
                });
            });
            match st.edit_kind.as_str() {
                "preset" => {
                    widgets::prop_row(ui, t, "Saved action", |ui| {
                        if let Some(p) = pick(ui, "deck-key-preset", &st.edit_target, words.presets(), "Pick a saved action") {
                            st.edit_target = p;
                        }
                    });
                    if words.presets().is_empty() {
                        widgets::hint(ui, t, "No saved actions yet. Make one under Automation → Actions.");
                    }
                }
                "scene" => {
                    widgets::prop_row(ui, t, "Scene", |ui| {
                        if let Some(p) = pick(ui, "deck-key-scene", &st.edit_target, words.scenes(), "Pick a scene") {
                            st.edit_target = p;
                        }
                    });
                    widgets::prop_row(ui, t, "Straight to air", |ui| {
                        widgets::toggle(ui, t, &mut st.edit_cut);
                        widgets::hint(ui, t, if st.edit_cut { "Pressing it switches the scene on air." } else { "Pressing it puts the scene up next." });
                    });
                }
                "toggle" | "momentary" => {
                    let switches = st.edit_kind == "toggle";
                    widgets::prop_row(ui, t, "Setting", |ui| {
                        if let Some(p) = setting_picker(app, ui, t, "deck-key-setting", &st.edit_target, switches) {
                            st.edit_target = p;
                        }
                    });
                    if let Some(sel) = app.selected.clone()
                        && sel != st.edit_target
                        && widgets::button_ex(ui, t, None, &format!("Use {}", setting_words(&sel)), Kind::Ghost, Size::Small, 0.0, true).clicked()
                    {
                        st.edit_target = sel;
                    }
                    widgets::hint(ui, t, if switches { "Each press switches it on or off." } else { "On (or at its highest) while you hold the key." });
                }
                "action" => {
                    ui.add_space(spacing::XS);
                    ui.label(RichText::new("When pressed").font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
                    steps_editor(app, ui, "deck-key-do", &mut st.edit_do, "deck.key");
                    ui.add_space(spacing::S);
                    ui.label(RichText::new("When let go").font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
                    steps_editor(app, ui, "deck-key-release", &mut st.edit_release, "deck.key");
                }
                "page" => {
                    let mut opts: Vec<(String, String)> = pages.iter().map(|p| (s(p, "page").to_string(), page_label(deck_v, s(p, "page")))).collect();
                    opts.push(("next".into(), "Next page".into()));
                    opts.push(("prev".into(), "Previous page".into()));
                    widgets::prop_row(ui, t, "Page", |ui| {
                        if let Some(p) = pick(ui, "deck-key-page", &st.edit_target, &opts, "Pick a page") {
                            st.edit_target = p;
                        }
                    });
                }
                _ => {
                    widgets::hint(ui, t, "Hold the key and say a command, like “switch to wide”.");
                }
            }
        },
    );
    widgets::inspector_section(
        ui,
        t,
        "deck-key-look",
        "Look",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Label", |ui| {
                ui.add(widgets::field(&mut st.edit_label).hint_text("From its name").desired_width(ui.available_width().min(260.0)));
            });
            widgets::prop_row(ui, t, "Icon", |ui| {
                ui.add(widgets::field(&mut st.edit_icon).hint_text("fire, mic, scene…").desired_width(ui.available_width().min(260.0)));
            });
            widgets::prop_row(ui, t, "Color", |ui| color_swatches(ui, t, &mut st.edit_color));
        },
    );
    widgets::inspector_section(
        ui,
        t,
        "deck-key-safety",
        "Safety",
        false,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Hold for", |ui| {
                ui.add(widgets::field(&mut st.edit_hold).hint_text("Just tap").desired_width(90.0));
                widgets::hint(ui, t, "Like 1s, for keys that need a firm press.");
            });
            widgets::prop_row(ui, t, "Press twice", |ui| {
                widgets::toggle(ui, t, &mut st.edit_confirm);
                widgets::hint(ui, t, "For keys you don't want to hit by accident.");
            });
        },
    );
    widgets::details(ui, t, "deck-key-details", "Details", |ui| {
        widgets::prop_row(ui, t, "Color code", |ui| {
            ui.add(widgets::field(&mut st.edit_color).hint_text("#rrggbb").font(font_mono(type_scale::BODY - 1.0)).desired_width(110.0));
        });
        widgets::fact(ui, t, "Key number", &k.to_string());
        widgets::fact(ui, t, "Page id", g.page);
    });
    ui.add_space(spacing::S);
    ui.horizontal(|ui| {
        clear = widgets::hold_button(ui, t, "Clear key", t.bright_red, 0.6);
        widgets::hint(ui, t, "Press and hold to empty this key.");
    });
    let base = || Value::map().with("deck", g.deck.to_string()).with("page", g.page.to_string()).with("key", k);
    if save {
        action(app, "deck.assign", assign_args(st, base()));
        app.m.toast(format!("Key {} saved.", k + 1), false);
    }
    if clear {
        action(app, "deck.assign", base().with("clear", true));
    }
    if press {
        let args = Value::map().with("deck", g.deck.to_string()).with("key", k).with("page", g.page.to_string());
        action(app, "deck.press", args.clone());
        action(app, "deck.release", args);
    }
}

/// The key color choices: default, then the theme colors.
fn color_swatches(ui: &mut egui::Ui, t: &Theme, color: &mut String) {
    let (r, resp) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
    ui.painter().circle_stroke(r.center(), 10.0, Stroke::new(if color.is_empty() { 2.5 } else { 1.0 }, if color.is_empty() { t.fg } else { t.border }));
    ui.painter().line_segment([r.center() + Vec2::new(-6.0, 6.0), r.center() + Vec2::new(6.0, -6.0)], Stroke::new(1.0, t.text_dim));
    if resp.on_hover_text("Default color").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
        color.clear();
    }
    for name in KEY_COLORS {
        let c = key_color(t, name).unwrap_or(t.accent);
        let (r, resp) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
        ui.painter().circle_filled(r.center(), 10.0, c);
        if color == name {
            ui.painter().circle_stroke(r.center(), 12.0, Stroke::new(2.0, t.fg));
        }
        if resp.on_hover_text(nice(name)).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
            *color = name.to_string();
        }
    }
}

// ---- MIDI ----------------------------------------------------------------------------------------

fn midi_editor(app: &mut App, st: &mut State, ui: &mut egui::Ui, t: &Theme, d: &Value) {
    let id = s(d, "id").to_string();
    let name = device_name(d, &id);
    let sub = if b(d, "online") {
        "Working".to_string()
    } else if s(d, "error").is_empty() {
        "Not connected. Plug it in and it connects by itself.".into()
    } else {
        format!("Not connected: {}", s(d, "error"))
    };
    let count = i(d, "bank_count");
    let mut bank: Option<i64> = None;
    widgets::detail_header(ui, t, midi_glyph(d), &name, &sub, |ui| {
        if count > 0 {
            if widgets::icon_button(ui, t, icon::RIGHT, "Next 8 faders").clicked() {
                bank = Some(8);
            }
            ui.label(RichText::new(format!("Faders {}–{} of {count}", i(d, "bank") + 1, (i(d, "bank") + 8).min(count))).color(t.text_dim));
            if widgets::icon_button(ui, t, icon::LEFT, "Previous 8 faders").clicked() {
                bank = Some(-8);
            }
        }
    });
    widgets::segmented(ui, t, &mut st.midi_view, &MIDI_VIEWS);
    ui.add_space(spacing::M);
    match st.midi_view {
        0 => {
            learn_box(app, st, ui, t, &name);
            mappings(app, ui, t, d);
        }
        1 => live_controls(ui, t, d),
        _ => message_log(app, ui, t),
    }
    ui.add_space(spacing::M);
    widgets::details(ui, t, ("midi-details", id.as_str()), "Details", |ui| {
        widgets::fact(ui, t, "Device id", &id);
        widgets::fact(ui, t, "Profile", s(d, "profile"));
        widgets::fact(ui, t, "USB port", d.get_path("usb").and_then(Value::as_str).unwrap_or("—"));
        widgets::fact(ui, t, "Messages", &format!("{} in · {} out", i(d, "msgs_in"), i(d, "msgs_out")));
        if b(d, "configured") {
            widgets::fact(ui, t, "Saved in", s(d, "file"));
        } else {
            widgets::fact(ui, t, "Saved in", "not set up (found automatically)");
        }
    });
    if let Some(delta) = bank {
        action(app, "midi.bank", Value::map().with("device", id).with("delta", delta));
    }
}

/// "It should…" → the learn target (`preset.x`, `scene.x`, an address, `do:<lines>`).
fn learn_target(st: &State) -> String {
    match st.learn_kind {
        0 if !st.learn_pick.is_empty() => format!("preset.{}", st.learn_pick),
        1 if !st.learn_pick.is_empty() => format!("scene.{}", st.learn_pick),
        2 => st.learn_pick.trim().to_string(),
        3 => {
            let cmds = clean(&st.learn_cmds);
            if cmds.is_empty() { String::new() } else { format!("do:{}", cmds.join("\n")) }
        }
        _ => String::new(),
    }
}

fn target_words(words: &Words, target: &str) -> String {
    if let Some(p) = target.strip_prefix("preset.") {
        format!("Run {}", words.preset(p))
    } else if let Some(sc) = target.strip_prefix("scene.") {
        format!("Put {} up next", words.scene(sc))
    } else if let Some(c) = target.strip_prefix("do:") {
        words.command(&c.replace('\n', "; "))
    } else {
        setting_name(target)
    }
}

fn learn_box(app: &mut App, st: &mut State, ui: &mut egui::Ui, t: &Theme, device: &str) {
    let words = Words::new(app);
    if app.m.b("controllers.learn.active") {
        let body =
            format!("Turn a knob, push a fader, press a button or stomp a pedal. It will: {}.", target_words(&words, app.m.str("controllers.learn.target")));
        if widgets::callout(ui, t, widgets::Tone::Warn, icon::HAND, &format!("Now move a control on your {device}"), &body, Some("Cancel")) {
            action(app, "midi.learn.cancel", Value::Null);
        }
        ui.add_space(spacing::M);
        return;
    }
    let mut learn = None;
    widgets::inspector_section(
        ui,
        t,
        "midi-learn",
        "Give a control a job",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, t, "Pick what it should do, press Learn, then move the knob, fader, button or pedal.");
            ui.add_space(spacing::S);
            widgets::prop_row(ui, t, "It should", |ui| {
                egui::ComboBox::from_id_salt("midi-learn-kind").width(ui.available_width().min(PICK_W)).selected_text(LEARN_KINDS[st.learn_kind]).show_ui(
                    ui,
                    |ui| {
                        for (n, label) in LEARN_KINDS.iter().enumerate() {
                            if ui.selectable_label(st.learn_kind == n, *label).clicked() && st.learn_kind != n {
                                st.learn_kind = n;
                                st.learn_pick.clear();
                                st.learn_cmds.clear();
                            }
                        }
                    },
                );
            });
            match st.learn_kind {
                0 => widgets::prop_row(ui, t, "Saved action", |ui| {
                    if let Some(p) = pick(ui, "learn-preset", &st.learn_pick, words.presets(), "Pick a saved action") {
                        st.learn_pick = p;
                    }
                }),
                1 => widgets::prop_row(ui, t, "Scene", |ui| {
                    if let Some(p) = pick(ui, "learn-scene", &st.learn_pick, words.scenes(), "Pick a scene") {
                        st.learn_pick = p;
                    }
                }),
                2 => {
                    widgets::prop_row(ui, t, "Setting", |ui| {
                        if let Some(p) = setting_picker(app, ui, t, "learn-setting", &st.learn_pick, false) {
                            st.learn_pick = p;
                        }
                    });
                    if let Some(sel) = app.selected.clone()
                        && sel != st.learn_pick
                        && widgets::button_ex(ui, t, None, &format!("Use {}", setting_words(&sel)), Kind::Ghost, Size::Small, 0.0, true).clicked()
                    {
                        st.learn_pick = sel;
                    }
                    widgets::hint(ui, t, "A fader or knob follows along; a button switches it on and off.");
                }
                _ => {
                    steps_editor(app, ui, "midi-learn-do", &mut st.learn_cmds, "");
                    widgets::hint(ui, t, "Learn these on a button or footswitch.");
                }
            }
            ui.add_space(spacing::S);
            let target = learn_target(st);
            if widgets::button_ex(ui, t, Some(icon::HAND), "Learn: move a control", Kind::Primary, Size::Large, 0.0, !target.is_empty()).clicked() {
                learn = Some(target);
            }
            if let Some(e) = app.m.events.iter().rev().find(|e| e.ty == "midi.learned") {
                let control = e.payload.get_path("control").and_then(Value::as_str).unwrap_or("");
                let target = e.payload.get_path("target").and_then(Value::as_str).unwrap_or("");
                ui.add_space(spacing::XS);
                widgets::hint(ui, t, &format!("Last learned: {} → {}", control_name(control), target_words(&words, target)));
            }
        },
    );
    if let Some(target) = learn {
        action(app, "midi.learn", Value::map().with("target", target));
    }
}

fn mapping_row(ui: &mut egui::Ui, t: &Theme, n: usize, control: &str, does: &str, tip: &str, lights_up: bool) -> bool {
    let mut remove = false;
    egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(radius::CONTROL)).inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.allocate_ui_with_layout(Vec2::new(140.0, 24.0), Layout::left_to_right(Align::Center), |ui| {
                ui.set_min_width(140.0);
                ui.label(RichText::new(control_name(control)).font(font_semibold(type_scale::BODY)).color(t.fg));
            });
            ui.label(RichText::new(icon::RIGHT).size(type_scale::SMALL).color(t.text_faint));
            ui.label(RichText::new(does).color(t.fg)).on_hover_text(tip);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.push_id(("unmap", n), |ui| {
                    ui.menu_button(icon::TRASH, |ui| {
                        ui.label(RichText::new(format!("Take the job away from {}?", control_name(control))).color(t.fg));
                        ui.add_space(spacing::XS);
                        if widgets::button_ex(ui, t, Some(icon::TRASH), "Remove", Kind::Danger, Size::Small, 0.0, true).clicked() {
                            remove = true;
                            ui.close();
                        }
                    })
                    .response
                    .on_hover_text("Remove this job");
                });
                if lights_up {
                    widgets::badge(ui, t, "Lights up", t.green).on_hover_text("The control's light or position follows along");
                }
            });
        });
    });
    remove
}

fn mappings(app: &mut App, ui: &mut egui::Ui, t: &Theme, d: &Value) {
    let id = s(d, "id").to_string();
    let words = Words::new(app);
    let maps = d.get_path("maps").and_then(Value::as_list).unwrap_or(&[]).to_vec();
    let prefix = format!("midi.{id}.");
    let bindings: Vec<Value> =
        app.m.q("config.bindings").and_then(Value::as_list).unwrap_or(&[]).iter().filter(|b| s(b, "signal").starts_with(&prefix)).cloned().collect();
    let mut unmap: Option<String> = None;
    widgets::inspector_section(
        ui,
        t,
        ("midi-maps", id.as_str()),
        &format!("What each control does ({})", maps.len() + bindings.len()),
        true,
        |_| {},
        |ui| {
            if maps.is_empty() && bindings.is_empty() {
                widgets::hint(ui, t, "No jobs yet. Use Learn above and move a control to give it one.");
                return;
            }
            let mut n = 0;
            for m in &maps {
                let control = s(m, "control");
                if mapping_row(ui, t, n, control, &words.command(s(m, "action")), s(m, "action"), b(m, "feedback")) {
                    unmap = Some(control.to_string());
                }
                n += 1;
                ui.add_space(spacing::XS);
            }
            for bnd in &bindings {
                let control = s(bnd, "signal").trim_start_matches(&prefix).to_string();
                let smooth = if bnd.get_path("takeover").and_then(Value::as_str) == Some("pickup") { " (picks up where it is)" } else { "" };
                let does = format!("Sets {}{smooth}", setting_words(s(bnd, "target")));
                let tip = format!("{} → {} · curve {}", s(bnd, "signal"), s(bnd, "target"), s(bnd, "curve"));
                if mapping_row(ui, t, n, &control, &does, &tip, b(bnd, "feedback")) {
                    unmap = Some(control);
                }
                n += 1;
                ui.add_space(spacing::XS);
            }
        },
    );
    if let Some(control) = unmap {
        action(app, "midi.unmap", Value::map().with("device", id).with("control", control));
    }
}

fn live_controls(ui: &mut egui::Ui, t: &Theme, d: &Value) {
    let controls = d.get_path("controls").and_then(Value::as_list).unwrap_or(&[]);
    if controls.is_empty() {
        widgets::empty_state(ui, t, icon::SLIDERS, "No controls listed", "This device hasn't told Stream Engine about any knobs or buttons yet.", None);
        return;
    }
    widgets::hint(ui, t, "Move something on the device and watch it here.");
    ui.add_space(spacing::S);
    let widths = vec![172.0; controls.len()];
    flow(ui, &widths, |ui, n| {
        let c = &controls[n];
        let name = s(c, "name");
        let v = c.get_path("value").and_then(Value::as_f64).unwrap_or(0.0) as f32;
        let touched = b(c, "touched") || (v > 0.0 && b(c, "button"));
        let stroke = if touched { Stroke::new(1.5, t.accent) } else { Stroke::new(1.0, t.border) };
        egui::Frame::new().fill(t.surface_hi).stroke(stroke).corner_radius(CornerRadius::same(radius::CONTROL)).inner_margin(egui::Margin::same(10)).show(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    ui.set_width(150.0);
                    ui.label(RichText::new(control_name(name)).font(font_medium(type_scale::BODY)).color(t.fg));
                    widgets::meter_h(ui, t, Vec2::new(150.0, 6.0), v.clamp(0.0, 1.0));
                    let kind = if b(c, "button") {
                        "button"
                    } else if b(c, "relative") {
                        "knob"
                    } else {
                        "slider"
                    };
                    ui.label(RichText::new(format!("{kind} · {v:.2}")).size(type_scale::SMALL).color(t.text_dim)).on_hover_text(format!(
                        "midi.{}.{name} · {}",
                        s(d, "id"),
                        s(c, "kind")
                    ));
                });
            },
        );
    });
}

fn message_log(app: &mut App, ui: &mut egui::Ui, t: &Theme) {
    let rows = app.m.q_list("controllers.midi.monitor");
    widgets::hint(ui, t, "Every message the device sends and receives, newest first. Useful when a control doesn't do what you expect.");
    ui.add_space(spacing::S);
    if rows.is_empty() {
        widgets::empty_state(ui, t, icon::LIST, "Quiet so far", "Move a control on the device and its messages appear here.", None);
        return;
    }
    for e in rows.iter().take(120) {
        ui.horizontal(|ui| {
            ui.add_sized([80.0, 18.0], egui::Label::new(RichText::new(i(e, "ts_ms").to_string()).font(font_mono(type_scale::SMALL)).color(t.text_faint)));
            if s(e, "dir") == "in" {
                widgets::badge(ui, t, "in", t.green);
            } else {
                widgets::badge(ui, t, "out", t.blue);
            }
            ui.add_sized([160.0, 18.0], egui::Label::new(RichText::new(s(e, "hex")).font(font_mono(type_scale::SMALL)).color(t.fg)));
            ui.label(RichText::new(s(e, "desc")).color(t.text_dim));
        });
    }
}

// ---- voice ---------------------------------------------------------------------------------------

fn voice_editor(app: &mut App, st: &mut State, ui: &mut egui::Ui, t: &Theme) {
    let v = app.m.q("controllers.voice").cloned().unwrap_or_default();
    let state = app.m.str("controllers.voice.state").to_string();
    let (words, _) = voice_words(t, &state);
    widgets::detail_header(ui, t, icon::MIC, "Voice control", words, |_| {});
    let w = ui.available_width().min(760.0);
    widgets::hint(ui, t, "Hold the button (or a key or pedal set to push to talk) and say something like “switch to wide”.");
    ui.add_space(spacing::M);
    if let Some(e) = v.get_path("error").and_then(Value::as_str) {
        widgets::callout(ui, t, widgets::Tone::Danger, icon::WARN, "Voice control needs a look", e, None);
        ui.add_space(spacing::S);
    }
    if let Some(p) = v.get_path("download").and_then(Value::as_f64) {
        ui.add(egui::ProgressBar::new(p as f32).desired_width(w).text("Downloading the voice model…"));
        ui.add_space(spacing::S);
    }
    // hold to talk
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 56.0), Sense::click_and_drag());
    let down = resp.is_pointer_button_down_on();
    let fill = if down {
        t.bright_red
    } else if resp.hovered() {
        mix(t.surface_hi, t.fg, 0.06)
    } else {
        t.surface_hi
    };
    ui.painter().rect(rect, CornerRadius::same(radius::CARD), fill, Stroke::new(1.0, t.border), StrokeKind::Inside);
    let label = if down { format!("{}  Listening… let go when you're done", icon::MIC) } else { format!("{}  Hold to talk", icon::MIC) };
    ui.painter().text(rect.center(), Align2::CENTER_CENTER, label, font_semibold(type_scale::LARGE), if down { widgets::contrast(fill) } else { t.fg });
    if down != st.ptt_down {
        st.ptt_down = down;
        action(app, "voice.ptt", Value::map().with("args", vec![Value::from(if down { "start" } else { "stop" })]));
    }
    let pending = app.m.str("controllers.voice.pending").to_string();
    if !pending.is_empty() {
        ui.add_space(spacing::M);
        if widgets::callout(
            ui,
            t,
            widgets::Tone::Info,
            icon::MIC,
            &format!("Did you mean “{pending}”?"),
            "Say yes to do it, or no to forget it.",
            Some("Yes, do it"),
        ) {
            action(app, "voice.confirm", Value::Null);
        }
        ui.add_space(spacing::XS);
        if widgets::button_ex(ui, t, Some(icon::CROSS), "No, forget it", Kind::Secondary, Size::Small, 0.0, true).clicked() {
            action(app, "voice.cancel", Value::Null);
        }
    }
    ui.add_space(spacing::L);
    let history = v.get_path("history").and_then(Value::as_list).unwrap_or(&[]).to_vec();
    widgets::inspector_section(
        ui,
        t,
        "voice-history",
        "Lately",
        true,
        |_| {},
        |ui| {
            if history.is_empty() {
                widgets::hint(ui, t, "Nothing said yet.");
            }
            for h in history.iter().take(5) {
                let done = b(h, "executed");
                let intent = h.get_path("intent").and_then(Value::as_str).unwrap_or("");
                let sub = if done {
                    format!("Done: {intent}")
                } else if intent.is_empty() {
                    "Didn't understand that".into()
                } else {
                    format!("Not done: {intent}")
                };
                widgets::list_row(ui, t, if done { icon::CHECK } else { icon::CROSS }, &format!("“{}”", s(h, "text")), &sub, "", false);
            }
        },
    );
    widgets::details(ui, t, "voice-details", "Details", |ui| {
        widgets::fact(ui, t, "State", if state.is_empty() { "—" } else { &state });
        widgets::fact(ui, t, "Speech model", s(&v, "model"));
        widgets::fact(ui, t, "Microphone", s(&v, "device"));
        widgets::fact(ui, t, "Model file", s(&v, "model_path"));
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn learn_targets_follow_the_chosen_kind() {
        let mut st = State { learn_kind: 0, learn_pick: "hype".into(), ..Default::default() };
        assert_eq!(learn_target(&st), "preset.hype");
        st.learn_kind = 1;
        st.learn_pick = "duo".into();
        assert_eq!(learn_target(&st), "scene.duo");
        st.learn_kind = 2;
        st.learn_pick = " fx.vhs.amount ".into();
        assert_eq!(learn_target(&st), "fx.vhs.amount");
        st.learn_kind = 3;
        st.learn_cmds = vec!["scene.take".into(), "  ".into(), " preset.fire hype".into()];
        assert_eq!(learn_target(&st), "do:scene.take\npreset.fire hype", "one line per step, blank steps dropped");
        st.learn_cmds.clear();
        assert_eq!(learn_target(&st), "", "nothing picked: Learn stays disabled");
    }

    #[test]
    fn key_assignment_carries_press_and_release_lists() {
        let base = || Value::map().with("key", 3);
        let mut st = State { edit_kind: "action".into(), edit_do: vec!["preset.fire hype".into(), " ".into()], ..Default::default() };
        let a = assign_args(&st, base());
        assert_eq!(strings(&a, "do"), ["preset.fire hype"]);
        assert!(a.get_path("release").is_none(), "no release list unless there are steps");
        st.edit_release = vec!["preset.release hype".into()];
        assert!(key_ready(&st));
        assert_eq!(strings(&assign_args(&st, base()), "release"), ["preset.release hype"]);
        st.edit_do.clear();
        st.edit_release.clear();
        assert!(!key_ready(&st), "a key with no steps can't be saved");
        let st = State { edit_kind: "scene".into(), edit_target: "duo".into(), edit_cut: true, ..Default::default() };
        let a = assign_args(&st, base());
        assert_eq!((s(&a, "scene"), b(&a, "cut")), ("duo", true));
        let st = State { edit_kind: "scene".into(), edit_target: "duo".into(), ..Default::default() };
        assert!(assign_args(&st, base()).get_path("cut").is_none(), "up next is the default, not written");
    }
}
