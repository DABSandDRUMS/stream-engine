//! Buttons & pedals (§15.6, §10): every controller as a card with its connection state (Stream
//! Deck, MIDI devices like the X-TOUCH or the FBV pedal, voice). The Stream Deck page editor is a
//! grid of keys you click (or drop effects and scenes onto) to choose what they do; MIDI devices
//! list "this control → that" with a big Learn button; voice has push-to-talk and its history.
//! Live control values, the raw MIDI message log and file names sit behind a sub-view or
//! "Details". Data: `controllers.*` queries and state (se-input); edits go through
//! `deck.assign|page|press|release|refresh`, `midi.learn|learn.cancel|unmap|bank`, `voice.*`.

use crate::app::App;
use crate::views::live::nice;
use crate::views::rules::{Words, nice_name, setting_name};
use base64::Engine;
use egui::{Align, Align2, Color32, CornerRadius, Layout, RichText, Sense, Stroke, StrokeKind, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_medium, font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Instant;

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
    ("preset", "Quick effect"),
    ("scene", "Scene"),
    ("page", "Deck page"),
    ("ptt", "Push to talk"),
    ("toggle", "Switch a setting"),
    ("momentary", "Hold a setting"),
    ("action", "Commands"),
];

/// Deck actions offered for dragging: (command, words).
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

const LEARN_KINDS: [&str; 4] = ["Quick effect", "Scene", "A setting", "A command"];
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
    learn_pick: String,
    learn_text: String,
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

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let st = state(ui);
    let mut st = st.lock().unwrap_or_else(|p| p.into_inner());
    let decks = app.m.q_list("controllers.deck").to_vec();
    let midis = app.m.q_list("controllers.midi").to_vec();
    let dev = resolve(&st.device, &decks, &midis);
    poll(app, &mut st, &dev);
    let t = app.t.clone();
    egui::ScrollArea::vertical().id_salt("controllers").auto_shrink([false, false]).show(ui, |ui| {
        // wide screens: one centered column
        let avail = ui.available_width();
        let w = avail.min(2400.0);
        ui.horizontal_top(|ui| {
            ui.add_space(((avail - w) / 2.0 - ui.spacing().item_spacing.x).max(0.0));
            ui.allocate_ui_with_layout(Vec2::new(w, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(w);
                body(app, &mut st, ui, &t, &decks, &midis, &dev);
            });
        });
        ui.add_space(spacing::XL);
    });
}

#[allow(clippy::too_many_arguments)]
fn body(app: &mut App, st: &mut State, ui: &mut egui::Ui, t: &Theme, decks: &[Value], midis: &[Value], dev: &Device) {
    device_cards(app, st, ui, t, decks, midis, dev);
    ui.add_space(spacing::L);
    match dev {
        Device::Deck(id) => {
            if let Some(d) = decks.iter().find(|d| s(d, "id") == id) {
                deck_editor(app, st, ui, t, d);
            }
        }
        Device::Midi(id) => {
            if let Some(d) = midis.iter().find(|d| s(d, "id") == id) {
                midi_editor(app, st, ui, t, d);
            }
        }
        Device::Voice => voice_editor(app, st, ui, t),
        Device::Auto => {}
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

// ---- device cards --------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn device_tile(
    ui: &mut egui::Ui,
    t: &Theme,
    w: f32,
    salt: &str,
    glyph: &str,
    name: &str,
    status: (&str, Color32),
    detail: &str,
    selected: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 108.0), Sense::click());
    let fill = if selected {
        mix(t.surface, t.accent, 0.12)
    } else if resp.hovered() {
        t.surface_hi
    } else {
        t.surface
    };
    let stroke = if selected { Stroke::new(2.0, t.accent) } else { Stroke::new(1.0, t.border) };
    ui.painter().rect(rect, CornerRadius::same(radius::CARD), fill, stroke, StrokeKind::Inside);
    // content in a child ui: it doesn't move the tile row's layout cursor
    let mut c = ui.new_child(egui::UiBuilder::new().max_rect(rect.shrink(14.0)).layout(Layout::top_down(Align::Min)).id_salt(("device-tile", salt)));
    c.spacing_mut().item_spacing.y = spacing::XS;
    c.horizontal(|ui| {
        ui.label(RichText::new(glyph).size(20.0).color(if selected { t.accent } else { t.text_dim }));
        ui.add(egui::Label::new(RichText::new(name).font(font_semibold(type_scale::BODY + 1.0)).color(t.fg)).truncate());
    });
    widgets::badge(&mut c, t, status.0, status.1);
    c.add(egui::Label::new(RichText::new(detail).size(type_scale::SMALL + 0.5).color(t.text_dim)).truncate());
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

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

