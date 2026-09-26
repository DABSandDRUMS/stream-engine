//! Sound → Mix (§8, §15.6). One strip per sound channel (level, meter, mute, whether it's on
//! stream, whether it's being lowered while you talk); the selected channel's effects as cards
//! and the sounds that play into it; "lowered while you talk" (ducking) and tempo; a sounds pad
//! grid and drum triggers; and an Advanced sub-view with the audio system internals (PipeWire
//! state and links, buffer/latency, real-time scheduling, dropouts, analysis).
//! Client of the se-audio `audio.mix` query, `audio.*` state/signals, and `audio.*` actions.

use crate::app::{App, ViewId};
use crate::views::live::nice;
use crate::views::mix::{bus_label, bus_rank, db_text};
use egui::{Align, Align2, CornerRadius, Layout, Pos2, Rect, Response, RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2};
use se_proto::{Op, Value, ValueType};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_bold, font_medium, font_mono, font_semibold, mix as blend, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, LedState, Size, Tone, icon};
use std::time::{Duration, Instant};

const MIX_EVERY: Duration = Duration::from_millis(250);
const META_EVERY: Duration = Duration::from_secs(3);
const DB_MIN: f64 = -60.0;
const DB_MAX: f64 = 12.0;
const VIEWS: [&str; 3] = ["Channels", "Sounds & drums", "Advanced"];
/// Strip height besides the fader: name, "on stream", level, mute, status line, margins.
const STRIP_EXTRA: f32 = 172.0;

#[derive(Clone, Default)]
struct AudioState {
    mix_at: Option<Instant>,
    meta_at: Option<Instant>,
    view: usize,
    /// Bus whose effects and sources are shown.
    selected: Option<String>,
}

fn db_to_fader(db: f64) -> f32 {
    // audio taper: more travel near unity
    let x = ((db - DB_MIN) / (DB_MAX - DB_MIN)).clamp(0.0, 1.0);
    x.powf(1.0 / 1.6) as f32
}

fn fader_to_db(v: f32) -> f64 {
    DB_MIN + (v as f64).powf(1.6) * (DB_MAX - DB_MIN)
}

fn lin_db(x: f32) -> f32 {
    20.0 * x.max(1e-5).log10()
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn list<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get_path(k).and_then(Value::as_list).unwrap_or(&[])
}

fn health_led(v: Option<&Value>) -> LedState {
    match v.and_then(|h| h.get_path("status")).and_then(Value::as_str) {
        Some("pass") => LedState::Healthy,
        Some("warn") => LedState::Armed,
        Some("fail") => LedState::Error,
        _ => LedState::Idle,
    }
}

fn health_detail(app: &App, key: &str) -> String {
    app.m.get(key).and_then(|h| h.get_path("detail")).and_then(Value::as_str).unwrap_or("").to_string()
}

fn set(app: &mut App, address: &str, value: Value) {
    app.m.command(Op::Set { address: address.into(), value });
}

/// A left-aligned label in a fixed-width column (cut with "…" when longer).
fn fixed_label(ui: &mut Ui, w: f32, text: RichText) {
    ui.allocate_ui_with_layout(Vec2::new(w, 20.0), Layout::left_to_right(Align::Center), |ui| {
        ui.set_width(w);
        ui.add(egui::Label::new(text).truncate());
    });
}

// ---- who hears what ----------------------------------------------------------------------------

fn bus_list(data: &Value) -> Vec<&Value> {
    let mut v: Vec<&Value> = list(data, "buses").iter().collect();
    v.sort_by_key(|b| (bus_rank(s(b, "name")), s(b, "name").to_string()));
    v
}

fn consumers<'a>(data: &'a Value, node: &str) -> &'a [Value] {
    data.get_path("consumers").and_then(|c| c.get_path(node)).and_then(Value::as_list).unwrap_or(&[])
}

fn program_bus(data: &Value) -> Option<&Value> {
    list(data, "buses").iter().find(|b| s(b, "name") == "program")
}

/// Something (normally OBS) is capturing the "Everything" channel.
fn program_heard(data: &Value) -> bool {
    program_bus(data).is_some_and(|p| !consumers(data, s(p, "node")).is_empty())
}

/// This channel reaches the stream: captured itself, or mixed into a captured "Everything".
fn heard(data: &Value, b: &Value) -> bool {
    !consumers(data, s(b, "node")).is_empty() || (b.get_path("to_program").is_some_and(Value::truthy) && program_heard(data))
}

// ---- page ---------------------------------------------------------------------------------------

pub fn ui(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    let id = ui.id().with("audio_view");
    let mut st: AudioState = ui.data_mut(|d| d.get_temp::<AudioState>(id).unwrap_or_default());
    let now = Instant::now();
    if app.m.connected {
        if st.mix_at.is_none_or(|x| now.duration_since(x) >= MIX_EVERY) {
            st.mix_at = Some(now);
            app.m.query("audio.mix", Value::Null);
        }
        if st.meta_at.is_none_or(|x| now.duration_since(x) >= META_EVERY) {
            st.meta_at = Some(now);
            app.m.fetch("audio.**");
        }
    }
    ui.ctx().request_repaint_after(Duration::from_millis(33));
    let data = app.m.q("audio.mix").cloned().unwrap_or_default();

    top_bar(app, ui, &t, &data, &mut st);
    ui.add_space(spacing::L);
    let body_h = ui.available_height();
    if data.is_null() {
        widgets::panel(ui, &t, |ui| {
            ui.set_width(ui.available_width());
            let connected = app.m.connected;
            let (title, body) = if connected {
                ("Sound isn't running", "The sound part of Stream Engine hasn't started. Troubleshooting shows why.")
            } else {
                ("Waiting for the engine…", "Your sound channels show up here as soon as Stream Engine is running.")
            };
            if widgets::empty_state(ui, &t, icon::VOLUME, title, body, connected.then_some("Open troubleshooting")) {
                app.open_view(ViewId::Troubleshoot);
            }
        });
    } else {
        egui::ScrollArea::vertical().id_salt("audio").auto_shrink([false, false]).show(ui, |ui| match st.view {
            0 => channels(app, ui, &t, &data, &mut st, body_h),
            1 => sounds(app, ui, &t, &data),
            _ => advanced(app, ui, &t, &data),
        });
    }
    ui.data_mut(|d| d.insert_temp(id, st));
}

fn top_bar(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value, st: &mut AudioState) {
    ui.horizontal(|ui| {
        widgets::segmented(ui, t, &mut st.view, &VIEWS);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            if widgets::hold_button(ui, t, "Reset sound", t.bright_red, 0.6) {
                app.m.action("audio.panic", Value::Null);
            }
            if data.is_null() {
                return;
            }
            ui.add_space(spacing::S);
            let trouble =
                ["pipewire", "xruns", "rt"].iter().any(|k| matches!(health_led(app.m.get(&format!("health.audio.{k}"))), LedState::Armed | LedState::Error))
                    || app.m.f("perf.audio.allocs") > 0.0
                    || !list(data, "errors").is_empty();
            if trouble
                && widgets::button_ex(ui, t, Some(icon::WARN), "Sound needs a look", Kind::Secondary, Size::Small, 0.0, true)
                    .on_hover_text("Something in the sound system isn't right. Open Advanced to see what.")
                    .clicked()
            {
                st.view = 2;
            }
            // on Channels a missing capture gets its own banner with the fix
            let heard = program_heard(data);
            if heard {
                widgets::pill(ui, t, icon::VOLUME, "OBS hears your sound", LedState::Healthy);
            } else if st.view != 0
                && widgets::pill(ui, t, icon::VOLUME, "OBS can't hear your sound yet", LedState::Armed).on_hover_text("Show me how to fix it").clicked()
            {
                st.view = 0;
            }
        });
    });
}

// ---- channels -----------------------------------------------------------------------------------

