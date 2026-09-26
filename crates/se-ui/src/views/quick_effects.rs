//! Scenes → Quick effects (§15.5): the console for quick effects — one tap that makes
//! something happen on stream (a shake, a flash, confetti, lights, a sound). Quick effects are
//! made for the owner in the project files; here he tries them, turns the few knobs each one
//! exposes, reads what it does and sees where it can be fired (assigning happens in
//! Inputs → Buttons & pedals).
//!
//! Left: every quick effect with its pad color, what it does in one line and where it's used
//! (search when there are many), plus any whose file has a mistake. Right: the selected one —
//! Try it (Stop for ones that stay on), its knobs (live while it runs, saved by themselves),
//! Reset knobs, what it does in words, and where you can fire it.
//!
//! Engine: queries `presets` (with `knobs`), `presets.catalog`, `errors`, `controllers.page`;
//! action `preset.knob {name, target, value, save}`; commands `preset.fire` / `preset.release`.

use crate::app::{App, ViewId};
use crate::views::knobs::{self, Moved};
use crate::views::live::nice;
use crate::views::rules;
use egui::{Align, Color32, CornerRadius, Layout, Rect, RichText, Sense, Stroke, StrokeKind, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_medium, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, Tone, icon};
use std::collections::HashMap;

/// Seconds a knob has to rest before its value is saved (a slider let go saves at once).
const SAVE_AFTER: f64 = 0.6;
/// How often the catalog (labels, what each one does, where each is used) is refreshed.
const CATALOG_EVERY: f64 = 3.0;

