//! Sound → Mixing desk (§8.7, §15.6): the StudioLive 16R as the engine sees it. A connection
//! card in plain words (addresses and firmware under Details), channel faders for the main mix
//! or any monitor (aux) / effect (FX) send mix with its master, meters, "Saved mixes" (store /
//! recall / crossfade), the safe-mix panic, and MIDI controls linked to the desk. Faders only
//! read state, so they move with the console, UC Surface, MIDI and presets live; dragging sends
//! `set` (the adapter rate-limits).
//!
//! Data: `mixer.16r.*` state, signals `mixer.16r.meter.*`, queries `mixer.status`,
//! `mixer.coverage` (writable controls), `mixer.snapshots`, `mixer.discover`, `bindings`; state
//! `controllers.learn.{active,target}`. Actions: `mixer.snapshot.{store,recall}`,
//! `mixer.panic`, `mixer.reconnect`, `midi.learn {target}`.

use crate::app::App;
use crate::views::live::nice;
use crate::views::mix::db_text;
use egui::{Align, Align2, CornerRadius, Layout, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, Ui, UiBuilder, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_medium, font_mono, font_semibold, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, icon};
use std::collections::{BTreeMap, HashSet};

const P: &str = "mixer.16r";
/// 0 dB on the console's fader law (reference `logVolumeToLinear(0)` / 100).
const UNITY: f64 = 0.7252;
/// Strip height besides the fader: name (up to two lines), level readout, mute, link line, margins.
const STRIP_EXTRA: f32 = 132.0;
/// Name font sizes to try, largest first, until every word fits the strip on at most two lines.
const NAME_SIZES: [f32; 5] = [type_scale::SMALL + 0.5, type_scale::SMALL - 0.5, 11.0, 10.5, 10.0];

#[derive(Clone, Default)]
struct Form {
    /// "main", "aux.N", or "fx.N".
    mix: String,
    /// Name typed for "Save current mix".
    store_name: String,
    fades: BTreeMap<String, f64>,
    last_status: f64,
    last_lists: f64,
    discover: bool,
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

struct Strip {
    /// Address segment below the mixer prefix (`ch.3`, `ret.1`, `fxret.2`, `tb`).
    seg: String,
    label: String,
    meter: Option<String>,
}

/// Friendly name of a desk strip segment (`ch.3` → the desk's name for it, else "Channel 3").
fn seg_label(app: &App, seg: &str) -> String {
    let name = app.m.str(&format!("{P}.{seg}.name"));
    if !name.is_empty() {
        return name.to_string();
    }
    match seg.split_once('.') {
        Some(("ch", n)) => format!("Channel {n}"),
        Some(("ret", n)) => format!("Return {n}"),
        Some(("fxret", n)) => format!("FX return {n}"),
        Some(("aux", n)) => format!("Aux {n}"),
        Some(("fx", n)) => format!("FX {n}"),
        _ if seg == "tb" => "Talkback".into(),
        _ if seg == "main" => "Main".into(),
        _ => nice(&seg.replace('.', " ")),
    }
}

fn strips(app: &App) -> Vec<Strip> {
    let mut v = Vec::new();
    let n = app.m.get(&format!("{P}.channels")).and_then(Value::as_i64).unwrap_or(0).max(0);
    for c in 1..=n {
        let seg = format!("ch.{c}");
        v.push(Strip { label: seg_label(app, &seg), meter: Some(format!("{P}.meter.ch.{c}")), seg });
    }
    for kind in ["ret", "fxret"] {
        for c in 1.. {
            if !app.m.has(&format!("{P}.{kind}.{c}.fader")) {
                break;
            }
            let seg = format!("{kind}.{c}");
            v.push(Strip { label: seg_label(app, &seg), meter: None, seg });
        }
    }
    if app.m.has(&format!("{P}.tb.fader")) {
        v.push(Strip { seg: "tb".into(), label: "Talkback".into(), meter: None });
    }
    v
}

fn buses(app: &App, kind: &str) -> Vec<(i64, String)> {
    let n = app.m.get(&format!("{P}.{}", if kind == "aux" { "auxes" } else { "fxes" })).and_then(Value::as_i64).unwrap_or(0).max(0);
    (1..=n).map(|b| (b, seg_label(app, &format!("{kind}.{b}")))).collect()
}

/// `mixer.16r.aux.1.ch.3.send` → "Kick in Aux 1", `mixer.16r.ch.3.mute` → "Kick mute".
fn target_label(app: &App, target: &str) -> String {
    let rest = target.strip_prefix(P).and_then(|r| r.strip_prefix('.')).unwrap_or(target);
    let parts: Vec<&str> = rest.split('.').collect();
    match parts.as_slice() {
        [kind @ ("aux" | "fx"), b, seg @ .., "send"] if !seg.is_empty() => {
            format!("{} in {}", seg_label(app, &seg.join(".")), seg_label(app, &format!("{kind}.{b}")))
        }
        [seg @ .., "fader"] if !seg.is_empty() => seg_label(app, &seg.join(".")),
        [seg @ .., last] if !seg.is_empty() => format!("{} {}", seg_label(app, &seg.join(".")), last),
        _ => nice(&rest.replace('.', " ")),
    }
}

/// `midi.xtouch.fader.3` → "Xtouch fader 3".
fn signal_label(sig: &str) -> String {
    nice(&sig.strip_prefix("midi.").unwrap_or(sig).replace('.', " "))
}

/// A saved-mix file name from what the user typed: "Just chatting!" → `just_chatting`.
fn slug(name: &str) -> String {
    let mut out = String::new();
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.ends_with('_') && !out.is_empty() {
            out.push('_');
        }
    }
    out.trim_end_matches('_').to_string()
}