fn channels(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value, st: &mut AudioState, body_h: f32) {
    let buses = bus_list(data);
    if buses.is_empty() {
        widgets::panel(ui, t, |ui| {
            ui.set_width(ui.available_width());
            widgets::empty_state(ui, t, icon::MIX, "No sound channels yet", "Channels like Mic, Music and Game come from your project's sound setup.", None);
        });
        return;
    }
    if !st.selected.as_deref().is_some_and(|sel| buses.iter().any(|b| s(b, "name") == sel)) {
        st.selected = buses.iter().find(|b| s(b, "name") != "program").or(buses.first()).map(|b| s(b, "name").to_string());
    }
    if !program_heard(data) {
        obs_callout(app, ui, t, data);
        ui.add_space(spacing::L);
    }
    let w = ui.available_width();
    let gap = spacing::L;
    let side = (w * 0.26).clamp(320.0, 420.0);
    let main_w = w - side - gap;
    let fader_h = (body_h * 0.34).clamp(180.0, 380.0);
    let selected = buses.iter().find(|b| Some(s(b, "name")) == st.selected.as_deref()).copied();
    // wide screens: strips, the selected channel and the side column sit side by side
    let n = buses.len() as f32;
    let strips_w = n * 176.0 + (n - 1.0) * 10.0 + spacing::L + 34.0;
    let wide = main_w - strips_w - gap >= 720.0;
    let space = |ui: &mut Ui| ui.add_space(gap - ui.spacing().item_spacing.x);
    ui.horizontal_top(|ui| {
        ui.allocate_ui_with_layout(Vec2::new(main_w, 0.0), Layout::top_down(Align::Min), |ui| {
            if wide {
                ui.horizontal_top(|ui| {
                    ui.allocate_ui_with_layout(Vec2::new(strips_w, 0.0), Layout::top_down(Align::Min), |ui| strips_card(app, ui, t, data, &buses, st, fader_h));
                    space(ui);
                    ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 0.0), Layout::top_down(Align::Min), |ui| {
                        if let Some(b) = selected {
                            channel_card(app, ui, t, data, b);
                        }
                    });
                });
            } else {
                strips_card(app, ui, t, data, &buses, st, fader_h);
                if let Some(b) = selected {
                    ui.add_space(gap);
                    channel_card(app, ui, t, data, b);
                }
            }
        });
        space(ui);
        ui.allocate_ui_with_layout(Vec2::new(ui.available_width(), 0.0), Layout::top_down(Align::Min), |ui| {
            ducking_card(app, ui, t, data);
            ui.add_space(gap);
            tempo_card(app, ui, t, data);
        });
    });
    ui.add_space(spacing::XL);
}

fn obs_callout(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value) {
    let obs_open = app.m.b("obs.link");
    let open = widgets::callout(
        ui,
        t,
        Tone::Warn,
        icon::VOLUME,
        "OBS can't hear your sound yet",
        if obs_open {
            "Your viewers won't hear anything on this page until OBS picks it up. One click adds it; nothing in OBS is removed."
        } else {
            "Your viewers won't hear anything on this page until OBS picks it up. Open OBS first."
        },
        Some(if obs_open { "Add our sound to OBS" } else { "Open OBS" }),
    );
    if open {
        if obs_open {
            app.m.command(Op::Action { name: "obs.setup".into(), args: Value::Null });
        } else {
            crate::views::status::open_obs(app);
        }
    }
    let name = program_bus(data).map(|p| s(p, "name")).unwrap_or("program");
    widgets::details(ui, t, "obs-sound-howto", "Or do it by hand in OBS", |ui| {
        for (i, step) in [
            "In OBS, click the + button under Sources.".to_string(),
            "Choose “Audio Input Capture (PipeWire)” and press OK.".to_string(),
            format!("Under Device, pick “stream-engine {name}” and press OK."),
            "Mute any other microphone or desktop sound in OBS, so nothing plays twice.".to_string(),
        ]
        .iter()
        .enumerate()
        {
            ui.label(RichText::new(format!("{}.  {step}", i + 1)).color(t.fg));
        }
    });
}

fn strips_card(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value, buses: &[&Value], st: &mut AudioState, fader_h: f32) {
    widgets::titled(
        ui,
        t,
        "Channels",
        "Drag a fader to set the level. Click a channel to see its effects.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let gap = 10.0;
            let has_program = buses.iter().any(|b| s(b, "name") == "program");
            let extra = if has_program { spacing::L } else { 0.0 };
            let n = buses.len() as f32;
            let sw = ((ui.available_width() - gap * (n - 1.0) - extra) / n).clamp(112.0, 176.0);
            egui::ScrollArea::horizontal().id_salt("audio-strips").show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for b in buses {
                        let name = s(b, "name");
                        if name == "program" {
                            ui.add_space(extra);
                        }
                        if strip(app, ui, t, data, b, Vec2::new(sw, fader_h), st.selected.as_deref() == Some(name)) {
                            st.selected = Some(name.to_string());
                        }
                    }
                });
            });
        },
    );
}

/// One channel strip. The whole tile selects the channel; the fader and mute button sit on top.
/// Returns true when the tile was clicked.
fn strip(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value, b: &Value, size: Vec2, selected: bool) -> bool {
    let name = s(b, "name").to_string();
    let addr = s(b, "address").to_string();
    let gain_a = format!("{addr}.gain");
    let mute_a = format!("{addr}.mute");
    let muted = app.m.b(&mute_a);
    let db = app.m.get(&gain_a).and_then(Value::as_f64).unwrap_or(0.0);
    let level = app.m.sig(&format!("audio.{name}.level")).unwrap_or(0.0);
    let peak = app.m.sig(&format!("audio.{name}.peak")).unwrap_or(level);
    let lowered = app.m.f(&format!("{addr}.ducked")) < -0.5;
    let duck_target = b.get_path("ducked").is_some_and(Value::truthy);
    let modulated = app.is_modulated(&gain_a);
    let label = bus_label(&name);
    let (sw, fader_h) = (size.x, size.y);

    let (rect, resp) = ui.allocate_exact_size(Vec2::new(sw, fader_h + STRIP_EXTRA), Sense::click());
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    let p = ui.painter().clone();
    let fill = if selected {
        blend(t.surface, t.accent, 0.10)
    } else if resp.hovered() {
        blend(t.surface_hi, t.fg, 0.04)
    } else {
        t.surface_hi
    };
    let stroke = if selected { Stroke::new(1.5, t.accent) } else { Stroke::new(1.0, t.border) };
    p.rect(rect, CornerRadius::same(radius::TILE), fill, stroke, StrokeKind::Inside);
    let cx = rect.center().x;
    let mut y = rect.top() + 14.0;

    p.text(Pos2::new(cx, y + 10.0), Align2::CENTER_CENTER, &label, font_semibold(type_scale::BODY + 0.5), if muted { t.text_dim } else { t.fg });
    y += 26.0;
    // Only "Everything" talks about OBS; the others say whether they reach it (the banner above
    // already explains a missing OBS capture, so they stay calm then).
    let captured = !consumers(data, s(b, "node")).is_empty();
    let into_program = b.get_path("to_program").is_some_and(Value::truthy);
    let (ic, cap, cap_c) = match (name == "program", heard(data, b)) {
        (true, true) => (icon::CHECK, "OBS hears this", t.green),
        (true, false) => (icon::WARN, "OBS can't hear this", t.yellow),
        (false, true) if captured && !into_program => (icon::CHECK, "Captured on its own", t.text_dim),
        (false, true) => (icon::CHECK, "On stream", t.text_dim),
        (false, false) if into_program => (icon::RIGHT, "Into Everything", t.text_dim),
        (false, false) => (icon::WARN, "Not on stream", t.yellow),
    };
    p.text(Pos2::new(cx, y + 8.0), Align2::CENTER_CENTER, format!("{ic}  {cap}"), font(type_scale::SMALL), cap_c);
    y += 28.0;

    let (fw, mw, gap) = (40.0, 10.0, 12.0);
    let x0 = cx - (fw + gap + mw) / 2.0;
    let fr = Rect::from_min_size(Pos2::new(x0, y), Vec2::new(fw, fader_h));
    let mr = Rect::from_min_size(Pos2::new(x0 + fw + gap, y), Vec2::new(mw, fader_h));
    let mut v = db_to_fader(db);
    let r = widgets::fader(&mut ui.new_child(UiBuilder::new().id_salt(("fader", &name)).max_rect(fr)), t, fr.size(), &mut v, modulated || lowered);
    if r.changed() {
        set(app, &gain_a, Value::Float((fader_to_db(v) * 10.0).round() / 10.0));
    }
    if r.double_clicked() {
        set(app, &gain_a, Value::Float(0.0));
    }
    r.on_hover_text(format!("{label}: {} · double-click for 0 dB", db_text(db)));
    widgets::meter(&mut ui.new_child(UiBuilder::new().id_salt(("meter", &name)).max_rect(mr)), t, mr.size(), if muted { 0.0 } else { level }, peak)
        .on_hover_text(format!("Loudest just now: {:.1} dB", lin_db(peak)));
    y += fader_h + 10.0;

    let (db_txt, fid) =
        if db <= DB_MIN + 0.05 { ("Off".to_string(), font_medium(type_scale::SMALL + 0.5)) } else { (db_text(db), font_mono(type_scale::SMALL + 0.5)) };
    p.text(Pos2::new(cx, y + 9.0), Align2::CENTER_CENTER, db_txt, fid, t.fg);
    y += 26.0;

    let br = Rect::from_min_size(Pos2::new(rect.left() + 12.0, y), Vec2::new(sw - 24.0, 28.0));
    let (ic, lbl, kind, tip) =
        if muted { (icon::MUTE, "Muted", Kind::Danger, "Click to unmute") } else { (icon::VOLUME, "Mute", Kind::Secondary, "Silence this channel") };
    let m = widgets::button_ex(&mut ui.new_child(UiBuilder::new().id_salt(("mute", &name)).max_rect(br)), t, Some(ic), lbl, kind, Size::Small, sw - 24.0, true);
    if m.on_hover_text(tip).clicked() {
        set(app, &mute_a, Value::Bool(!muted));
    }
    y += 40.0;

    let (txt, c) = if peak > 0.89 {
        ("Too loud!", t.bright_red)
    } else if lowered {
        ("Lowered now", t.modulated())
    } else if duck_target {
        ("Lowers when you talk", t.text_faint)
    } else {
        ("", t.text_faint)
    };
    if !txt.is_empty() {
        p.text(Pos2::new(cx, y + 8.0), Align2::CENTER_CENTER, txt, font_medium(type_scale::SMALL), c);
    }
    resp.clicked()
}

