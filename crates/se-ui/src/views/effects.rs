//! Scenes → Effects: the effect library. Every effect the show can use, grouped by what it works
//! on, as a list with the selected one on the right:
//! - Picture effects: the built-ins from [`scene_edit::EFFECTS`] (the one shared catalog), with
//!   their project-wide defaults (`fx.<name>.<param>`, saved with `set_base`; a layer or scene
//!   that writes its own value wins), "Flash it" (the `fx.<name>` trigger) and where they're used;
//! - Custom effects: shader patches with `layer = "effect"` (effect name `patch.<id>`): settings,
//!   flash, where they're used, files;
//! - Sound processors: `dsp` patches with `layer = "audio-effect"`, used on sound channels.
//!
//! Effects are put on layers and scenes in the scene inspector; this page sets them up. "New
//! custom effect" picks a kind, then a starting point, then makes it (`patch.new`).

use crate::app::{App, ViewId};
use crate::views::live::nice;
use crate::views::scene_edit::{self, FxDef};
use crate::views::{composition, links, media, patches, status};
use egui::RichText;
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_mono, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, Size, Tone, icon};
use std::collections::BTreeMap;

/// Width of the list pane.
const LIST_W: f32 = 290.0;
/// While a slider moves, the live preview is sent at most this often (seconds).
const LIVE_EVERY: f64 = 0.033;
/// Room kept for the list's footer line under its scroll area.
const FOOTER_H: f32 = 40.0;
/// Room a setting row keeps right of its slider rail: value box, ∿ and spacing.
const SLIDER_TAIL: f32 = 136.0;
/// Longest slider rail.
const SLIDER_MAX: f32 = 320.0;
/// Built-ins that only work attached to a layer (the engine runs them at the source point):
/// flashing the project-wide one shows nothing.
const NO_FLASH: &[&str] = &["chroma_key"];

/// Kinds of custom effect: (patch kind, manifest layer, name, icon, what it is).
const NEW_KINDS: [(&str, &str, &str, &str, &str); 2] = [
    ("shader", "effect", "Picture effect", icon::PALETTE, "A look for layers and scenes, drawn on the graphics card."),
    ("dsp", "audio-effect", "Sound processor", icon::VOLUME, "Changes the sound on a channel: drive, robot voice and more."),
];

/// What the detail pane shows.
#[derive(Clone, Debug, PartialEq)]
enum Sel {
    Builtin(String),
    Custom(String),
    Sound(String),
    New,
}

/// The "New custom effect" draft (nothing is made until Create).
#[derive(Clone, Default)]
struct Draft {
    kind: usize,
    template: String,
    name: String,
}

#[derive(Clone, Default)]
struct State {
    sel: Option<Sel>,
    /// Keep a just-made effect selected until the engine lists it.
    hold_until: f64,
    draft: Draft,
    /// On air, "Flash it" on this one waits for "Your viewers will see this".
    confirm: Option<Sel>,
    /// Addresses with a live preview held while a slider moves (released when it's saved).
    held: Vec<String>,
    last_live: f64,
    /// A color picked but not saved yet (first address, value), saved when the mouse is let go.
    pending: Option<(String, [f64; 3])>,
    /// Settings whose defaults were asked of the engine (`fetch` with metadata).
    asked: Vec<String>,
    /// Where every effect is used, from `config.scenes` (rebuilt when a new reply arrives).
    uses: Vec<(String, Use)>,
    uses_seq: u64,
    scenes_at: f64,
    mix_at: f64,
}

fn state_id() -> egui::Id {
    egui::Id::new("effects-view")
}

// ---- where effects are used ----------------------------------------------------------------------

/// One place an effect is attached in a scene.
#[derive(Clone, Debug, PartialEq)]
struct Use {
    scene: String,
    /// The layer (node id, what it shows); `None` = the whole scene.
    layer: Option<(String, String)>,
    /// Only on this canvas (`[canvas.<c>] fx`); `None` = both.
    canvas: Option<String>,
}