fn state_id() -> egui::Id {
    egui::Id::new("quick-effects-view")
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn strs(v: &Value, k: &str) -> Vec<String> {
    v.get_path(k).and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(Value::as_str).map(String::from).collect()
}

fn push_new(v: &mut Vec<String>, x: String) {
    if !v.contains(&x) {
        v.push(x);
    }
}

// ---- engine data ----------------------------------------------------------------------------------

#[derive(Clone, Default)]
struct Use {
    kind: String,
    file: String,
    file_label: String,
    name: String,
}

/// What a quick effect does, in words.
#[derive(Clone, Default)]
struct About {
    screen: Vec<String>,
    lights: String,
    sound: Vec<String>,
    also: Vec<String>,
    length: String,
    /// One line for the list: "Color split, Confetti, Fast chase · 8 seconds".
    line: String,
}

/// `presets.catalog`, parsed once per reply.
#[derive(Clone, Default)]
struct Catalog {
    seq: u64,
    loaded: bool,
    /// name → label of each kind of thing a quick effect runs
    effects: HashMap<String, String>,
    overlays: HashMap<String, String>,
    looks: HashMap<String, String>,
    cuelists: HashMap<String, String>,
    sounds: HashMap<String, String>,
    about: HashMap<String, About>,
    uses: HashMap<String, Vec<Use>>,
}

/// "1 second", "2.5 seconds".
fn secs_words(s: f64) -> String {
    let s = (s * 10.0).round() / 10.0;
    if s == 1.0 {
        "1 second".into()
    } else if s.fract() == 0.0 {
        format!("{} seconds", s as i64)
    } else {
        format!("{s} seconds")
    }
}

impl Catalog {
    fn parse(v: &Value, seq: u64) -> Catalog {
        let labels = |k: &str, key: &str| -> HashMap<String, String> {
            v.get_path(k)
                .and_then(Value::as_list)
                .unwrap_or(&[])
                .iter()
                .map(|x| {
                    let (n, l) = (s(x, key), s(x, "label"));
                    (n.to_string(), if l.is_empty() { nice(n) } else { l.to_string() })
                })
                .collect()
        };
        let mut c = Catalog {
            seq,
            loaded: true,
            effects: labels("effects", "name"),
            overlays: labels("overlays", "id"),
            looks: labels("looks", "name"),
            cuelists: labels("cuelists", "name"),
            sounds: labels("sounds", "name"),
            about: HashMap::new(),
            uses: HashMap::new(),
        };
        if let Some(m) = v.get_path("used_by").and_then(Value::as_map) {
            for (id, l) in m {
                let uses = l
                    .as_list()
                    .unwrap_or(&[])
                    .iter()
                    .map(|u| Use { kind: s(u, "kind").into(), file: s(u, "file").into(), file_label: s(u, "file_label").into(), name: s(u, "name").into() })
                    .collect();
                c.uses.insert(id.clone(), uses);
            }
        }
        if let Some(m) = v.get_path("presets").and_then(Value::as_map) {
            let about: HashMap<String, About> = m.iter().map(|(id, p)| (id.clone(), c.describe(p))).collect();
            c.about = about;
        }
        c
    }

    fn label(map: &HashMap<String, String>, name: &str) -> String {
        map.get(name).cloned().unwrap_or_else(|| nice(name))
    }

    /// The words for one quick effect's summary from the catalog.
    fn describe(&self, p: &Value) -> About {
        let mut screen: Vec<String> = strs(p, "effects").iter().map(|e| Self::label(&self.effects, e)).collect();
        screen.extend(strs(p, "overlays").iter().map(|o| Self::label(&self.overlays, o)));
        let lights_name = s(p, "lights_name");
        let mut lights = match s(p, "lights") {
            "look" => Self::label(&self.looks, lights_name),
            "cuelist" => Self::label(&self.cuelists, lights_name),
            _ => String::new(),
        };
        let mut sound = Vec::new();
        let mut also = Vec::new();
        if !s(p, "sound").is_empty() {
            sound.push(format!("Plays “{}”", Self::label(&self.sounds, s(p, "sound"))));
        }
        for t in strs(p, "triggers") {
            let segs: Vec<&str> = t.split('.').collect();
            match segs.as_slice() {
                ["audio", "bus", bus, "fx", fx] => push_new(
                    &mut sound,
                    format!("{} on the {}", nice(fx), if *bus == "music" { "music".to_string() } else { format!("{} sound", nice(bus).to_lowercase()) }),
                ),
                ["audio", ..] => push_new(&mut sound, "A sound effect".into()),
                _ => push_new(&mut also, nice(segs.last().copied().unwrap_or(""))),
            }
        }
        for a in strs(p, "set") {
            if let Some(rest) = a.strip_prefix("fx.") {
                push_new(&mut screen, Self::label(&self.effects, rest.split('.').next().unwrap_or("")));
            } else if a.starts_with("lights.") {
                if lights.is_empty() {
                    lights = "Changes the lights".into();
                }
            } else if a.starts_with("audio.duck") {
                push_new(&mut sound, "Turns the music down".into());
            } else if a.starts_with("audio.") || a.starts_with("mixer.") {
                push_new(&mut sound, "Changes the sound mix".into());
            } else {
                push_new(&mut also, "Changes a setting".into());
            }
        }
        if !s(p, "mix").is_empty() {
            sound.push(format!("Recalls the sound mix “{}”", nice(s(p, "mix"))));
        }
        if !s(p, "scene").is_empty() {
            also.push(format!("Switches to the scene “{}”", nice(s(p, "scene"))));
        }
        if !s(p, "mode").is_empty() {
            also.push(format!("Switches to {} mode", nice(s(p, "mode")).to_lowercase()));
        }
        match p.get_path("steps").and_then(Value::as_i64).unwrap_or(0) {
            0 => {}
            1 => also.push("Runs one extra step".into()),
            n => also.push(format!("Runs {n} extra steps")),
        }
        let held = p.get_path("settings").and_then(Value::as_i64).unwrap_or(0) > 0;
        let (length, short) = if p.get_path("toggle").is_some_and(Value::truthy) {
            ("Until you press it again".to_string(), "until pressed again".to_string())
        } else if let Some(ms) = p.get_path("hold_ms").and_then(Value::as_f64) {
            (rules::capitalize(&secs_words(ms / 1000.0)), secs_words(ms / 1000.0))
        } else if held {
            ("Until you stop it".to_string(), "until stopped".to_string())
        } else {
            ("One burst".to_string(), "once".to_string())
        };
        let mut parts: Vec<String> = screen.clone();
        if !lights.is_empty() {
            parts.push(if s(p, "lights").is_empty() { "lights".into() } else { format!("{lights} lights") });
        }
        // "Plays “Air horn”" → "plays “Air horn”" in the middle of the line
        let decap = |x: &String| {
            let mut c = x.chars();
            c.next().map(|f| f.to_lowercase().chain(c).collect::<String>()).unwrap_or_default()
        };
        parts.extend(sound.iter().map(decap));
        parts.extend(also.iter().map(decap));
        let what = if parts.is_empty() { "Nothing yet".to_string() } else { rules::capitalize(&parts.join(", ")) };
        About { screen, lights, sound, also, length, line: format!("{what} · {short}") }
    }
}

/// One row of the list.
struct Item {
    id: String,
    label: String,
    color: Option<Color32>,
    active: bool,
    remaining: Option<f64>,
    /// Stays on (for a while or until released), so it can be stopped.
    held: bool,
    confirm: bool,
    knobs: Vec<Value>,
    /// Its file has a mistake (what's wrong); `listed` = the engine still knows a working
    /// version of it.
    broken: Option<String>,
    listed: bool,
}

fn items(app: &App) -> Vec<Item> {
    let mut v: Vec<Item> = app
        .m
        .q_list("presets")
        .iter()
        .filter(|p| !s(p, "name").is_empty())
        .map(|p| Item {
            id: s(p, "name").to_string(),
            label: nice(s(p, "label")),
            color: se_ui_kit::theme::hex(s(p, "color")),
            active: p.get_path("active").is_some_and(Value::truthy),
            remaining: p.get_path("remaining_ms").and_then(Value::as_f64).map(|ms| ms / 1000.0),
            held: p.get_path("held").is_some_and(Value::truthy),
            confirm: p.get_path("confirm").is_some_and(Value::truthy),
            knobs: p.get_path("knobs").and_then(Value::as_list).map(<[Value]>::to_vec).unwrap_or_default(),
            broken: None,
            listed: true,
        })
        .collect();
    for e in app.m.q_list("errors") {
        let Some(id) = s(e, "file").strip_prefix("presets/").and_then(|f| f.strip_suffix(".toml")) else { continue };
        let msg = s(e, "msg").to_string();
        match v.iter_mut().find(|i| i.id == id) {
            Some(i) => i.broken = Some(msg),
            None => v.push(Item {
                id: id.to_string(),
                label: nice(id),
                color: None,
                active: false,
                remaining: None,
                held: false,
                confirm: false,
                knobs: Vec::new(),
                broken: Some(msg),
                listed: false,
            }),
        }
    }
    v.sort_by_key(|i| i.label.to_lowercase());
    v
}

/// A config error in plain words: `knob “Speed”: `min` (12) …` → `The “Speed” knob: min (12) …`.
fn plain_error(msg: &str) -> String {
    let msg = msg.replace('`', "");
    match msg.strip_prefix("knob “").and_then(|r| r.split_once("”: ")) {
        Some((name, rest)) => format!("The “{name}” knob: {rest}"),
        None => rules::capitalize(&msg),
    }
}

// ---- state ----------------------------------------------------------------------------------------

/// A knob being turned: the value it shows until the engine reports it back.
#[derive(Clone)]
struct Edit {
    value: Value,
    at: f64,
    saved: bool,
}

#[derive(Clone, Default)]
struct State {
    selected: Option<String>,
    search: String,
    confirm_air: bool,
    cat: Catalog,
    catalog_at: f64,
    /// (quick effect, knob target) → the value being set.
    edits: HashMap<(String, String), Edit>,
    /// The quick effect whose knobs were last saved, and when.
    saved_at: Option<(String, f64)>,
    /// Keep a just-made selection until the engine lists it.
    hold_until: f64,
}

/// Turn a knob in the engine: `save = false` only moves what's running (and what the next
/// firing uses); `save = true` also writes it into the quick effect's file.
fn send_knob(app: &mut App, id: &str, target: &str, v: &Value, save: bool) {
    app.m.action("preset.knob", Value::map().with("name", id).with("target", target).with("value", v.clone()).with("save", save));
}

/// Save knobs that have rested, and forget edits once the engine shows their value.
fn settle(app: &mut App, ui: &egui::Ui, st: &mut State, list: &[Item], now: f64) {
    let mut due = Vec::new();
    for ((id, target), e) in &mut st.edits {
        if !e.saved && now - e.at >= SAVE_AFTER {
            e.saved = true;
            due.push((id.clone(), target.clone(), e.value.clone()));
        }
    }
    for (id, target, v) in due {
        send_knob(app, &id, &target, &v, true);
        st.saved_at = Some((id, now));
        app.m.refresh_soon();
    }
    if st.edits.values().any(|e| !e.saved) {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(100));
    }
    let reported = |id: &str, target: &str| {
        list.iter().find(|i| i.id == id).and_then(|i| i.knobs.iter().find(|k| s(k, "target") == target)).and_then(|k| k.get_path("value")).cloned()
    };
    st.edits.retain(|(id, target), e| {
        let back = reported(id, target).is_some_and(|v| v.as_f64().zip(e.value.as_f64()).map_or(v == e.value, |(a, b)| (a - b).abs() < 1e-6));
        !e.saved || (!back && now - e.at < 10.0)
    });
}