// ---- selected channel -----------------------------------------------------------------------------

fn channel_card(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value, b: &Value) {
    let name = s(b, "name").to_string();
    let addr = s(b, "address").to_string();
    let program = name == "program";
    let blurb = if program {
        "Everything your viewers hear, after all the other channels are mixed together."
    } else {
        "Its effects, and the sounds that play into it."
    };
    widgets::titled(
        ui,
        t,
        &bus_label(&name),
        blurb,
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let lim_a = format!("{addr}.limiter");
            let mut lim = app.m.b(&lim_a);
            if widgets::toggle_row(ui, t, "Safety limiter", "Stops this channel from ever getting too loud.", &mut lim).changed() {
                set(app, &lim_a, Value::Bool(lim));
            }
            ui.add_space(spacing::L);

            let mut fx: Vec<(String, &Value)> = list(b, "fx").iter().map(|f| (String::new(), f)).collect();
            for i in list(data, "inputs").iter().filter(|i| s(i, "bus") == name) {
                fx.extend(list(i, "fx").iter().map(|f| (source_label(s(i, "name")), f)));
            }
            widgets::section(ui, t, "", &if fx.is_empty() { "Effects".to_string() } else { format!("Effects ({})", fx.len()) });
            if fx.is_empty() {
                widgets::empty_state(
                    ui,
                    t,
                    icon::WAND,
                    "No effects on this channel",
                    "Effects like reverb or a DJ filter are added in your project's sound setup.",
                    None,
                );
            } else {
                fx_grid(app, ui, t, data, &fx);
            }
            ui.add_space(spacing::M);

            widgets::section(ui, t, "", "What plays here");
            let srcs = sources(data, Some(&name));
            if program {
                widgets::hint(ui, t, "Every other channel that's on stream plays into this one.");
            } else if srcs.is_empty() {
                widgets::hint(ui, t, "Nothing plays into this channel yet.");
            }
            for src in &srcs {
                source_row(app, ui, t, src);
            }
            ui.add_space(spacing::S);
            widgets::details(ui, t, ("bus", &name), "Details", |ui| {
                let node = s(b, "node");
                let caps = consumers(data, node).iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
                widgets::fact(ui, t, "Sound system name", &format!("stream-engine {name} ({node})"));
                widgets::fact(ui, t, "Goes into Everything", if b.get_path("to_program").is_some_and(Value::truthy) { "yes" } else { "no" });
                widgets::fact(ui, t, "Captured by", if caps.is_empty() { "nothing" } else { &caps });
                widgets::fact(ui, t, "Address", &addr);
            });
        },
    );
}

/// Friendly effect type and one line on what it does. `mode` is the tone filter's current
/// mode, which decides what it actually does.
fn fx_kind(kind: &str, mode: &str) -> (&'static str, &'static str) {
    match kind {
        "svf" => match mode {
            "hp" => ("Rumble filter", "Takes out low rumble and thumps"),
            "lp" => ("High cut", "Takes out the highs for a softer sound"),
            "bp" => ("Middle only", "Keeps only the middle, like a phone call"),
            "notch" => ("Notch", "Takes out one narrow tone"),
            _ => ("Tone filter", "Takes out the lows or the highs"),
        },
        "eq" => ("EQ", "Shapes the tone"),
        "djfilter" => ("DJ filter", "One knob: left sounds muffled, right sounds thin"),
        "compressor" => ("Compressor", "Evens out loud and quiet parts"),
        "limiter" => ("Limiter", "Keeps it from getting too loud"),
        "gate" => ("Noise gate", "Silences background noise between sounds"),
        "transient" => ("Punch", "More or less attack on each hit"),
        "saturation" => ("Warmth", "Adds gentle grit"),
        "distortion" => ("Distortion", "Adds crunch"),
        "bitcrush" => ("Bitcrusher", "Lo-fi, retro sound"),
        "delay" => ("Echo", "Repeats in time with the beat"),
        "reverb" => ("Reverb", "Adds room and space"),
        "chorus" => ("Chorus", "Thicker, shimmering sound"),
        "flanger" => ("Flanger", "Jet-plane sweep"),
        "phaser" => ("Phaser", "Swirling sweep"),
        "pitch" => ("Pitch shift", "Makes it higher or lower"),
        "stutter" => ("Stutter", "Repeats a slice of the beat"),
        "tapestop" => ("Tape stop", "Slows down to a stop"),
        "vinylbrake" => ("Vinyl brake", "Record slow-down or backspin"),
        "reverse" => ("Reverse", "Plays backwards"),
        "chopper" => ("Chopper", "Chops the sound in time with the beat"),
        "gain" => ("Volume & width", "Level, balance and stereo width"),
        k if k.starts_with("patch.") => ("Custom effect", ""),
        _ => ("Effect", ""),
    }
}

/// Settings that only matter for fine-tuning: always under "More settings".
const FINE_TUNING: &[&str] = &[
    "resonance",
    "q",
    "low_q",
    "mid_q",
    "high_q",
    "knee",
    "lookahead",
    "hold",
    "range",
    "stages",
    "predelay",
    "damping",
    "polarity",
    "oversample",
    "makeup",
    "curve",
    "duty",
    "env",
];

