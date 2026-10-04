//! Live-page state and parts (§15.4): quick-effect pads mirroring the Stream Deck page (with
//! confirm and cooldown), the "running now" list, and the right-rail tabs. The page itself is
//! laid out in `views::live`.

use crate::app::{App, ViewId};
use crate::views::rail;
use egui::{Color32, Rect, RichText, Stroke, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RailTab {
    Events,
    Queue,
    Mod,
}

impl RailTab {
    pub const ALL: [(RailTab, &'static str, &'static str); 3] =
        [(RailTab::Queue, "Songs", icon::QUEUE), (RailTab::Events, "Activity", icon::ALERT), (RailTab::Mod, "Mod", icon::MOD)];
    pub fn id(self) -> &'static str {
        match self {
            RailTab::Events => "events",
            RailTab::Queue => "queue",
            RailTab::Mod => "mod",
        }
    }
    pub fn parse(s: &str) -> Option<RailTab> {
        RailTab::ALL.iter().map(|(t, _, _)| *t).find(|t| t.id() == s)
    }
}

pub struct ShowState {
    pub rail: RailTab,
    pub rail_width: f32,
    /// Explicit transition for the next Take (`None` = the scene's random pool).
    pub transition: Option<String>,
    /// Duration override for the next Take.
    pub take_ms: Option<u32>,
    /// Confirm-presets armed by a first press (`*` pads): name → armed at.
    pub armed: Option<(String, Instant)>,
    pub chat_input: String,
    pub chat_filter: String,
    pub multiview_in_main: bool,
    pub rail_state: rail::RailState,
    pub on_air: crate::views::live::OnAirView,
}

impl Default for ShowState {
    fn default() -> Self {
        ShowState {
            rail: RailTab::Queue,
            rail_width: 360.0,
            transition: None,
            take_ms: None,
            armed: None,
            chat_input: String::new(),
            chat_filter: String::new(),
            multiview_in_main: true,
            rail_state: rail::RailState::default(),
            on_air: Default::default(),
        }
    }
}

/// Seconds a confirm-preset stays armed after the first press.
const ARM_SECS: f32 = 3.0;

/// One pad of the Show-mode grid (deck key mirror or plain preset).
struct PadItem {
    label: String,
    icon: String,
    color: Option<Color32>,
    active: bool,
    /// Needs confirm or is chat-disabled (`*`).
    starred: bool,
    confirm: bool,
    progress: Option<f32>,
    cooldown: Option<f32>,
    fire: PadFire,
}

enum PadFire {
    Deck { key: i64, page: String },
    Preset(String),
    None,
}

fn preset_meta(app: &App, name: &str) -> Option<Value> {
    app.m.q_list("presets").iter().find(|p| p.get_path("name").and_then(Value::as_str) == Some(name)).cloned()
}

fn frac(rem_ms: Option<f64>, total_ms: Option<f64>) -> Option<f32> {
    let r = rem_ms.filter(|r| *r > 0.0)?;
    Some(match total_ms.filter(|t| *t > 0.0) {
        Some(t) => (r / t).clamp(0.0, 1.0) as f32,
        None => (r / 8000.0).min(1.0) as f32,
    })
}