/// Open this tab with `preset` selected (Lights' "Put on a pad").
pub fn open(app: &mut App, ctx: &egui::Context, preset: &str) {
    let now = ctx.input(|i| i.time);
    let mut st: State = ctx.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<State>(state_id())));
    st.selected = Some(preset.to_string());
    st.confirm_air = false;
    st.hold_until = now + 5.0;
    st.catalog_at = 0.0;
    ctx.data_mut(|d| d.insert_temp(state_id(), st));
    app.m.refresh_soon();
    app.open_view(ViewId::QuickEffects);
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let now = ui.input(|i| i.time);
    let mut st: State = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<State>(state_id())));
    view(app, ui, &mut st, now);
    ui.data_mut(|d| d.insert_temp(state_id(), st));
}

fn view(app: &mut App, ui: &mut egui::Ui, st: &mut State, now: f64) {
    let t = app.t.clone();
    if app.m.connected && (!st.cat.loaded || now - st.catalog_at > CATALOG_EVERY) {
        st.catalog_at = now;
        app.m.query("presets.catalog", Value::Null);
    }
    let seq = app.m.q_seq("presets.catalog");
    if seq != st.cat.seq
        && let Some(v) = app.m.q("presets.catalog")
    {
        st.cat = Catalog::parse(v, seq);
    }
    let list = items(app);
    // keep the selection while a just-made one waits for the engine to list it
    let keep = now < st.hold_until || st.selected.as_ref().is_some_and(|s| list.iter().any(|i| i.id == *s));
    if !keep {
        st.selected = list.first().map(|i| i.id.clone());
        st.confirm_air = false;
    }
    settle(app, ui, st, &list, now);

    let w = ui.available_width();
    let h = ui.available_height();
    let list_w = (w * 0.22).clamp(300.0, 380.0);
    let gap = spacing::L;
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = gap;
        ui.allocate_ui_with_layout(Vec2::new(list_w, h), Layout::top_down(Align::Min), |ui| list_panel(ui, &t, st, &list));
        ui.allocate_ui_with_layout(Vec2::new(w - list_w - gap, h), Layout::top_down(Align::Min), |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().id_salt("quick-console").auto_shrink([false, false]).show(ui, |ui| {
                match st.selected.clone().and_then(|id| list.iter().find(|i| i.id == id)) {
                    Some(it) => editor(app, ui, &t, st, it, &list, now),
                    None if st.selected.is_some() => {
                        widgets::panel(ui, &t, |ui| {
                            ui.set_width(ui.available_width());
                            widgets::empty_state(ui, &t, icon::CLOCK, "Opening…", "", None);
                        });
                    }
                    None => {
                        widgets::panel(ui, &t, |ui| {
                            ui.set_width(ui.available_width());
                            widgets::empty_state(
                                ui,
                                &t,
                                icon::BOLT,
                                "No quick effects yet",
                                "A quick effect is one tap that makes something happen on stream: a shake, a flash, confetti, lights or a sound. They're made for you — ask for the look you want.",
                                None,
                            );
                        });
                    }
                }
            });
        });
    });
}

