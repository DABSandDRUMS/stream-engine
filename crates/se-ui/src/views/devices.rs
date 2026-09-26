//! Settings → Devices (§15.6): everything plugged in, grouped the way a streamer thinks about
//! it — cameras (live picture, signal, which input, picture settings), sound, controllers and the
//! lights interface — with Connected / Missing badges and inline renaming. The technical identity
//! (udev identity, node, formats, modes, decoder) sits behind "Details". Pure client of the
//! `devices` / `sources` queries, `source.*` state, and the `devices.*` / `source.*` actions.

use crate::app::App;
use crate::frames::Canvas;
use crate::model::Model;
use crate::views::live::nice;
use crate::views::monitor;
use egui::{Align, Layout, Pos2, Rect, RichText, Ui, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_medium, font_mono, font_semibold, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const REFRESH_EVERY: Duration = Duration::from_secs(1);
const AFTER_ACTION: Duration = Duration::from_millis(250);
/// A value being dragged locally wins over the engine echo for this long.
const EDIT_HOLD: Duration = Duration::from_millis(1200);
/// A sent rename shows until the engine confirms it, or this long.
const RENAME_WAIT: Duration = Duration::from_secs(3);
/// Camera tiles are at least this wide.
const TILE_MIN: f32 = 300.0;
/// Hardware groups: (title, one line, registry kinds).
const GROUPS: [(&str, &str, &[&str]); 4] = [
    ("Camera inputs", "Capture cards and webcams your cameras plug into.", &["camera"]),
    ("Sound", "Audio interfaces and sound cards.", &["audio_node", "audio_card"]),
    ("Controllers", "Pads, pedals, faders and button boxes.", &["midi", "hid"]),
    ("Lights", "The box that talks to your lights.", &["serial"]),
];

#[derive(Clone, Default)]
struct DevicesState {
    refreshed: Option<Instant>,
    /// address → (value, when edited)
    edits: HashMap<String, (Value, Instant)>,
    /// device id → (name being typed, when it was sent to the engine)
    renames: HashMap<String, (String, Option<Instant>)>,
    filter: String,
}

impl DevicesState {
    fn refresh(&mut self, m: &mut Model, now: Instant) {
        if m.connected && self.refreshed.is_none_or(|t| now.duration_since(t) >= REFRESH_EVERY) {
            self.refreshed = Some(now);
            m.query("devices", Value::Null);
            m.query("sources", Value::Null);
        }
        self.edits.retain(|_, (_, at)| now.duration_since(*at) < EDIT_HOLD);
    }
    fn soon(&mut self, now: Instant) {
        self.refreshed = Some(now - REFRESH_EVERY + AFTER_ACTION);
    }
}

/// Live camera pictures: the multiview atlas and where each source sits in it.
struct Pics {
    atlas: Option<egui::TextureId>,
    tiles: Vec<(String, [f32; 4])>,
}

impl Pics {
    fn of(&self, source: &str) -> Option<(egui::TextureId, Rect)> {
        let id = self.atlas?;
        self.tiles.iter().find(|(s, _)| s == source).map(|(_, r)| (id, Rect::from_min_size(Pos2::new(r[0], r[1]), Vec2::new(r[2], r[3]))))
    }
}

pub fn ui(app: &mut App, ui: &mut Ui) {
    let id = ui.make_persistent_id("devices_view");
    let now = Instant::now();
    let mut st = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<DevicesState>(id)));
    st.refresh(&mut app.m, now);
    let hz = app.atlas_hz();
    let pics = Pics { atlas: monitor::texture(app, Canvas::Atlas, hz).map(|f| f.id), tiles: monitor::atlas_tiles(app) };
    let t = app.t.clone();
    let mut out = Vec::new();
    view(&app.m, &t, &mut st, ui, &pics, &mut out, now);
    if !out.is_empty() {
        st.soon(now);
    }
    ui.data_mut(|d| d.insert_temp(id, st));
    for op in out {
        app.m.command(op);
    }
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}
fn b(v: &Value, k: &str) -> bool {
    v.get_path(k).is_some_and(Value::truthy)
}
fn f(v: &Value, k: &str) -> f64 {
    v.get_path(k).and_then(Value::as_f64).unwrap_or(0.0)
}