/// Friendly name of an effect setting.
fn param_label(name: &str) -> String {
    match name {
        "cutoff" => "Where it cuts".into(),
        "mode" => "Takes out".into(),
        "resonance" => "Edge".into(),
        "division" => "Slice length".into(),
        "quantize" => "Starts on".into(),
        "decay" => "Fades each repeat".into(),
        "gate" => "Length".into(),
        "feedback" => "Repeats".into(),
        "threshold" => "Kicks in at".into(),
        "ratio" => "Strength".into(),
        "semitones" => "Notes up or down".into(),
        "size" => "Room size".into(),
        "rate" => "Speed".into(),
        "freq" => "Tone".into(),
        "time" | "delay" => "Time".into(),
        "output" => "Volume".into(),
        n => nice(n),
    }
}

fn fx_grid(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value, fx: &[(String, &Value)]) {
    let gap = spacing::M;
    let avail = ui.available_width();
    let cols = (((avail + gap) / (300.0 + gap)).floor() as usize).clamp(1, 4);
    let cw = (avail - gap * (cols - 1) as f32) / cols as f32;
    for row in fx.chunks(cols) {
        ui.horizontal_top(|ui| {
            for (i, (from, f)) in row.iter().enumerate() {
                if i > 0 {
                    ui.add_space(gap - ui.spacing().item_spacing.x);
                }
                ui.allocate_ui_with_layout(Vec2::new(cw, 0.0), Layout::top_down(Align::Min), |ui| fx_card(app, ui, t, data, f, from, cw));
            }
        });
        ui.add_space(gap);
    }
}

/// How many parameters show on the card; the rest sit under "More settings".
const FX_MAIN_PARAMS: usize = 3;

fn fx_card(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value, f: &Value, from: &str, w: f32) {
    let addr = s(f, "address").to_string();
    let kind = s(f, "kind");
    let (kind_name, blurb) = fx_kind(kind, app.m.str(&format!("{addr}.mode")));
    let title = nice(s(f, "name"));
    let active = list(data, "fx_active").iter().any(|x| s(x, "address") == addr && x.get_path("active").is_some_and(Value::truthy));
    let trigger = f.get_path("trigger").is_some_and(Value::truthy);
    let bypass_a = format!("{addr}.bypass");
    let mut on = !app.m.b(&bypass_a);
    let lit = on && active;
    egui::Frame::new()
        .fill(t.surface_hi)
        .stroke(Stroke::new(1.0, if lit { blend(t.border, t.accent, 0.5) } else { t.border }))
        .corner_radius(CornerRadius::same(radius::TILE))
        .inner_margin(egui::Margin::same(14))
        .show(ui, |ui| {
            ui.set_width(w - 30.0);
            ui.horizontal(|ui| {
                widgets::led(ui, t, if lit { LedState::Active } else { LedState::Idle });
                ui.label(RichText::new(&title).font(font_semibold(type_scale::BODY + 1.0)).color(if on { t.fg } else { t.text_dim }));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    let tip = if on { "On — click to turn it off" } else { "Off — click to turn it on" };
                    if widgets::toggle(ui, t, &mut on).on_hover_text(tip).changed() {
                        set(app, &bypass_a, Value::Bool(!on));
                    }
                });
            });
            let sub = match (blurb.is_empty(), from.is_empty()) {
                (true, true) => kind_name.to_string(),
                (true, false) => format!("{kind_name} · on {from}"),
                (false, true) => format!("{kind_name} · {blurb}"),
                (false, false) => format!("{kind_name} · {blurb} · on {from}"),
            };
            ui.label(RichText::new(sub).size(type_scale::SMALL).color(t.text_dim));
            ui.add_space(spacing::S);
            if trigger {
                ui.horizontal(|ui| {
                    let (ic, lbl, k, tip) = if active {
                        (icon::STOP, "Stop", Kind::Danger, "Stop the effect")
                    } else {
                        (icon::PLAY, "Start", Kind::Secondary, "Start the effect. It keeps going until you press Stop.")
                    };
                    if widgets::button_ex(ui, t, Some(ic), lbl, k, Size::Small, 92.0, on).on_hover_text(tip).clicked() {
                        app.m.command(if active { Op::Release { address: addr.clone() } } else { Op::Trigger { address: addr.clone(), payload: Value::Null } });
                    }
                    let env = app.m.f(&format!("{addr}.env")) as f32;
                    let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
                    ui.painter().rect_filled(rect, CornerRadius::same(3), t.inset);
                    ui.painter().rect_filled(
                        Rect::from_min_size(rect.min, Vec2::new(rect.width() * env.clamp(0.0, 1.0), rect.height())),
                        CornerRadius::same(3),
                        t.modulated(),
                    );
                });
                ui.add_space(spacing::S);
            }
            let wet_a = format!("{addr}.wet");
            let mut wet = app.m.f(&wet_a) as f32;
            let r = slider_row(ui, t, "Amount", &format!("{:.0}%", wet * 100.0), |ui| ui.add(egui::Slider::new(&mut wet, 0.0..=1.0).show_value(false)));
            if r.on_hover_text("How much of the effect you hear").changed() {
                set(app, &wet_a, Value::Float(wet as f64));
            }
            let params: Vec<String> = list(f, "params")
                .iter()
                .filter_map(Value::as_str)
                .map(|pn| if pn.starts_with("patch.") { pn.to_string() } else { format!("{addr}.{pn}") })
                .collect();
            let fine = |pa: &str| FINE_TUNING.contains(&pa.rsplit('.').next().unwrap_or(pa));
            let main: Vec<&String> = params.iter().filter(|p| !fine(p)).take(FX_MAIN_PARAMS).collect();
            for pa in &main {
                param_control(app, ui, t, pa);
            }
            ui.add_space(spacing::XS);
            let rest: Vec<&String> = params.iter().filter(|p| !main.contains(p)).collect();
            let more = if rest.is_empty() { "Details" } else { "More settings" };
            widgets::details(ui, t, ("fx", &addr), more, |ui| {
                for pa in &rest {
                    param_control(app, ui, t, pa);
                }
                let dry_a = format!("{addr}.dry");
                let mut dry = app.m.f(&dry_a) as f32;
                let r =
                    slider_row(ui, t, "Original sound", &format!("{:.0}%", dry * 100.0), |ui| ui.add(egui::Slider::new(&mut dry, 0.0..=1.0).show_value(false)));
                if r.on_hover_text("How much of the sound without the effect you hear (dry)").changed() {
                    set(app, &dry_a, Value::Float(dry as f64));
                }
                ui.add_space(spacing::XS);
                widgets::fact(ui, t, "Type", kind);
                let lat = f.get_path("latency").and_then(Value::as_i64).unwrap_or(0);
                if lat > 0 {
                    widgets::fact(ui, t, "Adds delay", &format!("{lat} samples"));
                }
                let when = s(f, "when");
                if !when.is_empty() {
                    widgets::fact(ui, t, "Only when", when);
                }
                widgets::fact(ui, t, "Address", &addr);
            });
        });
}

/// Label + value on one line, a full-width slider below.
fn slider_row(ui: &mut Ui, t: &Theme, label: &str, value: &str, add: impl FnOnce(&mut Ui) -> Response) -> Response {
    ui.horizontal(|ui| {
        ui.label(RichText::new(label).font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
        // mono only for numbers; words read in the normal font
        let numeric = value.starts_with(|c: char| c.is_ascii_digit() || c == '+' || c == '-');
        let fid = if numeric { font_mono(type_scale::SMALL) } else { font_medium(type_scale::SMALL + 0.5) };
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| ui.label(RichText::new(value).font(fid).color(t.fg)));
    });
    let r = ui
        .scope(|ui| {
            ui.spacing_mut().slider_width = ui.available_width();
            add(ui)
        })
        .inner;
    ui.add_space(spacing::XS);
    r
}