fn pad_items(app: &App) -> (String, Vec<PadItem>) {
    if let Some(page) = app.m.q("controllers.page").filter(|p| p.get_path("keys").is_some()) {
        let pname = page.get_path("page").and_then(Value::as_str).unwrap_or("").to_string();
        let label = page.get_path("label").and_then(Value::as_str).unwrap_or(&pname).to_string();
        let mut keys: Vec<&Value> = page.get_path("keys").and_then(Value::as_list).map(|l| l.iter().collect()).unwrap_or_default();
        keys.sort_by_key(|k| k.get_path("key").and_then(Value::as_i64).unwrap_or(0));
        let items = keys
            .into_iter()
            .map(|k| {
                let key = k.get_path("key").and_then(Value::as_i64).unwrap_or(0);
                let preset = k.get_path("preset").and_then(Value::as_str);
                let pm = preset.and_then(|p| preset_meta(app, p));
                let confirm = pm.as_ref().and_then(|p| p.get_path("confirm")).is_some_and(Value::truthy);
                let chat = pm.as_ref().and_then(|p| p.get_path("chat")).is_none_or(Value::truthy);
                let rem = pm.as_ref().and_then(|p| p.get_path("remaining_ms")).and_then(Value::as_f64);
                let cd = k.get_path("cooldown_ms").or_else(|| pm.as_ref().and_then(|p| p.get_path("cooldown_ms"))).and_then(Value::as_f64);
                let cd_total = k.get_path("cooldown_total_ms").or_else(|| pm.as_ref().and_then(|p| p.get_path("cooldown_total_ms"))).and_then(Value::as_f64);
                PadItem {
                    label: k.get_path("label").and_then(Value::as_str).unwrap_or(preset.unwrap_or("")).to_string(),
                    icon: k.get_path("icon").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(icon::PRESET).to_string(),
                    color: k.get_path("color").and_then(Value::as_str).and_then(se_ui_kit::theme::hex),
                    active: k.get_path("active").is_some_and(Value::truthy) || pm.as_ref().and_then(|p| p.get_path("active")).is_some_and(Value::truthy),
                    starred: confirm || (preset.is_some() && !chat),
                    confirm,
                    progress: frac(rem, None),
                    cooldown: frac(cd, cd_total),
                    fire: PadFire::Deck { key, page: pname.clone() },
                }
            })
            .collect();
        return (format!("Presets (= Stream Deck page \"{label}\")"), items);
    }
    let items = app
        .m
        .q_list("presets")
        .iter()
        .map(|p| {
            let name = p.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
            let confirm = p.get_path("confirm").is_some_and(Value::truthy);
            let chat = p.get_path("chat").is_none_or(Value::truthy);
            PadItem {
                label: p.get_path("label").and_then(Value::as_str).unwrap_or(&name).to_string(),
                icon: icon::PRESET.to_string(),
                color: p.get_path("color").and_then(Value::as_str).and_then(se_ui_kit::theme::hex),
                active: p.get_path("active").is_some_and(Value::truthy),
                starred: confirm || !chat,
                confirm,
                progress: frac(p.get_path("remaining_ms").and_then(Value::as_f64), None),
                cooldown: frac(p.get_path("cooldown_ms").and_then(Value::as_f64), p.get_path("cooldown_total_ms").and_then(Value::as_f64)),
                fire: if name.is_empty() { PadFire::None } else { PadFire::Preset(name) },
            }
        })
        .collect();
    ("Presets (no deck page — all presets)".into(), items)
}

/// Fire pad `index` (0-based) of the current grid — used by clicks and F1–F12.
pub fn fire_pad(app: &mut App, index: usize) {
    let (_, items) = pad_items(app);
    let Some(item) = items.get(index) else { return };
    let id = match &item.fire {
        PadFire::Deck { key, page } => format!("deck:{page}:{key}"),
        PadFire::Preset(p) => format!("preset:{p}"),
        PadFire::None => return,
    };
    if item.confirm {
        let armed = app.show.armed.as_ref().is_some_and(|(n, at)| *n == id && at.elapsed().as_secs_f32() < ARM_SECS);
        if !armed {
            app.show.armed = Some((id, Instant::now()));
            app.m.toast(format!("{}: press again within {ARM_SECS:.0}s to confirm", item.label), false);
            return;
        }
        app.show.armed = None;
    }
    match &item.fire {
        PadFire::Deck { key, page } => app.m.action("deck.press", Value::map().with("key", *key).with("page", page.clone())),
        PadFire::Preset(p) => app.m.command(Op::PresetFire { name: p.clone(), payload: Value::Null }),
        PadFire::None => {}
    }
}

fn release_pad(app: &mut App, index: usize) {
    let (_, items) = pad_items(app);
    match items.get(index).map(|i| &i.fire) {
        Some(PadFire::Deck { key, page }) => app.m.action("deck.release", Value::map().with("key", *key).with("page", page.clone())),
        Some(PadFire::Preset(p)) => app.m.command(Op::PresetRelease { name: p.clone() }),
        _ => {}
    }
}

/// Where the pads come from, for the card subtitle.
pub fn pads_source(app: &App) -> String {
    match app.m.q("controllers.page").filter(|p| p.get_path("keys").is_some()) {
        Some(page) => {
            let name = page.get_path("label").or_else(|| page.get_path("page")).and_then(Value::as_str).unwrap_or("");
            format!("Same as your Stream Deck page \"{name}\". Right-click stops one.")
        }
        None => "Tap to start. Right-click stops one.".into(),
    }
}

