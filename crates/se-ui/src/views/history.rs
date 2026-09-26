//! Settings → History: every version of the project (saved automatically a few seconds after
//! each change, or named by the user) as a timeline in plain words, "Save a version…", and
//! "Go back to this". Also the "Undo last change" / "Redo" buttons in the Scenes and Lights
//! page headers ([`undo_button`]).

use crate::app::App;
use crate::views::live::nice;
use egui::{Align, Layout, RichText, Sense, Ui, UiBuilder, Vec2};
use se_proto::Value;
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_medium, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, Tone, icon};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ---- words ------------------------------------------------------------------------------------

/// The plain name of a project file: "Hype (quick effect)", "Duo (scene)", "Project settings".
pub fn friendly(path: &str) -> String {
    let parts: Vec<&str> = path.split('/').collect();
    let file = parts.last().copied().unwrap_or(path);
    let stem = file.rsplit_once('.').map_or(file, |(s, _)| s);
    let named = |what: &str, id: &str| format!("{} ({what})", nice(id));
    match parts.as_slice() {
        ["project.toml"] => "Project settings".into(),
        ["scenes", _] => named("scene", stem),
        ["presets", _] => named("quick effect", stem),
        ["patches", id, ..] => named("overlay", id),
        ["transitions", _] => named("transition", stem),
        ["lights", "palettes", _] => named("light look", stem),
        ["lights", "cuelists", _] => named("cue list", stem),
        ["lights", "effects", _] => named("light effect", stem),
        ["lights", "fixtures", _] => named("light fixture", stem),
        ["lights", ..] => "Light setup".into(),
        ["assets", "images", ..] => named("picture", stem),
        ["assets", "video", ..] => named("video", stem),
        ["assets", "sounds", ..] => named("sound", stem),
        ["assets", "luts", ..] => named("color look", stem),
        ["assets", "fonts", ..] => named("font", stem),
        ["assets", ..] => named("media", stem),
        ["rules", _] => named("reactions", stem),
        ["alerts", _] => named("alert", stem),
        ["commands", _] => named("chat commands", stem),
        ["controllers", _] => named("buttons & pedals", stem),
        ["bindings", _] => named("signal link", stem),
        ["timelines", _] => named("timeline", stem),
        ["sources", _] => named("camera", stem),
        ["audio", ..] => "Sound settings".into(),
        ["layouts", _] => named("window layout", stem),
        ["rewards", _] => named("channel reward", stem),
        ["mixes", _] => named("mixing desk scene", stem),
        _ => named("file", stem),
    }
}

/// Friendly names of changed paths, each once (an overlay's several files are one thing).
fn things(paths: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for p in paths {
        let f = friendly(p);
        if !out.contains(&f) {
            out.push(f);
        }
    }
    out
}

fn paths(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_list).map(|l| l.iter().filter_map(|p| p.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// "A", "A and B", "A, B and 3 more".
fn summary(names: &[String], max: usize) -> String {
    match names {
        [] => String::new(),
        [a] => a.clone(),
        [a, b] if max >= 2 => format!("{a} and {b}"),
        _ if names.len() <= max => format!("{} and {}", names[..names.len() - 1].join(", "), names[names.len() - 1]),
        _ => format!("{} and {} more", names[..max].join(", "), names.len() - max),
    }
}

/// "Changed Hype (quick effect)", "Changed 3 things: Hype (quick effect), Duo (scene) and 1 more".
fn changed_sentence(names: &[String]) -> String {
    match names.len() {
        0 => "Nothing changed".into(),
        1 => format!("Changed {}", names[0]),
        n => format!("Changed {n} things: {}", summary(names, 2)),
    }
}

// ---- time -------------------------------------------------------------------------------------

fn now_s() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Local calendar time of a unix timestamp.
fn local(ts: i64) -> Option<libc::tm> {
    let t = ts as libc::time_t;
    // SAFETY: `localtime_r` only writes the `tm` we own; a zeroed `tm` is a valid value.
    unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        (!libc::localtime_r(&t, &mut tm).is_null()).then_some(tm)
    }
}

/// ("Today" / "Yesterday" / "Thu 24 Sep", "2:32 pm") for unix milliseconds.
fn day_time(at_ms: i64) -> (String, String) {
    const DAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
    const MONTHS: [&str; 12] = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];
    let (Some(tm), Some(today)) = (local(at_ms.div_euclid(1000)), local(now_s())) else { return (String::new(), String::new()) };
    let h12 = match tm.tm_hour % 12 {
        0 => 12,
        h => h,
    };
    let time = format!("{h12}:{:02} {}", tm.tm_min, if tm.tm_hour < 12 { "am" } else { "pm" });
    let same_year = tm.tm_year == today.tm_year;
    let day = if same_year && tm.tm_yday == today.tm_yday {
        "Today".to_string()
    } else if same_year && tm.tm_yday + 1 == today.tm_yday {
        "Yesterday".to_string()
    } else {
        let d = format!("{} {} {}", DAYS[tm.tm_wday.clamp(0, 6) as usize], tm.tm_mday, MONTHS[tm.tm_mon.clamp(0, 11) as usize]);
        if same_year { d } else { format!("{d} {}", tm.tm_year + 1900) }
    };
    (day, time)
}