/// Every effect attachment in `config.scenes`: (effect name, where). A layer on both canvases
/// counts once.
fn scene_uses(scenes: &BTreeMap<String, Value>) -> Vec<(String, Use)> {
    fn names(v: Option<&Value>) -> impl Iterator<Item = String> + '_ {
        v.and_then(Value::as_list).unwrap_or(&[]).iter().filter_map(|f| f.get_path("name").and_then(Value::as_str)).filter(|n| !n.is_empty()).map(String::from)
    }
    let mut out: Vec<(String, Use)> = Vec::new();
    let mut push = |fx: String, u: Use| {
        if !out.iter().any(|(f, x)| *f == fx && *x == u) {
            out.push((fx, u));
        }
    };
    for (scene, s) in scenes {
        for fx in names(s.get_path("fx")) {
            push(fx, Use { scene: scene.clone(), layer: None, canvas: None });
        }
        let Some(canvases) = s.get_path("canvas").and_then(Value::as_map) else { continue };
        for (c, cv) in canvases {
            for fx in names(cv.get_path("fx")) {
                push(fx, Use { scene: scene.clone(), layer: None, canvas: Some(c.clone()) });
            }
            for n in cv.get_path("nodes").and_then(Value::as_list).unwrap_or(&[]) {
                let src = n.get_path("src").and_then(Value::as_str).unwrap_or("");
                let id = n.get_path("id").and_then(Value::as_str).filter(|s| !s.is_empty()).unwrap_or(src);
                for fx in names(n.get_path("fx")) {
                    push(fx, Use { scene: scene.clone(), layer: Some((id.to_string(), src.to_string())), canvas: None });
                }
            }
        }
    }
    out
}

fn used_words(n: usize) -> String {
    match n {
        0 => "Not used yet".into(),
        1 => "Used once".into(),
        n => format!("Used in {n} places"),
    }
}

/// A use as a list row: (scene name, what in it).
fn use_words(app: &App, scenes: Option<&BTreeMap<String, Value>>, u: &Use) -> (String, String) {
    let scene = scenes
        .and_then(|s| s.get(&u.scene))
        .and_then(|s| s.get_path("label"))
        .and_then(Value::as_str)
        .filter(|l| !l.trim().is_empty())
        .map_or_else(|| nice(&u.scene), String::from);
    let what = match (&u.layer, u.canvas.as_deref()) {
        (Some((_, src)), _) => format!("Layer: {}", composition::source_label(app, src)),
        (None, None) => "The whole scene".into(),
        (None, Some("wide")) => "The whole scene, main video".into(),
        (None, Some("tall")) => "The whole scene, vertical video".into(),
        (None, Some(c)) => format!("The whole scene, {}", nice(c)),
    };
    (scene, what)
}

// ---- custom effects -------------------------------------------------------------------------------

fn text<'a>(p: &'a Value, k: &str) -> &'a str {
    p.get_path(k).and_then(Value::as_str).unwrap_or("")
}

/// Custom picture effects and sound processors from the `patches` query, by name.
fn custom_effects(app: &App) -> (Vec<Value>, Vec<Value>) {
    let (mut pictures, mut sounds) = (Vec::new(), Vec::new());
    for p in app.m.q_list("patches") {
        match (text(p, "kind"), text(p, "layer")) {
            (_, "effect") => pictures.push(p.clone()),
            ("dsp", "audio-effect") => sounds.push(p.clone()),
            _ => {}
        }
    }
    for v in [&mut pictures, &mut sounds] {
        v.sort_by_key(|p| patches::label(p).to_lowercase());
    }
    (pictures, sounds)
}

/// The patch's live state (`loaded`, `error`, `suspended`, `disabled`, …).
fn patch_state(app: &App, p: &Value) -> String {
    app.m.get(&format!("patch.{}.state", text(p, "id"))).and_then(Value::as_str).unwrap_or(text(p, "state")).to_string()
}

fn patch_on(app: &App, p: &Value) -> bool {
    p.get_path("enabled").is_none_or(Value::truthy) && patch_state(app, p) != "disabled"
}

// ---- the view ------------------------------------------------------------------------------------

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let now = ui.input(|i| i.time);
    patches::refresh(app, ui);
    let mut st: State = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<State>(state_id())));
    view(app, ui, &mut st, now);
    ui.data_mut(|d| d.insert_temp(state_id(), st));
}

/// One list row.
struct Row {
    sel: Sel,
    icon: &'static str,
    title: String,
    sub: String,
    trailing: &'static str,
}