/// Choices read as words: `hp` → "Lows" (what the filter takes out), `1/8t` → "1/8 triplet".
fn option_label(o: &str) -> String {
    match o {
        "lp" => "Highs".into(),
        "hp" => "Lows".into(),
        "bp" => "Lows and highs".into(),
        "notch" => "One narrow tone".into(),
        "off" => "Right away".into(),
        "1 bar" => "Next bar".into(),
        _ if o.contains('/') => {
            if let Some(b) = o.strip_suffix('t') {
                format!("{b} triplet")
            } else if let Some(b) = o.strip_suffix('d') {
                format!("{b} dotted")
            } else {
                o.to_string()
            }
        }
        _ => nice(o),
    }
}

/// Where `x` sits in `lo..=hi` (0–1), on a log scale for frequencies and times.
fn position(x: f64, lo: f64, hi: f64, log: bool) -> f64 {
    let p = if log && lo > 0.0 { (x / lo).ln() / (hi / lo).ln() } else { (x - lo) / (hi - lo).max(1e-9) };
    p.clamp(0.0, 1.0)
}

/// A setting's value in words or plain numbers: frequencies and times as low…high /
/// short…long (the exact figure is in the tooltip), levels as "+1.5 dB", fractions as "50%".
fn value_text(x: f64, unit: &str, lo: f64, hi: f64) -> String {
    let pick = |words: [&str; 5]| words[((position(x, lo, hi, lo > 0.0) * 5.0) as usize).min(4)].to_string();
    let pct = |v: f64| format!("{}%", (v * 100.0).round() as i64);
    match unit {
        "Hz" => pick(["Very low", "Low", "Middle", "High", "Very high"]),
        "ms" | "s" => pick(["Very short", "Short", "Medium", "Long", "Very long"]),
        "dB" => db_text(x),
        "st" => match x.round() as i64 {
            0 => "Same pitch".into(),
            n => format!("{n:+} notes"),
        },
        "" if lo == 0.0 && hi == 1.0 => pct(x),
        "" if lo == -1.0 && hi == 1.0 => pct(x),
        "" => format!("{x:.2}"),
        u if hi - lo > 20.0 => format!("{x:.0} {u}"),
        u => format!("{x:.2} {u}"),
    }
}

/// The exact figure behind a worded value, for the tooltip.
fn exact_text(x: f64, unit: &str) -> String {
    match unit {
        "Hz" if x >= 1000.0 => format!("{:.1} kHz", x / 1000.0),
        "Hz" => format!("{x:.0} Hz"),
        "ms" => format!("{x:.0} ms"),
        u => format!("{x:.2} {u}"),
    }
}

/// An effect parameter as a slider, switch or choice, from the address metadata.
fn param_control(app: &mut App, ui: &mut Ui, t: &Theme, addr: &str) {
    let meta = app.m.meta.get(addr).cloned();
    if meta.is_none() && app.m.connected {
        app.fetch_meta_once(addr);
    }
    let label = param_label(addr.rsplit('.').next().unwrap_or(addr));
    let v = app.m.get(addr).cloned().unwrap_or_default();
    let tip = meta.as_ref().and_then(|m| m.description.clone()).unwrap_or_default();
    match meta.as_ref().map(|m| m.ty) {
        Some(ValueType::Enum) => {
            let opts = meta.as_ref().map(|m| m.options.clone()).unwrap_or_default();
            let cur = v.as_str().unwrap_or("").to_string();
            ui.horizontal(|ui| {
                let l = ui.label(RichText::new(&label).font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
                if !tip.is_empty() {
                    l.on_hover_text(&tip);
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    egui::ComboBox::from_id_salt(addr).width(150.0).selected_text(option_label(&cur)).show_ui(ui, |ui| {
                        for o in opts {
                            if ui.selectable_label(o == cur, option_label(&o)).clicked() {
                                set(app, addr, Value::Str(o));
                            }
                        }
                    });
                });
            });
            ui.add_space(spacing::XS);
        }
        Some(ValueType::Bool) => {
            let mut b = v.truthy();
            if widgets::toggle_row(ui, t, &label, "", &mut b).changed() {
                set(app, addr, Value::Bool(b));
            }
            ui.add_space(spacing::XS);
        }
        _ => {
            let [lo, hi] = meta.as_ref().and_then(|m| m.range).unwrap_or([0.0, 1.0]);
            let mut x = v.as_f64().unwrap_or(lo);
            let unit = meta.as_ref().and_then(|m| m.unit.clone()).unwrap_or_default();
            let log = matches!(unit.as_str(), "Hz" | "ms" | "s") && lo > 0.0;
            let r =
                slider_row(ui, t, &label, &value_text(x, &unit, lo, hi), |ui| ui.add(egui::Slider::new(&mut x, lo..=hi).logarithmic(log).show_value(false)));
            let exact = if matches!(unit.as_str(), "Hz" | "ms" | "s") { format!("Now: {}", exact_text(x, &unit)) } else { String::new() };
            let tip = [tip.as_str(), exact.as_str()].iter().filter(|s| !s.is_empty()).copied().collect::<Vec<_>>().join("\n");
            let r = if tip.is_empty() { r } else { r.on_hover_text(tip) };
            if r.changed() {
                set(app, addr, Value::Float(x));
            }
        }
    }
}

// ---- sources ------------------------------------------------------------------------------------

struct Source {
    label: String,
    detail: String,
    addr: String,
}

/// `patch.alertbox` → "Alertbox", `tts` → "Voice (TTS)".
fn source_label(name: &str) -> String {
    match name {
        "tts" => "Read-out voice".into(),
        "youtube" => "YouTube songs".into(),
        n if n.starts_with("timecode") => "Show sync signal".into(),
        n => nice(&n.strip_prefix("patch.").unwrap_or(n).replace('.', " ")),
    }
}

fn slot_kind(name: &str) -> &'static str {
    match name {
        "tts" => "Text to speech",
        "youtube" => "Song requests",
        n if n.starts_with("patch.") => "Overlay sound",
        n if n.starts_with("timecode") => "Sync signal for lights and video",
        _ => "App sound",
    }
}

fn channels_text(v: Option<&Value>) -> String {
    let ch: Vec<i64> = v.and_then(Value::as_list).map(|l| l.iter().filter_map(Value::as_i64).collect()).unwrap_or_default();
    match ch.as_slice() {
        [] => String::new(),
        [one] => format!("input {one}"),
        [a, b] if *b == a + 1 => format!("inputs {a} and {b}"),
        [a, .., b] if ch.windows(2).all(|w| w[1] == w[0] + 1) => format!("inputs {a} to {b}"),
        _ => format!("inputs {}", ch.iter().map(i64::to_string).collect::<Vec<_>>().join(", ")),
    }
}

/// Inputs and app sounds that play into `bus` (`None`: the ones on no channel).
fn sources(data: &Value, bus: Option<&str>) -> Vec<Source> {
    let wanted = |b: &str| match bus {
        Some(x) => b == x,
        None => b.is_empty(),
    };
    let mut v = Vec::new();
    for i in list(data, "inputs").iter().filter(|i| wanted(s(i, "bus"))) {
        let ch = channels_text(i.get_path("channels"));
        let target = s(i, "target");
        v.push(Source {
            label: source_label(s(i, "name")),
            detail: if ch.is_empty() { format!("From {target}") } else { format!("From {target}, {ch}") },
            addr: s(i, "address").to_string(),
        });
    }
    for sl in list(data, "slots").iter().filter(|sl| wanted(s(sl, "bus"))) {
        v.push(Source { label: source_label(s(sl, "name")), detail: slot_kind(s(sl, "name")).to_string(), addr: s(sl, "address").to_string() });
    }
    v
}