/// "Today, 2:32 pm".
fn when(at_ms: i64) -> String {
    let (d, t) = day_time(at_ms);
    format!("{d}, {t}")
}

/// "today at 2:32 pm", "yesterday at 9:10 pm", "on Thu 24 Sep at 8:04 pm" (inside a sentence).
fn when_in_sentence(at_ms: i64) -> String {
    let (d, t) = day_time(at_ms);
    match d.as_str() {
        "Today" | "Yesterday" => format!("{} at {t}", d.to_lowercase()),
        _ => format!("on {d} at {t}"),
    }
}

// ---- the timeline -----------------------------------------------------------------------------

/// One version, ready to draw (built once per reply, not per frame).
struct Row {
    id: String,
    at_ms: i64,
    kind: String,
    label: String,
    day: String,
    time: String,
    title: String,
    subtitle: String,
    things: Vec<String>,
    paths: Vec<String>,
}

impl Row {
    fn from(v: &Value) -> Row {
        let s = |k: &str| v.get_path(k).and_then(Value::as_str).unwrap_or("").to_string();
        let at_ms = v.get_path("at_ms").and_then(Value::as_i64).unwrap_or(0);
        let kind = s("kind");
        let label = s("label");
        let paths = paths(v.get_path("changed"));
        let things = things(&paths);
        let from = v.get_path("restored_from").filter(|f| !matches!(f, Value::Null));
        let from_at = from.and_then(|f| f.get_path("at_ms")).and_then(Value::as_i64);
        let from_label = from.and_then(|f| f.get_path("label")).and_then(Value::as_str).filter(|l| !l.is_empty()).map(|l| format!("“{l}”"));
        let back_to = match (from_label, from_at) {
            (Some(l), _) => l,
            (None, Some(at)) => format!("how it was {}", when_in_sentence(at)),
            (None, None) => "an older version".into(),
        };
        let first = v.get_path("first").is_some_and(Value::truthy);
        let (title, subtitle) = match kind.as_str() {
            _ if first => ("History starts here".to_string(), "Your project as it was when Stream Engine started keeping versions".to_string()),
            "named" => {
                (format!("“{label}”"), if things.is_empty() { "Saved by you".to_string() } else { format!("Saved by you · {}", changed_sentence(&things)) })
            }
            "restore" => (format!("Went back to {back_to}"), changed_sentence(&things)),
            "undo" => ("Undid a change".to_string(), format!("Back to how it was {}", from_at.map_or_else(String::new, when_in_sentence))),
            "redo" => ("Redid the change you undid".to_string(), changed_sentence(&things)),
            _ => (changed_sentence(&things), "Saved automatically".to_string()),
        };
        let (day, time) = day_time(at_ms);
        Row { id: s("id"), at_ms, kind, label, day, time, title, subtitle, things, paths }
    }

    fn icon(&self) -> &'static str {
        match self.kind.as_str() {
            "named" => icon::STAR,
            "restore" | "undo" | "redo" => icon::UNDO,
            _ => icon::EDIT,
        }
    }
}

#[derive(Default)]
struct State {
    rows: Vec<Row>,
    /// Reply sequence the rows were built from, and when (day words go stale at midnight).
    built_seq: u64,
    built_at: Option<Instant>,
    /// `project.versions.latest` the list was fetched for.
    asked_for: Option<(u64, String)>,
    asked_at: Option<Instant>,
    selected: Option<String>,
    name: String,
}

type Shared = Arc<Mutex<State>>;