pub fn ui(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    let id = egui::Id::new("mixer-view");
    let mut form: Form = ui.data_mut(|d| d.get_temp::<Form>(id)).unwrap_or_default();
    if form.mix.is_empty() {
        form.mix = "main".into();
    }
    let now = ui.input(|i| i.time);
    if app.m.connected && now - form.last_status > 1.0 {
        form.last_status = now;
        app.m.query("mixer.status", Value::Null);
    }
    if app.m.connected && now - form.last_lists > 3.0 {
        form.last_lists = now;
        app.m.query("mixer.coverage", Value::Null);
        app.m.query("mixer.snapshots", Value::Null);
        app.m.query("bindings", Value::Null);
    }
    let body_h = ui.available_height();
    egui::ScrollArea::vertical().id_salt("mixer").auto_shrink([false, false]).show(ui, |ui| {
        connection_card(app, ui, &t, &mut form);
        if !app.m.b(&format!("{P}.connected")) {
            return;
        }
        ui.add_space(spacing::L);
        let writable: HashSet<String> =
            app.m.q_list("mixer.coverage").iter().filter(|c| c.get_path("writable").is_some_and(Value::truthy)).map(|c| s(c, "address").to_string()).collect();
        // binding target → signal, for the link marks and the linked-controls list
        let mut bound: BTreeMap<String, String> = BTreeMap::new();
        for b in app.m.q_list("bindings") {
            let target = s(b, "target");
            if target.starts_with(P) {
                bound.insert(target.to_string(), s(b, "signal").to_string());
            }
        }
        faders_card(app, ui, &t, &mut form, &writable, &bound, (body_h * 0.34).clamp(160.0, 340.0));
        ui.add_space(spacing::L);
        ui.columns(2, |cols| {
            saved_mixes(app, &mut cols[0], &t, &mut form);
            linked_controls(app, &mut cols[1], &t, &bound);
        });
        ui.add_space(spacing::XL);
    });
    ui.data_mut(|d| d.insert_temp(id, form));
}

// ---- connection -----------------------------------------------------------------------------------