fn view(app: &mut App, ui: &mut egui::Ui, st: &mut State, now: f64) {
    let t = app.t.clone();
    if app.m.connected && app.m.q_seq("config.scenes") == 0 && now - st.scenes_at > 2.0 {
        // normally kept fresh for the whole app; ask once if nobody has yet
        st.scenes_at = now;
        app.m.query("config.scenes", Value::Null);
    }
    let seq = app.m.q_seq("config.scenes");
    if seq != st.uses_seq {
        st.uses_seq = seq;
        st.uses = app.m.q("config.scenes").and_then(Value::as_map).map(scene_uses).unwrap_or_default();
    }
    let (pictures, sounds) = custom_effects(app);
    let id_of = |p: &Value| text(p, "id").to_string();
    let exists = match &st.sel {
        Some(Sel::Builtin(id)) => scene_edit::effect_def(id).is_some(),
        Some(Sel::Custom(id)) => pictures.iter().any(|p| id_of(p) == *id),
        Some(Sel::Sound(id)) => sounds.iter().any(|p| id_of(p) == *id),
        _ => true,
    };
    if !exists && now > st.hold_until {
        st.sel = None;
    }
    if st.confirm.is_some() && st.confirm != st.sel {
        st.confirm = None;
    }

    let count = |fx: &str| st.uses.iter().filter(|(f, _)| f == fx).count();
    let patch_row = |p: &Value, sound: bool| {
        let id = id_of(p);
        let sub = if patches::problem(app, p).is_some() {
            "Has a problem".to_string()
        } else if !patch_on(app, p) {
            "Off".to_string()
        } else if sound {
            "On".to_string()
        } else {
            used_words(count(&format!("patch.{id}")))
        };
        Row {
            sel: if sound { Sel::Sound(id) } else { Sel::Custom(id) },
            icon: if sound { icon::VOLUME } else { icon::PALETTE },
            title: patches::label(p),
            sub,
            trailing: "",
        }
    };
    let groups: [(&str, Vec<Row>); 3] = [
        (
            "Picture effects",
            scene_edit::EFFECTS
                .iter()
                .map(|d| Row {
                    sel: Sel::Builtin(d.id.to_string()),
                    icon: icon::EFFECT,
                    title: d.label.to_string(),
                    sub: if d.on_layers { used_words(count(d.id)) } else { "The whole picture".into() },
                    trailing: "",
                })
                .collect(),
        ),
        ("Custom effects", pictures.iter().map(|p| patch_row(p, false)).collect()),
        ("Sound processors", sounds.iter().map(|p| patch_row(p, true)).collect()),
    ];
    let total = groups.iter().map(|(_, r)| r.len()).sum();
    let shown = st.sel.clone();
    let (clicked, _) = widgets::split(
        ui,
        LIST_W,
        |ui| list_pane(ui, &t, &groups, shown.as_ref(), total),
        |ui| {
            egui::ScrollArea::vertical().id_salt("effects-detail").auto_shrink([false, false]).show(ui, |ui| {
                detail(app, ui, &t, st, &pictures, &sounds, now);
            });
        },
    );
    if let Some(s) = clicked {
        if s == Sel::New && st.sel != Some(Sel::New) {
            st.draft = Draft::default();
        }
        st.sel = Some(s);
        ui.ctx().request_repaint();
    }
}

fn list_pane(ui: &mut egui::Ui, t: &Theme, groups: &[(&str, Vec<Row>)], sel: Option<&Sel>, total: usize) -> Option<Sel> {
    let mut out = None;
    if widgets::pane_header(ui, t, "Effects", Some(total), Some("New custom effect")) {
        out = Some(Sel::New);
    }
    egui::ScrollArea::vertical().id_salt("effects-list").auto_shrink([false, false]).max_height((ui.available_height() - FOOTER_H).max(80.0)).show(ui, |ui| {
        for (label, rows) in groups.iter().filter(|(_, r)| !r.is_empty()) {
            widgets::group_label(ui, t, label);
            for r in rows {
                if widgets::list_row(ui, t, r.icon, &r.title, &r.sub, r.trailing, sel == Some(&r.sel)).clicked() {
                    out = Some(r.sel.clone());
                }
            }
        }
    });
    ui.add_space(spacing::S);
    widgets::hint(ui, t, "Add effects to layers in the scene inspector.");
    out
}

fn detail(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, pictures: &[Value], sounds: &[Value], now: f64) {
    let find = |list: &[Value], id: &str| list.iter().find(|p| text(p, "id") == id).cloned();
    match st.sel.clone() {
        None => {
            let body =
                "Effects change how a layer or a whole scene looks: blur, color, glitch and more. Pick one to set how it looks everywhere, or make your own.";
            if widgets::empty_state(ui, t, icon::WAND, "Effects", body, Some("New custom effect")) {
                st.sel = Some(Sel::New);
                st.draft = Draft::default();
            }
        }
        Some(Sel::New) => new_ui(app, ui, t, st, now),
        Some(Sel::Builtin(id)) => {
            if let Some(def) = scene_edit::effect_def(&id) {
                builtin_ui(app, ui, t, st, def, now);
            }
        }
        Some(Sel::Custom(id)) => match find(pictures, &id) {
            Some(p) => patch_ui(app, ui, t, st, &p, false, now),
            None => making(ui, t),
        },
        Some(Sel::Sound(id)) => match find(sounds, &id) {
            Some(p) => patch_ui(app, ui, t, st, &p, true, now),
            None => making(ui, t),
        },
    }
}

/// A just-made effect the engine hasn't listed yet.
fn making(ui: &mut egui::Ui, t: &Theme) {
    widgets::empty_state(ui, t, icon::CLOCK, "Making it…", "It shows up here in a moment.", None);
}

// ---- built-in effects ------------------------------------------------------------------------------