fn state(ui: &Ui) -> Shared {
    ui.ctx().data_mut(|d| d.get_temp_mut_or_insert_with::<Shared>(egui::Id::new("se.history.view"), Shared::default).clone())
}

/// Fetch the list when a version is added (or every 30 s), and rebuild rows on a new reply.
fn refresh(app: &mut App, st: &mut State) {
    if !app.m.connected {
        return;
    }
    let latest = (app.m.conn_gen, app.m.str("project.versions.latest").to_string());
    if st.asked_for.as_ref() != Some(&latest) || st.asked_at.is_none_or(|t| t.elapsed().as_secs() >= 30) {
        st.asked_for = Some(latest);
        st.asked_at = Some(Instant::now());
        app.m.query("project.versions", Value::Null);
    }
    let seq = app.m.q_seq("project.versions");
    if seq != st.built_seq || st.built_at.is_none_or(|t| t.elapsed().as_secs() >= 60) {
        st.built_seq = seq;
        st.built_at = Some(Instant::now());
        st.rows = app.m.q_list("project.versions").iter().map(Row::from).collect();
    }
}

fn go_back(app: &mut App, row: &Row) {
    app.m.action("project.version.restore", Value::map().with("id", row.id.clone()));
    let what = if row.kind == "named" { format!("“{}”", row.label) } else { when(row.at_ms) };
    app.m.toast(format!("Going back to {what}. Undo brings your changes back."), false);
}

pub fn ui(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    let st = state(ui);
    let mut st = st.lock().unwrap_or_else(|p| p.into_inner());
    refresh(app, &mut st);

    let failing = app.m.get("health.versions").and_then(|v| v.get_path("status")).and_then(Value::as_str) == Some("fail");
    if failing {
        let detail = app.m.get("health.versions").and_then(|v| v.get_path("detail")).and_then(Value::as_str).unwrap_or("").to_string();
        widgets::callout(
            ui,
            &t,
            Tone::Danger,
            icon::WARN,
            "New versions can't be saved right now",
            "Make sure the disk isn't full (Settings → Backups shows the space). Stream Engine tries again every minute.",
            None,
        );
        widgets::details(ui, &t, "history-problem", "Details", |ui| {
            ui.add(egui::Label::new(RichText::new(detail).color(t.text_dim)).wrap());
        });
        ui.add_space(spacing::M);
    }
    widgets::callout(
        ui,
        &t,
        Tone::Info,
        icon::CLOCK,
        "Every change is saved",
        "A few seconds after you change something, Stream Engine keeps a version of your project. Go back to any of them, and undo that too. Recordings and stream history aren't affected.",
        None,
    );
    ui.add_space(spacing::L);

    if !app.m.connected {
        widgets::panel(ui, &t, |ui| {
            ui.set_width(ui.available_width());
            widgets::empty_state(ui, &t, icon::CLOCK, "Stream Engine isn't running", "Your project's history shows up here once it's running.", None);
        });
        return;
    }

    let w = ui.available_width();
    let cols = if w >= 2400.0 {
        3
    } else if w >= 1200.0 {
        2
    } else {
        1
    };
    if st.selected.as_ref().is_none_or(|id| !st.rows.iter().any(|r| &r.id == id)) {
        st.selected = st.rows.first().map(|r| r.id.clone());
    }
    let gaps = ui.spacing().item_spacing;
    ui.spacing_mut().item_spacing.x = spacing::L;
    match cols {
        1 => {
            egui::ScrollArea::vertical().id_salt("history-one").auto_shrink([false, false]).show(ui, |ui| {
                ui.spacing_mut().item_spacing = gaps;
                save_card(app, ui, &t, &mut st);
                ui.add_space(spacing::L);
                timeline(app, ui, &t, &mut st, None);
                ui.add_space(spacing::L);
                selected_card(app, ui, &t, &st);
            });
        }
        n => {
            ui.columns(n, |c| {
                for col in c.iter_mut() {
                    col.spacing_mut().item_spacing = gaps;
                }
                let h = c[0].available_height();
                timeline(app, &mut c[0], &t, &mut st, Some(h));
                egui::ScrollArea::vertical().id_salt("history-side").auto_shrink([false, false]).show(&mut c[1], |ui| {
                    selected_card(app, ui, &t, &st);
                    if n == 2 {
                        ui.add_space(spacing::L);
                        save_card(app, ui, &t, &mut st);
                    }
                });
                if n == 3 {
                    save_card(app, &mut c[2], &t, &mut st);
                }
            });
        }
    }
}