fn connection_card(app: &mut App, ui: &mut Ui, t: &Theme, form: &mut Form) {
    let st = app.m.q("mixer.status").cloned().unwrap_or_default();
    let connected = app.m.b(&format!("{P}.connected"));
    let known = app.m.has(&format!("{P}.connected"));
    let health = app.m.get("health.mixer").cloned().unwrap_or_default();
    let hs = s(&health, "status").to_string();
    let model = app.m.str(&format!("{P}.model")).to_string();
    let title = if model.is_empty() { "StudioLive 16R".to_string() } else { model };
    let sub = if connected {
        "Faders here move with the desk, and moving them here changes the desk."
    } else if known {
        "Not connected. Check the desk is switched on and plugged into the same network as this computer."
    } else {
        "No mixing desk yet. If you have a PreSonus StudioLive, Stream Engine can run it from here."
    };
    let (mut scan, mut reconnect) = (false, false);
    widgets::titled(
        ui,
        t,
        &title,
        sub,
        |ui| {
            scan = widgets::button_ex(ui, t, Some(icon::SEARCH), "Find my desk", if connected { Kind::Ghost } else { Kind::Primary }, Size::Medium, 0.0, true)
                .on_hover_text("Look for the desk on your network")
                .clicked();
            if known {
                reconnect = widgets::button_ex(ui, t, Some(icon::UNDO), "Reconnect", Kind::Secondary, Size::Medium, 0.0, true)
                    .on_hover_text("Drop the connection and find the desk again")
                    .clicked();
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                let needs_look = connected && ((hs != "pass" && !hs.is_empty()) || !st.get_path("meters").is_some_and(Value::truthy));
                match (connected, known) {
                    (true, _) if needs_look => widgets::badge(ui, t, "Needs a look", t.yellow),
                    (true, _) => widgets::badge(ui, t, "Working", t.green),
                    (false, true) => widgets::badge(ui, t, "Not connected", t.bright_red),
                    (false, false) => widgets::badge(ui, t, "Off", t.text_dim),
                };
                if app.m.sig("mic.talking").is_some_and(|v| v > 0.5) {
                    widgets::badge(ui, t, "You're talking", t.accent);
                }
            });
            // what "Needs a look" means, in words
            let detail = s(&health, "detail");
            let reason = if !connected {
                ""
            } else if hs == "fail" {
                "The desk isn't answering properly. Press Reconnect."
            } else if !st.get_path("meters").is_some_and(Value::truthy) {
                "Level meters aren't coming through. The faders still work."
            } else if detail.contains("no safe mix") {
                "There's no mix named “Safe” yet, so “Go to safe mix” has nothing to go back to. Save one below."
            } else if hs == "warn" {
                "Something about the desk isn't quite right. Details has more."
            } else {
                ""
            };
            if !reason.is_empty() {
                ui.add_space(spacing::XS);
                widgets::hint(ui, t, reason);
            }
            if form.discover {
                discover_results(app, ui, t, form);
            }
            ui.add_space(spacing::S);
            widgets::details(ui, t, "mixer-conn", "Details", |ui| {
                for (label, key) in [
                    ("Model", "model"),
                    ("Name on the desk", "console_name"),
                    ("Firmware", "firmware"),
                    ("Serial number", "serial"),
                    ("Network address", "host"),
                ] {
                    let v = app.m.str(&format!("{P}.{key}"));
                    if !v.is_empty() {
                        widgets::fact(ui, t, label, v);
                    }
                }
                let n = |k: &str| st.get_path(k).and_then(Value::as_i64).unwrap_or(0);
                if connected {
                    widgets::fact(ui, t, "Size", &format!("{} channels, {} aux mixes, {} FX", n("channels"), n("auxes"), n("fxes")));
                    widgets::fact(ui, t, "Controls", &format!("{} ({} waiting for the desk)", n("controls"), n("unconfirmed")));
                    widgets::fact(ui, t, "Found via", s(&st, "via"));
                    if n("talk_channel") > 0 {
                        widgets::fact(ui, t, "Talk detection", &format!("channel {}", n("talk_channel")));
                    }
                }
                let detail = s(&health, "detail");
                if !detail.is_empty() {
                    widgets::fact(ui, t, "Status", detail);
                }
                widgets::hint(ui, t, "Talks to the desk over UCNET (TCP 53000), like UC Surface does.");
            });
        },
    );
    if scan {
        form.discover = true;
        app.m.query("mixer.discover", Value::map().with("seconds", 4));
    }
    if reconnect {
        app.m.action("mixer.reconnect", Value::Null);
    }
}