fn action(name: &str, args: Value) -> Op {
    Op::Action { name: name.into(), args }
}

/// What a source is called on screen.
fn source_title(src: &Value) -> String {
    if s(src, "label").is_empty() { nice(s(src, "name")) } else { s(src, "label").to_string() }
}

#[derive(Clone, Copy, PartialEq)]
enum Signal {
    Picture,
    NoPicture,
    Missing,
    Problem,
    Unused,
}

/// Display names of the scenes that show `source`.
fn scenes_using(m: &Model, source: &str) -> Vec<String> {
    m.q_list("scenes")
        .iter()
        .filter(|sc| sc.get_path("sources").and_then(Value::as_list).is_some_and(|l| l.iter().any(|v| v.as_str() == Some(source))))
        .map(|sc| nice(if s(sc, "label").is_empty() { s(sc, "name") } else { s(sc, "label") }))
        .collect()
}

fn source_signal(m: &Model, src: &Value) -> Signal {
    let a = |k: &str| format!("source.{}.{k}", s(src, "name"));
    let signal = m.get(&a("signal")).map(Value::truthy).unwrap_or(b(src, "signal"));
    let capturing = m.get(&a("capturing")).map(Value::truthy).unwrap_or(b(src, "capturing"));
    if !b(src, "used") {
        Signal::Unused
    } else if b(src, "missing") {
        Signal::Missing
    } else if !s(src, "error").is_empty() && !capturing {
        Signal::Problem
    } else if signal {
        Signal::Picture
    } else {
        Signal::NoPicture
    }
}

fn view(m: &Model, t: &Theme, st: &mut DevicesState, ui: &mut Ui, pics: &Pics, out: &mut Vec<Op>, now: Instant) {
    let Some(reg) = m.q("devices") else {
        widgets::panel(ui, t, |ui| {
            ui.set_width(ui.available_width());
            let (title, body) = if m.connected {
                ("Looking for devices…", "This takes a second.")
            } else {
                ("Stream Engine isn't running", "Devices show up here once it's running.")
            };
            widgets::empty_state(ui, t, icon::DEVICE, title, body, None);
        });
        return;
    };
    let devices: Vec<Value> = reg.get_path("devices").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let expected: Vec<Value> = reg.get_path("expected").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
    let sources: Vec<Value> = m.q_list("sources").to_vec();

    summary(m, t, st, ui, &expected, &sources, out);
    ui.add_space(spacing::L);

    let flt = st.filter.to_lowercase();
    let matches = |v: &Value| flt.is_empty() || ["id", "name", "label", "identity", "path"].iter().any(|k| s(v, k).to_lowercase().contains(&flt));
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = spacing::S;
        cameras(m, t, st, ui, pics, &sources, &devices, &matches, out, now);
        ui.add_space(spacing::S);
        let w = ui.available_width();
        let cols = if w >= 2200.0 {
            4
        } else if w >= 1100.0 {
            2
        } else {
            1
        };
        let source_names: Vec<(String, String)> =
            sources.iter().filter(|v| s(v, "kind") == "camera").map(|v| (s(v, "name").to_string(), source_title(v))).collect();
        let groups: Vec<usize> = (0..GROUPS.len()).collect();
        for row in groups.chunks(cols) {
            ui.columns(cols, |c| {
                for (k, gi) in row.iter().enumerate() {
                    hardware(m, t, st, &mut c[k], *gi, &devices, &expected, &source_names, &matches, out);
                }
            });
            ui.add_space(spacing::S);
        }
        other(t, st, ui, &devices, &matches, out);
        ui.add_space(spacing::XL);
    });
}