/// "Save a version": a name, kept forever.
fn save_card(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State) {
    widgets::titled(
        ui,
        t,
        "Save a version",
        "Give how things are now a name. Named versions are kept forever.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            let mut save = false;
            ui.horizontal(|ui| {
                let r = ui
                    .add(widgets::field(&mut st.name).hint_text("A name, like Before Friday's show").desired_width((ui.available_width() - 110.0).max(160.0)));
                save |= r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                let ok = !st.name.trim().is_empty();
                save |= widgets::button_ex(ui, t, Some(icon::SAVE), "Save", Kind::Primary, Size::Medium, 90.0, ok).clicked();
                save &= ok;
            });
            if save {
                let name = st.name.trim().to_string();
                app.m.action("project.version.save", Value::map().with("label", name.clone()));
                app.m.toast(format!("Saved “{name}”"), false);
                st.name.clear();
            }
            ui.add_space(spacing::S);
            widgets::hint(ui, t, "Versions from the last week are all kept, then one a day for 90 days.");
        },
    );
}

/// The versions, newest first, grouped by day.
fn timeline(app: &mut App, ui: &mut Ui, t: &Theme, st: &mut State, height: Option<f32>) {
    let n = st.rows.len();
    let sub = match n {
        0 => "Nothing saved yet".to_string(),
        1 => "1 version · newest first".to_string(),
        n => format!("{n} versions · newest first"),
    };
    let mut pick: Option<String> = None;
    let mut back: Option<usize> = None;
    widgets::titled(
        ui,
        t,
        "Versions",
        &sub,
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if st.rows.is_empty() {
                let waiting = app.m.q("project.versions").is_none();
                let (title, body) = if waiting {
                    ("Looking for versions…", "This takes a second.")
                } else {
                    ("No versions yet", "A version is saved by itself a few seconds after you change something.")
                };
                widgets::empty_state(ui, t, icon::CLOCK, title, body, None);
                return;
            }
            let body = |ui: &mut Ui| {
                let mut day = "";
                for (i, r) in st.rows.iter().enumerate() {
                    if r.day != day {
                        day = &r.day;
                        if i > 0 {
                            ui.add_space(spacing::S);
                        }
                        widgets::section(ui, t, "", &r.day);
                    }
                    let (clicked, confirmed) = row(ui, t, r, st.selected.as_deref() == Some(r.id.as_str()), i == 0);
                    if clicked {
                        pick = Some(r.id.clone());
                    }
                    if confirmed {
                        back = Some(i);
                    }
                }
            };
            let sa = egui::ScrollArea::vertical().id_salt("history-timeline").auto_shrink([false, false]);
            match height {
                // fill the column: the card's padding and title take ~90 px
                Some(h) => sa.max_height((h - 110.0).max(200.0)).show(ui, body),
                None => sa.max_height(560.0).show(ui, body),
            };
        },
    );
    if let Some(id) = pick {
        st.selected = Some(id);
    }
    if let Some(i) = back {
        go_back(app, &st.rows[i]);
    }
}

/// One timeline row: icon, time, what happened, and "Go back to this" (with a confirm).
/// Returns (row clicked, going back confirmed).
fn row(ui: &mut Ui, t: &Theme, r: &Row, selected: bool, newest: bool) -> (bool, bool) {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 58.0), Sense::click());
    let p = ui.painter();
    if selected {
        p.rect_filled(rect, radius::CONTROL, mix(t.surface, t.accent, 0.14));
    } else if resp.hovered() {
        p.rect_filled(rect, radius::CONTROL, t.surface_hi);
    }
    let mut confirmed = false;
    let mut inner = ui.new_child(UiBuilder::new().max_rect(rect.shrink2(Vec2::new(12.0, 6.0))).layout(Layout::left_to_right(Align::Center)));
    let ui = &mut inner;
    ui.add_sized(
        Vec2::new(22.0, 22.0),
        egui::Label::new(RichText::new(r.icon()).size(type_scale::BODY + 1.0).color(if selected { t.accent } else { t.text_dim })).selectable(false),
    );
    ui.add_space(spacing::XS);
    ui.add_sized(Vec2::new(74.0, 22.0), egui::Label::new(RichText::new(&r.time).size(type_scale::SMALL + 0.5).color(t.text_dim)).selectable(false));
    let right = if newest { 90.0 } else { 160.0 };
    let text_w = (ui.available_width() - right).max(80.0);
    ui.allocate_ui_with_layout(Vec2::new(text_w, 46.0), Layout::top_down(Align::Min), |ui| {
        ui.add_space(3.0);
        ui.add(egui::Label::new(RichText::new(&r.title).font(font_medium(type_scale::BODY)).color(t.fg)).truncate().selectable(false));
        ui.add(egui::Label::new(RichText::new(&r.subtitle).size(type_scale::SMALL).color(t.text_dim)).truncate().selectable(false));
    });
    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
        if newest {
            widgets::badge(ui, t, "Now", t.green).on_hover_text("This is how your project is right now.");
            return;
        }
        let b = widgets::button_ex(ui, t, Some(icon::UNDO), "Go back to this", Kind::Secondary, Size::Small, 0.0, true);
        confirmed = confirm_go_back(t, &b, r);
    });
    (resp.clicked(), confirmed)
}