// ---- left: the list ------------------------------------------------------------------------------

fn list_panel(ui: &mut egui::Ui, t: &Theme, st: &mut State, list: &[Item]) {
    let sub = match list.len() {
        0 => "None yet".to_string(),
        1 => "1 quick effect".to_string(),
        n => format!("{n} quick effects"),
    };
    widgets::titled(
        ui,
        t,
        "Quick effects",
        &sub,
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let body_h = ui.available_height() - 40.0;
            ui.set_min_height(body_h);
            if list.len() > 8 {
                ui.add(widgets::field(&mut st.search).hint_text(format!("{}  Search", icon::SEARCH)).desired_width(ui.available_width()));
                ui.add_space(spacing::S);
            }
            let q = st.search.trim().to_lowercase();
            egui::ScrollArea::vertical().id_salt("quick-list").auto_shrink([false, true]).max_height((ui.available_height() - 64.0).max(120.0)).show(
                ui,
                |ui| {
                    let mut shown = 0;
                    for it in list {
                        let line = match (&it.broken, st.cat.about.get(&it.id)) {
                            (Some(_), _) if !it.listed => "Its file has a mistake",
                            (_, Some(a)) => a.line.as_str(),
                            _ => "",
                        };
                        if !q.is_empty() && !it.label.to_lowercase().contains(&q) && !line.to_lowercase().contains(&q) {
                            continue;
                        }
                        shown += 1;
                        let uses = st.cat.uses.get(&it.id).map_or(0, Vec::len);
                        let (trailing, tc) = if it.broken.is_some() {
                            ("Needs a fix".to_string(), t.yellow)
                        } else if it.active {
                            ("Running".to_string(), t.green)
                        } else if uses == 1 {
                            ("Used once".to_string(), t.text_dim)
                        } else if uses > 1 {
                            (format!("Used {uses}×"), t.text_dim)
                        } else {
                            (String::new(), t.text_dim)
                        };
                        let selected = st.selected.as_deref() == Some(it.id.as_str());
                        if list_row(ui, t, it.color, &it.label, line, &trailing, tc, selected).clicked() && !selected {
                            st.selected = Some(it.id.clone());
                            st.confirm_air = false;
                        }
                    }
                    if list.is_empty() {
                        widgets::hint(ui, t, "Nothing here yet.");
                    } else if shown == 0 {
                        widgets::hint(ui, t, "Nothing matches your search.");
                    }
                },
            );
            ui.add_space(spacing::M);
            ui.horizontal_wrapped(|ui| {
                ui.label(RichText::new(icon::INFO).color(t.text_faint));
                ui.label(RichText::new("New quick effects are made for you — ask for the look you want.").size(type_scale::SMALL + 0.5).color(t.text_dim));
            });
        },
    );
}