#[allow(clippy::too_many_arguments)]
fn device_cards(app: &mut App, st: &mut State, ui: &mut egui::Ui, t: &Theme, decks: &[Value], midis: &[Value], dev: &Device) {
    let n = decks.len() + midis.len() + 1;
    let gap = spacing::M;
    let avail = ui.available_width();
    let per_row = (((avail + gap) / (236.0 + gap)).floor() as usize).clamp(1, n);
    let tw = ((avail - gap * (per_row - 1) as f32) / per_row as f32 - 0.5).floor();
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(gap);
        for d in decks {
            let id = s(d, "id");
            let (status, c) = if b(d, "connected") { ("Working", t.green) } else { ("Not connected", t.bright_red) };
            let detail = if b(d, "connected") {
                format!("{} keys · showing “{}”", i(d, "keys").max(1), page_label(d, s(d, "page")))
            } else {
                "Plug it in and it connects by itself".into()
            };
            let sel = *dev == Device::Deck(id.to_string());
            if device_tile(ui, t, tw, id, icon::GRID, &device_name(d, "Stream Deck"), (status, c), &detail, sel).clicked() && !sel {
                st.device = Device::Deck(id.to_string());
                st.page = None;
                st.edit_key = None;
                st.selected = None;
                st.preview_version = -1;
                st.textures.clear();
            }
        }
        for d in midis {
            let id = s(d, "id");
            let (status, c) = if b(d, "online") {
                ("Working", t.green)
            } else if b(d, "configured") {
                ("Not connected", t.bright_red)
            } else {
                ("Off", t.text_dim)
            };
            let maps = d.get_path("maps").and_then(Value::as_list).map_or(0, <[Value]>::len);
            let controls = d.get_path("controls").and_then(Value::as_list).map_or(0, <[Value]>::len);
            let detail = match (controls, maps) {
                (0, 0) => "No controls set up yet".to_string(),
                (c, 0) => format!("{c} controls"),
                (c, m) => format!("{c} controls · {m} with a job"),
            };
            let glyph = if s(d, "client").to_lowercase().contains("fbv") || id.contains("fbv") { icon::HAND } else { icon::SLIDERS };
            let sel = *dev == Device::Midi(id.to_string());
            if device_tile(ui, t, tw, id, glyph, &device_name(d, id), (status, c), &detail, sel).clicked() && !sel {
                st.device = Device::Midi(id.to_string());
            }
        }
        let vstate = app.m.str("controllers.voice.state").to_string();
        let (words, col) = voice_words(t, &vstate);
        let sel = *dev == Device::Voice;
        if device_tile(ui, t, tw, "voice", icon::MIC, "Voice control", (words, col), "Hold to talk to Stream Engine", sel).clicked() && !sel {
            st.device = Device::Voice;
        }
    });
    if decks.is_empty() && midis.is_empty() && app.m.connected {
        ui.add_space(spacing::S);
        widgets::hint(ui, t, "No Stream Deck, pedal or controller is set up yet. Add one under Settings → Devices, then it shows up here.");
    }
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
    widgets::titled(
        ui,
        t,
        "Stream Deck keys",
        "Click a key to choose what it does, or drag an effect or scene onto it.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if pages.is_empty() {
                widgets::empty_state(ui, t, icon::GRID, "No pages yet", "Your Stream Deck's pages show up here once it's set up.", None);
                return;
            }
            // pages + deck settings
            ui.horizontal(|ui| {
                ui.label(RichText::new("Page").color(t.text_dim));
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
                    widgets::hint(ui, t, "This page is on the deck now.");
                } else if widgets::button_ex(ui, t, Some(icon::PLAY), "Show this page on the deck", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    action(app, "deck.page", Value::map().with("deck", deck_id.clone()).with("args", vec![Value::from(page_name.as_str())]));
                    st.page = None;
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::icon_button(ui, t, icon::UNDO, "Send all key pictures to the deck again").clicked() {
                        action(app, "deck.refresh", Value::Null);
                    }
                    let addr = format!("controllers.{deck_id}.brightness");
                    let mut bright =
                        app.m.get(&addr).and_then(Value::as_f64).unwrap_or_else(|| deck.get_path("brightness").and_then(Value::as_f64).unwrap_or(70.0)) as f32;
                    ui.spacing_mut().slider_width = 140.0;
                    if ui.add(egui::Slider::new(&mut bright, 0.0..=100.0).show_value(false)).on_hover_text(format!("{bright:.0}%")).changed() {
                        app.m.command(Op::Set { address: addr, value: Value::Int(bright as i64) });
                    }
                    ui.label(RichText::new("\u{f185}  Brightness").color(t.text_dim));
                });
            });
            ui.add_space(spacing::M);
            let Some(page) = pages.iter().find(|p| s(p, "page") == page_name).cloned() else { return };
            let keys: HashMap<i64, Value> = page.get_path("keys").and_then(Value::as_list).unwrap_or(&[]).iter().map(|k| (i(k, "key"), k.clone())).collect();
            let nkeys = i(deck, "keys").max(1);
            let cols = i(deck, "cols").max(1);
            let live = page_name == st.preview_page;
            let w = ui.available_width();
            let gap = 10.0;
            // wide: keys | key settings | drag palette; else keys + palette | key settings
            let wide = w >= 1900.0;
            let grid_budget = if wide { w * 0.36 } else { w - 420.0 - spacing::XL };
            let size = ((grid_budget - gap * (cols - 1) as f32) / cols as f32).clamp(64.0, if wide { 160.0 } else { 132.0 });
            let left = size * cols as f32 + gap * (cols - 1) as f32;
            let rest = w - left - spacing::XL;
            let (right, third) = if wide { ((rest * 0.5 - spacing::XL / 2.0).min(760.0), 0.0) } else { (rest, 0.0) };
            let third = if wide { rest - right - spacing::XL } else { third };
            let mut dropped: Option<(i64, Drop)> = None;
            ui.horizontal_top(|ui| {
                ui.allocate_ui_with_layout(Vec2::new(left, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.spacing_mut().item_spacing = Vec2::splat(gap);
                    for row in 0..(nkeys as usize).div_ceil(cols as usize) as i64 {
                        ui.horizontal(|ui| {
                            for c in 0..cols {
                                let k = row * cols + c;
                                if k >= nkeys {
                                    break;
                                }
                                if let Some(d) = deck_key(app, st, ui, t, &words, k, keys.get(&k), live, size, &deck_id, &page_name) {
                                    dropped = Some((k, d));
                                }
                            }
                        });
                    }
                    ui.spacing_mut().item_spacing = Vec2::splat(spacing::S);
                    if !live {
                        widgets::hint(ui, t, "This page isn't on the deck right now, so keys show a sketch instead of their real pictures.");
                    }
                    if !wide {
                        ui.add_space(spacing::M);
                        palette(ui, t, &words);
                    }
                });
                ui.add_space(spacing::XL - ui.spacing().item_spacing.x);
                ui.allocate_ui_with_layout(Vec2::new(right, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(right);
                    key_editor(app, st, ui, t, &words, &deck_id, &page_name, &pages, deck);
                });
                if wide {
                    ui.add_space(spacing::XL - ui.spacing().item_spacing.x);
                    ui.allocate_ui_with_layout(Vec2::new(third, 0.0), Layout::top_down(Align::Min), |ui| {
                        ui.set_width(third);
                        palette(ui, t, &words);
                    });
                }
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
                load_editor(st, k, keys.get(&k));
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
        },
    );
}