fn discover_results(app: &mut App, ui: &mut Ui, t: &Theme, form: &mut Form) {
    ui.add_space(spacing::M);
    widgets::section(ui, t, "", "On your network");
    let Some(d) = app.m.q("mixer.discover").cloned() else {
        widgets::hint(ui, t, "Looking for your desk…");
        return;
    };
    let list = d.get_path("devices").and_then(Value::as_list).unwrap_or(&[]).to_vec();
    if list.is_empty() {
        widgets::hint(ui, t, "No desk answered. Check it's switched on and plugged into the same network as this computer.");
    }
    for dev in &list {
        let model = s(dev, "model");
        widgets::list_row(ui, t, icon::SLIDERS, if model.is_empty() { "Mixing desk" } else { model }, "Found on your network", "", false)
            .on_hover_text(format!("{} · serial {} · found via {}", s(dev, "host"), s(dev, "serial"), s(dev, "via")));
    }
    ui.horizontal(|ui| {
        if widgets::button_ex(ui, t, None, "Hide", Kind::Ghost, Size::Small, 0.0, true).clicked() {
            form.discover = false;
        }
        if let Some(h) = d.get_path("firewall_hint").and_then(Value::as_str) {
            widgets::details(ui, t, "mixer-discover", "Why might it not show up?", |ui| {
                widgets::hint(ui, t, h);
            });
        }
    });
}

// ---- faders ---------------------------------------------------------------------------------------

fn mix_blurb(app: &App, mix: &str) -> String {
    match mix.split_once('.') {
        Some(("aux", _)) => format!("{}: a monitor mix, like your headphones. Each fader sets how much of that channel you hear in it.", seg_label(app, mix)),
        Some(("fx", _)) => format!("How much of each channel goes into the desk's {} effect.", seg_label(app, mix)),
        _ => "The desk's main mix. Double-click a fader for 0 dB · right-click it to link a knob or fader on your controller.".into(),
    }
}

fn faders_card(app: &mut App, ui: &mut Ui, t: &Theme, form: &mut Form, writable: &HashSet<String>, bound: &BTreeMap<String, String>, fader_h: f32) {
    let mix = form.mix.clone();
    let mut panic = false;
    widgets::titled(
        ui,
        t,
        "Faders",
        &mix_blurb(app, &mix),
        |ui| {
            panic = widgets::hold_button(ui, t, "Go to safe mix", t.bright_red, 0.8);
        },
        |ui| {
            ui.set_width(ui.available_width());
            let mut keys = vec!["main".to_string()];
            let mut labels = vec!["Main mix".to_string()];
            for kind in ["aux", "fx"] {
                for (b, name) in buses(app, kind) {
                    keys.push(format!("{kind}.{b}"));
                    labels.push(name);
                }
            }
            let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
            let mut idx = keys.iter().position(|k| *k == mix).unwrap_or(0);
            ui.horizontal(|ui| {
                ui.label(RichText::new("Showing").font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
                if widgets::segmented(ui, t, &mut idx, &refs) {
                    form.mix = keys[idx].clone();
                }
            });
            ui.add_space(spacing::M);

            let list = strips(app);
            let master = match mix.as_str() {
                "main" => Some(("Main".to_string(), format!("{P}.main.fader"), format!("{P}.main.mute"), Some(format!("{P}.meter.main")))),
                m => Some((seg_label(app, m), format!("{P}.{m}.fader"), format!("{P}.{m}.mute"), Some(format!("{P}.meter.{m}")))),
            }
            .filter(|(_, fader, _, _)| app.m.has(fader));
            let rows: Vec<(String, String, String, Option<String>)> = list
                .iter()
                .map(|st| {
                    let level = if mix == "main" { format!("{P}.{}.fader", st.seg) } else { format!("{P}.{mix}.{}.send", st.seg) };
                    (st.label.clone(), level, format!("{P}.{}.mute", st.seg), st.meter.clone())
                })
                .filter(|(_, level, _, _)| app.m.has(level))
                .collect();
            let gap = 4.0;
            let n = (rows.len() + usize::from(master.is_some())).max(1) as f32;
            // strips share the whole width; only very narrow windows scroll
            let sw = ((ui.available_width() - gap * (n - 1.0) - spacing::L) / n).clamp(56.0, 150.0);
            egui::ScrollArea::horizontal().id_salt("mixer-strips").show(ui, |ui| {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for (label, level, mute, meter) in &rows {
                        strip_ui(app, ui, t, label, level, mute, meter.as_deref(), Vec2::new(sw, fader_h), writable, bound, mix == "main", false);
                    }
                    if let Some((label, level, mute, meter)) = &master {
                        ui.add_space(spacing::L - gap);
                        strip_ui(app, ui, t, label, level, mute, meter.as_deref(), Vec2::new(sw, fader_h), writable, bound, true, true);
                    }
                });
            });
            if let Some(fx) = mix.strip_prefix("fx.") {
                fx_readback(app, ui, t, fx);
            }
        },
    );
    if panic {
        app.m.action("mixer.panic", Value::Null);
    }
}

