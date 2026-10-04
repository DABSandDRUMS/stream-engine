//! Shared helpers for the engine's patches: web pages and generative visuals (Sources),
//! custom effects and sound processors (Scenes → Effects), custom transitions. Their settings
//! as real controls, the shipped templates, creating, renaming, removing, turning on/off,
//! opening and reloading their files, problems in plain words, and the "On every scene"
//! placement of overlay-layer patches (`[overlays.<id>]` in project.toml). No page of its own.

use crate::app::App;
use crate::views::composition::source_label;
use crate::views::live::nice;
use anyhow::{Result, anyhow};
use egui::Color32;
use se_proto::{Op, Value};
use se_ui_kit::widgets;
use toml_edit::{Array, DocumentMut, Item, Table, value};

fn action(app: &mut App, name: &str, args: Value) {
    app.m.command(Op::Action { name: name.into(), args });
}

fn text<'a>(p: &'a Value, k: &str) -> &'a str {
    p.get_path(k).and_then(Value::as_str).unwrap_or("")
}

/// Keep the `patches` list fresh (every second) and the templates (every 30 s) while a page
/// that shows them is on screen.
pub fn refresh(app: &mut App, ui: &egui::Ui) {
    let now = ui.input(|i| i.time);
    let poll = egui::Id::new("patches-poll");
    let last: f64 = ui.data_mut(|d| d.get_temp(poll)).unwrap_or(-100.0);
    if now - last > 1.0 && app.m.connected {
        ui.data_mut(|d| d.insert_temp(poll, now));
        app.m.query("patches", Value::Null);
        let asked = egui::Id::new("patch-templates-poll");
        let t_last: f64 = ui.data_mut(|d| d.get_temp(asked)).unwrap_or(-100.0);
        if app.m.q("patch.templates").is_none() || now - t_last > 30.0 {
            ui.data_mut(|d| d.insert_temp(asked, now));
            app.m.query("patch.templates", Value::Null);
        }
    }
}

/// One entry of the `patches` query by id.
pub fn find(app: &App, id: &str) -> Option<Value> {
    app.m.q_list("patches").iter().find(|p| text(p, "id") == id).cloned()
}

/// The name shown for a patch: its label, else its id in words.
pub fn label(p: &Value) -> String {
    let (l, id) = (text(p, "label"), text(p, "id"));
    if l.is_empty() || l == id { nice(id) } else { l.to_string() }
}

/// Plain descriptions for the patches that ship with Stream Engine (their manifests are written
/// for developers). The owner's own patches show their own description.
pub fn plain_description(id: &str) -> Option<&'static str> {
    Some(match id {
        "alertbox" => "Pops up when someone follows, subscribes, cheers, raids or tips.",
        "chatbox" => "Shows your chat on stream. Deleted messages disappear from it too.",
        "goals" => "Progress bars for your sub, bits and follower goals.",
        "labels" => "Your latest follower, subscriber and biggest cheer, as text on screen.",
        "eventlist" => "A running list of recent follows, subs, cheers, raids and tips.",
        "credits" => "Rolls the credits at the end of the stream with everyone who joined in.",
        "countdown" => "A \"starting soon\" countdown that also shows the song playing.",
        "confetti" => "A burst of confetti in your colors.",
        "sparks" => "Sparks that fly on cue.",
        "sub_meteors" => "Meteors rain down when someone subscribes.",
        "hype_detector" => "Watches chat, cheers and the music for big moments and marks them for clips.",
        "hype_meter" => "A meter that fills up as the stream gets more exciting.",
        "aurora" => "A calm, flowing light background that moves with the bass.",
        "ringmod" => "A robot-voice sound effect.",
        _ => return None,
    })
}

/// What a patch does, in one sentence (plain wording for shipped ones).
pub fn description(p: &Value) -> String {
    plain_description(text(p, "id")).map(String::from).unwrap_or_else(|| text(p, "description").to_string())
}

/// Running state in words and whether it needs attention: ("Working" | "Off" | "Has a problem"
/// | "Paused: too slow", problem).
pub fn status(app: &App, p: &Value) -> (&'static str, bool) {
    let id = text(p, "id");
    let st = app.m.get(&format!("patch.{id}.state")).and_then(Value::as_str).unwrap_or(text(p, "state"));
    match st {
        _ if !enabled(app, p) => ("Off", false),
        "error" => ("Has a problem", true),
        "suspended" => ("Paused: too slow", true),
        _ => ("Working", false),
    }
}

