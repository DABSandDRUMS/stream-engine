//! Show mode (§15.4): preview/program monitors, scenes with thumbnails and number keys,
//! transition picker + TAKE, preset pads mirroring the Stream Deck page, the Active list, the
//! right rail (events/chat/queue/mod), the multiview, and the mix strip.

use crate::app::App;
use crate::views::{mix, monitor, rail};
use egui::{Align2, Color32, CornerRadius, FontId, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::widgets::{self, LedState, icon, led_color};
use std::time::Instant;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RailTab {
    Events,
    Chat,
    Queue,
    Mod,
}

impl RailTab {
    pub const ALL: [(RailTab, &'static str, &'static str); 4] = [
        (RailTab::Events, "EVENTS", icon::ALERT),
        (RailTab::Chat, "CHAT", icon::CHAT),
        (RailTab::Queue, "QUEUE", icon::QUEUE),
        (RailTab::Mod, "MOD", icon::MOD),
    ];
    pub fn id(self) -> &'static str {
        match self {
            RailTab::Events => "events",
            RailTab::Chat => "chat",
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
}

impl Default for ShowState {
    fn default() -> Self {
        ShowState {
            rail: RailTab::Events,
            rail_width: 360.0,
            transition: None,
            take_ms: None,
            armed: None,
            chat_input: String::new(),
            chat_filter: String::new(),
            multiview_in_main: true,
            rail_state: rail::RailState::default(),
        }
    }
}

/// Seconds a confirm-preset stays armed after the first press.
const ARM_SECS: f32 = 3.0;

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    egui::Panel::right("rail").resizable(true).default_size(app.show.rail_width).min_size(260.0).show(ui, |ui| rail_panel(app, ui));
    egui::Panel::bottom("mix").resizable(true).default_size(150.0).min_size(90.0).show(ui, |ui| mix::strip(app, ui));
    if app.show.multiview_in_main && !app.is_popped_out("multiview") {
        egui::Panel::bottom("multiview").resizable(true).default_size(130.0).min_size(70.0).show(ui, |ui| {
            widgets::section(ui, &t, icon::SCENE, "Multiview");
            let h = (ui.available_height() - 4.0).max(40.0);
            monitor::multiview(app, ui, h);
        });
    }
    egui::CentralPanel::default().show(ui, |ui| {
        egui::ScrollArea::vertical().id_salt("show-main").show(ui, |ui| {
            monitor::preview_program_row(app, ui);
            ui.add_space(6.0);
            scenes(app, ui);
            transition_row(app, ui);
            ui.add_space(6.0);
            ui.columns(2, |cols| {
                pads(app, &mut cols[0]);
                active(app, &mut cols[1]);
            });
        });
    });
}

pub fn rail_panel(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    ui.horizontal(|ui| {
        for (tab, label, ic) in RailTab::ALL {
            let badge = rail::badge_count(app, tab);
            let text = if badge > 0 { format!("{ic} {label} ({badge})") } else { format!("{ic} {label}") };
            let rt = RichText::new(text).strong().color(if app.show.rail == tab { t.accent } else { t.fg_dim });
            if ui.selectable_label(app.show.rail == tab, rt).clicked() {
                app.show.rail = tab;
            }
        }
    });
    ui.separator();
    match app.show.rail {
        RailTab::Events => rail::events(app, ui),
        RailTab::Chat => rail::chat(app, ui),
        RailTab::Queue => rail::queue(app, ui),
        RailTab::Mod => rail::moderation(app, ui),
    }
}

fn scene_list(app: &App) -> Vec<(String, String, i64)> {
    app.m
        .q_list("scenes")
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let name = s.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
            let label = s.get_path("label").and_then(Value::as_str).unwrap_or(&name).to_string();
            let key = s.get_path("key").and_then(Value::as_i64).unwrap_or(i as i64 + 1);
            (name, label, key)
        })
        .collect()
}