pub fn pads(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let (_, items) = pad_items(app);
    if items.is_empty() {
        let deck_page = app.m.q("controllers.page").is_some_and(|p| p.get_path("keys").is_some());
        let (title, body, action, view) = if deck_page {
            (
                "No buttons assigned",
                "This Stream Deck page is blank. Assign a scene or saved action to a key in Buttons & pedals.",
                "Assign buttons",
                ViewId::Controllers,
            )
        } else {
            (
                "No buttons yet",
                "Pads mirror your Stream Deck page. Put saved actions or scenes on keys to see them here.",
                "Set up buttons",
                ViewId::Controllers,
            )
        };
        if widgets::empty_state(ui, &t, icon::BOLT, title, body, Some(action)) {
            app.open_view(view);
        }
        return;
    }
    let gap = 10.0;
    let cols = ((ui.available_width() + gap) / (118.0 + gap)).floor().clamp(2.0, 8.0) as usize;
    let w = (ui.available_width() - (cols as f32 - 1.0) * gap) / cols as f32;
    let size = Vec2::new(w, 84.0);
    let armed_id = app.show.armed.as_ref().filter(|(_, at)| at.elapsed().as_secs_f32() < ARM_SECS).map(|(n, _)| n.clone());
    let mut fire = None;
    let mut release = None;
    egui::Grid::new("pads").spacing(Vec2::splat(gap)).show(ui, |ui| {
        for (i, it) in items.iter().enumerate() {
            let id = match &it.fire {
                PadFire::Deck { key, page } => format!("deck:{page}:{key}"),
                PadFire::Preset(p) => format!("preset:{p}"),
                PadFire::None => String::new(),
            };
            let state = if it.active {
                LedState::Active
            } else if armed_id.as_deref() == Some(id.as_str()) {
                LedState::Armed
            } else {
                LedState::Idle
            };
            let hint = if i < 12 { app.keys_label(&format!("pad.{}", i + 1)) } else { String::new() };
            let label = crate::views::live::nice(&it.label);
            let r = widgets::pad(ui, &t, size, &it.icon, &label, it.color, state, it.progress, Some(&hint));
            if let Some(cd) = it.cooldown {
                // cooldown ring: arc around the pad edge proportional to the remaining cooldown
                let rect = r.rect.shrink(2.0);
                let p = ui.painter();
                let n = 48;
                let pts: Vec<egui::Pos2> = (0..=((n as f32 * cd) as usize)).map(|k| perimeter(rect, k as f32 / n as f32)).collect();
                if pts.len() >= 2 {
                    p.add(egui::Shape::line(pts, Stroke::new(3.0, t.blue)));
                }
            }
            let tip = match (it.confirm, it.starred) {
                (true, _) => "Tap twice to confirm. Right-click stops it.".to_string(),
                (false, true) => "Viewers can't trigger this one from chat. Right-click stops it.".to_string(),
                _ => "Right-click stops it.".to_string(),
            };
            let r = r.on_hover_text(tip);
            if r.clicked() {
                fire = Some(i);
            }
            if r.secondary_clicked() {
                release = Some(i);
            }
            if (i + 1) % cols == 0 {
                ui.end_row();
            }
        }
    });
    if let Some(i) = fire {
        fire_pad(app, i);
    }
    if let Some(i) = release {
        release_pad(app, i);
    }
}

/// Point at fraction `f` (0–1, clockwise from top-left) along a rect's perimeter.
fn perimeter(r: Rect, f: f32) -> egui::Pos2 {
    let (w, h) = (r.width(), r.height());
    let mut d = f.clamp(0.0, 1.0) * 2.0 * (w + h);
    if d <= w {
        return egui::pos2(r.left() + d, r.top());
    }
    d -= w;
    if d <= h {
        return egui::pos2(r.right(), r.top() + d);
    }
    d -= h;
    if d <= w {
        return egui::pos2(r.right() - d, r.bottom());
    }
    d -= w;
    egui::pos2(r.left(), r.bottom() - d.min(h))
}