fn builtin_ui(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, def: &'static FxDef, now: f64) {
    let id = def.id;
    let addr = |p: &str| format!("fx.{id}.{p}");
    let changed = def.params.iter().any(|p| app.m.get(&addr(p.name)).and_then(Value::as_f64).is_some_and(|v| (v - p.default).abs() > 1e-6));
    let mut reset = false;
    widgets::detail_header(ui, t, icon::EFFECT, def.label, def.about, |ui| {
        if changed {
            reset = widgets::button_ex(ui, t, Some(icon::UNDO), "Reset all", Kind::Ghost, Size::Small, 0.0, true)
                .on_hover_text("Put every default back to how it came")
                .clicked();
        }
    });
    if reset {
        for p in def.params {
            app.m.command(Op::SetBase { address: addr(p.name), value: Value::Float(p.default) });
        }
    }
    if !def.on_layers {
        widgets::callout(
            ui,
            t,
            Tone::Info,
            icon::INFO,
            "Works on the whole picture",
            "It can't go on a single layer. Flash it here, or from a trigger or a saved action.",
            None,
        );
        ui.add_space(spacing::M);
    }

    widgets::inspector_section(
        ui,
        t,
        ("fx-defaults", id),
        "Defaults",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, t, if def.on_layers { "Used wherever a layer or scene doesn't set its own." } else { "How it looks when it plays." });
            ui.add_space(spacing::XS);
            if id == "lut" {
                lut_row(app, ui, t);
            } else if def.params.is_empty() {
                widgets::hint(ui, t, "It has no settings of its own.");
            }
            // `<x>_r`, `<x>_g`, `<x>_b` show as one color
            let colors: Vec<&str> = def
                .params
                .iter()
                .filter_map(|p| p.name.strip_suffix("_r"))
                .filter(|b| ["g", "b"].iter().all(|c| def.params.iter().any(|q| q.name == format!("{b}_{c}"))))
                .collect();
            for p in def.params {
                match p.name.rsplit_once('_') {
                    Some((b, "r")) if colors.contains(&b) => color_row(app, ui, t, st, now, def, b),
                    Some((b, "g" | "b")) if colors.contains(&b) => {}
                    _ => number_row(app, ui, t, st, now, &addr(p.name), p.label, Some(p.default), (p.min, p.max), p.unit),
                }
            }
        },
    );

    if !NO_FLASH.contains(&id) {
        let level = addr("level");
        // `level` (the flash strength) isn't in the catalog: its default comes from the engine
        let level_default = app.m.meta.get(&level).and_then(|m| m.default.as_f64());
        if level_default.is_none() && app.m.connected && !st.asked.contains(&level) {
            st.asked.push(level.clone());
            app.m.fetch(&level);
        }
        let sel = Sel::Builtin(id.to_string());
        widgets::inspector_section(
            ui,
            t,
            ("fx-flash", id),
            "Flash it",
            true,
            |_| {},
            |ui| {
                widgets::hint(ui, t, "Plays it once over the whole picture, the way a trigger or a saved action does.");
                ui.add_space(spacing::XS);
                number_row(app, ui, t, st, now, &level, "Strength", level_default, (0.0, 1.0), "");
                fire_row(app, ui, t, st, &sel, &format!("fx.{id}"), "Flash it", true, false);
            },
        );
    }

    if def.on_layers {
        uses_section(app, ui, t, st, id);
    }
}

/// The LUT's `.cube` file (the one built-in setting that isn't a number).
fn lut_row(app: &mut App, ui: &mut egui::Ui, t: &Theme) {
    const FILE: &str = "fx.lut.file";
    let mut cur = app.m.str(FILE).to_string();
    widgets::prop_row(ui, t, "Look file", |ui| {
        if media::picker(app, ui, "fx-lut-file", media::MediaKind::Lut, &mut cur) {
            app.m.command(Op::SetBase { address: FILE.into(), value: Value::Str(cur.clone()) });
        }
    });
}

fn suffix(unit: &str) -> String {
    match unit {
        "" | "°" => unit.to_string(),
        u => format!(" {u}"),
    }
}