pub fn scenes(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    widgets::section(ui, &t, icon::SCENE, "Scenes");
    let program = app.m.str("show.scene.program").to_string();
    let preview = app.m.str("show.scene.preview").to_string();
    let direct = app.m.b("show.direct");
    let list = scene_list(app);
    if list.is_empty() {
        ui.label(RichText::new(if app.m.connected { "no scenes in the project (scenes/*.toml)" } else { "engine offline" }).color(t.fg_dim));
        return;
    }
    ui.horizontal_wrapped(|ui| {
        for (name, label, key) in &list {
            let st = if *name == program {
                LedState::Active
            } else if *name == preview {
                LedState::Armed
            } else {
                LedState::Idle
            };
            let size = Vec2::new(150.0, 104.0);
            let (rect, r) = ui.allocate_exact_size(size, Sense::click());
            if ui.is_rect_visible(rect) {
                let thumb = Rect::from_min_size(rect.min + Vec2::new(3.0, 3.0), Vec2::new(size.x - 6.0, (size.x - 6.0) * 9.0 / 16.0));
                monitor::scene_thumb(app, ui, thumb, name);
                let p = ui.painter_at(rect);
                let edge = if st == LedState::Idle { t.muted } else { led_color(&t, st) };
                let fill = if r.hovered() { t.bg_light } else { t.bg_dark };
                let bar = Rect::from_min_max(egui::pos2(rect.left(), thumb.bottom() + 2.0), rect.right_bottom());
                p.rect_filled(bar, CornerRadius::same(4), fill);
                p.text(bar.left_center() + Vec2::new(8.0, 0.0), Align2::LEFT_CENTER, format!("{key}"), FontId::proportional(12.0), t.fg_dim);
                p.text(bar.center(), Align2::CENTER_CENTER, label, FontId::proportional(13.0), t.fg);
                let tag = match st {
                    LedState::Active => "PGM",
                    LedState::Armed => "PVW",
                    _ => "",
                };
                p.text(bar.right_center() - Vec2::new(8.0, 0.0), Align2::RIGHT_CENTER, tag, FontId::proportional(10.0), edge);
                p.rect_stroke(rect, CornerRadius::same(5), Stroke::new(if st == LedState::Idle { 1.0 } else { 3.0 }, edge), StrokeKind::Inside);
            }
            let hint = format!("{key}: to preview · Shift+{key}: direct to program · double-click: take directly");
            let r = r.on_hover_text(hint);
            if r.double_clicked() || (r.clicked() && direct) {
                app.m.command(Op::SceneCut { scene: name.clone(), transition: app.show.transition.clone() });
            } else if r.clicked() {
                app.m.command(Op::SceneGo { scene: name.clone() });
            }
        }
    });
}

/// "random: morph ×3 · fade · zoomblur  500–900ms" for the preview scene's pool.
fn pool_label(app: &App, scene: &str) -> String {
    let Some(s) = app.build.scene_config(scene) else { return "scene pool (random)".into() };
    let Some(pool) = s.get_path("transitions.pool").and_then(Value::as_list).filter(|l| !l.is_empty()) else {
        return "default transition".into();
    };
    let parts: Vec<String> = pool
        .iter()
        .filter_map(|e| {
            let n = e.get_path("name").and_then(Value::as_str).or(e.as_str())?;
            let w = e.get_path("w").and_then(Value::as_f64).unwrap_or(1.0);
            Some(if (w - 1.0).abs() > f64::EPSILON { format!("{n} ×{w}") } else { n.to_string() })
        })
        .collect();
    let ms = match s.get_path("transitions.ms") {
        Some(Value::List(l)) if l.len() == 2 => format!("  {}–{}ms", l[0], l[1]),
        Some(v) if v.as_f64().is_some() => format!("  {v}ms"),
        _ => String::new(),
    };
    format!("random: {}{ms}", parts.join(" · "))
}

pub fn transition_row(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let preview = app.m.str("show.scene.preview").to_string();
    ui.horizontal(|ui| {
        ui.label(RichText::new("TRANSITION").small().strong().color(t.fg_dim));
        let pool = pool_label(app, &preview);
        let cur = app.show.transition.clone().unwrap_or_else(|| pool.clone());
        egui::ComboBox::from_id_salt("transition").width(300.0).selected_text(cur).show_ui(ui, |ui| {
            if ui.selectable_label(app.show.transition.is_none(), pool.as_str()).clicked() {
                app.show.transition = None;
            }
            for n in app.m.q_list("transitions").iter().filter_map(|v| v.as_str().map(String::from)).collect::<Vec<_>>() {
                if ui.selectable_label(app.show.transition.as_deref() == Some(&n), &n).clicked() {
                    app.show.transition = Some(n);
                }
            }
        });
        let mut ms = app.show.take_ms.unwrap_or(0) as f64;
        let r = ui.add(
            egui::DragValue::new(&mut ms)
                .range(0.0..=10000.0)
                .speed(10.0)
                .suffix(" ms")
                .custom_formatter(|v, _| if v <= 0.0 { "auto".into() } else { format!("{v:.0}") }),
        );
        if r.changed() {
            app.show.take_ms = (ms > 0.0).then_some(ms as u32);
        }
        r.on_hover_text("Take duration (auto = the pool's range)");
        if app.m.b("show.transition.active") {
            ui.add(egui::ProgressBar::new(app.m.f("show.transition.progress") as f32).desired_width(140.0).text(app.m.str("show.transition.name").to_string()));
        } else if app.m.has("show.transition.name") {
            ui.label(RichText::new(format!("last: {} {}ms", app.m.str("show.transition.name"), app.m.f("show.transition.ms") as i64)).small().color(t.fg_dim));
        }
        let mut direct = app.m.b("show.direct");
        if ui.checkbox(&mut direct, "direct").on_hover_text("Scene clicks go straight to program").changed() {
            app.m.command(Op::Set { address: "show.direct".into(), value: Value::Bool(direct) });
        }
        let take = egui::Button::new(RichText::new(format!("TAKE {}", app.keys_label("take"))).strong().size(16.0).color(t.fg_bright))
            .fill(t.red.gamma_multiply(0.75))
            .min_size(Vec2::new(130.0, 34.0));
        if ui.add_enabled(!preview.is_empty(), take).on_hover_text("Preview → program").clicked() {
            app.take();
        }
    });
}

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