#[allow(clippy::too_many_arguments)]
fn strip_ui(
    app: &mut App,
    ui: &mut Ui,
    t: &Theme,
    label: &str,
    level_addr: &str,
    mute_addr: &str,
    meter: Option<&str>,
    size: Vec2,
    writable: &HashSet<String>,
    bound: &BTreeMap<String, String>,
    show_mute: bool,
    master: bool,
) {
    let muted = app.m.b(mute_addr);
    let can_level = writable.contains(level_addr);
    let can_mute = writable.contains(mute_addr);
    let value = app.m.f(level_addr);
    let modulated = app.is_modulated(level_addr);
    let learning = app.m.b("controllers.learn.active") && app.m.str("controllers.learn.target") == level_addr;
    let (sw, fader_h) = (size.x, size.y);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(sw, fader_h + STRIP_EXTRA), Sense::hover());
    let p = ui.painter().clone();
    let fill = if master { se_ui_kit::theme::mix(t.surface_hi, t.accent, 0.08) } else { t.surface_hi };
    p.rect(rect, CornerRadius::same(radius::TILE), fill, Stroke::new(1.0, t.border), StrokeKind::Inside);
    let cx = rect.center().x;
    let mut y = rect.top() + 10.0;

    // name: up to two lines, shrinking the font until every word fits the strip
    let color = if muted { t.text_faint } else { t.fg };
    let family = |size: f32| if master { font_semibold(size) } else { font_medium(size) };
    let wrap = sw - 4.0;
    let fits = |size: f32| {
        label.split_whitespace().all(|w| ui.painter().layout_no_wrap(w.to_string(), family(size), color).size().x <= wrap)
            && ui.painter().layout(label.to_string(), family(size), color, wrap).rows.len() <= 2
    };
    let size = NAME_SIZES.into_iter().find(|s| fits(*s));
    let mut job = egui::text::LayoutJob::simple(label.to_string(), family(size.unwrap_or(NAME_SIZES[NAME_SIZES.len() - 1])), color, wrap);
    job.halign = Align::Center;
    job.wrap.max_rows = 2;
    // only a name too long for two lines even at the smallest size gets cut ("…", full name on hover)
    job.wrap.break_anywhere = size.is_none();
    let galley = ui.painter().layout_job(job);
    p.galley(Pos2::new(cx, y + (32.0 - galley.size().y).max(0.0) / 2.0), galley, color);
    resp.on_hover_text(label);
    y += 38.0;

    let fw = (sw * 0.42).clamp(24.0, 34.0);
    let (mw, g) = (7.0, 8.0);
    let x0 = cx - (fw + g + mw) / 2.0;
    let fr = Rect::from_min_size(Pos2::new(x0, y), Vec2::new(fw, fader_h));
    let mr = Rect::from_min_size(Pos2::new(x0 + fw + g, y), Vec2::new(mw, fader_h));
    let mut pos = value as f32;
    let mut fui = ui.new_child(UiBuilder::new().id_salt(("fader", level_addr)).max_rect(fr));
    if !can_level {
        fui.disable();
    }
    let r = widgets::fader(&mut fui, t, fr.size(), &mut pos, modulated);
    if r.changed() && can_level {
        app.m.command(Op::Set { address: level_addr.to_string(), value: Value::Float(pos as f64) });
    }
    if r.double_clicked() && can_level {
        app.m.command(Op::Set { address: level_addr.to_string(), value: Value::Float(UNITY) });
    }
    let hover = if can_level {
        format!("{label} · double-click for 0 dB · right-click to link a knob or fader on your controller")
    } else {
        format!("{label} · read-only: Stream Engine only watches this control")
    };
    let r = r.on_hover_text(hover);
    if can_level {
        r.context_menu(|ui| {
            if ui.button(format!("{}  Link a knob or fader…", icon::CONTROLLER)).clicked() {
                app.m.action("midi.learn", Value::map().with("target", level_addr));
                ui.close();
            }
        });
    }
    let level = meter.and_then(|m| app.m.sig(m)).unwrap_or(0.0);
    widgets::meter(&mut ui.new_child(UiBuilder::new().id_salt(("meter", level_addr)).max_rect(mr)), t, mr.size(), if muted { 0.0 } else { level }, level);
    y += fader_h + 8.0;

    let db_addr = level_addr.strip_suffix(".fader").map(|b| format!("{b}.db"));
    let db = db_addr.and_then(|a| app.m.get(&a).and_then(Value::as_f64));
    // numbers in mono, words ("Off") in the normal font; smaller on narrow strips so "-12.2 dB" fits
    let small = if sw < 72.0 { type_scale::SMALL - 1.5 } else { type_scale::SMALL };
    let (txt, fid) = match db {
        Some(db) if db <= -100.0 => ("Off".to_string(), font_medium(small)),
        Some(db) => (db_text(db), font_mono(small)),
        None => (format!("{}%", (value * 100.0).round() as i64), font_mono(small)),
    };
    p.text(Pos2::new(cx, y + 8.0), Align2::CENTER_CENTER, txt, fid, t.text_dim);
    y += 22.0;

    if show_mute && app.m.has(mute_addr) {
        let br = Rect::from_min_size(Pos2::new(rect.left() + 6.0, y), Vec2::new(sw - 12.0, 26.0));
        let (ic, kind, tip) = if muted { (icon::MUTE, Kind::Danger, "Muted — click to unmute") } else { (icon::VOLUME, Kind::Ghost, "Mute") };
        let m = widgets::button_ex(
            &mut ui.new_child(UiBuilder::new().id_salt(("mute", mute_addr)).max_rect(br)),
            t,
            Some(ic),
            "",
            kind,
            Size::Small,
            sw - 12.0,
            can_mute,
        );
        if m.on_hover_text(tip).clicked() && can_mute {
            app.m.command(Op::Set { address: mute_addr.to_string(), value: Value::Bool(!muted) });
        }
    }
    y += 32.0;

    // MIDI link mark
    let link = Rect::from_min_size(Pos2::new(rect.left() + 4.0, y), Vec2::new(sw - 8.0, 18.0));
    match bound.get(level_addr) {
        Some(sig) => {
            p.text(link.center(), Align2::CENTER_CENTER, icon::CONTROLLER, se_ui_kit::theme::font(type_scale::SMALL + 1.0), t.accent);
            ui.interact(link, ui.id().with(("link", level_addr)), Sense::hover()).on_hover_text(format!("Linked to {}", signal_label(sig)));
        }
        None if learning => {
            p.text(link.center(), Align2::CENTER_CENTER, "Move it…", font_medium(type_scale::SMALL), t.yellow);
        }
        None => {}
    }
}