/// One key of the grid. Returns something dropped on it.
#[allow(clippy::too_many_arguments)]
fn deck_key(
    app: &mut App,
    st: &mut State,
    ui: &mut egui::Ui,
    t: &Theme,
    words: &Words,
    k: i64,
    def: Option<&Value>,
    live: bool,
    size: f32,
    deck_id: &str,
    page: &str,
) -> Option<Drop> {
    let (rect, resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click());
    let p = ui.painter_at(rect.expand(4.0));
    let r = CornerRadius::same(radius::TILE);
    // the key face is a small screen: black like video
    p.rect_filled(rect, r, Color32::BLACK);
    match (live, st.textures.get(&k)) {
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
        _ => "Empty. Click to give it a job, or drop an effect or scene here.".into(),
    };
    let resp = resp.on_hover_text(hover).on_hover_cursor(egui::CursorIcon::PointingHand);
    if resp.clicked() {
        st.selected = Some(k);
        load_editor(st, k, def);
    }
    if resp.secondary_clicked() && def.is_some() {
        let args = Value::map().with("deck", deck_id.to_string()).with("key", k).with("page", page.to_string());
        action(app, "deck.press", args.clone());
        action(app, "deck.release", args);
    }
    dropped
}

fn drag_chip_width(ui: &egui::Ui, label: &str) -> f32 {
    ui.painter().layout_no_wrap(label.to_string(), font_medium(type_scale::SMALL + 0.5), Color32::WHITE).size().x + 20.0 + 24.0 + ui.spacing().item_spacing.x
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

fn drag_chip(ui: &mut egui::Ui, t: &Theme, glyph: &str, label: &str, color: Option<Color32>) {
    egui::Frame::new()
        .fill(t.surface_hi)
        .stroke(Stroke::new(1.0, t.border))
        .corner_radius(CornerRadius::same(radius::CONTROL))
        .inner_margin(egui::Margin::symmetric(10, 5))
        .show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 0.0;
                ui.label(RichText::new(glyph).color(color.unwrap_or(t.text_dim)));
                // glyphs draw wider than their advance: explicit gap
                ui.add_space(8.0);
                ui.label(RichText::new(label).font(font_medium(type_scale::SMALL + 0.5)).color(t.fg));
            });
        });
}