/// One line on how things are, plus search and "look again".
fn summary(m: &Model, t: &Theme, st: &mut DevicesState, ui: &mut Ui, expected: &[Value], sources: &[Value], out: &mut Vec<Op>) {
    let missing: Vec<String> = expected
        .iter()
        .filter(|e| !b(e, "present") && !b(e, "optional"))
        .map(|e| if s(e, "label").is_empty() { nice(s(e, "id")) } else { s(e, "label").to_string() })
        .collect();
    let dark: Vec<String> =
        sources.iter().filter(|v| matches!(source_signal(m, v), Signal::NoPicture | Signal::Missing | Signal::Problem)).map(source_title).collect();
    // search and "look again" on their own row, then how things are
    ui.horizontal(|ui| {
        ui.add(se_ui_kit::widgets::field(&mut st.filter).hint_text("Find a device").desired_width(260.0));
        if widgets::button_ex(ui, t, Some(icon::SEARCH), "Look again", Kind::Secondary, Size::Medium, 0.0, true)
            .on_hover_text("Check for devices that were just plugged in")
            .clicked()
        {
            out.push(action("devices.rescan", Value::Null));
        }
    });
    ui.add_space(spacing::M);
    let (tone, ic, title, line) = if !missing.is_empty() {
        (
            widgets::Tone::Danger,
            icon::WARN,
            if missing.len() == 1 { "1 device is not connected".to_string() } else { format!("{} devices are not connected", missing.len()) },
            format!("{}. Check they're plugged in and switched on.", missing.join(", ")),
        )
    } else if !dark.is_empty() {
        (
            widgets::Tone::Warn,
            icon::WARN,
            if dark.len() == 1 { "1 camera has no picture".to_string() } else { format!("{} cameras have no picture", dark.len()) },
            format!("{}. Check the camera is on and its cable is in.", dark.join(", ")),
        )
    } else {
        (widgets::Tone::Ok, icon::CHECK, "Everything's plugged in".to_string(), "All your cameras, sound and controllers are working.".to_string())
    };
    widgets::callout(ui, t, tone, ic, &title, &line, None);
}