/// Text cut to `max_w` with "…" (single line).
fn fit(ui: &egui::Ui, text: &str, fid: egui::FontId, color: Color32, max_w: f32) -> std::sync::Arc<egui::Galley> {
    let mut job = egui::text::LayoutJob::single_section(text.to_string(), egui::TextFormat::simple(fid, color));
    job.wrap = egui::text::TextWrapping::truncate_at_width(max_w.max(10.0));
    ui.painter().layout_job(job)
}

/// A list row with the pad color, name, one-line description and where it's used.
#[allow(clippy::too_many_arguments)]
fn list_row(ui: &mut egui::Ui, t: &Theme, color: Option<Color32>, title: &str, subtitle: &str, trailing: &str, tc: Color32, selected: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 56.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, CornerRadius::same(radius::CONTROL), mix(t.surface, t.accent, 0.14));
    } else if resp.hovered() {
        p.rect_filled(rect, CornerRadius::same(radius::CONTROL), t.surface_hi);
    }
    let sw = Rect::from_center_size(egui::pos2(rect.left() + 12.0 + 11.0, rect.center().y), Vec2::splat(22.0));
    swatch(ui, t, sw, color, 6);
    let x = sw.right() + 12.0;
    let mut right = rect.right() - 12.0;
    if !trailing.is_empty() {
        let g = ui.painter().layout_no_wrap(trailing.to_string(), font(type_scale::SMALL + 0.5), tc);
        ui.painter().galley(egui::pos2(right - g.size().x, rect.center().y - g.size().y / 2.0), g.clone(), tc);
        right -= g.size().x + 10.0;
    }
    let tg = fit(ui, title, font_medium(type_scale::BODY), t.fg, right - x);
    let sg = fit(ui, subtitle, font(type_scale::SMALL), t.text_dim, right - x);
    let elided = tg.elided || sg.elided;
    ui.painter().galley(egui::pos2(x, rect.center().y - 9.0 - tg.size().y / 2.0), tg, t.fg);
    ui.painter().galley(egui::pos2(x, rect.center().y + 10.0 - sg.size().y / 2.0), sg, t.text_dim);
    let resp = resp.on_hover_cursor(egui::CursorIcon::PointingHand);
    if elided { resp.on_hover_text(format!("{title}\n{subtitle}")) } else { resp }
}

/// The pad color as a rounded square.
fn swatch(ui: &egui::Ui, t: &Theme, r: Rect, color: Option<Color32>, rounding: u8) {
    let p = ui.painter();
    match color {
        Some(c) => p.rect(r, CornerRadius::same(rounding), c, Stroke::new(1.0, mix(c, t.border, 0.5)), StrokeKind::Inside),
        None => p.rect(r, CornerRadius::same(rounding), t.surface_hi, Stroke::new(1.0, t.border), StrokeKind::Inside),
    };
}

// ---- right: the console ------------------------------------------------------------------------

fn editor(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, it: &Item, list: &[Item], now: f64) {
    header(app, ui, t, st, it, now);
    if let Some(msg) = &it.broken {
        ui.add_space(spacing::M);
        let body = if it.listed {
            format!("{} It keeps working the way it was before the mistake. Ask for it to be fixed, or go back to an earlier version.", plain_error(msg))
        } else {
            format!("{} It can't run until that's fixed. Ask for it to be fixed, or go back to an earlier version.", plain_error(msg))
        };
        if widgets::callout(ui, t, Tone::Warn, icon::WARN, "This quick effect's file has a mistake", &body, Some("Open History")) {
            app.open_view(ViewId::History);
        }
    }
    if !it.listed {
        return;
    }
    ui.add_space(spacing::L);
    let cat = std::mem::take(&mut st.cat);
    let about = cat.about.get(&it.id).cloned().unwrap_or_default();
    let w = ui.available_width();
    let gap = spacing::L;
    if w >= 1000.0 {
        let lw = ((w - gap) * 0.56).floor();
        let rw = w - gap - lw;
        ui.horizontal_top(|ui| {
            ui.spacing_mut().item_spacing.x = gap;
            ui.allocate_ui_with_layout(Vec2::new(lw, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(lw);
                knobs_card(app, ui, t, st, it, now);
            });
            ui.allocate_ui_with_layout(Vec2::new(rw, 0.0), Layout::top_down(Align::Min), |ui| {
                ui.set_width(rw);
                about_card(ui, t, it, &about);
                ui.add_space(gap);
                where_fired(app, ui, t, &cat, &it.id, list);
            });
        });
    } else {
        knobs_card(app, ui, t, st, it, now);
        ui.add_space(gap);
        about_card(ui, t, it, &about);
        ui.add_space(gap);
        where_fired(app, ui, t, &cat, &it.id, list);
    }
    ui.add_space(spacing::XL);
    st.cat = cat;
}