/// The "Go back to this version?" popup under `button`; true when confirmed.
fn confirm_go_back(t: &Theme, button: &egui::Response, r: &Row) -> bool {
    let mut confirmed = false;
    egui::Popup::menu(button).show(|ui| {
        ui.set_min_width(320.0);
        ui.set_max_width(360.0);
        ui.label(RichText::new("Go back to this version?").font(font_semibold(type_scale::LARGE)).color(t.fg));
        ui.add_space(spacing::XS);
        let what = if r.kind == "named" { format!("“{}” ({})", r.label, when(r.at_ms)) } else { when(r.at_ms) };
        ui.add(egui::Label::new(RichText::new(format!("Your scenes, effects, overlays and settings go back to how they were at {what}."))).wrap());
        ui.add(
            egui::Label::new(
                RichText::new("You can undo this: how things are now is saved first. Recordings and stream history stay as they are.").color(t.text_dim),
            )
            .wrap(),
        );
        ui.add_space(spacing::S);
        ui.horizontal(|ui| {
            if widgets::button_ex(ui, t, Some(icon::UNDO), "Go back", Kind::Primary, Size::Medium, 0.0, true).clicked() {
                confirmed = true;
                ui.close();
            }
            if widgets::button_ex(ui, t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
                ui.close();
            }
        });
    });
    confirmed
}

/// Everything about the selected version: when, what changed (all of it), go back.
fn selected_card(app: &mut App, ui: &mut Ui, t: &Theme, st: &State) {
    let Some((i, r)) = st.rows.iter().enumerate().find(|(_, r)| st.selected.as_deref() == Some(r.id.as_str())) else {
        return;
    };
    let mut back = false;
    widgets::titled(
        ui,
        t,
        &r.title,
        &when(r.at_ms),
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            ui.add(egui::Label::new(RichText::new(&r.subtitle).color(t.text_dim)).wrap());
            if !r.things.is_empty() {
                ui.add_space(spacing::S);
                widgets::section(ui, t, "", &if r.things.len() == 1 { "What changed".to_string() } else { format!("What changed ({})", r.things.len()) });
                const SHOWN: usize = 24;
                for name in r.things.iter().take(SHOWN) {
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(icon::DOT).size(type_scale::SMALL).color(t.text_faint));
                        ui.add(egui::Label::new(RichText::new(name).color(t.fg)).truncate());
                    });
                }
                if r.things.len() > SHOWN {
                    widgets::hint(ui, t, &format!("…and {} more", r.things.len() - SHOWN));
                }
            }
            ui.add_space(spacing::M);
            if i == 0 {
                widgets::hint(ui, t, "This is how your project is right now.");
            } else {
                let b = widgets::button_ex(ui, t, Some(icon::UNDO), "Go back to this", Kind::Primary, Size::Medium, 0.0, true);
                back = confirm_go_back(t, &b, r);
            }
            if !r.paths.is_empty() {
                ui.add_space(spacing::S);
                widgets::details(ui, t, ("history-files", &r.id), "Details", |ui| {
                    for p in &r.paths {
                        ui.add(egui::Label::new(RichText::new(p).font(se_ui_kit::theme::font_mono(type_scale::SMALL)).color(t.text_dim)).truncate());
                    }
                });
            }
        },
    );
    if back {
        go_back(app, r);
    }
}

