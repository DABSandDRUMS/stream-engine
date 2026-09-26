//! Settings → Devices: discovered hardware, identity, health, and temporary previews.
//! Define and edit scene sources on Sources → Sources.

use crate::app::App;
use crate::frames::Canvas;
use crate::model::Model;
use crate::views::live::nice;
use crate::views::monitor;
use egui::{Align, Layout, Pos2, Rect, RichText, Ui, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_medium, font_mono, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::collections::HashMap;
use std::time::{Duration, Instant};

const REFRESH_EVERY: Duration = Duration::from_secs(1);
const AFTER_ACTION: Duration = Duration::from_millis(250);
/// A value being dragged locally wins over the engine echo for this long.
const EDIT_HOLD: Duration = Duration::from_millis(1200);
/// A sent rename shows until the engine confirms it, or this long.
const RENAME_WAIT: Duration = Duration::from_secs(3);
/// Camera input thumbnails in the device list.
const THUMB: Vec2 = Vec2::new(128.0, 72.0);
/// Preview leases last this long (seconds) and are renewed every `LEASE_RENEW` while shown, so
/// a closed page (or a crashed UI) releases the cameras within seconds.
const LEASE_SECS: f64 = 6.0;
const LEASE_RENEW: Duration = Duration::from_secs(2);
/// Preview pictures are fetched this often while shown.
const PREVIEW_EVERY: Duration = Duration::from_millis(250);
/// Hardware groups: (title, one line, registry kinds).
const GROUPS: [(&str, &str, &[&str]); 4] = [
    ("Discovered cameras", "Available capture cards and webcams. Discovery does not add a scene source.", &["camera"]),
    ("Sound", "Audio interfaces, sound cards and the mixing desk.", &["audio_node", "audio_card", "ucnet"]),
    ("Controllers", "Pads, pedals, faders and button boxes.", &["midi", "hid"]),
    ("Lights", "The boxes that talk to your lights, by cable or over the network.", &["serial", "artnet"]),
];

/// A camera's temporary preview as video-in last reported it.
#[derive(Clone)]
struct Thumb {
    seq: i64,
    tex: Option<egui::TextureHandle>,
    size: [usize; 2],
    state: String,
    error: String,
}

#[derive(Clone, Default)]
struct DevicesState {
    refreshed: Option<Instant>,
    /// address → (value, when edited)
    edits: HashMap<String, (Value, Instant)>,
    /// device id → (name being typed, when it was sent to the engine)
    renames: HashMap<String, (String, Option<Instant>)>,
    filter: String,
    /// camera identity → its preview picture
    thumbs: HashMap<String, Thumb>,
    /// camera identity → when its preview lease was last renewed
    leased: HashMap<String, Instant>,
    /// cameras that needed a preview this frame
    want_preview: Vec<String>,
    preview_asked: Option<Instant>,
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

    /// Take new preview pictures from the last `video_in.preview` reply (decoded once per
    /// picture; the texture is reused).
    fn take_previews(&mut self, m: &Model, ctx: &egui::Context) {
        use base64::Engine;
        let Some(list) = m.q("video_in.preview").and_then(|v| v.get_path("previews")).and_then(Value::as_list) else { return };
        self.thumbs.retain(|id, _| list.iter().any(|p| s(p, "identity") == id));
        for p in list {
            let id = s(p, "identity");
            let seq = p.get_path("seq").and_then(Value::as_i64).unwrap_or(0);
            let th = self.thumbs.entry(id.to_string()).or_insert_with(|| Thumb { seq: 0, tex: None, size: [0, 0], state: String::new(), error: String::new() });
            th.state = s(p, "state").to_string();
            th.error = s(p, "error").to_string();
            if seq == 0 {
                th.tex = None;
                continue;
            }
            if seq == th.seq {
                continue;
            }
            let Some(bytes) = p.get_path("jpeg").and_then(Value::as_str).and_then(|x| base64::engine::general_purpose::STANDARD.decode(x).ok()) else {
                continue;
            };
            let Ok(img) = image::load_from_memory_with_format(&bytes, image::ImageFormat::Jpeg) else { continue };
            let rgba = img.to_rgba8();
            let size = [rgba.width() as usize, rgba.height() as usize];
            let ci = egui::ColorImage::from_rgba_unmultiplied(size, rgba.as_raw());
            match &mut th.tex {
                Some(tex) => tex.set(ci, egui::TextureOptions::LINEAR),
                None => th.tex = Some(ctx.load_texture(format!("devices.preview.{id}"), ci, egui::TextureOptions::LINEAR)),
            }
            th.seq = seq;
            th.size = size;
        }
    }

    /// Renew preview leases for the cameras shown this frame and fetch their pictures.
    fn previews(&mut self, m: &mut Model, ctx: &egui::Context, now: Instant) {
        let want = std::mem::take(&mut self.want_preview);
        self.leased.retain(|id, _| want.contains(id));
        if want.is_empty() || !m.connected {
            return;
        }
        for id in &want {
            if self.leased.get(id).is_none_or(|t| now.duration_since(*t) >= LEASE_RENEW) {
                self.leased.insert(id.clone(), now);
                m.action("video_in.preview", Value::map().with("identity", id.as_str()).with("on", true).with("lease", LEASE_SECS));
            }
        }
        if self.preview_asked.is_none_or(|t| now.duration_since(t) >= PREVIEW_EVERY) {
            self.preview_asked = Some(now);
            let have = self.thumbs.iter().filter(|(_, t)| t.seq > 0).map(|(id, t)| Value::map().with("identity", id.as_str()).with("seq", t.seq)).collect();
            m.query("video_in.preview", Value::map().with("have", Value::List(have)));
        }
        ctx.request_repaint_after(PREVIEW_EVERY);
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
    st.take_previews(&app.m, ui.ctx());
    let hz = app.atlas_hz();
    let pics = Pics { atlas: monitor::texture(app, Canvas::Atlas, hz).map(|f| f.id), tiles: monitor::atlas_tiles(app) };
    let t = app.t.clone();
    let mut out = Vec::new();
    view(&app.m, &t, &mut st, ui, &pics, &mut out);
    if !out.is_empty() {
        st.soon(now);
    }
    st.previews(&mut app.m, ui.ctx(), now);
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

fn view(m: &Model, t: &Theme, st: &mut DevicesState, ui: &mut Ui, pics: &Pics, out: &mut Vec<Op>) {
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
    let network = reg.get_path("network").cloned().unwrap_or_default();
    let sources: Vec<Value> = m.q_list("sources").to_vec();

    ui.heading("Discovered devices");
    summary(t, st, ui, &expected, out);
    ui.add_space(spacing::L);

    let flt = st.filter.to_lowercase();
    let matches = |v: &Value| flt.is_empty() || ["id", "name", "label", "identity", "path"].iter().any(|k| s(v, k).to_lowercase().contains(&flt));
    egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
        ui.spacing_mut().item_spacing.y = spacing::S;
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
        let cx = RowCtx { m, t, pics, source_names: &source_names };
        let groups: Vec<usize> = (0..GROUPS.len()).collect();
        for row in groups.chunks(cols) {
            ui.columns(cols, |c| {
                for (k, gi) in row.iter().enumerate() {
                    hardware(&cx, st, &mut c[k], *gi, &devices, &expected, &network, &matches, out);
                }
            });
            ui.add_space(spacing::S);
        }
        other(&cx, st, ui, &devices, &matches, out);
        ui.add_space(spacing::XL);
    });
}

/// One line on how things are, plus search and "look again".
fn summary(t: &Theme, st: &mut DevicesState, ui: &mut Ui, expected: &[Value], out: &mut Vec<Op>) {
    let missing: Vec<String> = expected
        .iter()
        .filter(|e| !b(e, "present") && !b(e, "optional"))
        .map(|e| if s(e, "label").is_empty() { nice(s(e, "id")) } else { s(e, "label").to_string() })
        .collect();
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
    } else {
        (widgets::Tone::Ok, icon::CHECK, "No missing devices reported".to_string(), "Manage cameras and media on Sources → Sources.".to_string())
    };
    widgets::callout(ui, t, tone, ic, &title, &line, None);
}