// ---- header: pad, name, try it -------------------------------------------------------------------

fn header(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, it: &Item, now: f64) {
    let on_air = crate::views::status::on_air(app);
    let line = st.cat.about.get(&it.id).map(|a| a.line.clone()).unwrap_or_default();
    widgets::panel(ui, t, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::splat(44.0), Sense::hover());
            swatch(ui, t, r, it.color, 10);
            ui.add_space(spacing::S);
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(&it.label).font(font_semibold(type_scale::HEADING)).color(t.fg));
                    if it.active {
                        let txt = match it.remaining.filter(|r| *r > 0.0) {
                            Some(r) => format!("Running · {}", secs_words(r.ceil())),
                            None => "Running".into(),
                        };
                        widgets::badge(ui, t, &txt, if on_air { t.bright_red } else { t.green });
                    }
                });
                if it.listed && !line.is_empty() {
                    ui.add(egui::Label::new(RichText::new(&line).size(type_scale::BODY).color(t.text_dim)).truncate());
                }
            });
            if !it.listed {
                return;
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if it.active && it.held {
                    if widgets::button_ex(ui, t, Some(icon::STOP), "Stop", Kind::Danger, Size::Large, 0.0, true).clicked() {
                        app.m.command(Op::PresetRelease { name: it.id.clone() });
                    }
                } else {
                    let (lbl, kind) = if on_air { ("Try it on air", Kind::Live) } else { ("Try it", Kind::Primary) };
                    let tip = if on_air { "You're on air: your viewers will see it." } else { "You're off air: only you see it." };
                    if widgets::button_ex(ui, t, Some(icon::PLAY), lbl, kind, Size::Large, 0.0, true).on_hover_text(tip).clicked() {
                        if on_air {
                            st.confirm_air = true;
                        } else {
                            try_it(app, st, &it.id, now);
                        }
                    }
                }
            });
        });
    });
    if st.confirm_air {
        ui.add_space(spacing::M);
        ui.horizontal(|ui| {
            let w = ui.available_width() - 110.0;
            ui.allocate_ui_with_layout(Vec2::new(w, 0.0), Layout::top_down(Align::Min), |ui| {
                if widgets::callout(
                    ui,
                    t,
                    Tone::Danger,
                    icon::LIVE,
                    "Your viewers will see this",
                    "You're on air, so it runs on stream right now.",
                    Some("Try it on air"),
                ) {
                    st.confirm_air = false;
                    try_it(app, st, &it.id, now);
                }
            });
            if widgets::button_ex(ui, t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                st.confirm_air = false;
            }
        });
    }
}

fn try_it(app: &mut App, st: &mut State, id: &str, now: f64) {
    // knobs still resting go to the file now; the engine already fires with their values
    for ((eid, target), e) in &mut st.edits {
        if eid == id && !e.saved {
            e.saved = true;
            e.at = now;
            send_knob(app, eid, target, &e.value, true);
        }
    }
    app.m.command(Op::PresetFire { name: id.to_string(), payload: Value::Null });
    app.m.refresh_soon();
}

// ---- knobs ---------------------------------------------------------------------------------------