fn source_row(app: &mut App, ui: &mut Ui, t: &Theme, src: &Source) {
    let lvl = app.m.sig(&format!("{}.level", src.addr)).unwrap_or(0.0);
    let mute_a = format!("{}.mute", src.addr);
    let muted = app.m.b(&mute_a);
    egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(radius::CONTROL)).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.vertical(|ui| {
                ui.set_width(200.0);
                ui.add(egui::Label::new(RichText::new(&src.label).font(font_medium(type_scale::BODY)).color(if muted { t.text_dim } else { t.fg })).truncate());
                ui.add(egui::Label::new(RichText::new(&src.detail).size(type_scale::SMALL).color(t.text_dim)).truncate());
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let (ic, kind, tip) = if muted { (icon::MUTE, Kind::Danger, "Unmute") } else { (icon::VOLUME, Kind::Ghost, "Mute") };
                if widgets::button_ex(ui, t, Some(ic), "", kind, Size::Small, 28.0, true).on_hover_text(tip).clicked() {
                    set(app, &mute_a, Value::Bool(!muted));
                }
                let delay = format!("{}.delay_ms", src.addr);
                let mut d = app.m.f(&delay) as f32;
                if ui
                    .add(
                        egui::DragValue::new(&mut d)
                            .range(0.0..=2000.0)
                            .speed(1.0)
                            .custom_formatter(|v, _| if v < 0.5 { "In sync".into() } else { format!("{:.2} s later", v / 1000.0) })
                            .custom_parser(|s| {
                                let n: f64 = s.trim().trim_end_matches("s later").trim_end_matches('s').trim().parse().ok()?;
                                Some(n * 1000.0)
                            }),
                    )
                    .on_hover_text("Play this sound a little later so it lines up with your video. Drag, or type seconds.")
                    .changed()
                {
                    set(app, &delay, Value::Float(d as f64));
                }
                ui.label(RichText::new("Line up with video").size(type_scale::SMALL).color(t.text_dim));
                ui.add_space(spacing::S);
                let gain = format!("{}.gain", src.addr);
                let mut g = app.m.f(&gain) as f32;
                if ui
                    .add(
                        egui::DragValue::new(&mut g)
                            .range(-60.0..=24.0)
                            .speed(0.1)
                            .custom_formatter(|v, _| db_text(v))
                            .custom_parser(|s| s.trim().trim_end_matches("dB").trim().parse().ok()),
                    )
                    .on_hover_text("Drag to make it louder or quieter")
                    .changed()
                {
                    set(app, &gain, Value::Float(g as f64));
                }
                ui.label(RichText::new("Volume").size(type_scale::SMALL).color(t.text_dim));
                ui.add_space(spacing::S);
                widgets::meter_h(ui, t, Vec2::new(ui.available_width().min(160.0), 6.0), if muted { 0.0 } else { lvl });
            });
        });
    });
    ui.add_space(spacing::XS);
}

// ---- side column: ducking, tempo ------------------------------------------------------------------

fn join_words(v: &[String], last: &str) -> String {
    match v {
        [] => String::new(),
        [one] => one.clone(),
        [init @ .., tail] => format!("{} {last} {tail}", init.join(", ")),
    }
}

fn duck_triggers(d: &Value) -> Vec<String> {
    let mut v: Vec<String> = Vec::new();
    for sig in list(d, "signals").iter().filter_map(Value::as_str) {
        v.push(match sig {
            "mic.talking" => "you talk".into(),
            other => format!("{} is on", nice(&other.replace('.', " "))),
        });
    }
    for key in list(d, "keys").iter().filter_map(Value::as_str) {
        v.push(match key {
            "tts" => "the voice reads a message".into(),
            "mic" => "you talk".into(),
            "sfx" => "sound effects play".into(),
            other => format!("{} plays", bus_label(other)),
        });
    }
    v.dedup();
    v
}

fn ducking_card(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value) {
    let d = data.get_path("duck").cloned().unwrap_or_default();
    let targets: Vec<String> = list(&d, "targets").iter().filter_map(Value::as_str).map(bus_label).collect();
    widgets::titled(
        ui,
        t,
        "Lowered while you talk",
        "",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if targets.is_empty() {
                widgets::empty_state(ui, t, icon::MIC, "Nothing is lowered", "Your project doesn't turn any channel down while you talk.", None);
                return;
            }
            let who = join_words(&targets, "and");
            let when = join_words(&duck_triggers(&d), "or");
            let verb = if targets.len() == 1 { "gets" } else { "get" };
            ui.label(
                RichText::new(if when.is_empty() { format!("{who} {verb} quieter when asked to.") } else { format!("{who} {verb} quieter while {when}.") })
                    .color(t.text_dim),
            );
            ui.add_space(spacing::M);
            let talking = app.m.sig("mic.talking").unwrap_or(0.0) > 0.5;
            let amount = app.m.sig("audio.duck.amount").unwrap_or(0.0).clamp(0.0, 1.0);
            let now_db = data.get_path("duck_db").and_then(Value::as_f64).unwrap_or(0.0);
            ui.horizontal(|ui| {
                widgets::led(ui, t, if talking { LedState::Active } else { LedState::Idle });
                ui.label(RichText::new(if talking { "You're talking" } else { "You're quiet" }).font(font_medium(type_scale::BODY)).color(t.fg));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if now_db < -0.5 {
                        ui.label(RichText::new(format!("{who} lowered by {:.0} dB", -now_db)).size(type_scale::SMALL + 0.5).color(t.modulated()));
                    }
                });
            });
            let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 6.0), Sense::hover());
            ui.painter().rect_filled(rect, CornerRadius::same(3), t.inset);
            ui.painter().rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * amount, rect.height())), CornerRadius::same(3), t.modulated());
            ui.add_space(spacing::L);

            let mut quieter = -app.m.f("audio.duck.depth");
            let r = slider_row(ui, t, "How much quieter", &format!("{} dB", quieter.round() as i64), |ui| {
                ui.add(egui::Slider::new(&mut quieter, 0.0..=60.0).show_value(false))
            });
            if r.on_hover_text("How far the sound goes down while you talk").changed() {
                set(app, "audio.duck.depth", Value::Float(-quieter));
            }
            speed_slider(app, ui, t, "audio.duck.attack", "Goes down", (1.0, 2000.0), &[(25.0, "Instantly"), (120.0, "Quickly"), (500.0, "Gently")]);
            speed_slider(app, ui, t, "audio.duck.release", "Comes back", (1.0, 5000.0), &[(150.0, "Instantly"), (600.0, "Quickly"), (2000.0, "Gently")]);
            ui.add_space(spacing::S);
            let mut manual = app.m.b("audio.duck.active");
            if widgets::toggle_row(ui, t, "Lower it now", &format!("Keeps {who} down until you switch this off."), &mut manual).changed() {
                app.m.action("audio.duck", Value::map().with("on", manual));
            }
        },
    );
}

/// A time in milliseconds as a fast…slow slider described in words.
fn speed_slider(app: &mut App, ui: &mut Ui, t: &Theme, addr: &str, label: &str, (lo, hi): (f64, f64), words: &[(f64, &str)]) {
    let mut v = app.m.f(addr).clamp(lo, hi);
    let word = words.iter().find(|(lim, _)| v < *lim).map(|(_, w)| *w).unwrap_or("Slowly");
    let r = slider_row(ui, t, label, word, |ui| ui.add(egui::Slider::new(&mut v, lo..=hi).logarithmic(true).show_value(false)));
    if r.on_hover_text(format!("{v:.0} ms · left is faster, right is slower")).changed() {
        set(app, addr, Value::Float(v));
    }
}