// ---- hardware ------------------------------------------------------------------------------------

/// A device row's display name (the saved name, else what the device calls itself).
fn device_name(d: &Value) -> String {
    let l = s(d, "label");
    if l.is_empty() { nice(s(d, if s(d, "name").is_empty() { "id" } else { "name" })) } else { l.to_string() }
}

/// Why network discovery for one protocol isn't working, in plain words (None when it is).
fn network_problem(network: &Value, proto: &str) -> Option<(String, String)> {
    let st = network.get_path(proto)?;
    if b(st, "ok") {
        return None;
    }
    let detail = s(st, "detail").to_string();
    let what = if proto == "artnet" { "light boxes" } else { "the mixing desk" };
    let line = if detail.contains("in use") {
        format!("Can't look for {what} on the network: another program is using the connection it needs. Close that program and it starts again by itself.")
    } else {
        format!("Can't look for {what} on the network right now.")
    };
    Some((line, detail))
}

#[allow(clippy::too_many_arguments)]
fn hardware(
    cx: &RowCtx,
    st: &mut DevicesState,
    ui: &mut Ui,
    gi: usize,
    devices: &[Value],
    expected: &[Value],
    network: &Value,
    matches: &dyn Fn(&Value) -> bool,
    out: &mut Vec<Op>,
) {
    let (m, t) = (cx.m, cx.t);
    let (title, blurb, kinds) = GROUPS[gi];
    let present: Vec<&Value> = devices.iter().filter(|d| kinds.contains(&s(d, "kind")) && matches(d)).collect();
    let missing: Vec<&Value> = expected.iter().filter(|e| kinds.contains(&s(e, "kind")) && !b(e, "present") && matches(e)).collect();
    // every camera input shows (with its live thumbnail); other kinds fold unused extras away
    let (main, more): (Vec<&Value>, Vec<&Value>) =
        present.into_iter().partition(|d| gi == 0 || d.get_path("expected").is_some_and(|v| !v.is_null()) || !s(d, "source").is_empty());
    // the mixer adapter's own row, when discovery didn't list the desk
    let mixer_row = gi == 1 && m.has("mixer.16r.connected") && !devices.iter().any(|d| s(d, "kind") == "ucnet");
    let problem = match gi {
        1 => network_problem(network, "ucnet"),
        3 => network_problem(network, "artnet"),
        _ => None,
    };
    widgets::titled(
        ui,
        t,
        title,
        blurb,
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if mixer_row {
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
                missing_row(t, st, ui, e, network, out);
            }
            for d in &main {
                device_row(cx, st, ui, d, out);
            }
            if missing.is_empty() && main.is_empty() && !mixer_row {
                widgets::hint(ui, t, if more.is_empty() { "Nothing plugged in." } else { "Nothing in use yet." });
            }
            if !more.is_empty() {
                let label = if more.len() == 1 { "1 more plugged in".to_string() } else { format!("{} more plugged in", more.len()) };
                widgets::details(ui, t, ("more", gi), &label, |ui| {
                    for d in &more {
                        device_row(cx, st, ui, d, out);
                    }
                });
            }
            if let Some((line, detail)) = &problem {
                ui.add(egui::Label::new(RichText::new(line).size(type_scale::SMALL).color(t.yellow)).wrap()).on_hover_text(detail);
            }
        },
    );
}