/// Three 0–1 settings `<base>_r`, `<base>_g`, `<base>_b` as one color: live while the picker
/// moves, saved when the mouse is let go.
fn color_row(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, now: f64, def: &FxDef, base: &str) {
    let addrs = ["r", "g", "b"].map(|c| format!("fx.{}.{base}_{c}", def.id));
    let defaults = ["r", "g", "b"].map(|c| def.params.iter().find(|p| p.name == format!("{base}_{c}")).map_or(0.0, |p| p.default));
    let cur: [f64; 3] = std::array::from_fn(|i| app.m.get(&addrs[i]).and_then(Value::as_f64).unwrap_or(defaults[i]));
    let off = (0..3).any(|i| (cur[i] - defaults[i]).abs() > 1e-3);
    let label = if base == "color" { "Color".to_string() } else { format!("{} color", nice(base)) };
    let key = addrs[0].clone();
    let shown = match &st.pending {
        Some((k, v)) if *k == key => *v,
        _ => cur,
    };
    let mut live = None;
    let mut reset = false;
    widgets::prop_row(ui, t, &label, |ui| {
        let mut c = shown.map(|x| x as f32);
        if egui::color_picker::color_edit_button_rgb(ui, &mut c).changed() {
            live = Some(c.map(|x| (f64::from(x) * 1000.0).round() / 1000.0));
        }
        reset = off && widgets::icon_button(ui, t, icon::UNDO, "Back to how it came").clicked();
    });
    if let Some(v) = live {
        st.pending = Some((key.clone(), v));
        if now - st.last_live >= LIVE_EVERY {
            st.last_live = now;
            for (a, x) in addrs.iter().zip(v) {
                if !st.held.contains(a) {
                    st.held.push(a.clone());
                }
                app.m.command(Op::Set { address: a.clone(), value: Value::Float(x) });
            }
        }
    } else if let Some((k, v)) = st.pending.clone()
        && k == key
        && !ui.input(|i| i.pointer.any_down())
    {
        st.pending = None;
        for (a, x) in addrs.iter().zip(v) {
            save(app, st, a, x);
        }
    }
    if reset {
        for (a, x) in addrs.iter().zip(defaults) {
            save(app, st, a, x);
        }
    }
}

/// A numeric setting: slider (live preview while it moves, saved with `set_base` when let go),
/// ∿ modulation, and "back to how it came" when it differs from `default`.
#[allow(clippy::too_many_arguments)]
fn number_row(
    app: &mut App,
    ui: &mut egui::Ui,
    t: &Theme,
    st: &mut State,
    now: f64,
    addr: &str,
    label: &str,
    default: Option<f64>,
    range: (f64, f64),
    unit: &str,
) {
    let cur = app.m.get(addr).and_then(Value::as_f64).or(default).unwrap_or(range.0);
    let key = egui::Id::new(("fx-drag", addr));
    let off_default = default.filter(|d| (d - cur).abs() > 1e-6);
    widgets::prop_row(ui, t, label, |ui| {
        let mut x: f64 = ui.data(|d| d.get_temp(key)).unwrap_or(cur);
        // the rail leaves room for the value box, ∿ and ↺; a long rail reads worse, so it's capped
        let tail = SLIDER_TAIL + if off_default.is_some() { 32.0 } else { 0.0 };
        ui.spacing_mut().slider_width = (ui.available_width() - tail).clamp(60.0, SLIDER_MAX);
        let r = ui.add(egui::Slider::new(&mut x, range.0..=range.1).max_decimals(2).suffix(suffix(unit)));
        let x = (x * 1000.0).round() / 1000.0;
        if r.dragged() {
            ui.data_mut(|d| d.insert_temp(key, x));
            if r.changed() && now - st.last_live >= LIVE_EVERY {
                st.last_live = now;
                if !st.held.iter().any(|a| a == addr) {
                    st.held.push(addr.to_string());
                }
                app.m.command(Op::Set { address: addr.to_string(), value: Value::Float(x) });
            }
        } else {
            ui.data_mut(|d| d.remove::<f64>(key));
        }
        if r.drag_stopped() || (r.changed() && !r.dragged()) {
            save(app, st, addr, x);
        }
        links::modulate_button(app, ui, addr, label, range);
        if let Some(d) = off_default
            && widgets::icon_button(ui, t, icon::UNDO, "Back to how it came").clicked()
        {
            save(app, st, addr, d);
        }
    });
}

/// Save a default and drop the live preview held while it moved.
fn save(app: &mut App, st: &mut State, addr: &str, v: f64) {
    app.m.command(Op::SetBase { address: addr.to_string(), value: Value::Float(v) });
    if let Some(i) = st.held.iter().position(|a| a == addr) {
        st.held.remove(i);
        app.m.command(Op::Release { address: addr.to_string() });
    }
}