/// Turned on (not disabled by the owner).
pub fn enabled(app: &App, p: &Value) -> bool {
    let st = app.m.get(&format!("patch.{}.state", text(p, "id"))).and_then(Value::as_str).unwrap_or(text(p, "state"));
    p.get_path("enabled").is_none_or(Value::truthy) && st != "disabled"
}

/// A patch's problem: the message, where it is (`file:line`, may be empty) and whether the
/// previous version is still showing.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    pub message: String,
    pub location: String,
    pub still_showing: bool,
}

pub fn problem(app: &App, p: &Value) -> Option<Problem> {
    let id = text(p, "id");
    let live = app.m.get(&format!("patch.{id}.error")).and_then(Value::as_str).unwrap_or("");
    let message = if live.is_empty() { text(p, "error") } else { live };
    let st = app.m.get(&format!("patch.{id}.state")).and_then(Value::as_str).unwrap_or(text(p, "state"));
    if message.is_empty() || st != "error" {
        return None;
    }
    let location = match (p.get_path("error_file").and_then(Value::as_str), p.get_path("error_line").and_then(Value::as_i64)) {
        (Some(f), Some(l)) if l > 0 => format!("{f}:{l}"),
        (Some(f), _) => f.to_string(),
        _ => String::new(),
    };
    Some(Problem { message: message.to_string(), location, still_showing: p.get_path("live").is_some_and(Value::truthy) })
}

/// Everything beyond itself a patch's files let it control, in words ("lights", "chat", …).
pub fn grants_words(app: &App, p: &Value) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for g in p.get_path("grants").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(Value::as_str) {
        let w = grant_words(app, g);
        if !out.contains(&w) {
            out.push(w);
        }
    }
    out
}

/// A manifest grant in words: `lights.*` → "lights", `source.cam1` → the camera's name,
/// `patch.confetti` → "“Confetti”".
fn grant_words(app: &App, grant: &str) -> String {
    let mut segs = grant.split('.');
    let root = segs.next().unwrap_or("");
    let named = segs.next().filter(|s| !s.contains('*'));
    match (root, named) {
        ("source" | "sources", Some(n)) => source_label(app, n),
        ("source" | "sources", None) => "cameras and other sources".into(),
        ("patch", Some(n)) => format!("\u{201c}{}\u{201d}", nice(n)),
        ("patch", None) => "other sources and effects".into(),
        ("scene", Some(n)) if !["go", "cut", "take"].contains(&n) => format!("the {} scene", nice(n)),
        ("scene", _) => "scenes".into(),
        ("lights" | "dmx", _) => "lights".into(),
        ("mixer" | "audio", _) => "sound".into(),
        ("preset", _) => "saved actions".into(),
        ("mode", _) => "the show mode".into(),
        ("tts", _) => "the read-out voice".into(),
        ("bot" | "chat", _) => "chat".into(),
        ("queue" | "song" | "songs", _) => "music".into(),
        ("fx", _) => "effects".into(),
        (r, _) => nice(r),
    }
}

// ---- templates and actions ------------------------------------------------------------------

/// A shipped starting point (`patch.templates`).
#[derive(Clone, Debug, PartialEq)]
pub struct Template {
    pub kind: String,
    pub name: String,
    pub description: String,
    /// `source` | `overlay` | `effect` | `transition` | `audio-effect` | `audio-source`.
    pub layer: String,
}

/// Templates of `kind` (`web`, `shader`, `particles`, `script`, `dsp`), in the engine's order.
pub fn templates(app: &App, kind: &str) -> Vec<Template> {
    app.m
        .q_list("patch.templates")
        .iter()
        .filter(|v| text(v, "kind") == kind)
        .map(|v| Template { kind: kind.to_string(), name: text(v, "name").into(), description: text(v, "description").into(), layer: text(v, "layer").into() })
        .collect()
}

/// Make `patches/<id>/` from a template (nothing opens; the patch appears in `patches` once
/// loaded).
pub fn create(app: &mut App, id: &str, kind: &str, template: &str) {
    action(app, "patch.new", Value::map().with("id", id).with("kind", kind).with("template", template));
    app.m.refresh_soon();
}

/// Open the patch's main file in the owner's editor.
pub fn open_files(app: &mut App, id: &str) {
    action(app, "patch.open", Value::map().with("id", id));
}

/// Load the patch's files again.
pub fn reload(app: &mut App, id: &str) {
    action(app, "patch.reload", Value::map().with("id", id));
}

pub fn set_enabled(app: &mut App, id: &str, on: bool) {
    action(app, if on { "patch.enable" } else { "patch.disable" }, Value::map().with("id", id));
}