// ---- cameras (sources) ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn cameras(
    m: &Model,
    t: &Theme,
    st: &mut DevicesState,
    ui: &mut Ui,
    pics: &Pics,
    sources: &[Value],
    devices: &[Value],
    matches: &dyn Fn(&Value) -> bool,
    out: &mut Vec<Op>,
    now: Instant,
) {
    let list: Vec<&Value> = sources.iter().filter(|v| matches(v)).collect();
    widgets::titled(
        ui,
        t,
        "Cameras",
        "What each camera is showing right now.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if sources.is_empty() {
                widgets::empty_state(ui, t, icon::CAMERA, "No cameras yet", "Plug a camera in, then name it in Get started.", None);
                return;
            }
            if list.is_empty() {
                widgets::hint(ui, t, "No camera matches your search.");
                return;
            }
            let gap = spacing::L;
            let w = ui.available_width();
            let n = (((w + gap) / (TILE_MIN + gap)).floor() as usize).clamp(1, 6);
            let tile_w = ((w - gap * (n as f32 - 1.0)) / n as f32).floor();
            for row in list.chunks(n) {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for src in row {
                        ui.allocate_ui_with_layout(Vec2::new(tile_w, 10.0), Layout::top_down(Align::Min), |ui| {
                            ui.set_width(tile_w);
                            camera_tile(m, t, st, ui, pics, src, devices, out, now);
                        });
                    }
                });
                ui.add_space(gap);
            }
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn camera_tile(m: &Model, t: &Theme, st: &mut DevicesState, ui: &mut Ui, pics: &Pics, src: &Value, devices: &[Value], out: &mut Vec<Op>, now: Instant) {
    let name = s(src, "name").to_string();
    let a = |k: &str| format!("source.{name}.{k}");
    let sig = source_signal(m, src);
    let w = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, w * 9.0 / 16.0), egui::Sense::hover());
    let tally = match sig {
        Signal::Missing | Signal::Problem => Some(widgets::LedState::Error),
        _ => None,
    };
    widgets::video_frame(ui, t, rect, pics.of(&name), "", tally, None);
    ui.add_space(spacing::S);
    let fps = m.get(&a("fps")).and_then(Value::as_f64).unwrap_or(f(src, "measured_fps"));
    let dropped = m.get(&a("dropped")).and_then(Value::as_i64).unwrap_or(f(src, "dropped") as i64);
    let (badge, color, line) = match sig {
        Signal::Picture => {
            let skipped = if dropped > 0 { format!(" · {dropped} frames skipped") } else { String::new() };
            ("Working", t.green, format!("Showing a picture, {fps:.0} frames a second{skipped}"))
        }
        Signal::NoPicture => ("Needs a look", t.yellow, "Nothing is coming in. Is the camera on?".to_string()),
        Signal::Missing => ("Not connected", t.bright_red, "Its input isn't plugged in.".to_string()),
        Signal::Problem => ("Needs a look", t.bright_red, "It couldn't start. Try Restart.".to_string()),
        Signal::Unused => {
            // video only runs for sources on screen; a source some scene uses is just waiting
            let scenes = scenes_using(m, &name);
            if scenes.is_empty() {
                ("Off", t.text_dim, "Not in any scene yet.".to_string())
            } else {
                ("Off", t.text_dim, format!("Turns on when {} is showing.", scenes.join(" or ")))
            }
        }
    };
    ui.horizontal(|ui| {
        ui.label(RichText::new(source_title(src)).font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            widgets::badge(ui, t, badge, color);
        });
    });
    widgets::hint(ui, t, &line);
    ui.add_space(spacing::XS);
    if s(src, "kind") == "camera" {
        ui.horizontal(|ui| {
            let cams: Vec<&Value> = devices.iter().filter(|d| s(d, "kind") == "camera").collect();
            let current =
                cams.iter().find(|c| s(c, "identity") == s(src, "identity")).map(|c| s(c, "label").to_string()).unwrap_or_else(|| "Pick an input".into());
            // Restart takes its place on the right first; the input menu gets what's left
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::button_ex(ui, t, Some(icon::UNDO), "Restart", Kind::Ghost, Size::Small, 0.0, true)
                    .on_hover_text("Close and reopen the camera")
                    .clicked()
                {
                    out.push(action("source.reopen", Value::map().with("source", name.as_str())));
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.label(RichText::new("Input").color(t.text_dim));
                    egui::ComboBox::from_id_salt(("assign", &name))
                        .selected_text(current.clone())
                        .width((ui.available_width() - spacing::S).max(80.0))
                        .wrap_mode(egui::TextWrapMode::Truncate)
                        .show_ui(ui, |ui| {
                            for c in cams {
                                if ui.selectable_label(s(c, "identity") == s(src, "identity"), s(c, "label")).clicked() {
                                    out.push(action("source.assign", Value::map().with("source", name.as_str()).with("identity", s(c, "identity"))));
                                }
                            }
                        })
                        .response
                        .on_hover_text(&current);
                });
            });
        });
        let controls: Vec<Value> = src.get_path("controls").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
        if !controls.is_empty() {
            widgets::details(ui, t, ("pic", &name), "Picture settings", |ui| picture_settings(m, t, st, ui, &name, &controls, out, now));
        }
    } else {
        ui.horizontal(|ui| {
            let pos = m.get(&a("position")).and_then(Value::as_f64).unwrap_or(0.0);
            let dur = m.get(&a("duration")).and_then(Value::as_f64).unwrap_or(0.0);
            ui.label(RichText::new(format!("{} / {}", clock(pos), clock(dur))).font(font_mono(type_scale::SMALL + 0.5)).color(t.text_dim));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let paused = m.b(&a("paused"));
                if widgets::button_ex(
                    ui,
                    t,
                    Some(if paused { icon::PLAY } else { icon::PAUSE }),
                    if paused { "Play" } else { "Pause" },
                    Kind::Secondary,
                    Size::Small,
                    0.0,
                    true,
                )
                .clicked()
                {
                    out.push(Op::Set { address: a("paused"), value: Value::Bool(!paused) });
                }
                if widgets::button_ex(ui, t, Some(icon::UNDO), "From the start", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                    out.push(action("source.restart", Value::map().with("source", name.as_str())));
                }
            });
        });
    }
    widgets::details(ui, t, ("src-details", &name), "Details", |ui| source_details(m, t, ui, src));
}

fn clock(secs: f64) -> String {
    let s = secs.max(0.0) as i64;
    format!("{}:{:02}", s / 60, s % 60)
}