fn palette(ui: &mut egui::Ui, t: &Theme, words: &Words) {
    widgets::panel(ui, t, |ui| {
        ui.set_width(ui.available_width());
        ui.label(RichText::new("Drag onto a key").font(font_semibold(type_scale::BODY + 1.0)).color(t.fg));
        widgets::hint(ui, t, "Drop one on a key to give it that job.");
        ui.add_space(spacing::S);
        widgets::section(ui, t, "", "Quick effects");
        let presets = words.presets();
        let widths: Vec<f32> = presets.iter().map(|(_, l)| drag_chip_width(ui, l)).collect();
        flow(ui, &widths, |ui, n| {
            let (name, label) = &presets[n];
            ui.dnd_drag_source(egui::Id::new(("deck.drag.preset", name)), Drop::Preset(name.clone()), |ui| drag_chip(ui, t, icon::BOLT, label, Some(t.accent)));
        });
        widgets::section(ui, t, "", "Scenes");
        let scenes = words.scenes();
        let widths: Vec<f32> = scenes.iter().map(|(_, l)| drag_chip_width(ui, l)).collect();
        flow(ui, &widths, |ui, n| {
            let (name, label) = &scenes[n];
            ui.dnd_drag_source(egui::Id::new(("deck.drag.scene", name)), Drop::Scene(name.clone()), |ui| drag_chip(ui, t, icon::SCENE, label, None));
        });
        widgets::section(ui, t, "", "Actions");
        let widths: Vec<f32> = DECK_ACTIONS.iter().map(|(_, l)| drag_chip_width(ui, l)).collect();
        flow(ui, &widths, |ui, n| {
            let (c, label) = DECK_ACTIONS[n];
            ui.dnd_drag_source(egui::Id::new(("deck.drag.cmd", c)), Drop::Command(c.to_string()), |ui| drag_chip(ui, t, icon::PLAY, label, None));
        });
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
        "action" => s(&d, "action").split(';').map(str::trim).collect::<Vec<_>>().join("\n"),
        _ => String::new(),
    };
    st.edit_label.clear();
    st.edit_icon.clear();
    st.edit_color.clear();
    st.edit_hold = if i(&d, "hold_ms") > 0 { format!("{}ms", i(&d, "hold_ms")) } else { String::new() };
    st.edit_confirm = b(&d, "confirm");
}

/// A name picker over (name, label) pairs. Returns a new pick.
fn pick(ui: &mut egui::Ui, id: &str, cur: &str, opts: &[(String, String)], placeholder: &str, width: f32) -> Option<String> {
    let text = if cur.is_empty() { placeholder.to_string() } else { opts.iter().find(|(n, _)| n == cur).map_or_else(|| nice(cur), |(_, l)| l.clone()) };
    let mut out = None;
    egui::ComboBox::from_id_salt(id).width(width).selected_text(text).show_ui(ui, |ui| {
        for (name, label) in opts {
            if ui.selectable_label(name == cur, label).clicked() {
                out = Some(name.clone());
            }
        }
    });
    out
}

#[allow(clippy::too_many_arguments)]
fn key_editor(app: &mut App, st: &mut State, ui: &mut egui::Ui, t: &Theme, words: &Words, deck: &str, page: &str, pages: &[Value], deck_v: &Value) {
    let Some(k) = st.edit_key else {
        widgets::panel(ui, t, |ui| {
            ui.set_width(ui.available_width());
            widgets::empty_state(ui, t, icon::HAND, "Pick a key", "Click any key on the left to choose what it does.", None);
        });
        return;
    };
    let (mut save, mut clear, mut press) = (false, false, false);
    widgets::titled(
        ui,
        t,
        &format!("Key {}", k + 1),
        &format!("On the “{}” page", page_label(deck_v, page)),
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            widgets::section(ui, t, "", "What it does");
            ui.horizontal_wrapped(|ui| {
                for (kind, label) in KEY_KINDS {
                    if chip(ui, t, "", label, st.edit_kind == kind).clicked() && st.edit_kind != kind {
                        st.edit_kind = kind.to_string();
                        st.edit_target.clear();
                    }
                }
            });
            ui.add_space(spacing::S);
            let w = ui.available_width();
            match st.edit_kind.as_str() {
                "preset" => {
                    if let Some(p) = pick(ui, "deck-key-preset", &st.edit_target, words.presets(), "Pick a quick effect", w - 8.0) {
                        st.edit_target = p;
                    }
                }
                "scene" => {
                    if let Some(p) = pick(ui, "deck-key-scene", &st.edit_target, words.scenes(), "Pick a scene", w - 8.0) {
                        st.edit_target = p;
                    }
                    widgets::hint(ui, t, "Pressing it puts the scene up next.");
                }
                "page" => {
                    let mut opts: Vec<(String, String)> = pages.iter().map(|p| (s(p, "page").to_string(), page_label(deck_v, s(p, "page")))).collect();
                    opts.push(("next".into(), "Next page".into()));
                    opts.push(("prev".into(), "Previous page".into()));
                    if let Some(p) = pick(ui, "deck-key-page", &st.edit_target, &opts, "Pick a page", w - 8.0) {
                        st.edit_target = p;
                    }
                }
                "ptt" => {
                    widgets::hint(ui, t, "Hold the key and speak a command, like “switch to wide”.");
                }
                "toggle" | "momentary" => {
                    ui.add(
                        se_ui_kit::widgets::field(&mut st.edit_target)
                            .hint_text("Setting, e.g. fx.glow.enabled")
                            .font(font_mono(type_scale::BODY - 1.0))
                            .desired_width(w - 8.0),
                    );
                    widgets::hint(ui, t, if st.edit_kind == "toggle" { "Each press switches it on or off." } else { "On while you hold the key." });
                }
                _ => {
                    ui.add(
                        egui::TextEdit::multiline(&mut st.edit_target)
                            .hint_text("One command per line, e.g. scene.take")
                            .font(font_mono(type_scale::BODY - 1.0))
                            .desired_rows(3)
                            .desired_width(w - 8.0),
                    );
                    let cmds = st.edit_target.lines().map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("; ");
                    if !cmds.is_empty() {
                        widgets::hint(ui, t, &format!("Does: {}", words.command(&cmds)));
                    }
                }
            }
            ui.add_space(spacing::M);
            widgets::section(ui, t, "", "Look");
            ui.horizontal(|ui| {
                ui.label(RichText::new("Label").color(t.text_dim));
                ui.add(se_ui_kit::widgets::field(&mut st.edit_label).hint_text("from its name").desired_width(120.0));
                ui.label(RichText::new("Icon").color(t.text_dim));
                ui.add(se_ui_kit::widgets::field(&mut st.edit_icon).hint_text("fire, mic, scene…").desired_width(ui.available_width() - 8.0));
            });
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new("Color").color(t.text_dim));
                let (r, resp) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
                ui.painter().circle_stroke(
                    r.center(),
                    10.0,
                    Stroke::new(if st.edit_color.is_empty() { 2.5 } else { 1.0 }, if st.edit_color.is_empty() { t.fg } else { t.border }),
                );
                ui.painter().line_segment([r.center() + Vec2::new(-6.0, 6.0), r.center() + Vec2::new(6.0, -6.0)], Stroke::new(1.0, t.text_dim));
                if resp.on_hover_text("Default color").on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                    st.edit_color.clear();
                }
                for name in KEY_COLORS {
                    let c = key_color(t, name).unwrap_or(t.accent);
                    let (r, resp) = ui.allocate_exact_size(Vec2::splat(24.0), Sense::click());
                    ui.painter().circle_filled(r.center(), 10.0, c);
                    if st.edit_color == name {
                        ui.painter().circle_stroke(r.center(), 12.0, Stroke::new(2.0, t.fg));
                    }
                    if resp.on_hover_text(nice(name)).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                        st.edit_color = name.to_string();
                    }
                }
            });
            ui.add_space(spacing::M);
            widgets::section(ui, t, "", "Safety");
            ui.horizontal(|ui| {
                ui.label(RichText::new("Hold for").color(t.text_dim));
                ui.add(se_ui_kit::widgets::field(&mut st.edit_hold).hint_text("just tap").desired_width(80.0));
                widgets::hint(ui, t, "like 1s");
            });
            widgets::toggle_row(ui, t, "Press twice", "For keys you don't want to hit by accident.", &mut st.edit_confirm);
            ui.add_space(spacing::M);
            ui.horizontal(|ui| {
                let ready = st.edit_kind == "ptt" || !st.edit_target.trim().is_empty();
                save = widgets::button_ex(ui, t, Some(icon::CHECK), "Save key", Kind::Primary, Size::Medium, 0.0, ready).clicked();
                press = widgets::button_ex(ui, t, Some(icon::PLAY), "Try it", Kind::Secondary, Size::Medium, 0.0, true)
                    .on_hover_text("Runs it as if you pressed the key")
                    .clicked();
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    clear = widgets::hold_button(ui, t, "Clear key", t.bright_red, 0.6);
                });
            });
            widgets::details(ui, t, "deck-key-details", "Details", |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("Color code").color(t.text_dim));
                    ui.add(se_ui_kit::widgets::field(&mut st.edit_color).hint_text("#rrggbb").font(font_mono(type_scale::BODY - 1.0)).desired_width(110.0));
                });
                widgets::fact(ui, t, "Key number", &k.to_string());
                widgets::fact(ui, t, "Page id", page);
            });
        },
    );
    let base = || Value::map().with("deck", deck.to_string()).with("page", page.to_string()).with("key", k);
    if save {
        let target = st.edit_target.trim().to_string();
        let mut args = match st.edit_kind.as_str() {
            "action" => {
                base().with("do", target.split(['\n', ';']).map(|c| Value::Str(c.trim().to_string())).filter(|v| v.as_str() != Some("")).collect::<Vec<_>>())
            }
            "ptt" => base().with("ptt", true),
            kind => base().with(kind, target),
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
        app.m.toast(format!("Key {} saved.", k + 1), false);
    }
    if clear {
        action(app, "deck.assign", base().with("clear", true));
    }
    if press {
        let args = Value::map().with("deck", deck.to_string()).with("key", k).with("page", page.to_string());
        action(app, "deck.press", args.clone());
        action(app, "deck.release", args);
    }
}