enum Kill {
    Preset(String),
    Release(String),
    Action(&'static str, Value),
}

pub fn active(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let mut rows: Vec<(String, String, Option<f32>, String, Kill)> = Vec::new();
    for it in app.m.q_list("active") {
        let kind = it.get_path("kind").and_then(Value::as_str).unwrap_or("");
        let name = it.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
        let chat = it.get_path("chat").is_some_and(Value::truthy);
        let level = it.get_path("level").and_then(Value::as_f64).map(|l| l as f32);
        let rem = match it.get_path("remaining_ms").and_then(Value::as_f64) {
            Some(ms) => format!("{:.0}s left", (ms / 1000.0).ceil()),
            None => "until stopped".into(),
        };
        let (ic, kill) = if kind == "preset" { (icon::PRESET, Kill::Preset(name.clone())) } else { (icon::EFFECT, Kill::Release(name.clone())) };
        rows.push((ic.to_string(), format!("{}{}", name.replace('_', " "), if chat { "  · from chat" } else { "" }), level, rem, kill));
    }
    let cuelists: Vec<String> =
        app.m.under("lights.cuelist").filter_map(|(a, v)| a.strip_suffix(".playing").filter(|_| v.truthy()).map(String::from)).collect();
    for a in cuelists {
        let cl = a.trim_start_matches("lights.cuelist.").to_string();
        let cue = app.m.str(&format!("{a}.cue")).to_string();
        let master = app.m.get(&format!("{a}.master")).and_then(Value::as_f64).map(|v| v as f32);
        rows.push((
            icon::LIGHT.into(),
            format!("Lights: {cl}, cue {cue}"),
            master,
            "running".into(),
            Kill::Action("lights.release", Value::map().with("cuelist", cl)),
        ));
    }
    let timelines: Vec<String> = app.m.under("timeline").filter_map(|(a, v)| a.strip_suffix(".active").filter(|_| v.truthy()).map(String::from)).collect();
    for a in timelines {
        let name = a.trim_start_matches("timeline.").to_string();
        let tc = app.m.str(&format!("{a}.timecode")).to_string();
        let len = app.m.f(&format!("{a}.length"));
        let time = app.m.f(&format!("{a}.time"));
        let prog = (len > 0.0).then(|| (time / len).clamp(0.0, 1.0) as f32);
        rows.push((icon::TIMELINE.into(), format!("Timeline: {name}"), prog, tc, Kill::Action("timeline.stop", Value::map().with("name", name))));
    }
    if rows.is_empty() {
        widgets::hint(ui, &t, "Nothing running right now.");
        return;
    }
    let mut kill = None;
    for (i, (ic, name, level, rem, _)) in rows.iter().enumerate() {
        egui::Frame::new().fill(t.surface_hi).corner_radius(se_ui_kit::theme::radius::CONTROL).inner_margin(egui::Margin::symmetric(12, 8)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            ui.horizontal(|ui| {
                ui.label(RichText::new(ic).color(t.accent));
                ui.label(RichText::new(name).color(t.fg));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if widgets::icon_button(ui, &t, icon::CROSS, "Stop").clicked() {
                        kill = Some(i);
                    }
                    ui.label(RichText::new(rem).size(se_ui_kit::theme::type_scale::SMALL).color(t.text_dim));
                    if let Some(l) = level {
                        widgets::meter_h(ui, &t, Vec2::new(60.0, 5.0), *l);
                    }
                });
            });
        });
        ui.add_space(4.0);
    }
    if let Some(i) = kill {
        match rows.swap_remove(i).4 {
            Kill::Preset(n) => app.m.command(Op::PresetRelease { name: n }),
            Kill::Release(a) => app.m.command(Op::Release { address: a }),
            Kill::Action(n, args) => app.m.action(n, args),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn perimeter_walks_clockwise() {
        let r = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(10.0, 5.0));
        assert_eq!(perimeter(r, 0.0), egui::pos2(0.0, 0.0));
        assert_eq!(perimeter(r, 10.0 / 30.0), egui::pos2(10.0, 0.0));
        assert_eq!(perimeter(r, 0.5), egui::pos2(10.0, 5.0));
        assert_eq!(perimeter(r, 1.0), egui::pos2(0.0, 0.0));
    }

    #[test]
    fn remaining_fraction_uses_total_when_known() {
        assert_eq!(frac(Some(500.0), Some(1000.0)), Some(0.5));
        assert_eq!(frac(Some(0.0), Some(1000.0)), None);
        assert_eq!(frac(Some(16000.0), None), Some(1.0));
        assert_eq!(frac(None, Some(1.0)), None);
    }
}