fn fx_readback(app: &mut App, ui: &mut Ui, t: &Theme, fx: &str) {
    let ty = app.m.str(&format!("{P}.fx.{fx}.type")).to_string();
    let params: Vec<(String, f64)> =
        app.m.under(&format!("{P}.fx.{fx}.param")).filter_map(|(a, v)| Some((a.rsplit('.').next()?.to_string(), v.as_f64()?))).collect();
    if ty.is_empty() && params.is_empty() {
        return;
    }
    ui.add_space(spacing::M);
    egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(radius::CONTROL)).inner_margin(egui::Margin::symmetric(12, 10)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal_wrapped(|ui| {
            let name = seg_label(app, &format!("fx.{fx}"));
            ui.label(RichText::new(if ty.is_empty() { name } else { format!("{name}: {}", nice(&ty)) }).font(font_semibold(type_scale::BODY)).color(t.fg));
            for (k, v) in &params {
                ui.label(RichText::new(format!("{} {:.0}%", nice(k), v * 100.0)).size(type_scale::SMALL + 0.5).color(t.text_dim));
            }
        });
        widgets::hint(ui, t, "Change the effect itself on the desk or in UC Surface.");
    });
}

// ---- saved mixes --------------------------------------------------------------------------------

fn store(app: &mut App, form: &mut Form, label: &str) {
    let name = slug(label);
    if name.is_empty() {
        return;
    }
    app.m.action("mixer.snapshot.store", Value::map().with("snapshot", name).with("label", label.trim()));
    form.store_name.clear();
    form.last_lists = 0.0;
}