/// "Flash it" / "Try it": fires `address` once; on air it asks first.
#[allow(clippy::too_many_arguments)]
fn fire_row(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, sel: &Sel, address: &str, label: &str, enabled: bool, sound: bool) {
    let on_air = status::on_air(app);
    let fire = |app: &mut App| app.m.command(Op::Trigger { address: address.to_string(), payload: Value::Null });
    ui.add_space(spacing::XS);
    let (text, kind, tip) = match (on_air, sound) {
        (true, false) => (format!("{label} on air"), Kind::Live, "You're on air: your viewers will see it."),
        (true, true) => (format!("{label} on air"), Kind::Live, "You're on air: your viewers will hear it."),
        (false, false) => (label.to_string(), Kind::Secondary, "Plays it once now. You're off air: only you see it."),
        (false, true) => (label.to_string(), Kind::Secondary, "Plays it once now. You're off air: only you hear it."),
    };
    let off = "It's off. Turn it on first.";
    let r = widgets::button_ex(ui, t, Some(icon::PLAY), &text, kind, Size::Small, 0.0, enabled);
    if r.on_hover_text(tip).on_disabled_hover_text(off).clicked() {
        if on_air {
            st.confirm = if st.confirm.as_ref() == Some(sel) { None } else { Some(sel.clone()) };
        } else {
            fire(app);
        }
    }
    if on_air && st.confirm.as_ref() == Some(sel) {
        ui.add_space(spacing::S);
        let title = if sound { "Your viewers will hear this" } else { "Your viewers will see this" };
        if widgets::callout(ui, t, Tone::Danger, icon::LIVE, title, "You're on air, so it plays on stream right now.", Some(text.as_str())) {
            fire(app);
            st.confirm = None;
        } else if widgets::button_ex(ui, t, None, "Cancel", Kind::Ghost, Size::Small, 0.0, true).clicked() {
            st.confirm = None;
        }
    }
}

/// "Where it's used": every scene and layer that has the effect `fx`; a row opens it in Scenes.
fn uses_section(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &State, fx: &str) {
    let uses: Vec<&Use> = st.uses.iter().filter(|(f, _)| f == fx).map(|(_, u)| u).collect();
    let title = if uses.is_empty() { "Where it's used".to_string() } else { format!("Where it's used ({})", uses.len()) };
    let mut open = None;
    widgets::inspector_section(
        ui,
        t,
        ("fx-uses", fx),
        &title,
        true,
        |_| {},
        |ui| {
            if uses.is_empty() {
                widgets::hint(ui, t, "Not on any layer or scene yet. In Scenes, pick a layer and add it under Effects.");
                return;
            }
            let scenes = app.m.q("config.scenes").and_then(Value::as_map);
            for u in &uses {
                let (scene, what) = use_words(app, scenes, u);
                let ic = if u.layer.is_some() { icon::LAYERS } else { icon::SCENE };
                if widgets::list_row(ui, t, ic, &scene, &what, icon::RIGHT, false).on_hover_text("Open it in Scenes").clicked() {
                    open = Some((*u).clone());
                }
            }
        },
    );
    if let Some(u) = open {
        composition::open(app, &u.scene, u.layer.as_ref().map(|(id, _)| id.as_str()));
    }
}

// ---- custom effects and sound processors -----------------------------------------------------------