/// Devices whose kind fits no group.
fn other(cx: &RowCtx, st: &mut DevicesState, ui: &mut Ui, devices: &[Value], matches: &dyn Fn(&Value) -> bool, out: &mut Vec<Op>) {
    let list: Vec<&Value> = devices.iter().filter(|d| !GROUPS.iter().any(|(_, _, k)| k.contains(&s(d, "kind"))) && matches(d)).collect();
    if list.is_empty() {
        return;
    }
    widgets::titled(
        ui,
        cx.t,
        "Other devices",
        "Plugged in, not sorted into a group.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            for d in list {
                device_row(cx, st, ui, d, out);
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

fn missing_row(t: &Theme, st: &mut DevicesState, ui: &mut Ui, e: &Value, network: &Value, out: &mut Vec<Op>) {
    let id = s(e, "id").to_string();
    let optional = b(e, "optional");
    let on_network = matches!(s(e, "kind"), "ucnet" | "artnet");
    let line = match (on_network, optional) {
        (true, true) => "Not found on the network (it's optional)",
        (true, false) => "Not found on the network. Check it's switched on.",
        (false, true) => "Not plugged in (it's optional)",
        (false, false) => "Not plugged in. Check the cable.",
    };
    row_frame(ui, t, |ui| {
        ui.horizontal(|ui| {
            name_field(t, st, ui, &id, &device_name(e), out);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::hold_button(ui, t, "Forget", t.bright_red, 0.8) {
                    out.push(action("devices.forget", Value::map().with("id", id.as_str())));
                }
                widgets::badge(ui, t, "Not connected", if optional { t.yellow } else { t.bright_red });
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    ui.add(egui::Label::new(RichText::new(line).size(type_scale::SMALL + 0.5).color(t.text_dim)).truncate());
                });
            });
        });
        widgets::details(ui, t, ("dev", &id), "Details", |ui| {
            widgets::fact(ui, t, "Saved as", &id);
            widgets::fact(ui, t, "Kind", s(e, "kind"));
            if !s(e, "identity").is_empty() {
                widgets::fact(ui, t, "Identity", s(e, "identity"));
            }
            if on_network {
                ui.label(
                    RichText::new("Switched on and still missing? This computer's firewall may be blocking it. Run this in a terminal to let it through:")
                        .size(type_scale::SMALL)
                        .color(t.text_dim),
                );
                ui.label(RichText::new(s(network, "firewall_hint")).font(font_mono(type_scale::SMALL)).color(t.fg));
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
        "ucnet" => "Mixing desk on the network",
        "artnet" => "Light box on the network",
        _ => "Device",
    }
}

/// What device rows need besides the row itself.
struct RowCtx<'a> {
    m: &'a Model,
    t: &'a Theme,
    pics: &'a Pics,
    source_names: &'a [(String, String)],
}

fn device_row(cx: &RowCtx, st: &mut DevicesState, ui: &mut Ui, d: &Value, out: &mut Vec<Op>) {
    let t = cx.t;
    let id = s(d, "id").to_string();
    let expected = d.get_path("expected").and_then(Value::as_str).map(str::to_string);
    let used_by = s(d, "source");
    let kind = s(d, "kind");
    let line = match (kind, d.get_path("signal").filter(|v| !v.is_null())) {
        ("camera", _) if !used_by.is_empty() => {
            let title = cx.source_names.iter().find(|(n, _)| n == used_by).map(|(_, l)| l.clone()).unwrap_or_else(|| nice(used_by));
            let sig = match d.get_path("signal").filter(|v| !v.is_null()) {
                Some(v) if v.truthy() => "",
                Some(_) => " · no signal on the cable",
                None => "",
            };
            format!("Used by {title}{sig}")
        }
        ("camera", Some(v)) if !v.truthy() => "Not used · no signal on the cable".to_string(),
        ("camera", _) => "Not used yet".to_string(),
        ("ucnet", _) if cx.m.has("mixer.16r.connected") => {
            if cx.m.b("mixer.16r.connected") {
                "Connected over the network".to_string()
            } else {
                "On the network · not connected yet".to_string()
            }
        }
        (k, _) => kind_words(k).to_string(),
    };
    // camera rows sit next to a thumbnail: the status line gets its own row under the name
    let stacked = kind == "camera";
    let use_for = |ui: &mut Ui, out: &mut Vec<Op>| {
        if kind == "camera" && used_by.is_empty() && !cx.source_names.is_empty() {
            egui::ComboBox::from_id_salt(("use_as", s(d, "identity"))).selected_text("Use for…").width(130.0).show_ui(ui, |ui| {
                for (n, title) in cx.source_names {
                    if ui.selectable_label(false, title).clicked() {
                        out.push(action("source.assign", Value::map().with("source", n.as_str()).with("identity", s(d, "identity"))));
                    }
                }
            });
        }
    };
    let line_label = |ui: &mut Ui| {
        ui.add(egui::Label::new(RichText::new(&line).size(type_scale::SMALL + 0.5).color(t.text_dim)).truncate());
    };
    let body = |ui: &mut Ui, st: &mut DevicesState, out: &mut Vec<Op>| {
        ui.horizontal(|ui| {
            name_field(t, st, ui, &id, &device_name(d), out);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if expected.is_none()
                    && widgets::button_ex(ui, t, None, "Remember", Kind::Ghost, Size::Small, 0.0, true)
                        .on_hover_text("Stream Engine will warn you when it's missing")
                        .clicked()
                {
                    out.push(action("devices.expect", Value::map().with("identity", s(d, "identity"))));
                }
                widgets::badge(ui, t, "Working", t.green);
                if !stacked {
                    use_for(ui, out);
                    ui.with_layout(Layout::left_to_right(Align::Center), line_label);
                }
            });
        });
        if stacked {
            ui.horizontal(|ui| {
                line_label(ui);
                use_for(ui, out);
            });
        }
        widgets::details(ui, t, ("dev", &id), "Details", |ui| device_details(t, ui, d, expected.as_deref()));
    };
    row_frame(ui, t, |ui| {
        if kind == "camera" {
            ui.horizontal_top(|ui| {
                camera_thumb(cx, st, ui, d);
                ui.vertical(|ui| body(ui, st, out));
            });
        } else {
            body(ui, st, out);
        }
    });
}