// ---- MIDI ----------------------------------------------------------------------------------------

fn midi_editor(app: &mut App, st: &mut State, ui: &mut egui::Ui, t: &Theme, d: &Value) {
    let id = s(d, "id").to_string();
    let name = device_name(d, &id);
    let online = b(d, "online");
    let sub = if online {
        "Working".to_string()
    } else if s(d, "error").is_empty() {
        "Not connected. Plug it in and it connects by itself.".into()
    } else {
        format!("Not connected: {}", s(d, "error"))
    };
    let count = i(d, "bank_count");
    let mut bank: Option<i64> = None;
    widgets::titled(
        ui,
        t,
        &name,
        &sub,
        |ui| {
            if count > 0 {
                if widgets::icon_button(ui, t, icon::RIGHT, "Next 8 faders").clicked() {
                    bank = Some(8);
                }
                ui.label(RichText::new(format!("Faders {}–{} of {count}", i(d, "bank") + 1, (i(d, "bank") + 8).min(count))).color(t.text_dim));
                if widgets::icon_button(ui, t, icon::LEFT, "Previous 8 faders").clicked() {
                    bank = Some(-8);
                }
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            widgets::segmented(ui, t, &mut st.midi_view, &MIDI_VIEWS);
            ui.add_space(spacing::M);
            match st.midi_view {
                0 => {
                    learn_box(app, st, ui, t, &name);
                    ui.add_space(spacing::M);
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
        },
    );
    if let Some(delta) = bank {
        action(app, "midi.bank", Value::map().with("device", id).with("delta", delta));
    }
}

/// "What should it control?" → the learn target (`preset.x`, `scene.x`, an address, `do:cmd`).
fn learn_target(st: &State) -> String {
    match st.learn_kind {
        0 if !st.learn_pick.is_empty() => format!("preset.{}", st.learn_pick),
        1 if !st.learn_pick.is_empty() => format!("scene.{}", st.learn_pick),
        2 => st.learn_text.trim().to_string(),
        3 if !st.learn_text.trim().is_empty() => format!("do:{}", st.learn_text.trim()),
        _ => String::new(),
    }
}

fn target_words(words: &Words, target: &str) -> String {
    if let Some(p) = target.strip_prefix("preset.") {
        format!("Fire {}", words.preset(p))
    } else if let Some(sc) = target.strip_prefix("scene.") {
        format!("Put {} up next", words.scene(sc))
    } else if let Some(c) = target.strip_prefix("do:") {
        words.command(c)
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
        return;
    }
    egui::Frame::new()
        .fill(t.surface_hi)
        .stroke(Stroke::new(1.0, t.border))
        .corner_radius(CornerRadius::same(radius::CARD))
        .inner_margin(egui::Margin::same(16))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.label(RichText::new("Give a control a job").font(font_semibold(type_scale::LARGE)).color(t.fg));
            widgets::hint(ui, t, "Pick what it should do, press Learn, then move the knob, fader, button or pedal.");
            ui.add_space(spacing::S);
            ui.horizontal_wrapped(|ui| {
                for (n, label) in LEARN_KINDS.iter().enumerate() {
                    if chip(ui, t, "", label, st.learn_kind == n).clicked() && st.learn_kind != n {
                        st.learn_kind = n;
                        st.learn_pick.clear();
                        st.learn_text.clear();
                    }
                }
            });
            ui.add_space(spacing::S);
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 260.0).clamp(200.0, 420.0);
                match st.learn_kind {
                    0 => {
                        if let Some(p) = pick(ui, "learn-preset", &st.learn_pick, words.presets(), "Pick a quick effect", w) {
                            st.learn_pick = p;
                        }
                    }
                    1 => {
                        if let Some(p) = pick(ui, "learn-scene", &st.learn_pick, words.scenes(), "Pick a scene", w) {
                            st.learn_pick = p;
                        }
                    }
                    2 => {
                        ui.add(
                            se_ui_kit::widgets::field(&mut st.learn_text)
                                .hint_text("Setting, e.g. fx.rgb_split.amount")
                                .font(font_mono(type_scale::BODY - 1.0))
                                .desired_width(w),
                        );
                        if let Some(sel) = app.selected.clone()
                            && st.learn_text.is_empty()
                            && widgets::button_ex(ui, t, None, &format!("Use {}", setting_name(&sel)), Kind::Ghost, Size::Small, 0.0, true).clicked()
                        {
                            st.learn_text = sel;
                        }
                    }
                    _ => {
                        ui.add(
                            se_ui_kit::widgets::field(&mut st.learn_text)
                                .hint_text("Command, e.g. scene.take")
                                .font(font_mono(type_scale::BODY - 1.0))
                                .desired_width(w),
                        );
                    }
                }
                let target = learn_target(st);
                if widgets::button_ex(ui, t, Some(icon::HAND), "Learn: move a control", Kind::Primary, Size::Large, 0.0, !target.is_empty()).clicked() {
                    action(app, "midi.learn", Value::map().with("target", target));
                }
            });
            if let Some(e) = app.m.events.iter().rev().find(|e| e.ty == "midi.learned") {
                let control = e.payload.get_path("control").and_then(Value::as_str).unwrap_or("");
                let target = e.payload.get_path("target").and_then(Value::as_str).unwrap_or("");
                ui.add_space(spacing::XS);
                widgets::hint(ui, t, &format!("Last learned: {} → {}", control_name(control), target_words(&words, target)));
            }
        });
}