fn knobs_card(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, it: &Item, now: f64) {
    let value_of = |st: &State, k: &Value| -> Value {
        st.edits.get(&(it.id.clone(), s(k, "target").to_string())).map(|e| e.value.clone()).unwrap_or_else(|| k.get_path("value").cloned().unwrap_or_default())
    };
    let resettable = it
        .knobs
        .iter()
        .any(|k| knobs::default_of(k).is_some_and(|d| d.as_f64().zip(value_of(st, k).as_f64()).map_or(*d != value_of(st, k), |(a, b)| (a - b).abs() > 1e-9)));
    let sub = if it.knobs.is_empty() {
        "This one has nothing to tune."
    } else if it.active {
        "It's running: you see and hear changes right away."
    } else {
        "Changes save by themselves and apply every time it fires."
    };
    let mut reset = false;
    let saving = st.edits.iter().any(|((id, _), e)| *id == it.id && !e.saved);
    let saved = st.saved_at.as_ref().is_some_and(|(id, s)| *id == it.id && now - s < 2.5);
    widgets::titled(
        ui,
        t,
        "Knobs",
        sub,
        |ui| {
            if !it.knobs.is_empty() {
                reset = widgets::button_ex(ui, t, Some(icon::UNDO), "Reset knobs", Kind::Ghost, Size::Small, 0.0, resettable)
                    .on_hover_text("Put every knob back where it started.")
                    .clicked();
            }
            if saving {
                widgets::badge(ui, t, "Saving…", t.text_dim);
            } else if saved {
                widgets::badge(ui, t, "Saved", t.green);
                ui.ctx().request_repaint_after(std::time::Duration::from_millis(500));
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            if it.knobs.is_empty() {
                widgets::hint(ui, t, "Try it to see what it does. If you'd like to adjust something about it, ask for a knob.");
                return;
            }
            let gap = spacing::M;
            let w = ui.available_width();
            let cols = (((w + gap) / (360.0 + gap)).floor() as usize).clamp(1, it.knobs.len());
            let cw = ((w - gap * (cols as f32 - 1.0)) / cols as f32).floor();
            for row in it.knobs.chunks(cols) {
                ui.horizontal_top(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for k in row {
                        let target = s(k, "target").to_string();
                        let mut v = value_of(st, k);
                        let moved = ui
                            .allocate_ui_with_layout(Vec2::new(cw, 0.0), Layout::top_down(Align::Min), |ui| {
                                ui.set_width(cw);
                                egui::Frame::new()
                                    .fill(t.surface_hi)
                                    .corner_radius(CornerRadius::same(radius::CONTROL))
                                    .inner_margin(egui::Margin::symmetric(14, 12))
                                    .show(ui, |ui| {
                                        ui.set_width(ui.available_width());
                                        knobs::control(ui, t, ("quick", &it.id, &target), k, &mut v)
                                    })
                                    .inner
                            })
                            .inner;
                        if moved != Moved::No {
                            let done = moved == Moved::Done;
                            send_knob(app, &it.id, &target, &v, done);
                            if done {
                                st.saved_at = Some((it.id.clone(), now));
                                app.m.refresh_soon();
                            }
                            st.edits.insert((it.id.clone(), target), Edit { value: v, at: now, saved: done });
                        }
                    }
                });
                ui.add_space(gap);
            }
        },
    );
    if reset {
        for k in &it.knobs {
            if let Some(d) = knobs::default_of(k) {
                let target = s(k, "target").to_string();
                send_knob(app, &it.id, &target, d, true);
                st.edits.insert((it.id.clone(), target), Edit { value: d.clone(), at: now, saved: true });
            }
        }
        st.saved_at = Some((it.id.clone(), now));
        app.m.refresh_soon();
    }
}

// ---- what it does / where ------------------------------------------------------------------------

fn about_card(ui: &mut egui::Ui, t: &Theme, it: &Item, a: &About) {
    widgets::titled(
        ui,
        t,
        "What it does",
        "Everything it does when you fire it, all at once.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let row = |ui: &mut egui::Ui, ic: &str, label: &str, words: &str| {
                ui.horizontal_top(|ui| {
                    let cell = |ui: &mut egui::Ui, w: f32, text: RichText| {
                        ui.allocate_ui_with_layout(Vec2::new(w, 20.0), Layout::left_to_right(Align::Center), |ui| {
                            ui.set_min_width(w);
                            ui.label(text);
                        });
                    };
                    cell(ui, 22.0, RichText::new(ic).color(t.accent));
                    cell(ui, 96.0, RichText::new(label).font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
                    ui.add(egui::Label::new(RichText::new(words).size(type_scale::BODY).color(t.fg)).wrap());
                });
                ui.add_space(spacing::XS);
            };
            if !a.screen.is_empty() {
                row(ui, icon::WAND, "On screen", &a.screen.join(", "));
            }
            if !a.lights.is_empty() {
                row(ui, icon::LIGHT, "Lights", &a.lights);
            }
            if !a.sound.is_empty() {
                row(ui, icon::VOLUME, "Sound", &a.sound.join(", "));
            }
            if !a.also.is_empty() {
                row(ui, icon::LAYERS, "Also", &a.also.join(", "));
            }
            if !a.length.is_empty() {
                row(ui, icon::CLOCK, "How long", &a.length);
            }
            if it.confirm {
                row(ui, icon::HAND, "Safety", "You hold its pad to confirm, and chat can't fire it.");
            }
        },
    );
}