fn saved_mixes(app: &mut App, ui: &mut Ui, t: &Theme, form: &mut Form) {
    let q = app.m.q("mixer.snapshots").cloned().unwrap_or_default();
    let list = q.get_path("snapshots").and_then(Value::as_list).unwrap_or(&[]).to_vec();
    let has_safe = list.iter().any(|x| s(x, "name") == "safe");
    widgets::titled(
        ui,
        t,
        "Saved mixes",
        "Save how the desk is set, then bring it back with one click.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if list.is_empty() {
                widgets::empty_state(ui, t, icon::SAVE, "No saved mixes yet", "Save the desk as it is now, for example “Just chatting” or “Band live”.", None);
            }
            for snap in &list {
                let name = s(snap, "name").to_string();
                let label = Some(s(snap, "label")).filter(|l| !l.is_empty()).map(str::to_string).unwrap_or_else(|| nice(&name));
                let default_fade = snap.get_path("fade_ms").and_then(Value::as_f64).unwrap_or(0.0) / 1000.0;
                let fade = form.fades.entry(name.clone()).or_insert(if default_fade > 0.0 { default_fade } else { 2.0 });
                // outlined rather than filled, so the buttons in it read as buttons
                egui::Frame::new()
                    .stroke(Stroke::new(1.0, t.border))
                    .corner_radius(CornerRadius::same(radius::CONTROL))
                    .inner_margin(egui::Margin::symmetric(12, 8))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.vertical(|ui| {
                                ui.label(RichText::new(&label).font(font_semibold(type_scale::BODY)).color(t.fg));
                                let n = snap.get_path("values").and_then(Value::as_i64).unwrap_or(0);
                                ui.label(RichText::new(format!("Remembers {n} desk settings")).size(type_scale::SMALL).color(t.text_dim))
                                    .on_hover_text(format!("Saved as {name}"));
                            });
                            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                ui.add(egui::DragValue::new(fade).range(0.1..=60.0).speed(0.1).suffix(" s").fixed_decimals(1))
                                    .on_hover_text("How long the fade takes");
                                if widgets::button_ex(ui, t, None, "Fade to it", Kind::Secondary, Size::Small, 0.0, true)
                                    .on_hover_text("Glide the desk over to this mix")
                                    .clicked()
                                {
                                    app.m.action(
                                        "mixer.snapshot.recall",
                                        Value::map().with("snapshot", name.clone()).with("fade", (*fade * 1000.0).round() as i64),
                                    );
                                }
                                if widgets::button_ex(ui, t, None, "Switch now", Kind::Secondary, Size::Small, 0.0, true)
                                    .on_hover_text("Jump to this mix instantly")
                                    .clicked()
                                {
                                    app.m.action("mixer.snapshot.recall", Value::map().with("snapshot", name.clone()).with("fade", 0));
                                }
                            });
                        });
                    });
                ui.add_space(spacing::XS);
            }
            for e in q.get_path("errors").and_then(Value::as_list).unwrap_or(&[]) {
                ui.label(
                    RichText::new(format!("{}  A saved mix couldn't be read: {}", icon::WARN, e.as_str().unwrap_or("")))
                        .size(type_scale::SMALL + 0.5)
                        .color(t.bright_red),
                );
            }
            ui.add_space(spacing::M);
            ui.horizontal(|ui| {
                let w = (ui.available_width() - 180.0).max(160.0);
                let r = ui.add(se_ui_kit::widgets::field(&mut form.store_name).hint_text("Name this mix, e.g. Just chatting").desired_width(w));
                let enter = r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let clicked = widgets::button_ex(ui, t, Some(icon::SAVE), "Save current mix", Kind::Primary, Size::Medium, 0.0, true)
                    .on_hover_text("Saves channel faders, mutes, pans and sends (not the output levels)")
                    .clicked();
                if clicked || (enter && !slug(&form.store_name).is_empty()) {
                    // no name typed: the next free "Mix N", so nothing saved gets overwritten
                    let typed = form.store_name.trim().to_string();
                    let label = if slug(&typed).is_empty() {
                        (list.len() + 1..).map(|n| format!("Mix {n}")).find(|l| !list.iter().any(|x| s(x, "name") == slug(l))).unwrap_or_default()
                    } else {
                        typed
                    };
                    store(app, form, &label);
                }
            });
            if !has_safe {
                ui.add_space(spacing::XS);
                widgets::hint(ui, t, "Tip: save one named “Safe”. “Go to safe mix” brings it back in an emergency.");
            }
            ui.add_space(spacing::S);
            widgets::details(ui, t, "mixer-snapshots", "Details", |ui| {
                widgets::hint(
                    ui,
                    t,
                    "Saved mixes live in your project's mixes folder (mixes/<name>.toml). Looks, reactions and presets can bring one back, e.g. mix = { snapshot = \"brb\", fade = \"2s\" }.",
                );
            });
        },
    );
}