fn source_details(m: &Model, t: &Theme, ui: &mut Ui, src: &Value) {
    let name = s(src, "name");
    let size = src
        .get_path("size")
        .and_then(Value::as_list)
        .map(|l| format!("{}×{}", l.first().and_then(Value::as_i64).unwrap_or(0), l.get(1).and_then(Value::as_i64).unwrap_or(0)))
        .unwrap_or_else(|| format!("{}×{}", f(src, "width"), f(src, "height")));
    let fps = m.get(&format!("source.{name}.fps")).and_then(Value::as_f64).unwrap_or(f(src, "measured_fps"));
    let cpu = m.get(&format!("source.{name}.cpu")).and_then(Value::as_f64).unwrap_or(f(src, "cpu"));
    widgets::fact(ui, t, "Source", name);
    widgets::fact(ui, t, "Format", &format!("{} {size}", s(src, "format")));
    widgets::fact(ui, t, "Frame rate", &format!("{fps:.2} of {:.0}", f(src, "fps")));
    widgets::fact(ui, t, "Converted to", &format!("{} {}", s(src, "slot_format"), s(src, "decoder")));
    if !s(src, "matrix").is_empty() {
        widgets::fact(ui, t, "Color", &format!("{} {}", s(src, "matrix"), s(src, "range")));
    }
    widgets::fact(ui, t, "CPU", &format!("{cpu:.1}%"));
    widgets::fact(ui, t, "Device", s(src, "device"));
    widgets::fact(ui, t, "Node", s(src, "path"));
    for k in ["error", "config_error"] {
        if !s(src, k).is_empty() {
            ui.label(RichText::new(s(src, k)).size(type_scale::SMALL).color(t.bright_red));
        }
    }
}

/// Camera controls (brightness, focus, white balance…) as sliders, switches and menus.
#[allow(clippy::too_many_arguments)]
fn picture_settings(m: &Model, t: &Theme, st: &mut DevicesState, ui: &mut Ui, name: &str, list: &[Value], out: &mut Vec<Op>, now: Instant) {
    for c in list {
        let cname = s(c, "name");
        let addr = format!("source.{name}.ctrl.{cname}");
        let auto = b(c, "inactive");
        let ro = b(c, "read_only");
        let live = st.edits.get(&addr).map(|(v, _)| v.clone()).or_else(|| m.get(&addr).cloned()).or_else(|| c.get_path("value").cloned()).unwrap_or_default();
        let label = if s(c, "label").is_empty() { nice(cname) } else { s(c, "label").to_string() };
        let mut new: Option<Value> = None;
        ui.add_enabled_ui(!ro, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new(&label).font(font_medium(type_scale::SMALL + 0.5)).color(if auto { t.text_faint } else { t.text_dim }))
                    .on_hover_text(format!("{cname} · camera value {}", c.get_path("value").map(|v| v.to_string()).unwrap_or_default()));
                if auto {
                    ui.label(RichText::new("automatic").size(type_scale::SMALL).color(t.text_faint));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if widgets::icon_button(ui, t, icon::UNDO, "Back to the saved value").clicked() {
                        st.edits.remove(&addr);
                        out.push(Op::Release { address: addr.clone() });
                    }
                    match s(c, "type") {
                        "bool" => {
                            let mut v = live.truthy();
                            if widgets::toggle(ui, t, &mut v).changed() {
                                new = Some(Value::Bool(v));
                            }
                        }
                        "enum" => {
                            let menu: Vec<Value> = c.get_path("menu").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
                            let cur = live.as_str().unwrap_or("").to_string();
                            let shown = menu.iter().find(|x| s(x, "name") == cur).map(|x| s(x, "label").to_string()).unwrap_or(cur.clone());
                            egui::ComboBox::from_id_salt(("menu", &addr)).selected_text(shown).width(170.0).show_ui(ui, |ui| {
                                for it in &menu {
                                    if ui.selectable_label(s(it, "name") == cur, s(it, "label")).clicked() {
                                        new = Some(Value::Str(s(it, "name").to_string()));
                                    }
                                }
                            });
                        }
                        _ => {
                            ui.label(RichText::new(live.as_i64().unwrap_or(0).to_string()).font(font_mono(type_scale::SMALL)).color(t.fg));
                        }
                    }
                });
            });
            if !matches!(s(c, "type"), "bool" | "enum") {
                let (lo, hi) = (f(c, "min") as i64, f(c, "max") as i64);
                let mut v = live.as_i64().unwrap_or(lo);
                let step = f(c, "step").max(1.0);
                ui.spacing_mut().slider_width = ui.available_width();
                if ui.add(egui::Slider::new(&mut v, lo..=hi).step_by(step).show_value(false).clamping(egui::SliderClamping::Always)).changed() {
                    new = Some(Value::Int(v));
                }
            }
        });
        if let Some(v) = new {
            st.edits.insert(addr.clone(), (v.clone(), now));
            out.push(Op::Set { address: addr, value: v });
        }
        ui.add_space(spacing::XS);
    }
    ui.add_space(spacing::XS);
    if widgets::button_ex(ui, t, Some(icon::SAVE), "Keep these settings", Kind::Secondary, Size::Small, 0.0, true)
        .on_hover_text("Use these every time the camera starts")
        .clicked()
    {
        out.push(action("source.save_controls", Value::map().with("source", name)));
    }
}