fn where_fired(app: &mut App, ui: &mut egui::Ui, t: &Theme, cat: &Catalog, id: &str, list: &[Item]) {
    let uses = cat.uses.get(id).map(Vec::as_slice).unwrap_or(&[]);
    let deck_page = app.m.q("controllers.page").is_some_and(|p| p.get_path("keys").is_some());
    widgets::titled(
        ui,
        t,
        "Where you can fire it",
        "",
        |ui| {
            if widgets::button_ex(ui, t, Some(icon::CONTROLLER), "Buttons & pedals", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                app.open_view(ViewId::Controllers);
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            widgets::hint(ui, t, "Set buttons and pedals in Inputs; automatic reactions in Automation.");
            ui.add_space(spacing::S);
            let row = |ui: &mut egui::Ui, ic: &str, words: &str| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(ic).color(t.text_dim));
                    ui.add(egui::Label::new(RichText::new(words).color(t.fg)).truncate());
                });
            };
            if !deck_page {
                row(ui, icon::LIVE, "Its pad on Overview");
            }
            for u in uses {
                let (ic, words) = use_words(u, list);
                row(ui, ic, &words);
            }
            if uses.is_empty() {
                ui.add_space(spacing::XS);
                widgets::hint(ui, t, "Nothing else fires it yet: add it to a Stream Deck key, a pedal, a reaction or a chat command.");
            }
        },
    );
}

/// Plain words for one place that fires a quick effect.
fn use_words(u: &Use, list: &[Item]) -> (&'static str, String) {
    let stem = u.file.rsplit('/').next().unwrap_or("").trim_end_matches(".toml");
    let file = if u.file_label.is_empty() { nice(stem) } else { u.file_label.clone() };
    let name = if u.name.is_empty() { file.clone() } else { u.name.clone() };
    match u.kind.as_str() {
        "rules" => (icon::BOLT, format!("Reaction “{name}”")),
        "commands" => (icon::BOT, format!("Chat command “{name}”")),
        "controllers" => (icon::CONTROLLER, if u.name.is_empty() { file } else { format!("{file}: {name}") }),
        "timelines" => (icon::TIMELINE, format!("Timeline “{file}”")),
        "rewards" => (icon::TWITCH, format!("Channel points reward “{name}”")),
        "presets" => (icon::SPARKLE, format!("Quick effect “{}”", list.iter().find(|i| i.id == stem).map(|i| i.label.clone()).unwrap_or_else(|| nice(stem)))),
        "bindings" => (icon::LINK, format!("Link “{}” (only while it runs)", nice(&name))),
        "alerts" => (icon::ALERT, format!("Alert “{name}”")),
        "scenes" => (icon::LAYERS, format!("Scene “{name}”")),
        "layouts" => (icon::KEYBOARD, "A keyboard shortcut".into()),
        k => (icon::LINK, format!("{} “{name}”", nice(k))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_it_does_reads_as_words() {
        let cat = Catalog {
            effects: HashMap::from([("rgb_split".into(), "Color split".into()), ("vhs".into(), "VHS tape".into())]),
            overlays: HashMap::from([("confetti".into(), "Confetti".into())]),
            cuelists: HashMap::from([("chase_fast".into(), "Fast chase".into())]),
            sounds: HashMap::from([("airhorn".into(), "Air horn".into())]),
            ..Default::default()
        };
        let hype = Value::map()
            .with("effects", Value::List(vec!["rgb_split".into()]))
            .with("overlays", Value::List(vec!["confetti".into()]))
            .with("lights", "cuelist")
            .with("lights_name", "chase_fast")
            .with("sound", "airhorn")
            .with("hold_ms", 8000)
            .with("settings", 1)
            .with("set", Value::List(vec!["fx.rgb_split.amount".into()]));
        let a = cat.describe(&hype);
        assert_eq!(a.screen, ["Color split", "Confetti"], "an effect in both `fx` and `set` shows once");
        assert_eq!(a.lights, "Fast chase");
        assert_eq!(a.sound, ["Plays “Air horn”"]);
        assert_eq!(a.length, "8 seconds");
        let duck = Value::map().with("toggle", true).with("settings", 1).with("set", Value::List(vec!["audio.duck.active".into()]));
        let a = cat.describe(&duck);
        assert_eq!((a.sound.as_slice(), a.length.as_str()), (["Turns the music down".to_string()].as_slice(), "Until you press it again"));
        let stutter = Value::map().with("triggers", Value::List(vec!["audio.bus.music.fx.stutter".into()])).with("hold_ms", 2000);
        assert_eq!(cat.describe(&stutter).sound, ["Stutter on the music"]);
    }
}