// ---- linked controls ------------------------------------------------------------------------------

fn linked_controls(app: &mut App, ui: &mut Ui, t: &Theme, bound: &BTreeMap<String, String>) {
    widgets::titled(
        ui,
        t,
        "Linked controls",
        "Knobs and faders on your controllers that move the desk.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if bound.is_empty() {
                widgets::empty_state(
                    ui,
                    t,
                    icon::CONTROLLER,
                    "Nothing linked yet",
                    "Right-click a fader above, choose “Link a knob or fader”, then move the knob or fader you want.",
                    None,
                );
            }
            for (target, sig) in bound {
                egui::Frame::new().fill(t.surface_hi).corner_radius(CornerRadius::same(radius::CONTROL)).inner_margin(egui::Margin::symmetric(12, 8)).show(
                    ui,
                    |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(format!("{} ", icon::CONTROLLER)).color(t.accent));
                            ui.label(RichText::new(signal_label(sig)).font(font_medium(type_scale::BODY)).color(t.fg));
                            ui.label(RichText::new(icon::RIGHT).size(type_scale::SMALL).color(t.text_faint));
                            ui.label(RichText::new(target_label(app, target)).color(t.fg)).on_hover_text(format!("{sig} → {target}"));
                        });
                    },
                );
                ui.add_space(spacing::XS);
            }
            ui.add_space(spacing::S);
            widgets::details(ui, t, "mixer-midi", "Details", |ui| {
                widgets::hint(
                    ui,
                    t,
                    "Links are saved in your controller settings (controllers/faders.toml). Use takeover = \"pickup\" for faders without motors so levels don't jump. Desk fader values are positions (0.72 ≈ 0 dB), so keep the curve linear; each control updates the desk at most 50 times a second.",
                );
            });
        },
    );
}