fn tempo_card(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value) {
    widgets::titled(
        ui,
        t,
        "Tempo",
        "Lights and effects follow the beat.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let bpm = app.m.sig("beat.bpm").unwrap_or(0.0);
            let phase = app.m.sig("beat.phase").unwrap_or(0.0).clamp(0.0, 1.0);
            ui.horizontal(|ui| {
                let (r, _) = ui.allocate_exact_size(Vec2::splat(28.0), Sense::hover());
                let pulse = if bpm > 0.0 { 1.0 - phase } else { 0.0 };
                ui.painter().circle(r.center(), 10.0, blend(t.surface_hi, t.accent, pulse), Stroke::new(1.5, blend(t.border, t.accent, 0.6)));
                if bpm > 0.0 {
                    ui.label(RichText::new(format!("{bpm:.0}")).font(font_bold(type_scale::TITLE)).color(t.fg));
                    ui.label(RichText::new("beats a minute").font(font_medium(type_scale::BODY)).color(t.text_dim));
                } else {
                    ui.label(RichText::new("No beat yet").font(font_semibold(type_scale::LARGE)).color(t.text_dim));
                }
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if bpm > 0.0
                        && widgets::button_ex(ui, t, None, "Reset", Kind::Secondary, Size::Small, 0.0, true)
                            .on_hover_text("Forget the tapped tempo and follow the music again")
                            .clicked()
                    {
                        app.m.action("audio.tap.clear", Value::Null);
                    }
                    if widgets::button_ex(ui, t, Some(icon::HAND), "Tap", Kind::Secondary, Size::Medium, 88.0, true)
                        .on_hover_text("Tap along with the beat to set the tempo by hand")
                        .clicked()
                    {
                        app.m.action("audio.tap", Value::Null);
                    }
                });
            });
            let src = data.get_path("analysis.beat.source").and_then(Value::as_str).unwrap_or("");
            let line = if bpm <= 0.0 {
                "Play some music, or tap along with the beat.".to_string()
            } else if src.is_empty() {
                "Tap along to set it by hand.".to_string()
            } else {
                format!("Following {}. Tap along to set it by hand.", bus_label(src))
            };
            widgets::hint(ui, t, &line);
        },
    );
}

// ---- sounds & drums -------------------------------------------------------------------------------

fn sounds(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value) {
    let names: Vec<String> = list(data, "sounds").iter().filter_map(Value::as_str).map(String::from).collect();
    let mut stop = false;
    widgets::titled(
        ui,
        t,
        "Sounds",
        "Tap a pad to play it on stream.",
        |ui| {
            stop = widgets::button_ex(ui, t, Some(icon::STOP), "Stop all sounds", Kind::Secondary, Size::Small, 0.0, true).clicked();
        },
        |ui| {
            ui.set_width(ui.available_width());
            if names.is_empty() {
                widgets::empty_state(ui, t, icon::MUSIC, "No sounds yet", "Sound files in your project's sounds folder show up here as pads.", None);
                return;
            }
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(spacing::M);
                for n in &names {
                    let ic = if n.starts_with("alert") { icon::ALERT } else { icon::VOLUME };
                    if widgets::pad(ui, t, Vec2::new(164.0, 96.0), ic, &nice(n), None, LedState::Idle, None, None).on_hover_text("Play on stream").clicked() {
                        app.m.action("audio.play", Value::map().with("sound", n.as_str()));
                    }
                }
            });
        },
    );
    if stop {
        app.m.action("audio.stop", Value::Null);
    }
    ui.add_space(spacing::L);
    let pads: Vec<String> = list(data, "pads").iter().filter_map(Value::as_str).map(String::from).collect();
    widgets::titled(
        ui,
        t,
        "Drum triggers",
        "Each pad lights up when you hit that drum. Lights and effects can react to the hits.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if pads.is_empty() {
                widgets::empty_state(
                    ui,
                    t,
                    icon::DRUM,
                    "No drum triggers set up",
                    "Drum triggers turn hits on your drum mics into cues for lights and effects.",
                    None,
                );
                return;
            }
            let gap = spacing::M;
            let w = 192.0;
            let cols = (((ui.available_width() + gap) / (w + gap)).floor() as usize).max(1);
            for row in pads.chunks(cols) {
                ui.horizontal_top(|ui| {
                    for (i, n) in row.iter().enumerate() {
                        if i > 0 {
                            ui.add_space(gap - ui.spacing().item_spacing.x);
                        }
                        ui.allocate_ui_with_layout(Vec2::new(w, 0.0), Layout::top_down(Align::Min), |ui| drum_pad(app, ui, t, n));
                    }
                });
                ui.add_space(gap);
            }
        },
    );
    ui.add_space(spacing::XL);
}

fn drum_pad(app: &mut App, ui: &mut Ui, t: &Theme, n: &str) {
    let hit = app.m.sig(&format!("drums.{n}")).unwrap_or(0.0) > 0.0;
    let a = format!("audio.drums.{n}.threshold");
    let th = app.m.f(&a);
    egui::Frame::new()
        .fill(t.surface_hi)
        .stroke(Stroke::new(1.0, t.border))
        .corner_radius(CornerRadius::same(radius::TILE))
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(170.0);
            widgets::pad(ui, t, Vec2::new(170.0, 76.0), icon::DRUM, &nice(n), None, if hit { LedState::Active } else { LedState::Idle }, None, None);
            ui.add_space(spacing::S);
            // lower threshold = more sensitive; the slider reads left → right as less → more
            let mut sens = (-th).clamp(0.0, 80.0);
            let pct = (sens / 80.0 * 100.0).round() as i32;
            let r = slider_row(ui, t, "Sensitivity", &format!("{pct}%"), |ui| ui.add(egui::Slider::new(&mut sens, 0.0..=80.0).show_value(false)));
            if r.on_hover_text(format!("Hits louder than {th:.0} dB count. Turn it up if hits are missed, down if other drums set it off.")).changed() {
                set(app, &a, Value::Float(-sens));
            }
        });
}

// ---- advanced -------------------------------------------------------------------------------------

fn advanced(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value) {
    widgets::hint(ui, t, "How the sound system is running, for fixing problems. Nothing here needs changing for a normal stream.");
    ui.add_space(spacing::M);
    ui.columns(2, |cols| {
        system_card(app, &mut cols[0], t, data);
        cols[0].add_space(spacing::L);
        outputs_card(&mut cols[0], t, data);
        analysis_card(app, &mut cols[1], t, data);
    });
    ui.add_space(spacing::L);
    ui.columns(2, |cols| {
        widgets::titled(
            &mut cols[0],
            t,
            "Sources on no channel",
            "Sounds that go straight to their own output.",
            |_| {},
            |ui| {
                ui.set_width(ui.available_width());
                let srcs = sources(data, None);
                if srcs.is_empty() {
                    widgets::hint(ui, t, "Every source plays into a channel.");
                }
                for src in &srcs {
                    source_row(app, ui, t, src);
                }
            },
        );
        links_card(&mut cols[1], t, data);
    });
    ui.add_space(spacing::XL);
}

fn status_row(ui: &mut Ui, t: &Theme, led: LedState, label: &str, value: &str, hover: &str) {
    ui.horizontal(|ui| {
        widgets::led(ui, t, led);
        fixed_label(ui, 190.0, RichText::new(label).color(t.text_dim));
        let r = ui.add(egui::Label::new(RichText::new(value).color(t.fg)).wrap());
        if !hover.is_empty() {
            r.on_hover_text(hover);
        }
    });
}