pub fn pads(app: &mut App, ui: &mut egui::Ui) {
    let t = app.t.clone();
    let (title, items) = pad_items(app);
    widgets::section(ui, &t, icon::PRESET, &title);
    if items.is_empty() {
        ui.label(RichText::new("no presets (presets/*.toml) and no deck page").color(t.fg_dim));
        return;
    }
    let cols = 5usize;
    let w = ((ui.available_width() - (cols as f32 - 1.0) * ui.spacing().item_spacing.x) / cols as f32).clamp(70.0, 150.0);
    let size = Vec2::new(w, (w * 0.62).clamp(52.0, 90.0));
    let armed_id = app.show.armed.as_ref().filter(|(_, at)| at.elapsed().as_secs_f32() < ARM_SECS).map(|(n, _)| n.clone());
    let mut fire = None;
    let mut release = None;
    egui::Grid::new("pads").spacing(Vec2::splat(ui.spacing().item_spacing.x)).show(ui, |ui| {
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
            let label = if it.starred { format!("{}*", it.label) } else { it.label.clone() };
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
            let tip = match (&it.fire, it.confirm) {
                (PadFire::Deck { key, page }, _) => format!("deck page {page} key {key}{}", if it.confirm { " · press twice to confirm" } else { "" }),
                (PadFire::Preset(p), true) => format!("preset {p} · press twice to confirm"),
                (PadFire::Preset(p), false) => format!("preset {p} · right-click releases"),
                _ => String::new(),
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
    widgets::section(ui, &t, icon::EFFECT, "Active");
    let mut rows: Vec<(String, String, Option<f32>, String, Kill)> = Vec::new();
    for it in app.m.q_list("active") {
        let kind = it.get_path("kind").and_then(Value::as_str).unwrap_or("");
        let name = it.get_path("name").and_then(Value::as_str).unwrap_or("").to_string();
        let chat = it.get_path("chat").is_some_and(Value::truthy);
        let level = it.get_path("level").and_then(Value::as_f64).map(|l| l as f32);
        let rem = match it.get_path("remaining_ms").and_then(Value::as_f64) {
            Some(ms) => format!("{:.1}s", ms / 1000.0),
            None => "latched".into(),
        };
        let (ic, kill) = if kind == "preset" { (icon::PRESET, Kill::Preset(name.clone())) } else { (icon::EFFECT, Kill::Release(name.clone())) };
        rows.push((ic.to_string(), format!("{name}{}", if chat { " (chat)" } else { "" }), level, rem, kill));
    }
    let cuelists: Vec<String> =
        app.m.under("lights.cuelist").filter_map(|(a, v)| a.strip_suffix(".playing").filter(|_| v.truthy()).map(String::from)).collect();
    for a in cuelists {
        let cl = a.trim_start_matches("lights.cuelist.").to_string();
        let cue = app.m.str(&format!("{a}.cue")).to_string();
        let master = app.m.get(&format!("{a}.master")).and_then(Value::as_f64).map(|v| v as f32);
        rows.push((
            icon::LIGHT.into(),
            format!("{cl} · cue {cue}"),
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
        rows.push((
            icon::TIMELINE.into(),
            format!("{name} · {}", app.m.str(&format!("{a}.status"))),
            prog,
            tc,
            Kill::Action("timeline.stop", Value::map().with("name", name)),
        ));
    }
    if rows.is_empty() {
        ui.label(RichText::new("nothing running").color(t.fg_dim));
        return;
    }
    let mut kill = None;
    egui::Grid::new("active").striped(true).num_columns(4).show(ui, |ui| {
        for (i, (ic, name, level, rem, _)) in rows.iter().enumerate() {
            ui.label(format!("{ic} {name}"));
            match level {
                Some(l) => ui.add(egui::ProgressBar::new(*l).desired_width(90.0)),
                None => ui.label(""),
            };
            ui.label(RichText::new(rem).monospace());
            if ui.button(icon::CROSS).on_hover_text("kill").clicked() {
                kill = Some(i);
            }
            ui.end_row();
        }
    });
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