fn mapping_row(ui: &mut egui::Ui, t: &Theme, n: usize, control: &str, does: &str, tip: &str, lights_up: bool) -> bool {
    let mut remove = false;
    egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(radius::CONTROL)).inner_margin(egui::Margin::symmetric(12, 6)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            egui::Frame::new()
                .fill(t.surface)
                .stroke(Stroke::new(1.0, t.border))
                .corner_radius(CornerRadius::same(6))
                .inner_margin(egui::Margin::symmetric(10, 4))
                .show(ui, |ui| {
                    ui.set_min_width(130.0);
                    ui.label(RichText::new(control_name(control)).font(font_semibold(type_scale::BODY)).color(t.fg));
                });
            ui.label(RichText::new(icon::RIGHT).color(t.text_faint));
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
    if maps.is_empty() && bindings.is_empty() {
        widgets::empty_state(ui, t, icon::SLIDERS, "No jobs yet", "Press Learn above and move a control to give it one.", None);
        return;
    }
    widgets::section(ui, t, "", &format!("What each control does ({})", maps.len() + bindings.len()));
    let mut unmap: Option<String> = None;
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
        let does = format!("Sets {}{smooth}", crate::views::rules::setting_words(s(bnd, "target")));
        let tip = format!("{} → {} · curve {}", s(bnd, "signal"), s(bnd, "target"), s(bnd, "curve"));
        if mapping_row(ui, t, n, &control, &does, &tip, b(bnd, "feedback")) {
            unmap = Some(control);
        }
        n += 1;
        ui.add_space(spacing::XS);
    }
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
    widgets::titled(
        ui,
        t,
        "Voice control",
        words,
        |_| {},
        |ui| {
            ui.set_width(ui.available_width().min(760.0));
            widgets::hint(ui, t, "Hold the button (or a key or pedal set to push-to-talk) and say something like “switch to wide” or “fire hype”.");
            ui.add_space(spacing::M);
            if let Some(e) = v.get_path("error").and_then(Value::as_str) {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon::WARN).color(t.bright_red));
                    ui.label(RichText::new(e).color(t.fg));
                });
                ui.add_space(spacing::S);
            }
            if let Some(p) = v.get_path("download").and_then(Value::as_f64) {
                ui.add(egui::ProgressBar::new(p as f32).text("Downloading the voice model…"));
                ui.add_space(spacing::S);
            }
            // hold to talk
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 56.0), Sense::click_and_drag());
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
            ui.add_space(spacing::M);
            widgets::section(ui, t, "", "Lately");
            let history = v.get_path("history").and_then(Value::as_list).unwrap_or(&[]);
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
            ui.add_space(spacing::S);
            widgets::details(ui, t, "voice-details", "Details", |ui| {
                widgets::fact(ui, t, "State", if state.is_empty() { "—" } else { &state });
                widgets::fact(ui, t, "Speech model", s(&v, "model"));
                widgets::fact(ui, t, "Microphone", s(&v, "device"));
                widgets::fact(ui, t, "Model file", s(&v, "model_path"));
            });
        },
    );
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
        st.learn_text = " fx.vhs.amount ".into();
        assert_eq!(learn_target(&st), "fx.vhs.amount");
        st.learn_kind = 3;
        st.learn_text = "scene.take".into();
        assert_eq!(learn_target(&st), "do:scene.take");
        st.learn_text.clear();
        assert_eq!(learn_target(&st), "", "nothing picked: Learn stays disabled");
    }
}