// ---- undo / redo in page headers --------------------------------------------------------------

/// `project.versions.undo` / `.redo`: (kind, time, changed things).
fn step(app: &App, key: &str) -> Option<(String, i64, Vec<String>)> {
    let v = app.m.get(key).filter(|v| !matches!(v, Value::Null))?;
    let kind = v.get_path("kind").and_then(Value::as_str).unwrap_or("edit").to_string();
    let at = v.get_path("at_ms").and_then(Value::as_i64).unwrap_or(0);
    Some((kind, at, things(&paths(v.get_path("changed")))))
}

/// "Undo: Hype (quick effect) changed today at 2:32 pm".
fn undo_words(kind: &str, at: i64, things: &[String]) -> String {
    match kind {
        "restore" => format!("Undo going back to an earlier version ({})", when_in_sentence(at)),
        "redo" => format!("Undo the redo ({})", when_in_sentence(at)),
        _ if things.is_empty() => format!("Undo the change made {}", when_in_sentence(at)),
        _ => format!("Undo: {} changed {}", summary(things, 2), when_in_sentence(at)),
    }
}

fn undo_tip(app: &App) -> String {
    if !app.m.connected {
        return "Stream Engine isn't running".into();
    }
    match step(app, "project.versions.undo") {
        Some((kind, at, things)) => undo_words(&kind, at, &things),
        None => "Nothing to undo yet".into(),
    }
}

fn redo_tip(app: &App) -> String {
    match step(app, "project.versions.redo") {
        Some((_, _, things)) if !things.is_empty() => format!("Redo: bring back {}", summary(&things, 2)),
        _ => "Bring back what you just undid".into(),
    }
}

/// "Undo last change" (and "Redo" right after an undo), for a page header's action slot
/// (right-to-left layout). Words are only built when someone points at a button.
pub fn undo_button(app: &mut App, ui: &mut Ui) {
    let t = app.t.clone();
    let on = app.m.connected;
    let has = |app: &App, key: &str| app.m.get(key).is_some_and(|v| !matches!(v, Value::Null));
    let b = widgets::button_ex(ui, &t, Some(icon::UNDO), "Undo last change", Kind::Secondary, Size::Small, 0.0, on && has(app, "project.versions.undo"));
    let b = b
        .on_hover_ui(|ui| {
            ui.label(undo_tip(app));
        })
        .on_disabled_hover_ui(|ui| {
            ui.label(undo_tip(app));
        });
    if b.clicked()
        && let Some((kind, _, things)) = step(app, "project.versions.undo")
    {
        app.m.action("project.undo", Value::Null);
        let what = match kind.as_str() {
            "restore" => "went back to how things were before".to_string(),
            _ if things.is_empty() => "the last change".to_string(),
            _ => summary(&things, 2),
        };
        app.m.toast(format!("Undone: {what}"), false);
    }
    if on && has(app, "project.versions.redo") {
        ui.add_space(spacing::XS);
        let r = widgets::button_ex(ui, &t, None, "Redo", Kind::Secondary, Size::Small, 0.0, true).on_hover_ui(|ui| {
            ui.label(redo_tip(app));
        });
        if r.clicked() {
            app.m.action("project.redo", Value::Null);
            app.m.toast("Redone", false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_get_the_words_people_use() {
        assert_eq!(friendly("presets/hype.toml"), "Hype (quick effect)");
        assert_eq!(friendly("scenes/duo.toml"), "Duo (scene)");
        assert_eq!(friendly("patches/chat_box/index.html"), "Chat box (overlay)");
        assert_eq!(friendly("assets/sounds/air_horn.wav"), "Air horn (sound)");
        assert_eq!(friendly("project.toml"), "Project settings");
        // an overlay's (or transition's) several files are one thing
        let t = things(&["transitions/fade.toml".into(), "transitions/fade.wgsl".into(), "scenes/duo.toml".into()]);
        assert_eq!(t, ["Fade (transition)", "Duo (scene)"]);
        assert_eq!(changed_sentence(&t), "Changed 2 things: Fade (transition) and Duo (scene)");
        let many: Vec<String> = ["A", "B", "C", "D"].iter().map(|s| s.to_string()).collect();
        assert_eq!(summary(&many, 2), "A, B and 2 more");
        assert_eq!(summary(&many[..3], 3), "A, B and C");
    }
}