fn patch_ui(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, p: &Value, sound: bool, now: f64) {
    let id = text(p, "id").to_string();
    let state = patch_state(app, p);
    let enabled = patch_on(app, p);
    let about = patches::plain_description(&id).map(String::from).unwrap_or_else(|| text(p, "description").to_string());
    let kind = if sound { "Sound processor" } else { "Custom picture effect" };
    let sub = if about.trim().is_empty() { kind.to_string() } else { about };
    let mut on = enabled || state == "suspended";
    let mut flip = None;
    widgets::detail_header(ui, t, if sound { icon::VOLUME } else { icon::PALETTE }, &patches::label(p), &sub, |ui| {
        let tip = if on { "On. Turn it off everywhere." } else { "Off everywhere. Turn it on." };
        if widgets::toggle(ui, t, &mut on).on_hover_text(tip).changed() {
            flip = Some(on);
        }
    });
    if let Some(on) = flip {
        patches::set_enabled(app, &id, on);
    }
    if let Some(pr) = patches::problem(app, p) {
        let title = if pr.still_showing { "Its last change has a mistake" } else { "It has a mistake and can't run" };
        let body = if pr.location.is_empty() { pr.message.clone() } else { format!("{} ({})", pr.message, pr.location) };
        if widgets::callout(ui, t, Tone::Danger, icon::WARN, title, &body, Some("Open files")) {
            patches::open_files(app, &id);
        }
        ui.add_space(spacing::M);
    } else if state == "suspended" {
        if widgets::callout(ui, t, Tone::Warn, icon::PAUSE, "Paused: too slow", "It took too long, so it was paused to keep the show smooth.", Some("Resume")) {
            patches::set_enabled(app, &id, true);
        }
        ui.add_space(spacing::M);
    }

    let has_params = p.get_path("params").and_then(Value::as_list).is_some_and(|l| !l.is_empty());
    widgets::inspector_section(
        ui,
        t,
        ("fx-settings", &id),
        if sound { "Settings" } else { "Defaults" },
        true,
        |_| {},
        |ui| {
            if !sound {
                widgets::hint(ui, t, "Used wherever a layer or scene doesn't set its own.");
                ui.add_space(spacing::XS);
            }
            if has_params {
                patches::params_ui(app, ui, p);
            } else {
                widgets::hint(ui, t, "It has no settings of its own.");
            }
        },
    );

    let fx = format!("patch.{id}");
    if p.get_path("trigger").is_some_and(Value::truthy) {
        let label = if sound { "Try it" } else { "Flash it" };
        let unused = !sound && !st.uses.iter().any(|(f, _)| *f == fx);
        let sel = if sound { Sel::Sound(id.clone()) } else { Sel::Custom(id.clone()) };
        widgets::inspector_section(
            ui,
            t,
            ("fx-flash", &id),
            label,
            true,
            |_| {},
            |ui| {
                widgets::hint(
                    ui,
                    t,
                    match (sound, unused) {
                        (true, _) => "Plays it once on the channels it's on.",
                        (false, false) => "Plays it once over the whole picture, the way a trigger or a saved action does.",
                        (false, true) => "Plays it once over the whole picture. Put it on a layer or scene first: until then there's nothing to flash.",
                    },
                );
                fire_row(app, ui, t, st, &sel, &fx, label, enabled, sound);
            },
        );
    }

    if sound {
        sound_uses(app, ui, t, st, &fx, now);
    } else {
        uses_section(app, ui, t, st, &fx);
    }

    let used = st.uses.iter().any(|(f, _)| *f == fx);
    widgets::inspector_section(
        ui,
        t,
        ("fx-files", &id),
        "Files",
        false,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Folder", |ui| {
                ui.label(RichText::new(format!("patches/{id}/")).font(font_mono(type_scale::SMALL + 0.5)).color(t.fg));
            });
            ui.add_space(spacing::XS);
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, t, Some(icon::EDIT), "Edit files", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    patches::open_files(app, &id);
                }
                if widgets::button_ex(ui, t, Some(icon::UNDO), "Reload", Kind::Ghost, Size::Small, 0.0, true).on_hover_text("Read its files again").clicked() {
                    patches::reload(app, &id);
                }
            });
            ui.add_space(spacing::M);
            if used {
                widgets::hint(ui, t, "To remove it, take it off every layer and scene first.");
            } else if widgets::hold_button(ui, t, "Hold to remove", t.bright_red, 1.0) {
                patches::remove(app, &id);
                st.sel = None;
            }
        },
    );
}

/// "Where it's used" for a sound processor: the sound channels and inputs that run it.
fn sound_uses(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, kind: &str, now: f64) {
    if app.m.connected && now - st.mix_at > 3.0 {
        st.mix_at = now;
        app.m.query("audio.mix", Value::Null);
    }
    let runs = |v: &Value| v.get_path("fx").and_then(Value::as_list).unwrap_or(&[]).iter().any(|f| text(f, "kind") == kind);
    let mut places: Vec<(String, String)> = Vec::new();
    if let Some(mix) = app.m.q("audio.mix") {
        for b in mix.get_path("buses").and_then(Value::as_list).unwrap_or(&[]).iter().filter(|b| runs(b)) {
            places.push((format!("{} channel", nice(text(b, "name"))), String::new()));
        }
        for i in mix.get_path("inputs").and_then(Value::as_list).unwrap_or(&[]).iter().filter(|i| runs(i)) {
            let bus = text(i, "bus");
            places.push((nice(text(i, "name")), if bus.is_empty() { "Input".into() } else { format!("Input on the {} channel", nice(bus)) }));
        }
    }
    let mut open = false;
    widgets::inspector_section(
        ui,
        t,
        ("fx-uses", kind),
        "Where it's used",
        true,
        |_| {},
        |ui| {
            if places.is_empty() {
                widgets::hint(ui, t, "Not on a sound channel yet. Sound processors are added to a channel in your project's sound setup.");
            }
            for (title, sub) in &places {
                open |= widgets::list_row(ui, t, icon::VOLUME, title, sub, icon::RIGHT, false).on_hover_text("Open it in Sound").clicked();
            }
        },
    );
    if open {
        app.open_view(ViewId::Audio);
    }
}

// ---- new custom effect ---------------------------------------------------------------------------

/// A starting point's name (`default` is the plain starter).
fn template_title(name: &str) -> String {
    if name == "default" { "Starter".into() } else { nice(name) }
}