/// Fire the patch's trigger once.
pub fn play_once(app: &mut App, id: &str) {
    app.m.command(Op::Trigger { address: format!("patch.{id}"), payload: Value::Null });
}

/// Change the name shown for the patch (its manifest `label`).
pub fn rename(app: &mut App, id: &str, label: &str) {
    app.m.action("project.write", Value::map().with("path", format!("patches/{id}/patch.toml")).with("set", Value::map().with("label", label.trim())));
}

/// Remove the patch (its folder is kept aside in `patches/.removed/`).
pub fn remove(app: &mut App, id: &str) {
    action(app, "patch.remove", Value::map().with("id", id));
    app.m.refresh_soon();
}

// ---- "On every scene" (overlay-layer patches) ---------------------------------------------

/// Ids of the patches drawn above every scene (manifest layer `overlay`).
pub fn overlay_ids(app: &App) -> Vec<String> {
    app.m.q_list("patches").iter().filter(|p| text(p, "layer") == "overlay").map(|p| text(p, "id").to_string()).collect()
}

/// `[overlays.<id>]` in project.toml (every key optional).
#[derive(Clone, Debug, Default, PartialEq)]
pub struct OverlaySettings {
    /// None = the default (main, vertical and preview); empty = drawn nowhere by itself.
    pub canvases: Option<Vec<String>>,
    /// Per canvas placement (normalized x, y, w, h); none = fit to the whole canvas.
    pub rect: Vec<(String, [f64; 4])>,
    pub z: i64,
    pub when: Option<String>,
}

fn overlay_table<'a>(doc: &'a DocumentMut, id: &str) -> Option<&'a Item> {
    doc.get("overlays").and_then(|o| o.get(id))
}

pub fn overlay_settings(project_toml: &str, id: &str) -> OverlaySettings {
    let Ok(doc) = project_toml.parse::<DocumentMut>() else { return OverlaySettings::default() };
    let Some(t) = overlay_table(&doc, id) else { return OverlaySettings::default() };
    let num = |v: &toml_edit::Value| v.as_float().or_else(|| v.as_integer().map(|i| i as f64));
    let rect = t
        .get("rect")
        .and_then(Item::as_table_like)
        .map(|r| {
            r.iter()
                .filter_map(|(canvas, v)| {
                    let a = v.as_array()?;
                    let xs: Vec<f64> = a.iter().filter_map(num).collect();
                    (xs.len() == 4).then(|| (canvas.to_string(), [xs[0], xs[1], xs[2], xs[3]]))
                })
                .collect()
        })
        .unwrap_or_default();
    OverlaySettings {
        canvases: t.get("canvases").and_then(Item::as_array).map(|a| a.iter().filter_map(|v| v.as_str().map(String::from)).collect()),
        rect,
        z: t.get("z").and_then(Item::as_integer).unwrap_or(0),
        when: t.get("when").and_then(Item::as_str).map(String::from),
    }
}

/// Drawn above every scene by itself (the default), rather than only where a scene places it.
pub fn on_every_scene(project_toml: &str, id: &str) -> bool {
    overlay_settings(project_toml, id).canvases.is_none_or(|c| !c.is_empty())
}

/// `canvases` of `[overlays.<id>]`: `None` removes the key (the default: every canvas), and the
/// table when nothing else is left in it.
pub fn set_overlay_canvases(project_toml: &str, id: &str, canvases: Option<&[&str]>) -> Result<String> {
    let mut doc: DocumentMut = project_toml.parse()?;
    match canvases {
        Some(list) => {
            let overlays = doc.entry("overlays").or_insert_with(|| {
                let mut t = Table::new();
                t.set_implicit(true);
                Item::Table(t)
            });
            let overlays = overlays.as_table_like_mut().ok_or_else(|| anyhow!("[overlays] isn't a table"))?;
            let entry = overlays.entry(id).or_insert(Item::Table(Table::new()));
            let t = entry.as_table_like_mut().ok_or_else(|| anyhow!("[overlays.{id}] isn't a table"))?;
            t.insert("canvases", value(list.iter().copied().collect::<Array>()));
        }
        None => {
            if let Some(overlays) = doc.get_mut("overlays").and_then(Item::as_table_like_mut) {
                if let Some(t) = overlays.get_mut(id).and_then(Item::as_table_like_mut) {
                    t.remove("canvases");
                    if t.is_empty() {
                        overlays.remove(id);
                    }
                }
                if overlays.is_empty() {
                    doc.remove("overlays");
                }
            }
        }
    }
    Ok(doc.to_string())
}