fn system_card(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value) {
    widgets::titled(
        ui,
        t,
        "Sound system",
        "PipeWire, timing and load.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let pw = data.get_path("pipewire").cloned().unwrap_or_default();
            let reconnects = pw.get_path("reconnects").and_then(Value::as_i64).unwrap_or(0);
            let state = s(&pw, "state");
            let pw_text = if reconnects > 0 { format!("{state} · reconnected {reconnects}×") } else { state.to_string() };
            status_row(
                ui,
                t,
                health_led(app.m.get("health.audio.pipewire")),
                "Audio server (PipeWire)",
                &pw_text,
                &health_detail(app, "health.audio.pipewire"),
            );
            let err = s(&pw, "error");
            if !err.is_empty() {
                status_row(ui, t, LedState::Error, "Audio server error", err, "");
            }
            let rt = format!(
                "{} · priority {}",
                data.get_path("perf.rt_policy").and_then(Value::as_str).unwrap_or("?"),
                data.get_path("perf.rt_priority").and_then(Value::as_i64).unwrap_or(0)
            );
            status_row(ui, t, health_led(app.m.get("health.audio.rt")), "Real-time scheduling", &rt, &health_detail(app, "health.audio.rt"));
            let xr = app.m.f("perf.audio.xruns") as i64;
            status_row(
                ui,
                t,
                health_led(app.m.get("health.audio.xruns")),
                "Dropouts (xruns)",
                &format!("{xr} since start"),
                &health_detail(app, "health.audio.xruns"),
            );
            let load = app.m.f("perf.audio.load");
            let load_led = if load < 0.5 {
                LedState::Healthy
            } else if load < 0.8 {
                LedState::Armed
            } else {
                LedState::Error
            };
            status_row(ui, t, load_led, "DSP load", &format!("{:.0}% · {:.3} ms per block", load * 100.0, app.m.f("perf.audio.dsp_ms")), "");
            status_row(
                ui,
                t,
                LedState::Idle,
                "Buffer (quantum)",
                &format!(
                    "{} frames @ {} Hz · {:.2} ms",
                    app.m.f("perf.audio.quantum") as i64,
                    app.m.f("perf.audio.rate") as i64,
                    app.m.f("perf.audio.latency_ms")
                ),
                "",
            );
            status_row(ui, t, LedState::Idle, "Device delay", &format!("{:.2} ms", app.m.f("perf.audio.driver_delay_ms")), "");
            let allocs = app.m.f("perf.audio.allocs") as i64;
            status_row(ui, t, if allocs > 0 { LedState::Error } else { LedState::Healthy }, "RT-thread allocations", &allocs.to_string(), "");
            status_row(ui, t, health_led(app.m.get("health.audio.obs")), "OBS capture", &health_detail(app, "health.audio.obs"), "");
            status_row(ui, t, health_led(app.m.get("health.audio.config")), "Sound setup", &health_detail(app, "health.audio.config"), "");
            let inputs: Vec<(String, Value)> =
                app.m.under("health.audio.input").map(|(a, v)| (a.rsplit('.').next().unwrap_or(a).to_string(), v.clone())).collect();
            for (name, h) in &inputs {
                let detail = s(h, "detail");
                status_row(ui, t, health_led(Some(h)), &format!("Input: {}", source_label(name)), detail, "");
            }
            for e in list(data, "errors") {
                status_row(ui, t, LedState::Error, "Problem", e.as_str().unwrap_or(""), "");
            }
        },
    );
}

fn outputs_card(ui: &mut Ui, t: &Theme, data: &Value) {
    widgets::titled(
        ui,
        t,
        "Channel outputs",
        "The PipeWire node of each channel and what captures it.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            for b in bus_list(data) {
                let node = s(b, "node");
                let caps = consumers(data, node).iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", ");
                let into = if b.get_path("to_program").is_some_and(Value::truthy) { " · into Everything" } else { "" };
                let led = if caps.is_empty() { LedState::Idle } else { LedState::Healthy };
                status_row(ui, t, led, &bus_label(s(b, "name")), &format!("{node}{into} → {}", if caps.is_empty() { "not captured" } else { &caps }), "");
            }
        },
    );
}

fn analysis_card(app: &mut App, ui: &mut Ui, t: &Theme, data: &Value) {
    widgets::titled(
        ui,
        t,
        "Beat & music analysis",
        "What lights and reactions listen to.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let src = data.get_path("analysis.beat.source").and_then(Value::as_str).unwrap_or("—").to_string();
            ui.horizontal(|ui| {
                let phase = app.m.sig("beat.phase").unwrap_or(0.0);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(24.0, 24.0), Sense::hover());
                let p = ui.painter();
                p.circle_stroke(rect.center(), 10.0, Stroke::new(1.5, t.text_dim));
                let a = phase * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
                p.line_segment([rect.center(), rect.center() + Vec2::new(a.cos(), a.sin()) * 10.0], Stroke::new(2.0, t.accent));
                ui.label(
                    RichText::new(format!(
                        "{:.1} beats a minute · sure {:.0}% · listening to {}",
                        app.m.sig("beat.bpm").unwrap_or(0.0),
                        app.m.sig("beat.confidence").unwrap_or(0.0) * 100.0,
                        bus_label(&src)
                    ))
                    .font(font_medium(type_scale::SMALL + 0.5))
                    .color(t.fg),
                );
            });
            ui.add_space(spacing::S);
            for prefix in ["band", "music"] {
                ui.add_space(spacing::S);
                ui.horizontal(|ui| {
                    ui.label(RichText::new(bus_label(prefix)).font(font_semibold(type_scale::BODY)).color(t.fg));
                    ui.add_space(spacing::S);
                    for k in ["kick", "snare", "hat"] {
                        let v = app.m.sig(&format!("{prefix}.{k}")).unwrap_or(0.0);
                        widgets::led(ui, t, if v > 0.3 { LedState::Active } else { LedState::Idle });
                        ui.label(RichText::new(nice(k)).size(type_scale::SMALL).color(t.text_dim));
                    }
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        ui.label(
                            RichText::new(format!("{:.1} LUFS", app.m.sig(&format!("{prefix}.lufs")).unwrap_or(-70.0)))
                                .font(font_mono(type_scale::SMALL))
                                .color(t.text_dim),
                        );
                    });
                });
                for k in ["bass", "mid", "high"] {
                    let v = app.m.sig(&format!("{prefix}.{k}")).unwrap_or(0.0);
                    ui.horizontal(|ui| {
                        fixed_label(ui, 44.0, RichText::new(nice(k)).size(type_scale::SMALL).color(t.text_dim));
                        let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width().min(260.0), 6.0), Sense::hover());
                        let x = ((lin_db(v) + 60.0) / 60.0).clamp(0.0, 1.0);
                        ui.painter().rect_filled(rect, CornerRadius::same(3), t.inset);
                        ui.painter().rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * x, rect.height())), CornerRadius::same(3), t.accent);
                    });
                }
                // 1/3-octave spectrum
                ui.add_space(spacing::XS);
                let (rect, _) = ui.allocate_exact_size(Vec2::new(ui.available_width(), 64.0), Sense::hover());
                let p = ui.painter_at(rect);
                p.rect_filled(rect, CornerRadius::same(radius::CONTROL), t.inset);
                let n = 31;
                let w = rect.width() / n as f32;
                for i in 0..n {
                    let v = app.m.sig(&format!("{prefix}.b.{i}")).unwrap_or(0.0);
                    let h = ((lin_db(v) + 70.0) / 70.0).clamp(0.0, 1.0) * rect.height();
                    let r = Rect::from_min_max(
                        Pos2::new(rect.left() + i as f32 * w + 1.0, rect.bottom() - h),
                        Pos2::new(rect.left() + (i + 1) as f32 * w - 1.0, rect.bottom()),
                    );
                    p.rect_filled(r, CornerRadius::same(1), blend(t.accent, t.fg, 0.15));
                }
            }
        },
    );
}

fn links_card(ui: &mut Ui, t: &Theme, data: &Value) {
    widgets::titled(
        ui,
        t,
        "Connections",
        "PipeWire links between devices and channels.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let links = list(data, "links");
            if links.is_empty() {
                widgets::hint(ui, t, "No connections yet.");
            }
            for l in links {
                ui.horizontal(|ui| {
                    let ok = l.get_path("ok").is_some_and(Value::truthy);
                    widgets::led(ui, t, if ok { LedState::Healthy } else { LedState::Error });
                    fixed_label(ui, 140.0, RichText::new(s(l, "port")).font(font_mono(type_scale::SMALL)).color(t.fg));
                    ui.add(egui::Label::new(RichText::new(s(l, "target")).font(font_mono(type_scale::SMALL)).color(t.text_dim)).truncate());
                });
            }
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fader_taper_round_trips_and_clamps() {
        // positions are f32, levels f64: a round trip must stay well inside the 0.1 dB the strip shows
        for db in [DB_MIN, -24.0, -3.0, 0.0, 6.0, DB_MAX] {
            assert!((fader_to_db(db_to_fader(db)) - db).abs() < 0.01, "{db}");
        }
        assert_eq!(db_to_fader(DB_MIN - 10.0), 0.0);
        assert_eq!(db_to_fader(DB_MAX + 5.0), 1.0);
    }
}