/// The part of a picture `size` that fills a 16:9 box (center crop).
fn crop_16_9(size: [usize; 2]) -> Rect {
    let aspect = size[0] as f32 / size[1].max(1) as f32;
    let target = 16.0 / 9.0;
    if aspect < target {
        let m = (1.0 - aspect / target) / 2.0;
        Rect::from_min_max(Pos2::new(0.0, m), Pos2::new(1.0, 1.0 - m))
    } else {
        let m = (1.0 - target / aspect) / 2.0;
        Rect::from_min_max(Pos2::new(m, 0.0), Pos2::new(1.0 - m, 1.0))
    }
}

/// A camera input's live thumbnail: the multiview picture of the source showing it, else a
/// temporary preview (asked for while the thumbnail is on screen).
fn camera_thumb(cx: &RowCtx, st: &mut DevicesState, ui: &mut Ui, d: &Value) {
    let t = cx.t;
    let (rect, resp) = ui.allocate_exact_size(THUMB, egui::Sense::hover());
    let used_by = s(d, "source");
    if let Some(tex) = Some(used_by).filter(|u| !u.is_empty()).and_then(|u| cx.pics.of(u)) {
        widgets::video_frame(ui, t, rect, Some(tex), "", None, None);
        return;
    }
    let identity = s(d, "identity").to_string();
    if ui.is_rect_visible(rect) {
        st.want_preview.push(identity.clone());
    }
    let th = st.thumbs.get(&identity);
    let tex = th.and_then(|th| th.tex.as_ref().map(|tex| (tex.id(), crop_16_9(th.size))));
    let (label, tip) = match th.map(|th| (th.state.as_str(), th.error.as_str())) {
        Some(("live", _)) => ("", None),
        Some(("waiting", _)) => ("Opening…", Some("A camera source is opening this input.".to_string())),
        Some(("error", e)) => ("Can't show", Some(format!("Can't show a picture: {e}."))),
        Some(("no_picture", _)) => ("No picture", Some("Nothing is coming in. Is the camera on?".to_string())),
        Some(("no_signal", _)) => ("No signal", Some("This input is connected but gets no picture. Is the camera on, and is its HDMI cable in?".to_string())),
        _ => ("Starting…", None),
    };
    if tex.is_some() {
        widgets::video_frame(ui, t, rect, tex, label, None, None);
    } else {
        let p = ui.painter_at(rect);
        p.rect_filled(rect, radius::TILE, egui::Color32::from_rgb(8, 9, 11));
        p.rect_stroke(rect, radius::TILE, egui::Stroke::new(1.0, t.border), egui::StrokeKind::Inside);
        p.text(rect.center(), egui::Align2::CENTER_CENTER, label, font_medium(type_scale::SMALL), t.text_faint);
    }
    if let Some(tip) = tip {
        resp.on_hover_text(tip);
    }
}

fn device_details(t: &Theme, ui: &mut Ui, d: &Value, expected: Option<&str>) {
    widgets::fact(ui, t, "Name", s(d, "name"));
    if let Some(e) = expected {
        widgets::fact(ui, t, "Saved as", e);
    }
    widgets::fact(ui, t, "Kind", s(d, "kind"));
    widgets::fact(ui, t, if s(d, "bus") == "network" { "Address" } else { "Node" }, s(d, "path"));
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