/// Show above every scene (`on`), or only where a scene places it (`canvases = []`).
pub fn set_on_every_scene(project_toml: &str, id: &str, on: bool) -> Result<String> {
    set_overlay_canvases(project_toml, id, if on { None } else { Some(&[]) })
}

// ---- settings -------------------------------------------------------------------------------

/// A patch's settings as real controls (colors, sliders with ∿ modulation, switches, choices,
/// text), one property row each, saved as they change. `p` is its entry in the `patches` query.
pub fn params_ui(app: &mut App, ui: &mut egui::Ui, p: &Value) {
    for q in p.get_path("params").and_then(Value::as_list).unwrap_or(&[]) {
        param_row(app, ui, q);
    }
}

pub(crate) fn rgba(v: &Value) -> Option<Color32> {
    match v {
        Value::Str(s) => se_ui_kit::theme::hex(s),
        Value::List(l) if l.len() >= 3 => {
            let c = |i: usize| (l.get(i).and_then(Value::as_f64).unwrap_or(1.0).clamp(0.0, 1.0) * 255.0).round() as u8;
            Some(Color32::from_rgba_unmultiplied(c(0), c(1), c(2), if l.len() > 3 { c(3) } else { 255 }))
        }
        _ => None,
    }
}

/// A color in the same form as the parameter's default (`"#rrggbb"` or `[r, g, b, a]`).
pub(crate) fn color_value(c: Color32, like: &Value) -> Value {
    let [r, g, b, a] = c.to_srgba_unmultiplied();
    match like {
        Value::List(l) => {
            let f = |x: u8| Value::Float(((x as f64 / 255.0) * 1000.0).round() / 1000.0);
            let mut v = vec![f(r), f(g), f(b)];
            if l.len() > 3 {
                v.push(f(a));
            }
            Value::List(v)
        }
        _ if a < 255 => Value::Str(format!("#{r:02x}{g:02x}{b:02x}{a:02x}")),
        _ => Value::Str(format!("#{r:02x}{g:02x}{b:02x}")),
    }
}