fn new_ui(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut State, now: f64) {
    widgets::detail_header(ui, t, icon::PLUS, "New custom effect", "Starts from a working example you can change in its files.", |_| {});
    widgets::inspector_section(
        ui,
        t,
        "fx-new-kind",
        "What kind",
        true,
        |_| {},
        |ui| {
            ui.horizontal_wrapped(|ui| {
                for (i, (_, _, name, ic, body)) in NEW_KINDS.iter().enumerate() {
                    if widgets::kind_tile(ui, t, ic, name, body, st.draft.kind == i).clicked() && st.draft.kind != i {
                        st.draft.kind = i;
                        st.draft.template.clear();
                    }
                }
            });
        },
    );
    let (kind, layer, ..) = NEW_KINDS[st.draft.kind];
    let templates: Vec<patches::Template> = patches::templates(app, kind).into_iter().filter(|x| x.layer == layer).collect();
    if !templates.iter().any(|x| x.name == st.draft.template) {
        st.draft.template = templates.first().map(|x| x.name.clone()).unwrap_or_default();
    }
    if templates.len() > 1 {
        widgets::inspector_section(
            ui,
            t,
            ("fx-new-template", kind),
            "Start from",
            true,
            |_| {},
            |ui| {
                ui.horizontal_wrapped(|ui| {
                    for x in &templates {
                        let body = patches::plain_description(&x.name).unwrap_or(x.description.as_str());
                        if widgets::kind_tile(ui, t, NEW_KINDS[st.draft.kind].3, &template_title(&x.name), body, st.draft.template == x.name).clicked() {
                            st.draft.template = x.name.clone();
                        }
                    }
                });
            },
        );
    }
    widgets::inspector_section(
        ui,
        t,
        "fx-new-name",
        "Name",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Name", |ui| {
                ui.add(widgets::field(&mut st.draft.name).hint_text(if kind == "dsp" { "e.g. Robot voice" } else { "e.g. Neon edges" }).desired_width(260.0));
            });
        },
    );
    let id = scene_edit::slug(&st.draft.name);
    let taken = !id.is_empty() && app.m.q_list("patches").iter().any(|p| text(p, "id") == id);
    let ready = !id.is_empty() && !taken && !st.draft.template.is_empty();
    ui.add_space(spacing::S);
    ui.horizontal(|ui| {
        if widgets::button_ex(ui, t, Some(icon::CHECK), "Create", Kind::Primary, Size::Medium, 0.0, ready).clicked() {
            patches::create(app, &id, kind, &st.draft.template);
            patches::open_files(app, &id);
            app.m.toast(format!("Made \"{}\". Its files open in your editor.", st.draft.name.trim()), false);
            st.sel = Some(if kind == "dsp" { Sel::Sound(id.clone()) } else { Sel::Custom(id.clone()) });
            st.hold_until = now + 8.0;
            st.draft = Draft::default();
        }
        if widgets::button_ex(ui, t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
            st.sel = None;
        }
        if taken {
            widgets::hint(ui, t, "That name is taken.");
        } else if st.draft.template.is_empty() {
            widgets::hint(ui, t, "Loading the starting points…");
        }
    });
    ui.add_space(spacing::S);
    widgets::hint(
        ui,
        t,
        if kind == "dsp" {
            "Its files open in your editor. Add it to a sound channel in your project's sound setup."
        } else {
            "Its files open in your editor. Then add it to a layer in Scenes."
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fx(names: &[&str]) -> Value {
        Value::List(names.iter().map(|n| Value::map().with("name", *n)).collect())
    }

    #[test]
    fn uses_cover_layers_scenes_and_canvases_once() {
        let node = |id: &str, src: &str, f: &[&str]| Value::map().with("id", id).with("src", src).with("fx", fx(f));
        let duo = Value::map().with("label", "Duo").with("fx", fx(&["grade"])).with(
            "canvas",
            Value::map()
                .with(
                    "wide",
                    Value::map().with("nodes", Value::List(vec![node("cam_kit", "cam_kit", &["blur", "patch.neon"]), node("", "cam_room", &["blur"])])),
                )
                .with("tall", Value::map().with("nodes", Value::List(vec![node("cam_kit", "cam_kit", &["blur"])])).with("fx", fx(&["vhs"]))),
        );
        let scenes: BTreeMap<String, Value> = [("duo".to_string(), duo)].into();
        let u = scene_uses(&scenes);
        let of = |f: &str| u.iter().filter(|(n, _)| n == f).map(|(_, x)| x.clone()).collect::<Vec<_>>();
        assert_eq!(of("grade"), [Use { scene: "duo".into(), layer: None, canvas: None }]);
        assert_eq!(of("vhs"), [Use { scene: "duo".into(), layer: None, canvas: Some("tall".into()) }]);
        // the kit layer is on both canvases: one use; a layer without an id is known by its source
        assert_eq!(
            of("blur"),
            [
                Use { scene: "duo".into(), layer: Some(("cam_kit".into(), "cam_kit".into())), canvas: None },
                Use { scene: "duo".into(), layer: Some(("cam_room".into(), "cam_room".into())), canvas: None },
            ]
        );
        assert_eq!(of("patch.neon").len(), 1);
        assert!(of("glitch").is_empty());
    }
}