// ---- hardware ------------------------------------------------------------------------------------

/// A device row's display name (the saved name, else what the device calls itself).
fn device_name(d: &Value) -> String {
    let l = s(d, "label");
    if l.is_empty() { nice(s(d, if s(d, "name").is_empty() { "id" } else { "name" })) } else { l.to_string() }
}

#[allow(clippy::too_many_arguments)]
fn hardware(
    m: &Model,
    t: &Theme,
    st: &mut DevicesState,
    ui: &mut Ui,
    gi: usize,
    devices: &[Value],
    expected: &[Value],
    source_names: &[(String, String)],
    matches: &dyn Fn(&Value) -> bool,
    out: &mut Vec<Op>,
) {
    let (title, blurb, kinds) = GROUPS[gi];
    let present: Vec<&Value> = devices.iter().filter(|d| kinds.contains(&s(d, "kind")) && matches(d)).collect();
    let missing: Vec<&Value> = expected.iter().filter(|e| kinds.contains(&s(e, "kind")) && !b(e, "present") && matches(e)).collect();
    let (main, more): (Vec<&Value>, Vec<&Value>) =
        present.into_iter().partition(|d| d.get_path("expected").is_some_and(|v| !v.is_null()) || !s(d, "source").is_empty());
    widgets::titled(
        ui,
        t,
        title,
        blurb,
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if gi == 1 && m.has("mixer.16r.connected") {
                let on = m.b("mixer.16r.connected");
                row_frame(ui, t, |ui| {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(icon::SLIDERS).size(type_scale::LARGE).color(t.text_dim));
                        ui.vertical(|ui| {
                            ui.label(RichText::new("Mixing desk (StudioLive 16R)").font(font_medium(type_scale::BODY)).color(t.fg));
                            ui.label(
                                RichText::new(if on { "Talking to the desk over the network." } else { "Can't reach the desk. Is it on and on the network?" })
                                    .size(type_scale::SMALL)
                                    .color(t.text_dim),
                            );
                        });
                        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                            widgets::badge(ui, t, if on { "Working" } else { "Not connected" }, if on { t.green } else { t.yellow });
                        });
                    });
                });
            }
            for e in &missing {
                missing_row(t, st, ui, e, out);
            }
            for d in &main {
                device_row(t, st, ui, d, source_names, out);
            }
            if missing.is_empty() && main.is_empty() && !(gi == 1 && m.has("mixer.16r.connected")) {
                widgets::hint(ui, t, if more.is_empty() { "Nothing plugged in." } else { "Nothing in use yet." });
            }
            if !more.is_empty() {
                let label = if more.len() == 1 { "1 more plugged in".to_string() } else { format!("{} more plugged in", more.len()) };
                widgets::details(ui, t, ("more", gi), &label, |ui| {
                    for d in &more {
                        device_row(t, st, ui, d, source_names, out);
                    }
                });
            }
        },
    );
}

/// Devices whose kind fits no group.
fn other(t: &Theme, st: &mut DevicesState, ui: &mut Ui, devices: &[Value], matches: &dyn Fn(&Value) -> bool, out: &mut Vec<Op>) {
    let list: Vec<&Value> = devices.iter().filter(|d| !GROUPS.iter().any(|(_, _, k)| k.contains(&s(d, "kind"))) && matches(d)).collect();
    if list.is_empty() {
        return;
    }
    widgets::titled(
        ui,
        t,
        "Other devices",
        "Plugged in, not sorted into a group.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            for d in list {
                device_row(t, st, ui, d, &[], out);
            }
        },
    );
}