/// One setting as a property row, saved with `set_base`.
fn param_row(app: &mut App, ui: &mut egui::Ui, q: &Value) {
    let t = app.t.clone();
    let addr = text(q, "address").to_string();
    let ty = text(q, "type").to_string();
    let default = q.get_path("default").cloned().unwrap_or(Value::Null);
    let cur = app.m.get(&addr).cloned().or_else(|| q.get_path("value").cloned()).unwrap_or_else(|| default.clone());
    let label = if text(q, "description").is_empty() { nice(text(q, "name")) } else { text(q, "description").to_string() };
    let mut set = None;
    widgets::prop_row(ui, &t, &label, |ui| match ty.as_str() {
        "color" => {
            let mut c = rgba(&cur).unwrap_or(Color32::WHITE);
            if egui::color_picker::color_edit_button_srgba(ui, &mut c, egui::color_picker::Alpha::OnlyBlend).changed() {
                set = Some(color_value(c, &default));
            }
        }
        "bool" => {
            let mut b = cur.truthy();
            if widgets::toggle(ui, &t, &mut b).changed() {
                set = Some(Value::Bool(b));
            }
        }
        "enum" => {
            let opts: Vec<String> = q.get_path("options").and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|v| v.as_str().map(String::from)).collect();
            let c = cur.as_str().unwrap_or("").to_string();
            if opts.len() <= 4 {
                let labels: Vec<String> = opts.iter().map(|o| nice(o)).collect();
                let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
                let mut i = opts.iter().position(|o| *o == c).unwrap_or(0);
                if widgets::segmented(ui, &t, &mut i, &refs) {
                    set = Some(Value::Str(opts[i].clone()));
                }
            } else {
                egui::ComboBox::from_id_salt(("param", &addr)).selected_text(nice(&c)).show_ui(ui, |ui| {
                    for o in &opts {
                        if ui.selectable_label(*o == c, nice(o)).clicked() {
                            set = Some(Value::Str(o.clone()));
                        }
                    }
                });
            }
        }
        "float" | "int" => {
            let (lo, hi) = match q.get_path("range").and_then(Value::as_list) {
                Some([a, b, ..]) => (a.as_f64().unwrap_or(0.0), b.as_f64().unwrap_or(1.0)),
                _ => (0.0, cur.as_f64().unwrap_or(1.0).abs().max(1.0) * 2.0),
            };
            // while dragging, the slider keeps its own value; it's saved on release
            let key = egui::Id::new(("param-drag", &addr));
            let mut x = ui.data(|d| d.get_temp::<f64>(key)).unwrap_or_else(|| cur.as_f64().unwrap_or(0.0));
            ui.spacing_mut().slider_width = (ui.available_width() - 110.0).max(80.0);
            let slider = egui::Slider::new(&mut x, lo..=hi);
            let slider = if ty == "int" { slider.integer() } else { slider.max_decimals(2) };
            let slider = match q.get_path("unit").and_then(Value::as_str) {
                Some(u) if !u.is_empty() => slider.suffix(format!(" {u}")),
                _ => slider,
            };
            let r = ui.add(slider);
            if r.dragged() {
                ui.data_mut(|d| d.insert_temp(key, x));
            } else {
                ui.data_mut(|d| d.remove::<f64>(key));
            }
            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                set = Some(if ty == "int" { Value::Int(x.round() as i64) } else { Value::Float((x * 1000.0).round() / 1000.0) });
            }
            // ∿ link a signal to it, beside the slider
            crate::views::links::modulate_button(app, ui, &addr, &label, (lo, hi));
        }
        _ => {
            let key = egui::Id::new(("param-text", &addr));
            let mut buf: String = ui.data_mut(|d| d.get_temp(key)).unwrap_or_else(|| cur.as_str().map(String::from).unwrap_or_else(|| cur.to_string()));
            let r = ui.add(widgets::field(&mut buf).desired_width((ui.available_width() - 8.0).max(80.0)));
            if r.lost_focus() && buf != cur.as_str().unwrap_or("") {
                set = Some(Value::Str(buf.clone()));
            }
            if r.has_focus() {
                ui.data_mut(|d| d.insert_temp(key, buf));
            } else {
                ui.data_mut(|d| d.remove::<String>(key));
            }
        }
    });
    if let Some(v) = set {
        app.m.command(Op::SetBase { address: addr, value: v });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROJECT: &str = "# my show\nschema = 1\n\n[overlays.chatbox]\nz = 2\nwhen = \"mode == 'live'\"\nrect.wide = [0.7, 0.1, 0.28, 0.6]\n";

    #[test]
    fn overlay_settings_read_every_key_and_default_to_every_scene() {
        let s = overlay_settings(PROJECT, "chatbox");
        assert_eq!(s.canvases, None);
        assert_eq!(s.z, 2);
        assert_eq!(s.when.as_deref(), Some("mode == 'live'"));
        assert_eq!(s.rect, vec![("wide".to_string(), [0.7, 0.1, 0.28, 0.6])]);
        assert!(on_every_scene(PROJECT, "chatbox"));
        // no table at all: the engine default
        assert_eq!(overlay_settings(PROJECT, "alertbox"), OverlaySettings::default());
        assert!(on_every_scene(PROJECT, "alertbox"));
    }

    #[test]
    fn on_every_scene_round_trips_and_keeps_the_rest_of_the_file() {
        let off = set_on_every_scene(PROJECT, "chatbox", false).unwrap();
        assert!(!on_every_scene(&off, "chatbox"));
        assert_eq!(overlay_settings(&off, "chatbox").canvases, Some(vec![]));
        assert!(off.starts_with("# my show\n"), "comments kept");
        let on = set_on_every_scene(&off, "chatbox", true).unwrap();
        assert!(on_every_scene(&on, "chatbox"));
        assert_eq!(overlay_settings(&on, "chatbox").z, 2, "other keys stay");

        // a patch without a table: off adds `[overlays.<id>]`, on removes it again
        let off = set_on_every_scene("schema = 1\n", "alertbox", false).unwrap();
        assert!(off.contains("[overlays.alertbox]") && !off.contains("[overlays]\n"), "{off}");
        assert!(!on_every_scene(&off, "alertbox"));
        assert_eq!(set_on_every_scene(&off, "alertbox", true).unwrap().trim(), "schema = 1");
    }

    #[test]
    fn overlay_canvases_pick_some_canvases() {
        let some = set_overlay_canvases(PROJECT, "chatbox", Some(&["wide"])).unwrap();
        assert_eq!(overlay_settings(&some, "chatbox").canvases, Some(vec!["wide".to_string()]));
        assert!(on_every_scene(&some, "chatbox"), "drawn by itself (on the main canvas)");
        assert!(set_overlay_canvases("overlays = 3\n", "x", Some(&[])).is_err());
    }
}