fn row_frame(ui: &mut Ui, t: &Theme, body: impl FnOnce(&mut Ui)) {
    egui::Frame::new().fill(t.surface_hi).corner_radius(radius::CONTROL).inner_margin(egui::Margin::symmetric(14, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        body(ui);
    });
}

/// Inline name field; saves when you press Enter or click away. While typing (and until the
/// engine confirms a rename, at most a few seconds) the typed text wins over the engine's name.
fn name_field(t: &Theme, st: &mut DevicesState, ui: &mut Ui, id: &str, current: &str, out: &mut Vec<Op>) {
    let mut buf = st.renames.get(id).map(|(b, _)| b.clone()).unwrap_or_else(|| current.to_string());
    // reads as the device's name; a frame (and the pencil) only while you edit
    let editing = st.renames.get(id).is_some_and(|(_, sent)| sent.is_none());
    let mut te = se_ui_kit::widgets::field(&mut buf)
        .font(font_medium(type_scale::BODY))
        .text_color(t.fg)
        .hint_text("Name it")
        .desired_width((ui.available_width() - 150.0).clamp(140.0, 280.0));
    if !editing {
        te = te.frame(egui::Frame::NONE);
    }
    let r = ui.add(te);
    if r.lost_focus() {
        let label = buf.trim().to_string();
        if !label.is_empty() && label != current {
            out.push(action("devices.rename", Value::map().with("id", id).with("label", label.as_str())));
            st.renames.insert(id.to_string(), (label, Some(Instant::now())));
        } else {
            st.renames.remove(id);
        }
    } else if r.has_focus() {
        st.renames.insert(id.to_string(), (buf, None));
    } else if let Some((typed, sent)) = st.renames.get(id)
        && (typed == current || sent.is_some_and(|at| at.elapsed() > RENAME_WAIT))
    {
        st.renames.remove(id);
    }
    let hovered = r.hovered();
    r.on_hover_text("Click to rename");
    if hovered && !editing {
        ui.label(RichText::new(icon::EDIT).size(type_scale::SMALL).color(t.text_faint));
    }
}

fn missing_row(t: &Theme, st: &mut DevicesState, ui: &mut Ui, e: &Value, out: &mut Vec<Op>) {
    let id = s(e, "id").to_string();
    let optional = b(e, "optional");
    row_frame(ui, t, |ui| {
        ui.horizontal(|ui| {
            name_field(t, st, ui, &id, &device_name(e), out);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::hold_button(ui, t, "Forget", t.bright_red, 0.8) {
                    out.push(action("devices.forget", Value::map().with("id", id.as_str())));
                }
                widgets::badge(ui, t, "Not connected", if optional { t.yellow } else { t.bright_red });
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(
                        egui::Label::new(
                            RichText::new(if optional { "Not plugged in (it's optional)" } else { "Not plugged in. Check the cable." })
                                .size(type_scale::SMALL + 0.5)
                                .color(t.text_dim),
                        )
                        .truncate(),
                    );
                });
            });
        });
        widgets::details(ui, t, ("dev", &id), "Details", |ui| {
            widgets::fact(ui, t, "Saved as", &id);
            widgets::fact(ui, t, "Kind", s(e, "kind"));
            if !s(e, "identity").is_empty() {
                widgets::fact(ui, t, "Identity", s(e, "identity"));
            }
        });
    });
}

fn kind_words(kind: &str) -> &'static str {
    match kind {
        "camera" => "Video input",
        "audio_node" => "Sound device",
        "audio_card" => "Sound card",
        "midi" | "hid" => "Controller",
        "serial" => "Light box",
        _ => "Device",
    }
}

fn device_row(t: &Theme, st: &mut DevicesState, ui: &mut Ui, d: &Value, source_names: &[(String, String)], out: &mut Vec<Op>) {
    let id = s(d, "id").to_string();
    let expected = d.get_path("expected").and_then(Value::as_str).map(str::to_string);
    let used_by = s(d, "source");
    let line = match (s(d, "kind"), d.get_path("signal").filter(|v| !v.is_null())) {
        ("camera", _) if !used_by.is_empty() => {
            let title = source_names.iter().find(|(n, _)| n == used_by).map(|(_, l)| l.clone()).unwrap_or_else(|| nice(used_by));
            let sig = match d.get_path("signal").filter(|v| !v.is_null()) {
                Some(v) if v.truthy() => "",
                Some(_) => " · no signal on the cable",
                None => "",
            };
            format!("Used by {title}{sig}")
        }
        ("camera", Some(v)) if !v.truthy() => "Not used · no signal on the cable".to_string(),
        ("camera", _) => "Not used yet".to_string(),
        (k, _) => kind_words(k).to_string(),
    };
    row_frame(ui, t, |ui| {
        ui.horizontal(|ui| {
            name_field(t, st, ui, &id, &device_name(d), out);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if expected.is_none()
                    && widgets::button_ex(ui, t, None, "Remember", Kind::Ghost, Size::Small, 0.0, true)
                        .on_hover_text("Stream Engine will warn you when it's unplugged")
                        .clicked()
                {
                    out.push(action("devices.expect", Value::map().with("identity", s(d, "identity"))));
                }
                widgets::badge(ui, t, "Working", t.green);
                if s(d, "kind") == "camera" && used_by.is_empty() && !source_names.is_empty() {
                    egui::ComboBox::from_id_salt(("use_as", s(d, "identity"))).selected_text("Use for…").width(130.0).show_ui(ui, |ui| {
                        for (n, title) in source_names {
                            if ui.selectable_label(false, title).clicked() {
                                out.push(action("source.assign", Value::map().with("source", n.as_str()).with("identity", s(d, "identity"))));
                            }
                        }
                    });
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(&line).size(type_scale::SMALL + 0.5).color(t.text_dim)).truncate());
                });
            });
        });
        widgets::details(ui, t, ("dev", &id), "Details", |ui| device_details(t, ui, d, expected.as_deref()));
    });
}

fn device_details(t: &Theme, ui: &mut Ui, d: &Value, expected: Option<&str>) {
    widgets::fact(ui, t, "Name", s(d, "name"));
    if let Some(e) = expected {
        widgets::fact(ui, t, "Saved as", e);
    }
    widgets::fact(ui, t, "Kind", s(d, "kind"));
    widgets::fact(ui, t, "Node", s(d, "path"));
    for k in ["usb", "serial", "driver", "card", "port"] {
        if !s(d, k).is_empty() {
            widgets::fact(ui, t, &nice(k), s(d, k));
        }
    }
    if let Some(Value::Map(x)) = d.get_path("extra") {
        for (k, v) in x {
            widgets::fact(ui, t, &nice(k), v.as_str().unwrap_or(""));
        }
    }
    if !s(d, "input_status").is_empty() {
        widgets::fact(ui, t, "Input", s(d, "input_status"));
    }
    ui.label(RichText::new(s(d, "identity")).font(font_mono(type_scale::SMALL)).color(t.text_dim))
        .on_hover_text("Stays the same across reboots and replugging");
    if s(d, "kind") == "camera" {
        if let Some(cur) = d.get_path("current") {
            widgets::fact(
                ui,
                t,
                "Now",
                &format!(
                    "{} {}×{} at {:.2} fps ({} {})",
                    s(cur, "format"),
                    f(cur, "width"),
                    f(cur, "height"),
                    f(cur, "fps"),
                    s(cur, "matrix"),
                    s(cur, "range")
                ),
            );
        }
        let modes: Vec<Value> = d.get_path("modes").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default();
        if !modes.is_empty() {
            ui.label(RichText::new(format!("{} modes", modes.len())).size(type_scale::SMALL).color(t.text_dim));
            for md in &modes {
                let rates: Vec<String> = md
                    .get_path("fps")
                    .and_then(Value::as_list)
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(Value::as_f64)
                    .map(|r| format!("{r:.2}").trim_end_matches('0').trim_end_matches('.').to_string())
                    .collect();
                ui.label(
                    RichText::new(format!("{:<6} {:>4}x{:<4} {}", s(md, "format"), f(md, "width"), f(md, "height"), rates.join(" / ")))
                        .font(font_mono(type_scale::SMALL))
                        .color(t.text_dim),
                );
            }
        }
        if !s(d, "error").is_empty() {
            ui.label(RichText::new(s(d, "error")).size(type_scale::SMALL).color(t.bright_red));
        }
    }
}
