//! Lights (§9, §15.6), built around what a streamer does with lights:
//! - **Looks**: tap a look to put it on your lights, run cue lists (Go / Back / Stop, level),
//!   start and stop effects. Brightness, blackout and the safe look sit above every sub-view.
//! - **Edit a look**: choose lights, adjust brightness / color / position, save the look (cues
//!   that use it change too), or save into a cue list step or a quick effect.
//! - **Stage**: the live visualizer; click lights to choose them.
//! - **Setup**: the technical side — lights and groups, the connection, finding lights on the
//!   cable, and the output monitor.
//!
//! "Look" = an se-dmx palette. Pure client of the se-dmx queries/actions/state addresses.

use crate::app::App;
use crate::model::Model;
use crate::views::live::nice;
use egui::ecolor::Hsva;
use egui::text::{LayoutJob, TextWrapping};
use egui::{Align, Align2, Color32, CornerRadius, Galley, Layout, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, Ui, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font, font_bold, font_medium, font_mono, font_semibold, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, LedState, Size, icon};
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Structural queries, refreshed about once a second.
const SLOW_QUERIES: [&str; 6] = ["lights.rig", "lights.cuelists", "lights.palettes", "lights.effects", "lights.programmer", "lights.rdm"];
const SLOW_EVERY: Duration = Duration::from_secs(1);
/// Live output (visualizer, output monitor, frame rate), refreshed while the view is visible.
const FAST_EVERY: Duration = Duration::from_millis(70);
/// After an action, re-query structure this soon (the engine applies actions asynchronously).
const AFTER_ACTION: Duration = Duration::from_millis(150);
/// Locally edited values win over the (lagging) engine echo for this long.
const EDIT_HOLD: Duration = Duration::from_millis(1500);
/// Pan travel assumed by the visualizer (typical mover: 540°); pan 0.5 = centre.
const PAN_TRAVEL_DEG: f32 = 540.0;
const CROSSHAIRS: &str = "\u{f05b}";
/// Attributes shown as 0..1 sliders under "More" (intensity/color/pan/tilt have their own controls).
const UNIT_ATTRS: [&str; 12] = ["white", "amber", "uv", "lime", "cto", "zoom", "focus", "iris", "frost", "prism", "gobo_rotate", "strobe"];
/// What a new look keeps: engine palette kinds and their plain labels (same order).
const LOOK_KINDS: [&str; 5] = ["color", "intensity", "position", "beam", "any"];
const LOOK_KIND_LABELS: [&str; 5] = ["Colors", "Brightness", "Position", "Beam", "Everything"];
/// One-tap light colors in the editor. These are what the lights emit (not interface colors),
/// so they are fixed rather than taken from the theme.
const LIGHT_COLORS: [(&str, Color32); 9] = [
    ("White", Color32::from_rgb(255, 255, 255)),
    ("Warm white", Color32::from_rgb(255, 214, 170)),
    ("Red", Color32::from_rgb(255, 0, 0)),
    ("Orange", Color32::from_rgb(255, 128, 0)),
    ("Yellow", Color32::from_rgb(255, 230, 0)),
    ("Green", Color32::from_rgb(0, 255, 0)),
    ("Cyan", Color32::from_rgb(0, 255, 255)),
    ("Blue", Color32::from_rgb(0, 0, 255)),
    ("Magenta", Color32::from_rgb(255, 0, 255)),
];
/// Tiles in the look and effect grids: at least this wide, grown to fill the row (up to the max),
/// taller as they get wider.
const TILE_MIN_W: f32 = 132.0;
const TILE_MAX_W: f32 = 300.0;
/// Height of the name + kind lines under a tile's face.
const TILE_TEXT_H: f32 = 50.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Tab {
    #[default]
    Looks,
    Edit,
    Stage,
    Setup,
}

impl Tab {
    const ALL: [Tab; 4] = [Tab::Looks, Tab::Edit, Tab::Stage, Tab::Setup];
    const LABELS: [&'static str; 4] = ["Looks", "Edit a look", "Stage", "Setup"];
}

/// Per-view UI state, kept in egui memory.
#[derive(Clone, Default)]
struct LightsState {
    tab: Tab,
    slow_at: Option<Instant>,
    fast_at: Option<Instant>,
    /// Output line shown by the output monitor.
    universe: Option<u16>,
    /// Look (palette name) open in the editor; the first look when unset.
    look: Option<String>,
    /// The editor is making a new look instead of changing `look`.
    new_look: bool,
    new_name: String,
    /// Index into `LOOK_KINDS` for a new look.
    new_kind: usize,
    preset_name: String,
    store_cuelist: String,
    store_cue: String,
    /// Recently edited values (`@<address>` for state addresses, `<attr>` for programmer attrs).
    edits: HashMap<String, (Value, Instant)>,
    /// Hue/saturation survive low brightness while the colour picker is being dragged.
    hsva: Option<(Hsva, Instant)>,
}

impl LightsState {
    fn refresh(&mut self, m: &mut Model, now: Instant) {
        if !m.connected {
            return;
        }
        if self.slow_at.is_none_or(|t| now.saturating_duration_since(t) >= SLOW_EVERY) {
            self.slow_at = Some(now);
            for q in SLOW_QUERIES {
                m.query(q, Value::Null);
            }
        }
        if self.fast_at.is_none_or(|t| now.saturating_duration_since(t) >= FAST_EVERY) {
            self.fast_at = Some(now);
            m.query("lights.output", Value::Null);
        }
    }

    /// Schedule the structural refresh `AFTER_ACTION` from now.
    fn refresh_soon(&mut self, now: Instant) {
        self.slow_at = now.checked_sub(SLOW_EVERY.saturating_sub(AFTER_ACTION));
    }

    fn edit(&mut self, key: &str, v: Value, now: Instant) {
        self.edits.insert(key.to_string(), (v, now));
    }

    fn edited(&self, key: &str, now: Instant) -> Option<&Value> {
        self.edits.get(key).filter(|(_, at)| now.saturating_duration_since(*at) < EDIT_HOLD).map(|(v, _)| v)
    }
}

/// Everything a sub-view reads this frame.
struct Cx<'a> {
    m: &'a Model,
    t: &'a Theme,
    rig: &'a Rig,
    output: Option<&'a Value>,
    /// Programmer selection targets (fixtures, groups, `all`).
    selection: &'a [String],
    /// Heads covered by the selection.
    heads: &'a [String],
    now: Instant,
}

/// Full-area Lights view.
pub fn ui(app: &mut App, ui: &mut Ui) {
    let id = ui.make_persistent_id("lights_view");
    let now = Instant::now();
    let mut st = ui.data_mut(|d| std::mem::take(d.get_temp_mut_or_default::<LightsState>(id)));
    st.refresh(&mut app.m, now);
    let t = app.t.clone();
    let mut out = Vec::new();
    view(&app.m, &t, &mut st, ui, &mut out, now);
    if !out.is_empty() {
        st.refresh_soon(now);
    }
    ui.data_mut(|d| d.insert_temp(id, st));
    for op in out {
        app.m.command(op);
    }
}

fn view(m: &Model, t: &Theme, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, now: Instant) {
    st.edits.retain(|_, (_, at)| now.saturating_duration_since(*at) < EDIT_HOLD);
    let rig = Rig::parse(m.q("lights.rig"));
    let output = m.q("lights.output");
    top_bar(m, t, st, ui, out, output, now);
    ui.add_space(spacing::L);
    if !rig.present {
        widgets::panel(ui, t, |ui| {
            ui.set_width(ui.available_width());
            widgets::empty_state(
                ui,
                t,
                icon::LIGHT,
                "Waiting for your lights",
                "Your looks, cue lists and lights show up here once Stream Engine is running.",
                None,
            );
        });
        return;
    }
    let selection = selection(m);
    let heads = resolve_heads(&rig, &selection);
    let cx = Cx { m, t, rig: &rig, output, selection: &selection, heads: &heads, now };
    match st.tab {
        Tab::Looks => {
            egui::ScrollArea::vertical().id_salt("lights_looks").auto_shrink([false, false]).show(ui, |ui| looks_tab(&cx, st, ui, out));
        }
        Tab::Edit => {
            egui::ScrollArea::vertical().id_salt("lights_edit").auto_shrink([false, false]).show(ui, |ui| edit_tab(&cx, st, ui, out));
        }
        Tab::Stage => stage_tab(&cx, st, ui, out),
        Tab::Setup => {
            egui::ScrollArea::vertical().id_salt("lights_setup").auto_shrink([false, false]).show(ui, |ui| setup_tab(&cx, st, ui, out));
        }
    }
}

// ---------------------------------------------------------------- data

fn act(name: &str, args: Value) -> Op {
    Op::Action { name: name.into(), args }
}

fn select_op(target: &str, add: bool) -> Op {
    act("lights.programmer.select", Value::map().with("targets", vec![target.to_string()]).with("add", add))
}

fn prog_set(out: &mut Vec<Op>, attr: &str, value: Value) {
    out.push(act("lights.programmer.set", Value::map().with("attr", attr).with("value", value)));
}

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

fn num(v: &Value, k: &str) -> Option<f64> {
    v.get_path(k).and_then(Value::as_f64)
}

fn list<'a>(v: &'a Value, k: &str) -> &'a [Value] {
    v.get_path(k).and_then(Value::as_list).unwrap_or(&[])
}

fn strs(v: &Value, k: &str) -> Vec<String> {
    list(v, k).iter().map(text).collect()
}

/// Display form of a scalar-ish value (strings unquoted).
fn text(v: &Value) -> String {
    match v {
        Value::Str(s) => s.clone(),
        other => other.to_string(),
    }
}

fn vec2_of(v: &Value, k: &str) -> [f32; 2] {
    v.get_path(k).and_then(Value::as_vec2).unwrap_or([0.5, 0.5])
}

#[derive(Clone, Debug, Default)]
struct Fixture {
    id: String,
    label: String,
    profile: String,
    profile_name: String,
    mode: String,
    universe: u16,
    address: u16,
    footprint: u16,
    position: [f32; 2],
    heads: Vec<String>,
}

impl Fixture {
    fn name(&self) -> String {
        if self.label.is_empty() { nice(&self.id) } else { self.label.clone() }
    }
}

#[derive(Clone, Debug, Default)]
struct HeadInfo {
    id: String,
    fixture: String,
    position: [f32; 2],
    rotation: f32,
    kind: String,
    beam: f32,
    attrs: Vec<String>,
    moving: bool,
}

#[derive(Clone, Debug, Default)]
struct Rig {
    fixtures: Vec<Fixture>,
    heads: Vec<HeadInfo>,
    groups: BTreeMap<String, Vec<String>>,
    /// profile id → mode id → channel role/name per offset.
    channels: HashMap<String, HashMap<String, Vec<String>>>,
    outputs: Vec<Value>,
    errors: Vec<String>,
    present: bool,
}

impl Rig {
    fn parse(v: Option<&Value>) -> Rig {
        let Some(v) = v.filter(|v| v.as_map().is_some()) else {
            return Rig::default();
        };
        let fixtures = list(v, "fixtures")
            .iter()
            .map(|f| Fixture {
                id: s(f, "id").to_string(),
                label: s(f, "label").to_string(),
                profile: s(f, "profile").to_string(),
                profile_name: s(f, "profile_name").to_string(),
                mode: s(f, "mode").to_string(),
                universe: num(f, "universe").unwrap_or(1.0).clamp(0.0, u16::MAX as f64) as u16,
                address: num(f, "address").unwrap_or(0.0).clamp(0.0, 512.0) as u16,
                footprint: num(f, "footprint").unwrap_or(0.0).clamp(0.0, 512.0) as u16,
                position: vec2_of(f, "position"),
                heads: strs(f, "heads"),
            })
            .collect();
        let heads = list(v, "heads")
            .iter()
            .map(|h| HeadInfo {
                id: s(h, "id").to_string(),
                fixture: s(h, "fixture").to_string(),
                position: vec2_of(h, "position"),
                rotation: num(h, "rotation").unwrap_or(0.0) as f32,
                kind: s(h, "kind").to_string(),
                beam: num(h, "beam").unwrap_or(25.0) as f32,
                attrs: strs(h, "attrs"),
                moving: h.get_path("moving").is_some_and(Value::truthy),
            })
            .collect();
        let groups = v
            .get_path("groups")
            .and_then(Value::as_map)
            .map(|g| g.iter().map(|(k, heads)| (k.clone(), heads.as_list().unwrap_or(&[]).iter().map(text).collect())).collect())
            .unwrap_or_default();
        let channels = list(v, "profiles")
            .iter()
            .map(|p| (s(p, "id").to_string(), list(p, "modes").iter().map(|md| (s(md, "id").to_string(), strs(md, "channels"))).collect()))
            .collect();
        Rig { fixtures, heads, groups, channels, outputs: list(v, "outputs").to_vec(), errors: strs(v, "errors"), present: true }
    }

    fn fixture(&self, id: &str) -> Option<&Fixture> {
        self.fixtures.iter().find(|f| f.id == id)
    }

    fn head(&self, id: &str) -> Option<&HeadInfo> {
        self.heads.iter().find(|h| h.id == id)
    }

    /// Heads of a fixture (a fixture without explicit heads is its own single head).
    fn fixture_heads(&self, f: &Fixture) -> Vec<String> {
        if f.heads.is_empty() { vec![f.id.clone()] } else { f.heads.clone() }
    }

    /// Group names shown as selection chips; `all` is implicit unless defined.
    fn group_names(&self) -> Vec<String> {
        let mut g: Vec<String> = self.groups.keys().cloned().collect();
        if !self.groups.contains_key("all") {
            g.insert(0, "all".into());
        }
        g
    }

    /// Heads addressed by one target (fixture, group, `all`, or head id).
    fn target_heads(&self, target: &str) -> Vec<String> {
        if let Some(g) = self.groups.get(target) {
            return g.clone();
        }
        if target == "all" {
            return self.heads.iter().map(|h| h.id.clone()).collect();
        }
        if let Some(f) = self.fixture(target) {
            return self.fixture_heads(f);
        }
        if self.head(target).is_some() {
            return vec![target.to_string()];
        }
        Vec::new()
    }

    /// Friendly name of a selection/palette target.
    fn target_name(&self, target: &str) -> String {
        if target == "all" {
            return "All lights".into();
        }
        if self.groups.contains_key(target) {
            return nice(target);
        }
        if let Some(f) = self.fixture(target) {
            return f.name();
        }
        match self.head(target).and_then(|h| self.fixture(&h.fixture)) {
            Some(f) if f.id != target => format!("{} · {}", f.name(), nice(target.strip_prefix(&format!("{}.", f.id)).unwrap_or(target))),
            Some(f) => f.name(),
            None => nice(target),
        }
    }

    /// Chips for choosing lights: all, groups, then single lights.
    fn chip_targets(&self) -> Vec<(String, String)> {
        let mut v: Vec<(String, String)> = self
            .group_names()
            .into_iter()
            .map(|g| {
                let n = self.target_name(&g);
                (g, n)
            })
            .collect();
        v.extend(self.fixtures.iter().map(|f| (f.id.clone(), f.name())));
        v
    }
}

/// Current programmer selection (live state first, query as fallback).
fn selection(m: &Model) -> Vec<String> {
    m.get("lights.programmer.selection")
        .and_then(Value::as_list)
        .or_else(|| m.q("lights.programmer").and_then(|p| p.get_path("selection")).and_then(Value::as_list))
        .unwrap_or(&[])
        .iter()
        .map(text)
        .collect()
}

/// Expand selection targets to head ids (deduplicated, order kept).
fn resolve_heads(rig: &Rig, targets: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for t in targets {
        for h in rig.target_heads(t) {
            if !out.contains(&h) {
                out.push(h);
            }
        }
    }
    out
}

fn highlight_on(m: &Model) -> bool {
    m.get("lights.programmer.highlight")
        .map(Value::truthy)
        .unwrap_or_else(|| m.q("lights.programmer").and_then(|p| p.get_path("highlight")).is_some_and(Value::truthy))
}

/// A palette/cue attribute entry: plain value or `{ value, fade, … }`.
fn plain(v: &Value) -> &Value {
    v.get_path("value").filter(|_| v.as_map().is_some()).unwrap_or(v)
}

/// Attribute values a palette contributes to the selected heads, merged with the palette
/// precedence (fixture/head > smaller group > larger group > all).
fn palette_values(palette: &Value, rig: &Rig, heads: &[String]) -> BTreeMap<String, Value> {
    let Some(set) = palette.get_path("set").and_then(Value::as_map) else {
        return BTreeMap::new();
    };
    // (level, -size) ascending → later entries override earlier ones
    let mut entries: Vec<((u8, i64), &BTreeMap<String, Value>)> = Vec::new();
    for (target, attrs) in set {
        let Some(attrs) = attrs.as_map() else { continue };
        let rank = if target == "all" && !rig.groups.contains_key("all") {
            Some((0, 0))
        } else if let Some(g) = rig.groups.get(target) {
            g.iter().any(|h| heads.contains(h)).then_some((if target == "all" { 0 } else { 1 }, -(g.len() as i64)))
        } else if let Some(f) = rig.fixture(target) {
            rig.fixture_heads(f).iter().any(|h| heads.contains(h)).then_some((2, 0))
        } else {
            heads.iter().any(|h| h == target).then_some((3, 0))
        };
        if let Some(r) = rank {
            entries.push((r, attrs));
        }
    }
    entries.sort_by_key(|(r, _)| *r);
    let mut out = BTreeMap::new();
    for (_, attrs) in entries {
        for (a, v) in attrs {
            out.insert(a.clone(), plain(v).clone());
        }
    }
    out
}

/// Commands for tapping a look: with nothing chosen the look goes on every light.
fn look_ops(selection: &[String], values: BTreeMap<String, Value>) -> Vec<Op> {
    let mut ops = Vec::with_capacity(values.len() + 1);
    if selection.is_empty() {
        ops.push(select_op("all", false));
    }
    for (attr, value) in values {
        prog_set(&mut ops, &attr, value);
    }
    ops
}

/// Two attribute values show the same thing (colours compared as 8-bit RGB).
fn same_value(a: &Value, b: &Value) -> bool {
    match (color_of(a), color_of(b)) {
        (Some(x), Some(y)) => x == y,
        _ => match (a.as_f64(), b.as_f64()) {
            (Some(x), Some(y)) => (x - y).abs() < 1e-3,
            _ => a == b,
        },
    }
}

/// The look is what the programmer holds for `head` right now.
fn look_on(values: &BTreeMap<String, Value>, head: &str, prog: Option<&BTreeMap<String, Value>>) -> bool {
    let Some(prog) = prog else { return false };
    !values.is_empty() && values.iter().all(|(a, v)| prog.get(&format!("lights.{head}.{a}")).is_some_and(|have| same_value(plain(have), v)))
}

fn color_of(v: &Value) -> Option<Color32> {
    let c = plain(v).as_color()?;
    Some(rgb(c[0], c[1], c[2]))
}

fn rgb(r: f32, g: f32, b: f32) -> Color32 {
    let q = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    Color32::from_rgb(q(r), q(g), q(b))
}

fn hex_of(c: Color32) -> String {
    format!("#{:02x}{:02x}{:02x}", c.r(), c.g(), c.b())
}

fn pct(v: f32) -> String {
    format!("{:.0}%", v.clamp(0.0, 1.0) * 100.0)
}

/// A number with at most two decimals and no trailing zeros: 4 → "4", 1.50 → "1.5".
fn plain_num(x: f64) -> String {
    let mut txt = format!("{x:.2}");
    while txt.ends_with('0') {
        txt.pop();
    }
    if txt.ends_with('.') {
        txt.pop();
    }
    txt
}

/// Plain duration: "instant", "0.25 s", "2 s", "1.5 s".
fn dur(ms: f64) -> String {
    if ms <= 0.0 { "instant".into() } else { format!("{} s", plain_num(ms / 1000.0)) }
}

/// A name the engine accepts for a new look / quick effect: `Deep blue!` → `deep_blue`.
fn slug(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c.to_ascii_lowercase());
        } else if c.is_whitespace() && !out.ends_with('_') {
            out.push('_');
        }
    }
    out
}

fn health_led(status: &str) -> LedState {
    match status {
        "pass" | "ok" => LedState::Healthy,
        "warn" => LedState::Armed,
        "fail" | "error" => LedState::Error,
        _ => LedState::Idle,
    }
}

/// Plain names for fixture attributes.
fn attr_label(a: &str) -> String {
    match a {
        "intensity" => "Brightness".into(),
        "color" => "Color".into(),
        "pan" => "Turn".into(),
        "tilt" => "Tilt".into(),
        "cto" => "Warmth".into(),
        "uv" => "UV".into(),
        "gobo" => "Pattern".into(),
        "gobo_rotate" => "Pattern spin".into(),
        other => nice(other),
    }
}

fn kind_label(kind: &str) -> &'static str {
    match kind {
        "color" => "Color",
        "intensity" => "Brightness",
        "position" => "Position",
        "beam" => "Beam",
        _ => "Mixed",
    }
}

/// What a look of `kind` keeps when saved (typed looks drop everything else).
fn kind_keeps(kind: &str) -> Option<&'static str> {
    match kind {
        "color" => Some("A color look keeps only colors."),
        "intensity" => Some("A brightness look keeps only brightness."),
        "position" => Some("A position look keeps only where the lights point."),
        "beam" => Some("A beam look keeps only beam settings."),
        _ => None,
    }
}

fn look_label(pal: &Value) -> String {
    let (name, label) = (s(pal, "name"), s(pal, "label"));
    if label.is_empty() || label == name { nice(name) } else { label.to_string() }
}

/// "main/1" → "Main, step 1".
fn cue_ref(r: &str) -> String {
    match r.split_once('/') {
        Some((l, c)) => format!("{}, step {c}", nice(l)),
        None => nice(r),
    }
}

fn used_by_text(used: &[String]) -> String {
    match used.len() {
        0 => "Not used in any cue list yet".into(),
        1..=3 => format!("Used in {}", used.iter().map(|u| cue_ref(u)).collect::<Vec<_>>().join(" and ")),
        n => format!("Used in {n} cue list steps"),
    }
}

// ---------------------------------------------------------------- local widgets

/// One line of text, cut with "…" when wider than `max_w`.
fn one_line(ui: &Ui, text: &str, font: egui::FontId, color: Color32, max_w: f32) -> Arc<Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = TextWrapping::truncate_at_width(max_w.max(8.0));
    ui.painter().layout_job(job)
}

/// Horizontal 0..1 slider: track, accent fill, round knob.
fn level(ui: &mut Ui, t: &Theme, width: f32, value: &mut f32) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(width.max(40.0), 24.0), Sense::click_and_drag());
    let track = Rect::from_center_size(rect.center(), Vec2::new(rect.width() - 18.0, 6.0));
    if let Some(p) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.clicked())
    {
        let v = ((p.x - track.left()) / track.width()).clamp(0.0, 1.0);
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            resp.mark_changed();
        }
    }
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        p.rect_filled(track, CornerRadius::same(3), t.inset);
        let x = track.left() + track.width() * value.clamp(0.0, 1.0);
        p.rect_filled(Rect::from_min_max(track.min, Pos2::new(x, track.bottom())), CornerRadius::same(3), t.accent);
        let active = resp.hovered() || resp.dragged();
        p.circle(Pos2::new(x, rect.center().y), if active { 9.0 } else { 8.0 }, t.fg, Stroke::new(2.0, if active { t.accent } else { t.border }));
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Text left-aligned in a fixed-width slot (cut with "…" when longer; the full text on hover).
fn fixed_text(ui: &mut Ui, text: &str, font: egui::FontId, color: Color32, width: f32) -> egui::Response {
    let g = one_line(ui, text, font, color, width);
    let elided = g.elided;
    let (r, resp) = ui.allocate_exact_size(Vec2::new(width, 24.0), Sense::hover());
    ui.painter().galley_with_override_text_color(Pos2::new(r.left(), r.center().y - g.size().y / 2.0), g, color);
    if elided { resp.on_hover_text(text) } else { resp }
}

/// One line of text as wide as it needs (at most `max_w`, then "…" with the full text on hover).
fn line_text(ui: &mut Ui, text: &str, font: egui::FontId, color: Color32, max_w: f32) -> egui::Response {
    let g = one_line(ui, text, font, color, max_w);
    let elided = g.elided;
    let (r, resp) = ui.allocate_exact_size(g.size(), Sense::hover());
    ui.painter().galley_with_override_text_color(r.min, g, color);
    if elided { resp.on_hover_text(text) } else { resp }
}

/// Label (left-aligned in `label_w`), slider and percentage on one row.
fn level_row(ui: &mut Ui, t: &Theme, label: &str, label_w: f32, value: &mut f32, width: f32) -> egui::Response {
    ui.horizontal(|ui| {
        fixed_text(ui, label, font(type_scale::SMALL + 1.0), t.text_dim, label_w);
        let r = level(ui, t, width - label_w - 48.0 - 2.0 * ui.spacing().item_spacing.x, value);
        ui.add_sized([48.0, 24.0], egui::Label::new(RichText::new(pct(*value)).font(font_mono(type_scale::SMALL + 0.5)).color(t.fg)));
        r
    })
    .inner
}

/// What a tile shows on top: a color, a brightness level, or an icon.
#[derive(Clone, Copy)]
enum Face {
    Color(Color32),
    Level(f32),
    Icon(&'static str, Color32),
}

fn paint_face(p: &egui::Painter, t: &Theme, r: Rect, face: Face, dim: bool, icon_size: f32) {
    let cr = CornerRadius::same(if r.height() > 40.0 { radius::TILE - 3 } else { 7 });
    match face {
        Face::Color(c) => {
            p.rect_filled(r, cr, if dim { mix(t.surface_hi, c, 0.35) } else { c });
        }
        Face::Level(v) => {
            let lit = mix(t.inset, t.fg_bright, v.clamp(0.0, 1.0) * if dim { 0.3 } else { 0.9 });
            p.rect_filled(r, cr, lit);
            if r.height() > 40.0 {
                p.text(r.center(), Align2::CENTER_CENTER, pct(v), font_semibold(15.0), widgets::contrast(lit));
            }
        }
        Face::Icon(glyph, c) => {
            let c = if dim { t.text_faint } else { c };
            p.rect_filled(r, cr, mix(t.surface, c, 0.16));
            p.text(r.center(), Align2::CENTER_CENTER, glyph, font(icon_size), c);
        }
    }
}

/// A tap target in the look / effect grids: face on top, name and a short line below (the line
/// is left out when it just repeats the name).
#[allow(clippy::too_many_arguments)]
fn tile(ui: &mut Ui, t: &Theme, size: Vec2, face: Face, label: &str, sub: &str, on: bool, enabled: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(size, if enabled { Sense::click() } else { Sense::hover() });
    if ui.is_rect_visible(rect) {
        let hovered = enabled && resp.hovered();
        let fill = if enabled && resp.is_pointer_button_down_on() {
            mix(t.surface_hi, Color32::BLACK, 0.12)
        } else if on {
            mix(t.surface_hi, t.accent, 0.1)
        } else if hovered {
            mix(t.surface_hi, t.fg, 0.05)
        } else {
            t.surface_hi
        };
        let edge = if on {
            t.accent
        } else if hovered {
            mix(t.border, t.fg, 0.3)
        } else {
            t.border
        };
        let p = ui.painter();
        p.rect(rect, CornerRadius::same(radius::TILE), fill, Stroke::new(if on { 2.0 } else { 1.0 }, edge), StrokeKind::Inside);
        let face_rect = Rect::from_min_size(rect.min + Vec2::splat(6.0), Vec2::new(rect.width() - 12.0, rect.height() - 6.0 - TILE_TEXT_H));
        paint_face(p, t, face_rect, face, !enabled, (face_rect.height() * 0.42).clamp(20.0, 34.0));
        if on {
            let g = p.layout_no_wrap("ON".into(), font_bold(10.5), t.fg);
            let chip = Rect::from_min_size(Pos2::new(face_rect.right() - g.size().x - 20.0, face_rect.top() + 6.0), Vec2::new(g.size().x + 14.0, 18.0));
            p.rect_filled(chip, CornerRadius::same(radius::PILL), t.bg.gamma_multiply(0.85));
            p.galley_with_override_text_color(chip.center() - g.size() / 2.0, g, t.fg);
        }
        let max_w = rect.width() - 20.0;
        let x = rect.left() + 10.0;
        let name = one_line(ui, label, font_semibold(type_scale::BODY), t.fg, max_w);
        let repeat = sub.is_empty() || sub.eq_ignore_ascii_case(label);
        let name_y = if repeat { face_rect.bottom() + (TILE_TEXT_H - name.size().y) / 2.0 } else { face_rect.bottom() + 8.0 };
        ui.painter().galley_with_override_text_color(Pos2::new(x, name_y), name, if enabled { t.fg } else { t.text_faint });
        if !repeat {
            let sub = one_line(ui, sub, font(type_scale::SMALL), t.text_dim, max_w);
            ui.painter().galley_with_override_text_color(Pos2::new(x, face_rect.bottom() + 28.0), sub, if enabled { t.text_dim } else { t.text_faint });
        }
    }
    if enabled { resp.on_hover_cursor(egui::CursorIcon::PointingHand) } else { resp }
}

/// Tile size for `n` tiles in a row of `width`: as many columns as fit (never more than there
/// are tiles), widened to fill the row up to `TILE_MAX_W`; the face grows with the width.
fn tile_size(width: f32, n: usize) -> Vec2 {
    let gap = spacing::M;
    let fit = ((width + gap) / (TILE_MIN_W + gap)).floor().max(2.0);
    let cols = fit.min(n.max(1) as f32);
    let w = ((width - gap * (cols - 1.0)) / cols).floor().min(TILE_MAX_W);
    Vec2::new(w, 6.0 + (w * 0.34).clamp(44.0, 110.0) + TILE_TEXT_H)
}

/// A selectable row with a swatch (the editor's list of looks).
fn look_row(ui: &mut Ui, t: &Theme, face: Face, title: &str, sub: &str, selected: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 52.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if selected {
            p.rect_filled(rect, CornerRadius::same(radius::CONTROL), mix(t.surface, t.accent, 0.14));
        } else if resp.hovered() {
            p.rect_filled(rect, CornerRadius::same(radius::CONTROL), t.surface_hi);
        }
        let sw = Rect::from_min_size(Pos2::new(rect.left() + 10.0, rect.center().y - 16.0), Vec2::splat(32.0));
        paint_face(p, t, sw, face, false, 15.0);
        let x = sw.right() + 12.0;
        let max_w = rect.right() - x - 8.0;
        let title = one_line(ui, title, font_medium(type_scale::BODY), t.fg, max_w);
        let sub = one_line(ui, sub, font(type_scale::SMALL), t.text_dim, max_w);
        let p = ui.painter();
        p.galley_with_override_text_color(Pos2::new(x, rect.center().y - 18.0), title, t.fg);
        p.galley_with_override_text_color(Pos2::new(x, rect.center().y + 2.0), sub, t.text_dim);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// A clickable row inside a cue list: step number, name, timing.
fn step_row(ui: &mut Ui, t: &Theme, id: &str, label: &str, timing: &str, current: bool) -> egui::Response {
    let w = ui.available_width();
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, 34.0), Sense::click());
    if ui.is_rect_visible(rect) {
        let p = ui.painter();
        if current {
            p.rect_filled(rect, CornerRadius::same(6), mix(t.surface_hi, t.accent, 0.2));
        } else if resp.hovered() {
            p.rect_filled(rect, CornerRadius::same(6), mix(t.surface_hi, t.fg, 0.06));
        }
        p.text(
            Pos2::new(rect.left() + 10.0, rect.center().y),
            Align2::LEFT_CENTER,
            id,
            font_mono(type_scale::SMALL),
            if current { t.accent } else { t.text_faint },
        );
        let timing = one_line(ui, timing, font(type_scale::SMALL), t.text_dim, (rect.width() * 0.5).max(60.0));
        let tx = rect.right() - 10.0 - timing.size().x;
        let name = one_line(ui, label, font_medium(type_scale::SMALL + 1.0), t.fg, (tx - rect.left() - 52.0).max(40.0));
        let p = ui.painter();
        p.galley_with_override_text_color(Pos2::new(rect.left() + 40.0, rect.center().y - name.size().y / 2.0), name, t.fg);
        p.galley_with_override_text_color(Pos2::new(tx, rect.center().y - timing.size().y / 2.0), timing, t.text_dim);
    }
    resp.on_hover_cursor(egui::CursorIcon::PointingHand)
}

/// Light color field: hue left → right, pale (bottom) → full color (top). Brightness is its own
/// control, so the picked color is always at full value.
fn color_field(ui: &mut Ui, t: &Theme, size: Vec2, hsva: &mut Hsva) -> egui::Response {
    const NX: u32 = 24;
    const NY: u32 = 6;
    let (rect, mut resp) = ui.allocate_exact_size(size, Sense::click_and_drag());
    if let Some(p) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.clicked())
    {
        let h = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        let s = ((rect.bottom() - p.y) / rect.height()).clamp(0.0, 1.0);
        if (h - hsva.h).abs() > 1e-4 || (s - hsva.s).abs() > 1e-4 || hsva.v < 1.0 {
            *hsva = Hsva::new(h, s, 1.0, 1.0);
            resp.mark_changed();
        }
    }
    if ui.is_rect_visible(rect) {
        let mut mesh = egui::Mesh::default();
        for yi in 0..=NY {
            for xi in 0..=NX {
                let (fx, fy) = (xi as f32 / NX as f32, yi as f32 / NY as f32);
                mesh.colored_vertex(rect.min + Vec2::new(rect.width() * fx, rect.height() * fy), Color32::from(Hsva::new(fx, 1.0 - fy, 1.0, 1.0)));
            }
        }
        for yi in 0..NY {
            for xi in 0..NX {
                let i = yi * (NX + 1) + xi;
                mesh.add_triangle(i, i + 1, i + NX + 1);
                mesh.add_triangle(i + 1, i + NX + 2, i + NX + 1);
            }
        }
        let p = ui.painter();
        p.add(Shape::mesh(mesh));
        p.rect_stroke(rect, CornerRadius::same(2), Stroke::new(1.0, t.border), StrokeKind::Outside);
        let dot = Pos2::new(rect.left() + rect.width() * hsva.h, rect.bottom() - rect.height() * hsva.s);
        p.circle(dot, 8.0, Color32::from(Hsva::new(hsva.h, hsva.s, 1.0, 1.0)), Stroke::new(2.5, t.fg));
        p.circle_stroke(dot, 9.5, Stroke::new(1.0, t.bg));
    }
    resp.on_hover_cursor(egui::CursorIcon::Crosshair)
}

/// 2D pad: x = pan (left→right), y = tilt (bottom→top), both 0..1.
fn xy_pad(ui: &mut Ui, t: &Theme, size: f32, v: &mut [f32; 2]) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::splat(size), Sense::click_and_drag());
    if let Some(p) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.clicked())
    {
        let n = [((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0), ((rect.bottom() - p.y) / rect.height()).clamp(0.0, 1.0)];
        if n != *v {
            *v = n;
            resp.mark_changed();
        }
    }
    let p = ui.painter_at(rect);
    let r = CornerRadius::same(radius::CONTROL);
    p.rect_filled(rect, r, t.inset);
    let grid = Stroke::new(1.0, t.border);
    for i in 1..4 {
        let f = i as f32 / 4.0;
        p.line_segment([Pos2::new(rect.left() + rect.width() * f, rect.top()), Pos2::new(rect.left() + rect.width() * f, rect.bottom())], grid);
        p.line_segment([Pos2::new(rect.left(), rect.top() + rect.height() * f), Pos2::new(rect.right(), rect.top() + rect.height() * f)], grid);
    }
    let dot = Pos2::new(rect.left() + rect.width() * v[0], rect.bottom() - rect.height() * v[1]);
    let cross = Stroke::new(1.0, t.accent.gamma_multiply(0.5));
    p.line_segment([Pos2::new(dot.x, rect.top()), Pos2::new(dot.x, rect.bottom())], cross);
    p.line_segment([Pos2::new(rect.left(), dot.y), Pos2::new(rect.right(), dot.y)], cross);
    p.circle_filled(dot, 6.0, t.accent);
    p.circle_stroke(dot, 10.0, Stroke::new(1.5, t.accent.gamma_multiply(0.6)));
    p.rect_stroke(rect, r, Stroke::new(1.0, if resp.hovered() { t.accent } else { t.border }), StrokeKind::Inside);
    resp.on_hover_cursor(egui::CursorIcon::Crosshair)
}

fn progress_bar(ui: &mut Ui, t: &Theme, width: f32, frac: f32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 4.0), Sense::hover());
    let p = ui.painter();
    p.rect_filled(rect, CornerRadius::same(2), t.inset);
    p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * frac.clamp(0.0, 1.0), rect.height())), CornerRadius::same(2), t.accent);
}

/// Small group label inside a card ("Brightness", "Color" …).
fn caption(ui: &mut Ui, t: &Theme, text: &str) {
    widgets::section(ui, t, "", text);
}

/// Two columns side by side (`left` wide); both closures get a top-down Ui of their width and
/// the shared mutable state `s` in turn.
fn columns<S>(ui: &mut Ui, left: f32, s: &mut S, body_l: impl FnOnce(&mut Ui, &mut S), body_r: impl FnOnce(&mut Ui, &mut S)) {
    let w = ui.available_width();
    let right = (w - left - spacing::L).max(200.0);
    ui.horizontal_top(|ui| {
        ui.spacing_mut().item_spacing.x = spacing::L;
        ui.allocate_ui_with_layout(Vec2::new(left, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(left);
            body_l(ui, s);
        });
        ui.allocate_ui_with_layout(Vec2::new(right, 0.0), Layout::top_down(Align::Min), |ui| {
            ui.set_width(right);
            body_r(ui, s);
        });
    });
}

/// "Which lights" chips (click = choose, shift-click = add).
fn which_lights(cx: &Cx, ui: &mut Ui, out: &mut Vec<Op>) {
    let shift = ui.input(|i| i.modifiers.shift);
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = Vec2::splat(spacing::S);
        for (target, label) in cx.rig.chip_targets() {
            let on = cx.selection.contains(&target);
            if widgets::chip(ui, cx.t, "", &label, on).on_hover_text("Click to choose · Shift-click to add more").clicked() {
                out.push(select_op(&target, shift));
            }
        }
    });
}

// ---------------------------------------------------------------- top bar

/// Sub-view switch plus what must always be in reach: status, brightness, blackout, safe look.
fn top_bar(m: &Model, t: &Theme, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, output: Option<&Value>, now: Instant) {
    ui.horizontal(|ui| {
        let mut idx = Tab::ALL.iter().position(|x| *x == st.tab).unwrap_or(0);
        if widgets::segmented(ui, t, &mut idx, &Tab::LABELS) {
            st.tab = Tab::ALL[idx];
        }
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            ui.spacing_mut().item_spacing.x = spacing::S;
            if widgets::hold_button(ui, t, "Go to safe look", t.bright_red, 0.8) {
                out.push(act("lights.panic", Value::Null));
            }
            let bkey = "@lights.blackout";
            let blackout = st.edited(bkey, now).map(Value::truthy).unwrap_or_else(|| m.b("lights.blackout"));
            let (label, kind, tip) = if blackout {
                ("Blackout is on", Kind::Live, "Every light is off. Click to bring them back.")
            } else {
                ("Blackout", Kind::Secondary, "Turn every light off right away. Click again to bring them back.")
            };
            if widgets::button_ex(ui, t, Some(icon::EYE_OFF), label, kind, Size::Medium, 0.0, true).on_hover_text(tip).clicked() {
                st.edit(bkey, Value::Bool(!blackout), now);
                out.push(Op::Set { address: "lights.blackout".into(), value: Value::Bool(!blackout) });
            }
            ui.add_space(spacing::M);
            let key = "@lights.master";
            let mut gm = st.edited(key, now).and_then(Value::as_f32).or_else(|| m.get("lights.master").and_then(Value::as_f32)).unwrap_or(1.0);
            ui.add_sized([44.0, 24.0], egui::Label::new(RichText::new(pct(gm)).font(font_mono(type_scale::SMALL + 0.5)).color(t.fg)));
            if level(ui, t, 180.0, &mut gm).on_hover_text("How bright all your lights are overall").changed() {
                st.edit(key, Value::from(gm), now);
                out.push(Op::Set { address: "lights.master".into(), value: Value::from(gm) });
            }
            ui.label(RichText::new("Brightness").font(font_medium(type_scale::SMALL + 0.5)).color(t.text_dim));
            ui.add_space(spacing::M);
            let (status, detail) = m.get("health.dmx").map(|h| (s(h, "status"), s(h, "detail"))).unwrap_or(("", ""));
            let text = match health_led(status) {
                LedState::Healthy => "Lights working",
                LedState::Armed => "Lights need a look",
                LedState::Error => "Lights not connected",
                _ => "Lights",
            };
            let tip = if detail.is_empty() { "Click for the connection details in Setup".to_string() } else { format!("{detail}\n\nClick for more in Setup") };
            if widgets::pill(ui, t, icon::DEVICE, text, health_led(status)).on_hover_text(tip).on_hover_cursor(egui::CursorIcon::PointingHand).clicked() {
                st.tab = Tab::Setup;
            }
            if let Some(l) = output.and_then(|o| o.get_path("limiter")).filter(|l| l.get_path("active").is_some_and(Value::truthy)) {
                widgets::badge(ui, t, "Flash limiter on", t.yellow).on_hover_text(format!(
                    "Some fast flashes are being softened so they stay safe for viewers.\nSoftened: {} · strobes slowed: {}",
                    num(l, "suppressed").unwrap_or(0.0),
                    num(l, "strobe_capped").unwrap_or(0.0)
                ));
            }
        });
    });
}

// ---------------------------------------------------------------- looks

fn looks_tab(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    let w = ui.available_width();
    let right = (w * 0.42).clamp(380.0, 900.0);
    columns(
        ui,
        w - right - spacing::L,
        &mut (st, out),
        |ui, (st, out)| {
            looks_card(cx, st, ui, out);
            ui.add_space(spacing::L);
            effects_card(cx, ui, out);
            ui.add_space(spacing::L);
        },
        |ui, (st, out)| {
            cuelists_card(cx, st, ui, out);
            ui.add_space(spacing::L);
        },
    );
}

/// Face of a look: its color (stream colors follow the live palette), brightness, or an icon.
fn look_face(m: &Model, t: &Theme, pal: &Value) -> Face {
    let set = pal.get_path("set").and_then(Value::as_map);
    let first = |attr: &str| set.and_then(|set| set.get("all").into_iter().chain(set.values()).find_map(|a| a.get_path(attr).map(plain)));
    if let Some(v) = first("color") {
        if let Some(c) = color_of(v) {
            return Face::Color(c);
        }
        if let Some(slot) = v.as_str().and_then(|s| s.strip_prefix("stream:")) {
            return Face::Color(m.get(&format!("palette.{slot}")).and_then(color_of).unwrap_or(t.accent));
        }
    }
    if let Some(v) = first("intensity").and_then(Value::as_f32) {
        return Face::Level(v);
    }
    match s(pal, "kind") {
        "position" => Face::Icon(CROSSHAIRS, t.cyan),
        "beam" => Face::Icon(icon::LIGHT, t.yellow),
        _ => Face::Icon(icon::PALETTE, t.accent),
    }
}

fn looks_card(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    let (m, t, rig) = (cx.m, cx.t, cx.rig);
    let pals = m.q_list("lights.palettes");
    let holding = m.b("lights.programmer.active");
    let sub = if cx.selection.is_empty() {
        "Tap a look to put it on all your lights, or choose lights first."
    } else {
        "Tap a look to put it on the lights you chose."
    };
    let mut let_go = false;
    widgets::titled(
        ui,
        t,
        "Looks",
        sub,
        |ui| {
            if holding {
                let_go = widgets::button_ex(ui, t, Some(icon::UNDO), "Let go", Kind::Secondary, Size::Small, 0.0, true)
                    .on_hover_text("Stop your changes. The lights go back to your cue lists and effects.")
                    .clicked();
                widgets::badge(ui, t, "Your changes are on top", t.yellow)
                    .on_hover_text("Looks you tap stay on the chosen lights, on top of cue lists, until you let go.");
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            which_lights(cx, ui, out);
            ui.add_space(spacing::L);
            if pals.is_empty() {
                if widgets::empty_state(
                    ui,
                    t,
                    icon::PALETTE,
                    "No looks yet",
                    "A look is a color, brightness or position you put on your lights with one tap.",
                    Some("Make a look"),
                ) {
                    st.tab = Tab::Edit;
                    st.new_look = true;
                }
                return;
            }
            let all: Vec<String>;
            let targets = if cx.heads.is_empty() {
                all = rig.heads.iter().map(|h| h.id.clone()).collect();
                &all[..]
            } else {
                cx.heads
            };
            let prog = m.q("lights.programmer").and_then(|p| p.get_path("values")).and_then(Value::as_map);
            let size = tile_size(ui.available_width(), pals.len());
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(spacing::M);
                for pal in pals {
                    let values = palette_values(pal, rig, targets);
                    let usable = !values.is_empty();
                    let on = cx.heads.first().is_some_and(|h| look_on(&values, h, prog));
                    let label = look_label(pal);
                    let resp = tile(ui, t, size, look_face(m, t, pal), &label, kind_label(s(pal, "kind")), on, usable);
                    let tip = if usable {
                        format!("{label}\n{}", used_by_text(&strs(pal, "used_by")))
                    } else {
                        format!("{label}\nThis look doesn't change the lights you chose")
                    };
                    if resp.on_hover_text(tip).clicked() {
                        out.extend(look_ops(cx.selection, values));
                    }
                }
            });
        },
    );
    if let_go {
        out.push(act("lights.programmer.release", Value::Null));
    }
}

/// Plain name of an effect kind (se-dmx `effects::Kind`).
fn effect_kind_name(kind: &str) -> String {
    match kind {
        "dimmer_sine" => "Smooth pulse".into(),
        "dimmer_saw" => "Ramp pulse".into(),
        "dimmer_square" => "On-off flash".into(),
        "color_chase" => "Color chase".into(),
        "rainbow" => "Rainbow".into(),
        "circle" => "Moving circle".into(),
        "sparkle" => "Sparkle".into(),
        other => nice(other),
    }
}

/// One icon per effect, never the same twice in the grid: each kind has a few fitting glyphs
/// (a "fast" effect prefers fast-forward) and takes the first one not used yet.
fn effect_icons(fx: &[Value]) -> Vec<&'static str> {
    const HEARTBEAT: &str = "\u{f21e}";
    const FAST: &str = "\u{f050}";
    const REPEAT: &str = "\u{f01e}";
    const TINT: &str = "\u{f043}";
    const NOTCH: &str = "\u{f1ce}";
    const SUN: &str = "\u{f185}";
    const FIRE: &str = "\u{f06d}";
    const SNOW: &str = "\u{f2dc}";
    let mut used: Vec<&'static str> = Vec::with_capacity(fx.len());
    for e in fx {
        let fast = s(e, "name").contains("fast") || s(e, "label").to_lowercase().contains("fast");
        let wanted: &[&'static str] = match s(e, "kind") {
            "dimmer_sine" => &[HEARTBEAT, icon::HEART],
            "dimmer_saw" => &[icon::SCOPE, HEARTBEAT],
            "dimmer_square" => &[icon::BOLT, icon::LIGHT],
            "color_chase" if fast => &[FAST, icon::SHUFFLE, REPEAT],
            "color_chase" => &[icon::SHUFFLE, FAST, REPEAT],
            "rainbow" => &[TINT, icon::PALETTE],
            "circle" => &[NOTCH, CROSSHAIRS],
            "sparkle" => &[icon::STAR, icon::SPARKLE],
            _ => &[icon::SPARKLE],
        };
        let spare = [SUN, FIRE, SNOW, icon::STAR, icon::SPARKLE, HEARTBEAT, REPEAT];
        let pick = wanted.iter().chain(spare.iter()).copied().find(|g| !used.contains(g)).unwrap_or(wanted[0]);
        used.push(pick);
    }
    used
}

fn effects_card(cx: &Cx, ui: &mut Ui, out: &mut Vec<Op>) {
    let (m, t) = (cx.m, cx.t);
    let fx = m.q_list("lights.effects");
    widgets::titled(
        ui,
        t,
        "Effects",
        "Moving patterns like chases and pulses. Tap to start or stop.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if fx.is_empty() {
                widgets::empty_state(
                    ui,
                    t,
                    icon::SPARKLE,
                    "No effects yet",
                    "Chases, pulses and rainbows come with your lighting setup and show up here.",
                    None,
                );
                return;
            }
            let size = tile_size(ui.available_width(), fx.len());
            let icons = effect_icons(fx);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = Vec2::splat(spacing::M);
                for (e, glyph) in fx.iter().zip(icons) {
                    let name = s(e, "name");
                    let active =
                        m.get(&format!("lights.effect.{name}.active")).map(Value::truthy).unwrap_or_else(|| e.get_path("active").is_some_and(Value::truthy));
                    let label = if s(e, "label").is_empty() { nice(name) } else { s(e, "label").to_string() };
                    let resp = tile(ui, t, size, Face::Icon(glyph, t.magenta), &label, &effect_kind_name(s(e, "kind")), active, true);
                    let rate = m.get(&format!("lights.effect.{name}.rate")).and_then(Value::as_f64).or_else(|| num(e, "rate")).unwrap_or(0.0);
                    let size_v = m.get(&format!("lights.effect.{name}.size")).and_then(Value::as_f64).or_else(|| num(e, "size")).unwrap_or(0.0);
                    let speed =
                        if s(e, "unit") == "beats" { format!("once every {} beats", plain_num(rate)) } else { format!("{} times a second", plain_num(rate)) };
                    let on: Vec<String> = strs(e, "targets").iter().map(|x| cx.rig.target_name(x)).collect();
                    let tip = format!(
                        "{}\nSpeed: {speed} · size {}\nOn: {}",
                        if active { "Running. Tap to stop." } else { "Tap to start." },
                        pct(size_v as f32),
                        if on.is_empty() { "All lights".to_string() } else { on.join(", ") }
                    );
                    if resp.on_hover_text(tip).clicked() {
                        out.push(act(if active { "lights.effect.stop" } else { "lights.effect.start" }, Value::map().with("name", name)));
                    }
                }
            });
        },
    );
}

fn cuelists_card(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    let t = cx.t;
    let lists = cx.m.q_list("lights.cuelists");
    let mut make = false;
    widgets::titled(
        ui,
        t,
        "Cue lists",
        "Go steps to the next look. Stop hands the lights back.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if lists.is_empty() {
                make = widgets::empty_state(ui, t, icon::LIST, "No cue lists yet", "A cue list is a row of looks you step through with Go.", Some("Make one"));
                return;
            }
            // cue lists you step through first, then the one-step looks (engine order within each)
            let mut lists: Vec<&Value> = lists.iter().collect();
            lists.sort_by_key(|cl| list(cl, "cues").len() <= 1);
            let w = ui.available_width();
            if w >= 720.0 {
                let cw = ((w - spacing::M) / 2.0).floor();
                for pair in lists.chunks(2) {
                    ui.horizontal_top(|ui| {
                        ui.spacing_mut().item_spacing.x = spacing::M;
                        for cl in pair {
                            ui.allocate_ui_with_layout(Vec2::new(cw, 0.0), Layout::top_down(Align::Min), |ui| {
                                ui.set_width(cw);
                                cuelist_row(cx, st, ui, out, cl);
                            });
                        }
                    });
                    ui.add_space(spacing::M);
                }
            } else {
                for cl in &lists {
                    cuelist_row(cx, st, ui, out, cl);
                    ui.add_space(spacing::S);
                }
            }
        },
    );
    if make {
        st.tab = Tab::Edit;
    }
}

fn step_label(c: &Value) -> String {
    let label = s(c, "label");
    if label.is_empty() { format!("Step {}", text(c.get_path("id").unwrap_or(&Value::Null))) } else { label.to_string() }
}

/// "Fades in 3 s, out 1 s · next step 2 s after".
fn cue_timing(c: &Value) -> String {
    let ms = |k: &str| c.get_path(k).filter(|v| !v.is_null()).and_then(Value::as_f64);
    let fade = ms("fade_ms").unwrap_or(0.0);
    let mut s = if fade > 0.0 { format!("Fades in {}", dur(fade)) } else { "Snaps on".to_string() };
    if let Some(o) = ms("fade_out_ms")
        && (o - fade).abs() > 0.5
    {
        s.push_str(&format!(", out {}", dur(o)));
    }
    if let Some(d) = ms("delay_ms").filter(|d| *d > 0.0) {
        s.push_str(&format!(" · waits {} first", dur(d)));
    }
    if let Some(f) = ms("follow_ms") {
        s.push_str(&format!(" · next step {} after", dur(f)));
    }
    if let Some(w) = ms("wait_ms") {
        s.push_str(&format!(" · next step {} after it starts", dur(w)));
    }
    s
}

fn step_tip(rig: &Rig, c: &Value) -> String {
    let mut lines = vec![cue_timing(c)];
    let names = |k: &str, f: &dyn Fn(&str) -> String| strs(c, k).iter().map(|x| f(x)).collect::<Vec<_>>().join(", ");
    let targets = names("targets", &|x| rig.target_name(x));
    if !targets.is_empty() {
        lines.push(format!("Changes: {targets}"));
    }
    let looks = names("palettes", &nice);
    if !looks.is_empty() {
        lines.push(format!("Looks: {looks}"));
    }
    let fx = names("effects", &nice);
    if !fx.is_empty() {
        lines.push(format!("Effects: {fx}"));
    }
    if c.get_path("block").is_some_and(Value::truthy) {
        lines.push("Starts fresh: nothing carries over from earlier steps".into());
    }
    lines.push("Click to jump here".into());
    lines.join("\n")
}

fn cuelist_row(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, cl: &Value) {
    let (m, t, now) = (cx.m, cx.t, cx.now);
    let name = s(cl, "name");
    let base = format!("lights.cuelist.{name}");
    let playing = m.get(&format!("{base}.playing")).map(Value::truthy).unwrap_or_else(|| cl.get_path("playing").is_some_and(Value::truthy));
    let current = m.get(&format!("{base}.cue")).map(text).filter(|c| !c.is_empty()).or_else(|| cl.get_path("current").filter(|v| !v.is_null()).map(text));
    let next = m.get(&format!("{base}.next")).map(text).filter(|c| !c.is_empty()).or_else(|| cl.get_path("next").filter(|v| !v.is_null()).map(text));
    let cues = list(cl, "cues");
    let multi = cues.len() > 1;
    let cue_name = |id: &str| cues.iter().find(|c| c.get_path("id").map(text).as_deref() == Some(id)).map(step_label).unwrap_or_else(|| format!("Step {id}"));
    let title = if s(cl, "label").is_empty() { nice(name) } else { s(cl, "label").to_string() };
    let frame = egui::Frame::new()
        .fill(if playing { mix(t.surface, t.accent, 0.07) } else { t.surface })
        .stroke(Stroke::new(1.0, if playing { mix(t.border, t.accent, 0.7) } else { t.border }))
        .corner_radius(radius::TILE)
        .inner_margin(egui::Margin::same(spacing::M as i8));
    // one-step lists are simple looks: their step name only matters when it adds something
    let aside = cues.first().map(step_label).filter(|l| !multi && !l.eq_ignore_ascii_case(&title));
    frame.show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 20.0), Sense::hover());
            ui.painter().circle_filled(r.center(), 4.5, if playing { t.green } else { t.text_faint });
            let args = || Value::map().with("cuelist", name);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.spacing_mut().item_spacing.x = spacing::XS + 2.0;
                if widgets::button_ex(ui, t, Some(icon::STOP), "Stop", Kind::Secondary, Size::Small, 0.0, playing)
                    .on_hover_text("Stop this cue list; the lights it controls fade back")
                    .clicked()
                {
                    out.push(act("lights.release", args()));
                }
                if multi
                    && widgets::button_ex(ui, t, Some(icon::LEFT), "Back", Kind::Secondary, Size::Small, 0.0, true).on_hover_text("Go back one step").clicked()
                {
                    out.push(act("lights.back", args()));
                }
                let go_tip = if !playing {
                    "Start this cue list"
                } else if multi {
                    "Go to the next step"
                } else {
                    "Start this look again"
                };
                if widgets::button_ex(ui, t, Some(icon::PLAY), "Go", if playing && multi { Kind::Primary } else { Kind::Secondary }, Size::Small, 64.0, true)
                    .on_hover_text(go_tip)
                    .clicked()
                {
                    out.push(act("lights.go", args()));
                }
                ui.with_layout(Layout::left_to_right(Align::Center), |ui| {
                    line_text(ui, &title, font_semibold(type_scale::BODY + 1.0), t.fg, ui.available_width());
                    if let Some(a) = &aside {
                        line_text(ui, &format!("·  {a}"), font(type_scale::SMALL + 0.5), t.text_dim, ui.available_width());
                    }
                });
            });
        });
        let line = match (&current, playing) {
            (Some(c), true) if multi => {
                let n = cues.iter().position(|x| x.get_path("id").map(text).as_deref() == Some(c.as_str())).map(|i| i + 1).unwrap_or(0);
                let nx = next.as_deref().map(|x| format!("  ·  next: {}", cue_name(x))).unwrap_or_default();
                Some(format!("Now: {} ({n} of {}){nx}", cue_name(c), cues.len()))
            }
            _ if multi => Some(format!("{} steps · starts with {}", cues.len(), cues.first().map(step_label).unwrap_or_default())),
            _ if cues.is_empty() => Some("Empty — no steps yet".into()),
            _ => None,
        };
        if let Some(line) = line {
            ui.add_space(2.0);
            line_text(ui, &line, font(type_scale::SMALL + 0.5), if playing { t.fg } else { t.text_dim }, ui.available_width());
        }
        let (progress, total) = (num(cl, "progress").unwrap_or(0.0) as f32, num(cl, "total_ms").unwrap_or(0.0));
        if playing && total > 0.0 && progress < 1.0 {
            ui.add_space(spacing::XS);
            progress_bar(ui, t, ui.available_width(), progress);
        }
        ui.add_space(spacing::XS);
        let key = format!("@{base}.master");
        let mut master = st
            .edited(&key, now)
            .and_then(Value::as_f32)
            .or_else(|| m.get(&format!("{base}.master")).and_then(Value::as_f32))
            .or_else(|| num(cl, "master").map(|x| x as f32))
            .unwrap_or(1.0);
        if level_row(ui, t, "Level", 52.0, &mut master, ui.available_width()).on_hover_text("How strong this cue list is").changed() {
            st.edit(&key, Value::from(master), now);
            out.push(Op::Set { address: format!("{base}.master"), value: Value::from(master) });
        }
        if multi {
            widgets::details(ui, t, ("lights_steps", name), &format!("All {} steps", cues.len()), |ui| {
                ui.spacing_mut().item_spacing.y = 2.0;
                for c in cues {
                    let id = text(c.get_path("id").unwrap_or(&Value::Null));
                    let is_cur = playing && current.as_deref() == Some(id.as_str());
                    if step_row(ui, t, &id, &step_label(c), &cue_timing(c), is_cur).on_hover_text(step_tip(cx.rig, c)).clicked() {
                        out.push(act("lights.goto", Value::map().with("cuelist", name).with("cue", id)));
                    }
                }
            });
        }
    });
}

// ---------------------------------------------------------------- edit a look

fn edit_tab(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    let pals = cx.m.q_list("lights.palettes");
    let current = if st.new_look { None } else { st.look.as_deref().and_then(|n| pals.iter().find(|p| s(p, "name") == n)).or_else(|| pals.first()) };
    let w = ui.available_width();
    let list_w = (w * 0.24).clamp(260.0, 360.0);
    if w < 2400.0 {
        columns(ui, list_w, &mut (st, out), |ui, (st, _)| look_list(cx, st, ui, pals, current), |ui, (st, out)| editor(cx, st, ui, out, current));
        return;
    }
    // wide screens: the live stage sits beside the editor, so changes can be watched as they happen
    let editor_w = ((w - list_w) * 0.55).clamp(1100.0, 1500.0);
    columns(
        ui,
        list_w,
        &mut (st, out),
        |ui, (st, _)| look_list(cx, st, ui, pals, current),
        |ui, s| {
            columns(
                ui,
                editor_w,
                s,
                |ui, (st, out)| editor(cx, st, ui, out, current),
                |ui, (_, out)| {
                    let heads = stage_heads(cx.rig, cx.output);
                    widgets::titled(
                        ui,
                        cx.t,
                        "Your stage",
                        "What your lights are doing right now. Click lights to choose them.",
                        |_| {},
                        |ui| {
                            ui.set_width(ui.available_width());
                            if heads.is_empty() {
                                widgets::hint(ui, cx.t, "No lights added yet.");
                            } else {
                                let w = ui.available_width();
                                ui.allocate_ui(Vec2::new(w, w * 0.5 + 24.0), |ui| stage_floor(cx.t, ui, out, cx.rig, &heads, cx.heads));
                            }
                        },
                    );
                },
            )
        },
    );
}

fn look_list(cx: &Cx, st: &mut LightsState, ui: &mut Ui, pals: &[Value], current: Option<&Value>) {
    let (m, t) = (cx.m, cx.t);
    let mut new = false;
    let count = match pals.len() {
        0 => String::new(),
        1 => "1 look".into(),
        n => format!("{n} looks"),
    };
    widgets::titled(
        ui,
        t,
        "Your looks",
        &count,
        |ui| {
            new = widgets::button_ex(ui, t, Some(icon::PLUS), "New", Kind::Secondary, Size::Small, 0.0, true).on_hover_text("Make a new look").clicked();
        },
        |ui| {
            ui.set_width(ui.available_width());
            ui.spacing_mut().item_spacing.y = 2.0;
            if pals.is_empty() && !st.new_look {
                widgets::hint(ui, t, "No looks yet. Set up your lights on the right, then save it as your first look.");
            }
            let cur = current.map(|c| s(c, "name"));
            for pal in pals {
                let name = s(pal, "name");
                if look_row(ui, t, look_face(m, t, pal), &look_label(pal), kind_label(s(pal, "kind")), cur == Some(name)).clicked() {
                    st.look = Some(name.to_string());
                    st.new_look = false;
                }
            }
            if st.new_look || pals.is_empty() {
                look_row(ui, t, Face::Icon(icon::PLUS, t.accent), "New look", "Not saved yet", true);
            }
        },
    );
    if new {
        st.new_look = true;
        st.new_name.clear();
    }
}

/// "All lights: ■ color · 80% brightness" rows for what a look sets.
fn look_summary(cx: &Cx, ui: &mut Ui, pal: &Value) {
    let t = cx.t;
    let Some(set) = pal.get_path("set").and_then(Value::as_map) else { return };
    for (target, attrs) in set {
        let Some(attrs) = attrs.as_map() else { continue };
        ui.horizontal(|ui| {
            fixed_text(ui, &cx.rig.target_name(target), font_medium(type_scale::SMALL + 0.5), t.fg, 150.0);
            for (a, v) in attrs {
                let v = plain(v);
                let value = if let Some(c) = color_of(v) {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(14.0), Sense::hover());
                    ui.painter().rect(r, CornerRadius::same(3), c, Stroke::new(1.0, t.border), StrokeKind::Inside);
                    String::new()
                } else if let Some(x) = v.as_str() {
                    match (x.strip_prefix("palette:"), x.strip_prefix("stream:")) {
                        (Some(p), _) => format!("from {}", nice(p)),
                        (_, Some(slot)) => format!("follows your stream's {slot} color"),
                        _ => x.to_string(),
                    }
                } else if let (Some(f), false) = (v.as_f64(), matches!(v, Value::Int(_))) {
                    pct(f as f32)
                } else {
                    text(v)
                };
                let txt = if value.is_empty() { attr_label(a) } else { format!("{} {value}", attr_label(a)) };
                ui.label(RichText::new(txt).size(type_scale::SMALL + 0.5).color(t.text_dim));
                ui.add_space(spacing::S);
            }
        });
    }
}

fn editor(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, current: Option<&Value>) {
    let (m, t) = (cx.m, cx.t);
    let holding = m.b("lights.programmer.active");
    // 1. which look
    match current {
        Some(pal) => {
            let kind = s(pal, "kind");
            let sub = format!("{} look · {}", kind_label(kind), used_by_text(&strs(pal, "used_by")));
            let mut try_it = false;
            widgets::titled(
                ui,
                t,
                &look_label(pal),
                &sub,
                |ui| {
                    try_it = widgets::button_ex(ui, t, Some(icon::PLAY), "Try it on the lights", Kind::Secondary, Size::Small, 0.0, true)
                        .on_hover_text("Puts this look on the chosen lights (all lights if none are chosen) so you can change it")
                        .clicked();
                },
                |ui| {
                    ui.set_width(ui.available_width());
                    look_summary(cx, ui, pal);
                },
            );
            if try_it {
                let all: Vec<String>;
                let targets = if cx.heads.is_empty() {
                    all = cx.rig.heads.iter().map(|h| h.id.clone()).collect();
                    &all[..]
                } else {
                    cx.heads
                };
                out.extend(look_ops(cx.selection, palette_values(pal, cx.rig, targets)));
            }
        }
        None => {
            widgets::titled(
                ui,
                t,
                "New look",
                "Set up the lights below, give the look a name, and save it.",
                |_| {},
                |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        ui.label(RichText::new("Name").color(t.text_dim));
                        ui.add(se_ui_kit::widgets::field(&mut st.new_name).desired_width(260.0).hint_text("e.g. Deep blue"));
                        ui.add_space(spacing::L);
                        ui.label(RichText::new("Keeps").color(t.text_dim));
                        widgets::segmented(ui, t, &mut st.new_kind, &LOOK_KIND_LABELS);
                    });
                },
            );
        }
    }
    ui.add_space(spacing::L);

    // 2. which lights
    let hl = highlight_on(m);
    let (mut show, mut locate, mut clear) = (false, false, false);
    widgets::titled(
        ui,
        t,
        "Lights to change",
        "Choose the lights you're changing. Shift-click to choose more than one.",
        |ui| {
            clear = widgets::button_ex(ui, t, Some(icon::UNDO), "Start over", Kind::Secondary, Size::Small, 0.0, !cx.selection.is_empty() || holding)
                .on_hover_text("Forget the chosen lights and your changes")
                .clicked();
            locate = widgets::button_ex(ui, t, Some(icon::LIGHT), "Plain white", Kind::Secondary, Size::Small, 0.0, !cx.heads.is_empty())
                .on_hover_text("Puts the chosen lights at full, plain white, pointing straight ahead — a clean start")
                .clicked();
            show = widgets::button_ex(
                ui,
                t,
                Some(icon::EYE),
                if hl { "Stop showing" } else { "Show me" },
                Kind::Secondary,
                Size::Small,
                0.0,
                hl || !cx.heads.is_empty(),
            )
            .on_hover_text("Flashes the chosen lights to bright white so you can spot them")
            .clicked();
        },
        |ui| {
            ui.set_width(ui.available_width());
            which_lights(cx, ui, out);
        },
    );
    if show {
        out.push(act("lights.programmer.highlight", Value::map().with("on", !hl)));
    }
    if locate {
        out.push(act("lights.programmer.locate", Value::Null));
    }
    if clear {
        out.push(act("lights.programmer.clear", Value::Null));
    }
    ui.add_space(spacing::L);

    // 3. adjust
    widgets::titled(
        ui,
        t,
        "Adjust",
        "Changes show on your lights right away.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if cx.heads.is_empty() {
                widgets::empty_state(ui, t, icon::HAND, "Choose lights first", "Pick All lights, a group or single lights above, then adjust them here.", None);
            } else {
                let prog = m.q("lights.programmer");
                attribute_controls(cx, st, ui, out, prog);
            }
        },
    );
    ui.add_space(spacing::L);

    // 4. save
    save_card(cx, st, ui, out, current, holding);
    ui.add_space(spacing::L);
}

fn save_card(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, current: Option<&Value>, holding: bool) {
    let (m, t) = (cx.m, cx.t);
    widgets::panel(ui, t, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            match current {
                Some(pal) => {
                    let (name, kind) = (s(pal, "name"), s(pal, "kind"));
                    if widgets::button_ex(ui, t, Some(icon::SAVE), "Save look", Kind::Primary, Size::Medium, 0.0, holding).clicked() {
                        let mut args = Value::map().with("palette", name);
                        if !kind.is_empty() {
                            args = args.with("kind", kind);
                        }
                        out.push(act("lights.programmer.store", args));
                    }
                    let used = strs(pal, "used_by").len();
                    let mut hint = if !holding {
                        "Change something above first.".to_string()
                    } else if used == 1 {
                        format!("Saves the chosen lights into “{}”. The cue list step using it changes too.", look_label(pal))
                    } else if used > 1 {
                        format!("Saves the chosen lights into “{}”. The {used} cue list steps using it change too.", look_label(pal))
                    } else {
                        format!("Saves the chosen lights into “{}”.", look_label(pal))
                    };
                    if let Some(k) = kind_keeps(kind) {
                        hint.push(' ');
                        hint.push_str(k);
                    }
                    widgets::hint(ui, t, &hint);
                }
                None => {
                    let name = slug(&st.new_name);
                    let kind = LOOK_KINDS[st.new_kind.min(LOOK_KINDS.len() - 1)];
                    if widgets::button_ex(ui, t, Some(icon::SAVE), "Save new look", Kind::Primary, Size::Medium, 0.0, holding && !name.is_empty()).clicked() {
                        out.push(act("lights.programmer.store", Value::map().with("palette", name.as_str()).with("kind", kind)));
                        st.look = Some(name.clone());
                        st.new_look = false;
                    }
                    widgets::hint(
                        ui,
                        t,
                        if !holding {
                            "Change something above first."
                        } else if name.is_empty() {
                            "Give the look a name first."
                        } else {
                            "Saves the chosen lights as a new look."
                        },
                    );
                }
            }
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if widgets::button_ex(ui, t, Some(icon::UNDO), "Let go", Kind::Secondary, Size::Medium, 0.0, holding)
                    .on_hover_text("Stop your changes. The lights go back to your cue lists and effects.")
                    .clicked()
                {
                    out.push(act("lights.programmer.release", Value::Null));
                }
            });
        });
        ui.add_space(spacing::S);
        widgets::details(ui, t, "lights_more_save", "More ways to save", |ui| {
            ui.add_space(spacing::XS);
            caption(ui, t, "Put it in a cue list");
            let lists: Vec<(String, String)> = m
                .q_list("lights.cuelists")
                .iter()
                .map(|c| (s(c, "name").to_string(), if s(c, "label").is_empty() { nice(s(c, "name")) } else { s(c, "label").to_string() }))
                .filter(|(n, _)| !n.is_empty())
                .collect();
            if !lists.is_empty() && !lists.iter().any(|(n, _)| *n == st.store_cuelist) {
                st.store_cuelist = lists[0].0.clone();
            }
            ui.horizontal(|ui| {
                if lists.is_empty() {
                    ui.add(se_ui_kit::widgets::field(&mut st.store_cuelist).desired_width(200.0).hint_text("New cue list name"));
                } else {
                    let shown = lists.iter().find(|(n, _)| *n == st.store_cuelist).map(|(_, l)| l.clone()).unwrap_or_default();
                    egui::ComboBox::from_id_salt("lights_store_cuelist").selected_text(shown).width(200.0).show_ui(ui, |ui| {
                        for (n, l) in &lists {
                            ui.selectable_value(&mut st.store_cuelist, n.clone(), l);
                        }
                    });
                }
                ui.label(RichText::new("step").color(t.text_dim));
                ui.add(se_ui_kit::widgets::field(&mut st.store_cue).desired_width(60.0).hint_text("1"));
                // existing lists are stored under their own name; a typed new one is made valid
                let list = if lists.is_empty() { slug(&st.store_cuelist) } else { st.store_cuelist.clone() };
                let cue = st.store_cue.trim().to_string();
                if widgets::button_ex(
                    ui,
                    t,
                    Some(icon::LIST),
                    "Save to cue list",
                    Kind::Secondary,
                    Size::Small,
                    0.0,
                    holding && !list.is_empty() && !cue.is_empty(),
                )
                .clicked()
                {
                    out.push(act("lights.programmer.store", Value::map().with("cuelist", list.as_str()).with("cue", cue.as_str())));
                }
            });
            widgets::hint(ui, t, "Replaces what that step changes with the chosen lights. A new step number adds a step.");
            ui.add_space(spacing::M);
            caption(ui, t, "Make it a quick effect");
            ui.horizontal(|ui| {
                ui.add(se_ui_kit::widgets::field(&mut st.preset_name).desired_width(200.0).hint_text("Quick effect name"));
                let name = slug(&st.preset_name);
                if widgets::button_ex(ui, t, Some(icon::BOLT), "Save quick effect", Kind::Secondary, Size::Small, 0.0, holding && !name.is_empty()).clicked() {
                    out.push(act("lights.programmer.store", Value::map().with("preset", name.as_str())));
                }
            });
            widgets::hint(ui, t, "Quick effects show up on the Live page, one tap away.");
        });
    });
}

/// Current value shown for a programmer attribute: local edit → programmer value → live state.
fn attr_value<'a>(m: &'a Model, st: &'a LightsState, prog: Option<&'a Value>, head: &str, attr: &str, now: Instant) -> Option<&'a Value> {
    let addr = format!("lights.{head}.{attr}");
    st.edited(attr, now)
        .or_else(|| prog.and_then(|p| p.get_path("values")).and_then(Value::as_map).and_then(|v| v.get(&addr)))
        .or_else(|| m.get(&addr))
        .map(plain)
}

fn attribute_controls(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, prog: Option<&Value>) {
    let (m, t, rig, heads, now) = (cx.m, cx.t, cx.rig, cx.heads, cx.now);
    let mut attrs: Vec<String> = Vec::new();
    for h in heads.iter().filter_map(|h| rig.head(h)) {
        for a in &h.attrs {
            if !attrs.contains(a) {
                attrs.push(a.clone());
            }
        }
    }
    let has = |a: &str| attrs.iter().any(|x| x == a);
    let first = heads[0].as_str();
    let f32_of = |st: &LightsState, a: &str, d: f32| attr_value(m, st, prog, first, a, now).and_then(Value::as_f32).unwrap_or(d);
    ui.horizontal_top(|ui| {
        // brightness: every head has one (virtual dimmer when unpatched)
        ui.vertical(|ui| {
            caption(ui, t, "Brightness");
            let mut v = f32_of(st, "intensity", 0.0);
            ui.horizontal(|ui| {
                if widgets::fader(ui, t, Vec2::new(40.0, 176.0), &mut v, false).changed() {
                    st.edit("intensity", Value::from(v), now);
                    prog_set(out, "intensity", Value::from(v));
                }
                ui.vertical(|ui| {
                    ui.label(RichText::new(pct(v)).font(font_mono(type_scale::LARGE)).color(t.fg));
                    ui.add_space(spacing::S);
                    for (l, x) in [("Full", 1.0f32), ("Half", 0.5), ("Off", 0.0)] {
                        if widgets::button_ex(ui, t, None, l, Kind::Secondary, Size::Small, 56.0, true).clicked() {
                            st.edit("intensity", Value::from(x), now);
                            prog_set(out, "intensity", Value::from(x));
                        }
                    }
                });
            });
        });
        if has("color") {
            ui.add_space(spacing::XL);
            ui.vertical(|ui| {
                caption(ui, t, "Color");
                let cur = attr_value(m, st, prog, first, "color", now).and_then(color_of).unwrap_or(Color32::WHITE);
                let mut hsva = st.hsva.filter(|(_, at)| now.saturating_duration_since(*at) < EDIT_HOLD).map(|(h, _)| h).unwrap_or_else(|| Hsva::from(cur));
                if color_field(ui, t, Vec2::new(236.0, 132.0), &mut hsva).on_hover_text("Drag to pick a color: across for the color, up for stronger").changed()
                {
                    let c = Color32::from(hsva);
                    st.hsva = Some((hsva, now));
                    st.edit("color", Value::from(hex_of(c)), now);
                    prog_set(out, "color", Value::from(hex_of(c)));
                }
                ui.add_space(spacing::S);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for (name, c) in LIGHT_COLORS {
                        let (r, resp) = ui.allocate_exact_size(Vec2::splat(20.0), Sense::click());
                        let sel = c == cur;
                        let ring = if sel {
                            t.fg
                        } else if resp.hovered() {
                            t.accent
                        } else {
                            t.border
                        };
                        ui.painter().circle(r.center(), 9.5, c, Stroke::new(if sel || resp.hovered() { 2.0 } else { 1.0 }, ring));
                        if resp.on_hover_cursor(egui::CursorIcon::PointingHand).on_hover_text(name).clicked() {
                            st.hsva = None;
                            st.edit("color", Value::from(hex_of(c)), now);
                            prog_set(out, "color", Value::from(hex_of(c)));
                        }
                    }
                });
            });
        }
        if has("pan") || has("tilt") {
            ui.add_space(spacing::XL);
            ui.vertical(|ui| {
                caption(ui, t, "Where it points");
                let before = [f32_of(st, "pan", 0.5), f32_of(st, "tilt", 0.5)];
                let mut v = before;
                if xy_pad(ui, t, 160.0, &mut v).on_hover_text("Drag to point the moving lights").changed() {
                    for (i, a) in ["pan", "tilt"].into_iter().enumerate() {
                        if has(a) && (v[i] - before[i]).abs() > f32::EPSILON {
                            st.edit(a, Value::from(v[i]), now);
                            prog_set(out, a, Value::from(v[i]));
                        }
                    }
                }
                ui.label(RichText::new(format!("Turn {} · Tilt {}", pct(v[0]), pct(v[1]))).size(type_scale::SMALL + 0.5).color(t.text_dim));
                if widgets::button_ex(ui, t, Some(CROSSHAIRS), "Point straight", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    for a in ["pan", "tilt"] {
                        if has(a) {
                            st.edit(a, Value::from(0.5f32), now);
                            prog_set(out, a, Value::from(0.5f32));
                        }
                    }
                }
            });
        }
        let unit: Vec<&str> = UNIT_ATTRS.iter().copied().filter(|a| has(a)).collect();
        let ints: Vec<&String> =
            attrs.iter().filter(|a| !UNIT_ATTRS.contains(&a.as_str()) && !["intensity", "color", "pan", "tilt"].contains(&a.as_str())).collect();
        if !unit.is_empty() || !ints.is_empty() {
            ui.add_space(spacing::XL);
            ui.vertical(|ui| {
                caption(ui, t, "More");
                let w = ui.available_width().clamp(220.0, 340.0);
                for a in unit {
                    let mut v = f32_of(st, a, 0.0);
                    if level_row(ui, t, &attr_label(a), 96.0, &mut v, w).changed() {
                        st.edit(a, Value::from(v), now);
                        prog_set(out, a, Value::from(v));
                    }
                }
                for a in ints {
                    ui.horizontal(|ui| {
                        let max = if a == "gobo" { 63 } else { 255 };
                        let mut v = attr_value(m, st, prog, first, a, now).and_then(Value::as_i64).unwrap_or(0).clamp(0, max);
                        fixed_text(ui, &attr_label(a), font(type_scale::SMALL + 1.0), t.text_dim, 96.0);
                        if ui.add(egui::DragValue::new(&mut v).range(0..=max)).changed() {
                            st.edit(a, Value::from(v), now);
                            prog_set(out, a, Value::from(v));
                        }
                    });
                }
            });
        }
    });
}

// ---------------------------------------------------------------- stage

/// A head as drawn on the stage: rig layout merged with live output.
#[derive(Clone, Debug)]
struct StageHead {
    id: String,
    fixture: String,
    pos: [f32; 2],
    rotation: f32,
    kind: String,
    beam: f32,
    moving: bool,
    intensity: f32,
    color: [f32; 3],
    pan: f32,
    tilt: f32,
    limited: bool,
}

fn stage_heads(rig: &Rig, output: Option<&Value>) -> Vec<StageHead> {
    let live: HashMap<&str, &Value> =
        output.map(|o| list(o, "heads")).unwrap_or(&[]).iter().map(|h| (s(h, "id"), h)).filter(|(id, _)| !id.is_empty()).collect();
    let from_live = |base: StageHead, o: Option<&&Value>| -> StageHead {
        let Some(o) = o else { return base };
        let f = |k: &str, d: f32| num(o, k).map(|x| x as f32).unwrap_or(d);
        let mut c = o.get_path("color").and_then(Value::as_color).map(|c| [c[0], c[1], c[2]]).unwrap_or([1.0; 3]);
        if c.iter().any(|x| *x > 1.0) {
            c = c.map(|x| x / 255.0);
        }
        StageHead {
            pos: [f("x", base.pos[0]), f("y", base.pos[1])],
            rotation: f("rotation", base.rotation),
            beam: f("beam", base.beam),
            intensity: f("intensity", 0.0).clamp(0.0, 1.0),
            color: c,
            pan: f("pan", 0.5),
            tilt: f("tilt", 0.5),
            limited: o.get_path("limited").is_some_and(Value::truthy),
            ..base
        }
    };
    let mut heads: Vec<StageHead> = rig
        .heads
        .iter()
        .map(|h| {
            let base = StageHead {
                id: h.id.clone(),
                fixture: if h.fixture.is_empty() { h.id.clone() } else { h.fixture.clone() },
                pos: h.position,
                rotation: h.rotation,
                kind: h.kind.clone(),
                beam: h.beam,
                moving: h.moving,
                intensity: 0.0,
                color: [1.0; 3],
                pan: 0.5,
                tilt: 0.5,
                limited: false,
            };
            from_live(base, live.get(h.id.as_str()))
        })
        .collect();
    // live heads the rig query has not delivered (yet)
    for (id, o) in &live {
        if rig.head(id).is_none() {
            let base = StageHead {
                id: id.to_string(),
                fixture: id.to_string(),
                pos: [0.5, 0.5],
                rotation: 0.0,
                kind: String::new(),
                beam: 25.0,
                moving: false,
                intensity: 0.0,
                color: [1.0; 3],
                pan: 0.5,
                tilt: 0.5,
                limited: false,
            };
            heads.push(from_live(base, Some(o)));
        }
    }
    heads
}

/// Stage floor for the available area (2:1, centred horizontally).
fn stage_rect(avail: Rect) -> Rect {
    let h = (avail.width() * 0.5).min(avail.height() - 24.0).max(120.0);
    let w = avail.width().min(h * 2.0);
    Rect::from_min_size(Pos2::new(avail.left() + (avail.width() - w) * 0.5, avail.top()), Vec2::new(w, h))
}

/// Stage coordinates (x 0 = stage left … 1 = stage right; y 0 = downstage … 1 = upstage) → screen.
fn to_screen(stage: Rect, p: [f32; 2]) -> Pos2 {
    let r = stage.shrink(28.0);
    Pos2::new(r.left() + r.width() * p[0].clamp(0.0, 1.0), r.bottom() - r.height() * p[1].clamp(0.0, 1.0))
}

/// Screen-space beam direction and reach (0..1) of a mover. Rotation 0° faces downstage
/// (screen down), 90° faces stage right; pan 0.5 = rotation, tilt 0.5 = straight down.
fn beam_dir(rotation_deg: f32, pan: f32, tilt: f32) -> (Vec2, f32) {
    let mut a = (rotation_deg + (pan - 0.5) * PAN_TRAVEL_DEG).to_radians();
    let off = tilt - 0.5;
    if off < 0.0 {
        a += std::f32::consts::PI;
    }
    (Vec2::new(a.sin(), a.cos()), 0.12 + 0.88 * (off.abs() * 2.0).min(1.0))
}

fn head_at(heads: &[StageHead], stage: Rect, p: Pos2) -> Option<&StageHead> {
    heads.iter().map(|h| (h, to_screen(stage, h.pos).distance(p))).filter(|(_, d)| *d <= 18.0).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(h, _)| h)
}

fn stage_tab(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    let t = cx.t;
    let heads = stage_heads(cx.rig, cx.output);
    if heads.is_empty() {
        let mut open = false;
        widgets::panel(ui, t, |ui| {
            ui.set_width(ui.available_width());
            open = widgets::empty_state(
                ui,
                t,
                icon::LIGHT,
                "No lights on the stage yet",
                "Once your lights are added they show up here, live.",
                Some("Open Setup"),
            );
        });
        if open {
            st.tab = Tab::Setup;
        }
        return;
    }
    let w = ui.available_width();
    let side = (w * 0.24).clamp(280.0, 380.0);
    let h = ui.available_height();
    columns(
        ui,
        w - side - spacing::L,
        &mut (st, out),
        |ui, (_, out)| {
            ui.set_max_height(h);
            widgets::panel(ui, t, |ui| {
                ui.set_width(ui.available_width());
                stage_floor(t, ui, out, cx.rig, &heads, cx.heads);
            });
        },
        |ui, (st, out)| stage_side(cx, st, ui, out, &heads),
    );
}

/// The stage seen from above: lights glow in their live color, movers throw a beam cone.
fn stage_floor(t: &Theme, ui: &mut Ui, out: &mut Vec<Op>, rig: &Rig, heads: &[StageHead], selected: &[String]) {
    let stage = stage_rect(ui.available_rect_before_wrap());
    let resp = ui.allocate_rect(stage, Sense::click());
    let p = ui.painter_at(stage);
    let floor = t.inset;
    p.rect_filled(stage, CornerRadius::same(radius::CONTROL), floor);
    let grid = Stroke::new(1.0, mix(floor, t.fg, 0.05));
    let inner = stage.shrink(28.0);
    for i in 0..=8 {
        let x = inner.left() + inner.width() * i as f32 / 8.0;
        p.line_segment([Pos2::new(x, inner.top()), Pos2::new(x, inner.bottom())], grid);
    }
    for i in 0..=4 {
        let y = inner.top() + inner.height() * i as f32 / 4.0;
        p.line_segment([Pos2::new(inner.left(), y), Pos2::new(inner.right(), y)], grid);
    }
    let edge = Stroke::new(2.0, mix(floor, t.fg, 0.18));
    p.line_segment([Pos2::new(inner.left(), stage.bottom() - 18.0), Pos2::new(inner.right(), stage.bottom() - 18.0)], edge);
    let small = font_medium(type_scale::SMALL - 1.0);
    p.text(stage.center_top() + Vec2::new(0.0, 8.0), Align2::CENTER_TOP, "Back of the stage", small.clone(), t.text_faint);
    p.text(stage.center_bottom() - Vec2::new(0.0, 3.0), Align2::CENTER_BOTTOM, "Front (camera side)", small, t.text_faint);
    // beams first, glyphs on top
    for h in heads.iter().filter(|h| h.moving) {
        let c = to_screen(stage, h.pos);
        let (dir, reach) = beam_dir(h.rotation, h.pan, h.tilt);
        let len = reach * stage.height() * 0.45;
        let half = (h.beam * 0.5).clamp(1.5, 45.0).to_radians();
        let base = dir.y.atan2(dir.x);
        let mut pts = vec![c];
        for i in 0..=8 {
            let a = base - half + 2.0 * half * i as f32 / 8.0;
            pts.push(c + Vec2::new(a.cos(), a.sin()) * len);
        }
        let col = rgb(h.color[0], h.color[1], h.color[2]);
        let alpha = 0.08 + 0.4 * h.intensity;
        p.add(Shape::convex_polygon(pts, col.gamma_multiply(alpha), Stroke::new(1.0, col.gamma_multiply(alpha + 0.1))));
    }
    for h in heads {
        let c = to_screen(stage, h.pos);
        let lit = rgb(h.color[0] * h.intensity, h.color[1] * h.intensity, h.color[2] * h.intensity);
        if h.intensity > 0.01 {
            let glow = rgb(h.color[0], h.color[1], h.color[2]);
            p.circle_filled(c, 10.0 + 22.0 * h.intensity, glow.gamma_multiply(0.10 * h.intensity));
            p.circle_filled(c, 10.0 + 10.0 * h.intensity, glow.gamma_multiply(0.25 * h.intensity));
        }
        let fill = if h.intensity > 0.01 { lit } else { mix(floor, t.fg, 0.1) };
        let is_sel = selected.contains(&h.id);
        let ring = if is_sel { Stroke::new(3.0, t.accent) } else { Stroke::new(1.5, mix(floor, t.fg, 0.35)) };
        if h.kind == "bar" {
            let r = Rect::from_center_size(c, Vec2::splat(16.0));
            p.rect_filled(r, CornerRadius::same(3), fill);
            p.rect_stroke(r, CornerRadius::same(3), ring, StrokeKind::Outside);
        } else {
            p.circle_filled(c, 9.0, fill);
            p.circle_stroke(c, 9.0, ring);
            if h.moving {
                p.circle_filled(c, 2.5, t.fg);
            }
        }
        if h.limited {
            p.circle_stroke(c, 15.0, Stroke::new(2.0, t.yellow));
        }
    }
    if rig.fixtures.len() <= 48 {
        for f in &rig.fixtures {
            let pos = rig.fixture_heads(f).first().and_then(|h| heads.iter().find(|x| &x.id == h)).map(|h| h.pos).unwrap_or(f.position);
            let sel = rig.fixture_heads(f).iter().any(|h| selected.contains(h));
            let g = one_line(ui, &f.name(), font_medium(type_scale::SMALL), if sel { t.fg } else { t.text_dim }, 180.0);
            let at = to_screen(stage, pos) + Vec2::new(-g.size().x / 2.0, 18.0);
            p.galley_with_override_text_color(at, g, if sel { t.fg } else { t.text_dim });
        }
    }
    if resp.clicked()
        && let Some(pos) = resp.interact_pointer_pos()
        && let Some(h) = head_at(heads, stage, pos)
    {
        let shift = ui.input(|i| i.modifiers.shift);
        out.push(select_op(&h.fixture, shift));
    }
    if let Some(pos) = resp.hover_pos()
        && let Some(h) = head_at(heads, stage, pos)
    {
        let mut tip = format!("{}\nBrightness {}", rig.target_name(&h.id), pct(h.intensity));
        if h.moving {
            tip.push_str(&format!("\nPointing: turn {} · tilt {}", pct(h.pan), pct(h.tilt)));
        }
        if h.limited {
            tip.push_str("\nSoftened by the flash limiter");
        }
        tip.push_str("\n\nClick to choose · Shift-click to add");
        resp.on_hover_text_at_pointer(tip);
    }
}

fn stage_side(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, heads: &[StageHead]) {
    let (m, t) = (cx.m, cx.t);
    let hl = highlight_on(m);
    widgets::titled(
        ui,
        t,
        "Chosen lights",
        "",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            widgets::hint(ui, t, "Click a light on the stage to choose it. Shift-click adds more.");
            ui.add_space(spacing::S);
            which_lights(cx, ui, out);
            ui.add_space(spacing::M);
            ui.horizontal_wrapped(|ui| {
                if widgets::button_ex(ui, t, Some(icon::EDIT), "Change these lights", Kind::Primary, Size::Small, 0.0, !cx.heads.is_empty()).clicked() {
                    st.tab = Tab::Edit;
                }
                if widgets::button_ex(
                    ui,
                    t,
                    Some(icon::EYE),
                    if hl { "Stop showing" } else { "Show me" },
                    Kind::Secondary,
                    Size::Small,
                    0.0,
                    hl || !cx.heads.is_empty(),
                )
                .on_hover_text("Flashes the chosen lights to bright white so you can spot them")
                .clicked()
                {
                    out.push(act("lights.programmer.highlight", Value::map().with("on", !hl)));
                }
            });
        },
    );
    ui.add_space(spacing::L);
    let movers = heads.iter().filter(|h| h.moving).count();
    let limited = heads.iter().filter(|h| h.limited).count();
    widgets::titled(
        ui,
        t,
        "On stage",
        "",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            widgets::fact(ui, t, "Lights", &heads.len().to_string());
            widgets::fact(ui, t, "Moving lights", &movers.to_string());
            widgets::fact(ui, t, "Chosen", &cx.heads.len().to_string());
            if limited > 0 {
                ui.add_space(spacing::S);
                widgets::badge(ui, t, &format!("{limited} softened by the flash limiter"), t.yellow);
            }
            if cx.output.is_none() {
                ui.add_space(spacing::S);
                widgets::hint(ui, t, "Waiting for the live picture of your lights…");
            }
            ui.add_space(spacing::M);
            let row = |ui: &mut Ui, draw: &dyn Fn(&egui::Painter, Pos2), label: &str| {
                ui.horizontal(|ui| {
                    let (r, _) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::hover());
                    draw(ui.painter(), r.center());
                    ui.add(egui::Label::new(RichText::new(label).size(type_scale::SMALL + 0.5).color(t.text_dim)).wrap());
                });
            };
            let ring = Stroke::new(1.5, t.text_dim);
            row(
                ui,
                &|p, c| {
                    p.circle_stroke(c, 7.0, ring);
                },
                "A light",
            );
            row(
                ui,
                &|p, c| {
                    p.circle_stroke(c, 7.0, ring);
                    p.circle_filled(c, 2.0, t.fg);
                },
                "A moving light (the cone shows where it points)",
            );
            row(
                ui,
                &|p, c| {
                    p.rect_stroke(Rect::from_center_size(c, Vec2::splat(12.0)), CornerRadius::same(2), ring, StrokeKind::Inside);
                },
                "A light bar",
            );
            row(
                ui,
                &|p, c| {
                    p.circle_stroke(c, 8.0, Stroke::new(2.5, t.accent));
                },
                "Chosen",
            );
        },
    );
}

// ---------------------------------------------------------------- setup

fn setup_tab(cx: &Cx, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    let (t, rig) = (cx.t, cx.rig);
    if !rig.errors.is_empty() {
        widgets::callout(
            ui,
            t,
            widgets::Tone::Danger,
            icon::WARN,
            "Some lighting files need fixing",
            "A few lights, looks or cue lists couldn't load. Everything else keeps working; fixes load by themselves.",
            None,
        );
        widgets::details(ui, t, "lights_errors", "Details", |ui| {
            for e in &rig.errors {
                ui.add(egui::Label::new(RichText::new(e).size(type_scale::SMALL + 0.5).color(t.text_dim)).wrap());
            }
        });
        ui.add_space(spacing::L);
    }
    let w = ui.available_width();
    columns(
        ui,
        ((w - spacing::L) * 0.5).floor(),
        out,
        |ui, out| {
            fixtures_card(cx, ui, out);
            ui.add_space(spacing::L);
            groups_card(cx, ui, out);
        },
        |ui, out| {
            connection_card(cx, ui);
            ui.add_space(spacing::L);
            rdm_card(cx, ui, out);
        },
    );
    ui.add_space(spacing::L);
    monitor_card(cx, st, ui);
    ui.add_space(spacing::L);
}

/// "Channel 1–3" (with the line when there is more than one).
fn channel_text(f: &Fixture, many_lines: bool) -> String {
    let end = f.address + f.footprint.saturating_sub(1);
    let ch = if end > f.address { format!("Channel {}–{end}", f.address) } else { format!("Channel {}", f.address) };
    if many_lines { format!("Line {} · {ch}", f.universe) } else { ch }
}

fn fixtures_card(cx: &Cx, ui: &mut Ui, out: &mut Vec<Op>) {
    let (t, rig) = (cx.t, cx.rig);
    let many_lines = rig.fixtures.iter().any(|f| f.universe != rig.fixtures[0].universe);
    let sub = match rig.fixtures.len() {
        0 => String::new(),
        1 => "1 light · click one to choose it".into(),
        n => format!("{n} lights · click one to choose it"),
    };
    widgets::titled(
        ui,
        t,
        "Your lights",
        &sub,
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if rig.fixtures.is_empty() {
                widgets::empty_state(
                    ui,
                    t,
                    icon::LIGHT,
                    "No lights added yet",
                    "Lights are added in your lighting setup file. The lighting guide shows how.",
                    None,
                );
                return;
            }
            for f in &rig.fixtures {
                let kind = if f.profile_name.is_empty() { nice(&f.profile) } else { f.profile_name.clone() };
                let parts = rig.fixture_heads(f).len();
                let sub = if parts > 1 { format!("{kind} · {parts} parts") } else { kind };
                let end = f.address + f.footprint.saturating_sub(1);
                let chosen = cx.selection.contains(&f.id);
                let resp = widgets::list_row(ui, t, icon::LIGHT, &f.name(), &sub, &channel_text(f, many_lines), chosen).on_hover_text(format!(
                    "Click to choose this light\n\nDetails: DMX universe {} · address {}–{end} ({} channels)\nProfile {} · mode {} · id {}",
                    f.universe, f.address, f.footprint, f.profile, f.mode, f.id
                ));
                if resp.clicked() {
                    out.push(select_op(&f.id, ui.input(|i| i.modifiers.shift)));
                }
            }
        },
    );
}

fn groups_card(cx: &Cx, ui: &mut Ui, out: &mut Vec<Op>) {
    let (t, rig) = (cx.t, cx.rig);
    widgets::titled(
        ui,
        t,
        "Groups",
        "Lights you change together. “All lights” is always there.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if rig.groups.is_empty() {
                widgets::hint(ui, t, "No groups yet. Groups are made in your lighting setup file.");
                return;
            }
            for (g, members) in &rig.groups {
                let mut names: Vec<String> = Vec::new();
                for h in members {
                    let n = rig.head(h).and_then(|h| rig.fixture(&h.fixture)).map(Fixture::name).unwrap_or_else(|| nice(h));
                    if !names.contains(&n) {
                        names.push(n);
                    }
                }
                let count = if members.len() == 1 { "1 light".to_string() } else { format!("{} lights", members.len()) };
                let resp = widgets::list_row(ui, t, icon::LAYERS, &rig.target_name(g), &names.join(", "), &count, cx.selection.contains(g))
                    .on_hover_text("Click to choose this group");
                if resp.clicked() {
                    out.push(select_op(g, ui.input(|i| i.modifiers.shift)));
                }
            }
        },
    );
}

fn output_kind(kind: &str) -> String {
    match kind {
        "enttec_pro" | "enttec" | "enttec_open" => "USB lighting box (ENTTEC)".into(),
        "sacn" => "Network lights (sACN)".into(),
        "artnet" => "Network lights (Art-Net)".into(),
        other => nice(other),
    }
}

fn connection_card(cx: &Cx, ui: &mut Ui) {
    let (m, t, rig) = (cx.m, cx.t, cx.rig);
    widgets::titled(
        ui,
        t,
        "Connection",
        "How Stream Engine reaches your lights.",
        |_| {},
        |ui| {
            ui.set_width(ui.available_width());
            if rig.outputs.is_empty() {
                widgets::hint(ui, t, "No way to reach the lights is set up yet.");
            }
            for o in &rig.outputs {
                let enabled = o.get_path("enabled").is_none_or(Value::truthy);
                let status = s(o, "status");
                let (word, color) = if !enabled {
                    ("Off", t.text_dim)
                } else {
                    match health_led(status) {
                        LedState::Healthy => ("Working", t.green),
                        LedState::Armed => ("Needs a look", t.yellow),
                        LedState::Error => ("Not connected", t.bright_red),
                        _ => ("Needs a look", t.yellow),
                    }
                };
                ui.horizontal(|ui| {
                    ui.set_min_height(34.0);
                    ui.label(RichText::new(icon::DEVICE).size(type_scale::BODY + 1.0).color(if enabled { t.text_dim } else { t.text_faint }));
                    ui.label(RichText::new(output_kind(s(o, "kind"))).font(font_medium(type_scale::BODY)).color(if enabled { t.fg } else { t.text_dim }));
                    ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                        widgets::badge(ui, t, word, color);
                    });
                });
            }
            ui.add_space(spacing::S);
            let output = cx.output;
            let fps = output.and_then(|o| num(o, "fps")).or_else(|| m.get("lights.output.fps").and_then(Value::as_f64));
            widgets::fact(ui, t, "Updates per second", &fps.map(|f| format!("{f:.0}")).unwrap_or_else(|| "—".into()));
            let limiter = output.and_then(|o| o.get_path("limiter"));
            let softened = limiter.and_then(|l| num(l, "suppressed")).unwrap_or(0.0);
            widgets::fact(
                ui,
                t,
                "Flash limiter",
                if limiter.is_some_and(|l| l.get_path("active").is_some_and(Value::truthy)) { "Working · softening flashes now" } else { "Working" },
            );
            ui.add_space(spacing::XS);
            widgets::details(ui, t, "lights_connection", "Details", |ui| {
                if let Some(h) = m.get("health.dmx") {
                    let d = s(h, "detail");
                    if !d.is_empty() {
                        widgets::hint(ui, t, d);
                    }
                }
                let p99 = output.and_then(|o| num(o, "jitter.p99_ms")).or_else(|| m.get("lights.output.jitter_ms").and_then(Value::as_f64));
                if let Some(j) = p99 {
                    widgets::fact(ui, t, "Timing wobble (p99)", &format!("{j:.2} ms"));
                }
                if let Some(f) = output.and_then(|o| num(o, "frames")) {
                    widgets::fact(ui, t, "Frames sent", &format!("{f:.0}"));
                }
                widgets::fact(ui, t, "Flashes softened", &format!("{softened:.0}"));
                widgets::fact(ui, t, "Strobes slowed", &format!("{:.0}", limiter.and_then(|l| num(l, "strobe_capped")).unwrap_or(0.0)));
                for o in &rig.outputs {
                    let unis: Vec<String> = list(o, "universes").iter().map(text).collect();
                    ui.add_space(spacing::XS);
                    ui.label(RichText::new(format!("{} ({}) — {}", s(o, "id"), s(o, "kind"), s(o, "status"))).size(type_scale::SMALL + 0.5).color(t.fg));
                    let detail = s(o, "detail");
                    ui.label(
                        RichText::new(format!(
                            "{}universes {} · frames {} · errors {}",
                            if detail.is_empty() { String::new() } else { format!("{detail} · ") },
                            if unis.is_empty() { "—".into() } else { unis.join(", ") },
                            num(o, "frames").unwrap_or(0.0),
                            num(o, "errors").unwrap_or(0.0)
                        ))
                        .size(type_scale::SMALL + 0.5)
                        .color(t.text_dim),
                    );
                }
            });
        },
    );
}

/// Render a DeviceInfo uid (number → `MMMM:DDDDDDDD`, strings verbatim).
fn uid_text(v: Option<&Value>) -> String {
    match v {
        Some(Value::Int(i)) => format!("{:04X}:{:08X}", (*i >> 32) & 0xFFFF, *i & 0xFFFF_FFFF),
        Some(Value::Str(s)) => s.clone(),
        Some(other) => other.to_string(),
        None => "—".into(),
    }
}

fn rdm_card(cx: &Cx, ui: &mut Ui, out: &mut Vec<Op>) {
    let (m, t) = (cx.m, cx.t);
    let rdm = m.q("lights.rdm");
    let mut search = false;
    widgets::titled(
        ui,
        t,
        "Find lights on the cable",
        "Asks connected lights to introduce themselves (needs lights and a box that support it).",
        |ui| {
            search = widgets::button_ex(ui, t, Some(icon::SEARCH), "Search", Kind::Secondary, Size::Small, 0.0, true).clicked();
        },
        |ui| {
            ui.set_width(ui.available_width());
            let Some(r) = rdm else {
                widgets::hint(ui, t, "Not searched yet.");
                return;
            };
            let devices = list(r, "devices");
            let status = s(r, "status");
            let summary = if status == "not run yet" {
                "Not searched yet.".to_string()
            } else if status.starts_with("discovering") {
                "Searching…".to_string()
            } else if status.starts_with("discovery finished") {
                match devices.len() {
                    0 => "Searched — no lights answered.".to_string(),
                    1 => "Found 1 light.".to_string(),
                    n => format!("Found {n} lights."),
                }
            } else if status.starts_with("RDM unavailable") {
                "Your USB box can't search for lights (it needs different firmware). Add lights by hand instead.".to_string()
            } else if status.contains("not connected") {
                "The USB lighting box isn't plugged in.".to_string()
            } else if status.contains("configured") {
                "Searching needs a USB lighting box (ENTTEC) set up first.".to_string()
            } else {
                nice(status)
            };
            ui.add(egui::Label::new(RichText::new(summary).color(t.fg)).wrap());
            for d in devices {
                let o = |k: &str| d.get_path(k).filter(|v| !v.is_null()).map(text);
                let title = [o("manufacturer"), o("model")].into_iter().flatten().collect::<Vec<_>>().join(" ");
                let title = o("label").unwrap_or(if title.is_empty() { "Unknown light".into() } else { title });
                let sub = format!(
                    "Channel {} · {} channels · setting {} of {}",
                    o("start_address").unwrap_or_else(|| "—".into()),
                    o("footprint").unwrap_or_else(|| "—".into()),
                    o("personality").unwrap_or_else(|| "—".into()),
                    o("personalities").unwrap_or_else(|| "—".into())
                );
                widgets::list_row(ui, t, icon::LIGHT, &title, &sub, "", false).on_hover_text(format!("Details: RDM id {}", uid_text(d.get_path("uid"))));
            }
            widgets::details(ui, t, "lights_rdm", "Details", |ui| {
                let opt = |k: &str| r.get_path(k).filter(|v| !v.is_null()).map(text).unwrap_or_else(|| "—".into());
                widgets::fact(ui, t, "Box firmware", &opt("firmware"));
                widgets::fact(ui, t, "Box serial", &opt("serial"));
                ui.add(egui::Label::new(RichText::new(status).size(type_scale::SMALL + 0.5).color(t.text_dim)).wrap());
            });
        },
    );
    if search {
        out.push(act("lights.rdm.discover", Value::Null));
    }
}

fn tints(t: &Theme) -> [Color32; 7] {
    [t.accent, t.cyan, t.green, t.yellow, t.magenta, t.blue, t.orange]
}

/// Fixture index owning each channel (0-based) of `universe`, clipped to 512.
fn channel_owners(rig: &Rig, universe: u16) -> [Option<u16>; 512] {
    let mut owners = [None; 512];
    for (i, f) in rig.fixtures.iter().enumerate().filter(|(_, f)| f.universe == universe && f.address >= 1) {
        let start = (f.address - 1) as usize;
        let end = (start + f.footprint as usize).min(512);
        for o in owners.iter_mut().take(end).skip(start) {
            *o = Some(i as u16);
        }
    }
    owners
}

fn universes(rig: &Rig, output: Option<&Value>) -> Vec<u16> {
    let mut u: Vec<u16> =
        output.and_then(|o| o.get_path("universes")).and_then(Value::as_map).map(|m| m.keys().filter_map(|k| k.parse().ok()).collect()).unwrap_or_default();
    u.extend(rig.fixtures.iter().map(|f| f.universe));
    u.sort_unstable();
    u.dedup();
    u
}

fn monitor_card(cx: &Cx, st: &mut LightsState, ui: &mut Ui) {
    let (t, rig, output) = (cx.t, cx.rig, cx.output);
    let us = universes(rig, output);
    let universe = st.universe.filter(|u| us.contains(u)).or_else(|| us.first().copied());
    st.universe = universe;
    let mut pick = us.iter().position(|u| Some(*u) == universe).unwrap_or(0);
    let labels: Vec<String> = us.iter().map(|u| format!("Line {u}")).collect();
    widgets::titled(
        ui,
        t,
        "Output monitor",
        "What every channel sends right now. Taller bars are brighter.",
        |ui| {
            if us.len() > 1 {
                let l: Vec<&str> = labels.iter().map(String::as_str).collect();
                if widgets::segmented(ui, t, &mut pick, &l) {
                    st.universe = us.get(pick).copied();
                }
            }
        },
        |ui| {
            ui.set_width(ui.available_width());
            let Some(universe) = universe else {
                widgets::hint(ui, t, "Nothing is being sent yet.");
                return;
            };
            if output.is_none() {
                widgets::hint(ui, t, "Waiting for live output…");
            }
            let data = output.and_then(|o| o.get_path("universes")).and_then(|m| m.get_path(&universe.to_string())).and_then(Value::as_list).unwrap_or(&[]);
            let value = |ch: usize| data.get(ch).and_then(Value::as_i64).unwrap_or(0).clamp(0, 255) as u8;
            let owners = channel_owners(rig, universe);
            let tint = tints(t);
            let avail = ui.available_width().max(320.0);
            let cell = Vec2::new((avail / 32.0).floor(), 24.0);
            let (rect, resp) = ui.allocate_exact_size(Vec2::new(cell.x * 32.0, cell.y * 16.0), Sense::hover());
            let p = ui.painter_at(rect);
            let digits = font_mono(if cell.x >= 30.0 { 10.5 } else { 8.5 });
            let well = t.inset;
            for ch in 0..512 {
                let (col, row) = (ch % 32, ch / 32);
                let r = Rect::from_min_size(rect.min + Vec2::new(col as f32 * cell.x, row as f32 * cell.y), cell).shrink(1.0);
                let v = value(ch);
                let owner = owners[ch];
                let fix_tint = owner.map(|i| tint[i as usize % tint.len()]);
                p.rect_filled(r, CornerRadius::same(3), fix_tint.map(|c| mix(well, c, 0.2)).unwrap_or(well));
                if v > 0 {
                    let h = r.height() * v as f32 / 255.0;
                    p.rect_filled(
                        Rect::from_min_max(Pos2::new(r.left(), r.bottom() - h), r.right_bottom()),
                        CornerRadius::same(3),
                        mix(well, fix_tint.unwrap_or(t.fg), 0.35 + 0.5 * v as f32 / 255.0),
                    );
                }
                if let Some(i) = owner
                    && (ch == 0 || owners[ch - 1] != Some(i))
                {
                    p.line_segment([r.left_top(), r.left_bottom()], Stroke::new(2.0, tint[i as usize % tint.len()]));
                }
                if cell.x >= 18.0 {
                    p.text(r.center(), Align2::CENTER_CENTER, v.to_string(), digits.clone(), if v > 140 { t.fg_bright } else { t.text_faint });
                }
            }
            if let Some(pos) = resp.hover_pos() {
                let col = ((pos.x - rect.left()) / cell.x).floor() as i64;
                let row = ((pos.y - rect.top()) / cell.y).floor() as i64;
                if (0..32).contains(&col) && (0..16).contains(&row) {
                    let ch = (row * 32 + col) as usize;
                    let who = owners[ch].and_then(|i| rig.fixtures.get(i as usize)).map(|f| {
                        let off = ch + 1 - f.address as usize;
                        let role = rig
                            .channels
                            .get(&f.profile)
                            .and_then(|modes| modes.get(&f.mode))
                            .and_then(|c| c.get(off))
                            .map(|r| format!(" · {r}"))
                            .unwrap_or_default();
                        format!("{}{role}", f.name())
                    });
                    resp.on_hover_text_at_pointer(format!("Channel {} = {} ({})", ch + 1, value(ch), who.as_deref().unwrap_or("no light here")));
                }
            }
            ui.add_space(spacing::S);
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing.x = spacing::L;
                for (i, f) in rig.fixtures.iter().enumerate().filter(|(_, f)| f.universe == universe) {
                    ui.horizontal(|ui| {
                        let (r, _) = ui.allocate_exact_size(Vec2::new(10.0, 16.0), Sense::hover());
                        ui.painter().circle_filled(r.center(), 4.5, tint[i % tint.len()]);
                        ui.label(RichText::new(format!("{} · {}", f.name(), channel_text(f, false))).size(type_scale::SMALL + 0.5).color(t.text_dim));
                    });
                }
            });
        },
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rig_value() -> Value {
        let fixture = |id: &str, label: &str, profile: &str, mode: &str, addr: i64, fp: i64, pos: [f32; 2], heads: Vec<&str>| {
            Value::map()
                .with("id", id)
                .with("label", label)
                .with("profile", profile)
                .with("profile_name", "Generic")
                .with("mode", mode)
                .with("kind", "par")
                .with("universe", 1)
                .with("address", addr)
                .with("footprint", fp)
                .with("position", pos)
                .with("rotation", 0.0)
                .with("beam", 25.0)
                .with("heads", heads)
                .with("attrs", vec!["intensity", "color"])
        };
        let head = |id: &str, fixture: &str, pos: [f32; 2], moving: bool, attrs: Vec<&str>| {
            Value::map()
                .with("id", id)
                .with("fixture", fixture)
                .with("position", pos)
                .with("rotation", 0.0)
                .with("kind", if moving { "spot" } else { "par" })
                .with("beam", 20.0)
                .with("attrs", attrs)
                .with("moving", moving)
        };
        let groups: BTreeMap<String, Value> =
            [("front".to_string(), Value::from(vec!["par1", "par2"])), ("left".to_string(), Value::from(vec!["par1"]))].into_iter().collect();
        let mode = |id: &str, ch: Vec<&str>| Value::map().with("id", id).with("footprint", ch.len()).with("channels", ch);
        Value::map()
            .with(
                "fixtures",
                vec![
                    fixture("par1", "Par L", "generic_rgb", "3ch", 1, 3, [0.25, 0.2], vec!["par1"]),
                    fixture("par2", "Par R", "generic_rgb", "3ch", 4, 3, [0.75, 0.2], vec!["par2"]),
                    fixture("spot1", "Spot", "mover", "8ch", 510, 8, [0.5, 0.8], vec!["spot1"]),
                ],
            )
            .with(
                "heads",
                vec![
                    head("par1", "par1", [0.25, 0.2], false, vec!["intensity", "color"]),
                    head("par2", "par2", [0.75, 0.2], false, vec!["intensity", "color"]),
                    head("spot1", "spot1", [0.5, 0.8], true, vec!["intensity", "color", "pan", "tilt", "zoom", "gobo", "program"]),
                ],
            )
            .with("groups", groups)
            .with(
                "profiles",
                vec![Value::map().with("id", "generic_rgb").with("name", "Generic RGB PAR").with("modes", vec![mode("3ch", vec!["red", "green", "blue"])])],
            )
            .with(
                "outputs",
                vec![Value::map().with("id", "usb").with("kind", "enttec_pro").with("enabled", true).with("status", "ok").with("detail", "EN405589 44 Hz")],
            )
            .with("errors", vec!["cuelists/bad.toml: unknown palette `nope`"])
    }

    fn payload(m: &mut Model) {
        let cue = |id: &str, follow: Option<i64>| {
            Value::map()
                .with("id", id)
                .with("label", format!("cue {id}"))
                .with("fade_ms", 2000)
                .with("fade_out_ms", 1500)
                .with("delay_ms", 0)
                .with("follow_ms", follow.map(Value::from).unwrap_or(Value::Null))
                .with("wait_ms", Value::Null)
                .with("block", false)
                .with("targets", vec!["all"])
                .with("effects", vec!["chase"])
                .with("palettes", vec!["warm"])
        };
        let cl = Value::map()
            .with("name", "main")
            .with("label", "Main")
            .with("priority", 200)
            .with("master", 1.0)
            .with("playing", true)
            .with("current", "1")
            .with("current_index", 0)
            .with("next", "2")
            .with("progress", 0.4)
            .with("elapsed_ms", 800)
            .with("total_ms", 2000)
            .with("cues", vec![cue("1", Some(2000)), cue("2", None)]);
        let single = Value::map().with("name", "strobe").with("label", "Strobe").with("playing", false).with("master", 0.5).with("cues", vec![cue("1", None)]);
        let set: BTreeMap<String, Value> = [("all".to_string(), Value::map().with("color", "#ffb070"))].into_iter().collect();
        let palette = Value::map().with("name", "warm").with("label", "Warm").with("kind", "color").with("set", set).with("used_by", vec!["main/1"]);
        let half: BTreeMap<String, Value> = [("all".to_string(), Value::map().with("intensity", 0.5))].into_iter().collect();
        let half = Value::map().with("name", "half").with("label", "Half").with("kind", "intensity").with("set", half).with("used_by", Vec::<String>::new());
        let fx = Value::map()
            .with("name", "chase")
            .with("label", "Beat chase")
            .with("kind", "color_chase")
            .with("targets", vec!["all"])
            .with("unit", "beats")
            .with("rate", 4.0)
            .with("size", 1.0)
            .with("spread", 1.0)
            .with("active", true);
        let universe: Vec<i64> = (0..512).map(|i| (i * 7 % 256) as i64).collect();
        let universes: BTreeMap<String, Value> = [("1".to_string(), Value::from(universe))].into_iter().collect();
        let out_head = |id: &str, x: f32, y: f32, limited: bool| {
            Value::map()
                .with("id", id)
                .with("x", x)
                .with("y", y)
                .with("rotation", 0.0)
                .with("intensity", 0.8)
                .with("color", vec![1.0, 0.5, 0.2])
                .with("pan", 0.6)
                .with("tilt", 0.8)
                .with("beam", 20.0)
                .with("limited", limited)
        };
        let output = Value::map()
            .with("fps", 44.0)
            .with("frames", 1234)
            .with("jitter", Value::map().with("mean_ms", 0.2).with("p99_ms", 0.9).with("max_ms", 2.0))
            .with("universes", universes)
            .with("heads", vec![out_head("par1", 0.25, 0.2, false), out_head("par2", 0.75, 0.2, true), out_head("spot1", 0.5, 0.8, false)])
            .with("limiter", Value::map().with("active", true).with("suppressed", 2).with("strobe_capped", 1));
        let device = Value::map()
            .with("uid", 0x454E_0405_589Ai64)
            .with("manufacturer", "ENTTEC")
            .with("model", "PAR")
            .with("label", Value::Null)
            .with("footprint", 7)
            .with("start_address", 1)
            .with("personality", 1)
            .with("personalities", 2);
        let q = &mut m.queries;
        q.insert("lights.rig".into(), rig_value());
        q.insert("lights.cuelists".into(), Value::from(vec![cl, single]));
        q.insert("lights.palettes".into(), Value::from(vec![palette, half]));
        q.insert("lights.effects".into(), Value::from(vec![fx]));
        q.insert(
            "lights.programmer".into(),
            Value::map().with("selection", vec!["spot1"]).with("heads", vec!["spot1"]).with("values", Value::map()).with("highlight", false),
        );
        q.insert("lights.output".into(), output);
        q.insert(
            "lights.rdm".into(),
            Value::map().with("status", "done").with("firmware", "4.4").with("serial", "EN405589").with("devices", vec![device]).with("at", 1),
        );
        m.state.insert("lights.programmer.selection".into(), Value::from(vec!["spot1"]));
        m.state.insert("lights.programmer.active".into(), Value::from(true));
        m.state.insert("health.dmx".into(), Value::map().with("status", "pass").with("detail", "ENTTEC USB PRO"));
        m.state.insert("lights.master".into(), Value::from(0.75));
    }

    fn model() -> Model {
        Model::new(Some(std::env::temp_dir().join(format!("se-lights-ui-test-{}.sock", std::process::id()))))
    }

    fn input(mut events: Vec<egui::Event>, shift: bool) -> egui::RawInput {
        events.insert(0, egui::Event::ModifiersChanged(egui::Modifiers { shift, ..Default::default() }));
        egui::RawInput { screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 900.0))), events, ..Default::default() }
    }

    /// A headless context with the app's fonts (the view uses the Inter weights).
    fn ctx() -> egui::Context {
        let ctx = egui::Context::default();
        se_ui_kit::theme::install_font(&ctx, None);
        ctx
    }

    fn frame(ctx: &egui::Context, m: &Model, st: &mut LightsState, raw: egui::RawInput) -> Vec<Op> {
        let t = Theme::default();
        let mut out = Vec::new();
        ctx.run_ui(raw, |ui| view(m, &t, st, ui, &mut out, Instant::now())).drop_without_applying_deltas();
        out
    }

    fn click(p: Pos2, pressed: bool, shift: bool) -> egui::Event {
        egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers { shift, ..Default::default() } }
    }

    /// Run `ui()` on a real (headless) App with the given sub-view selected.
    fn run_app_tab(ctx: &egui::Context, app: &mut App, tab: Tab, new_look: bool) {
        ctx.run_ui(input(Vec::new(), false), |ui| {
            let sid = ui.make_persistent_id("lights_view");
            ui.data_mut(|d| {
                let st = d.get_temp_mut_or_default::<LightsState>(sid);
                st.tab = tab;
                st.new_look = new_look;
            });
            super::ui(app, ui);
            assert_eq!(ui.data_mut(|d| d.get_temp_mut_or_default::<LightsState>(sid).tab), tab, "view state survives the frame");
        })
        .drop_without_applying_deltas();
    }

    /// `ui()` runs headless on a real App for every sub-view: no data, full data, and malformed data.
    #[test]
    fn renders_every_tab_headless() {
        let ctx = egui::Context::default();
        let cc = eframe::CreationContext::_new_kittest(ctx.clone());
        let socket = std::env::temp_dir().join(format!("se-lights-ui-app-{}.sock", std::process::id()));
        let mut app = App::new(&cc, crate::UiOpts { socket: Some(socket), ..Default::default() });
        for full in [false, true] {
            if full {
                payload(&mut app.m);
            }
            for tab in Tab::ALL {
                for new_look in [false, true] {
                    run_app_tab(&ctx, &mut app, tab, new_look);
                    run_app_tab(&ctx, &mut app, tab, new_look);
                }
            }
        }
        for q in ["lights.rig", "lights.cuelists", "lights.palettes", "lights.effects", "lights.programmer", "lights.output", "lights.rdm"] {
            app.m.queries.insert(q.into(), Value::from("garbage"));
        }
        app.m.state.insert("lights.programmer.selection".into(), Value::from(42));
        app.m.state.insert("health.dmx".into(), Value::from(vec![1, 2]));
        for tab in Tab::ALL {
            run_app_tab(&ctx, &mut app, tab, false);
        }
    }

    /// Press + release on a light on the stage emits `lights.programmer.select` for its fixture.
    #[test]
    fn stage_click_selects_fixture_and_shift_adds() {
        let mut m = model();
        payload(&mut m);
        let rig = Rig::parse(m.q("lights.rig"));
        let output = m.q("lights.output");
        let heads = stage_heads(&rig, output);
        let t = Theme::default();
        let ctx = ctx();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 900.0));
        let par2 = heads.iter().find(|h| h.id == "par2").map(|h| h.pos).expect("par2 on stage");
        let p = to_screen(stage_rect(screen), par2);
        let stage_frame = |events: Vec<egui::Event>, shift: bool| {
            let mut out = Vec::new();
            ctx.run_ui(input(events, shift), |ui| stage_floor(&t, ui, &mut out, &rig, &heads, &[])).drop_without_applying_deltas();
            out
        };
        assert!(stage_frame(Vec::new(), false).is_empty());
        let mut got = Vec::new();
        for shift in [false, true] {
            assert!(stage_frame(vec![egui::Event::PointerMoved(p), click(p, true, shift)], shift).is_empty());
            got.extend(stage_frame(vec![click(p, false, shift)], shift));
        }
        assert_eq!(got, vec![select_op("par2", false), select_op("par2", true)]);
        // clicking empty floor selects nothing
        let floor = to_screen(stage_rect(screen), [0.5, 0.5]);
        stage_frame(vec![egui::Event::PointerMoved(floor), click(floor, true, false)], false);
        assert!(stage_frame(vec![click(floor, false, false)], false).is_empty());
    }

    /// Rendering alone (every sub-view, full data, fader values ≠ defaults) never sends commands.
    #[test]
    fn view_emits_nothing_without_interaction() {
        let mut m = model();
        payload(&mut m);
        let ctx = ctx();
        let mut st = LightsState::default();
        for tab in Tab::ALL {
            for new_look in [false, true] {
                st.tab = tab;
                st.new_look = new_look;
                assert!(frame(&ctx, &m, &mut st, input(Vec::new(), false)).is_empty(), "{tab:?} sent a command without input");
            }
        }
    }

    #[test]
    fn selection_resolves_groups_fixtures_and_implicit_all() {
        let rig = Rig::parse(Some(&rig_value()));
        assert_eq!(resolve_heads(&rig, &["front".into(), "par1".into()]), vec!["par1", "par2"]);
        assert_eq!(resolve_heads(&rig, &["all".into()]), vec!["par1", "par2", "spot1"]);
        assert!(resolve_heads(&rig, &["nope".into()]).is_empty());
        assert_eq!(rig.group_names(), vec!["all", "front", "left"]);
    }

    #[test]
    fn palette_precedence_fixture_over_smaller_group_over_all() {
        let rig = Rig::parse(Some(&rig_value()));
        let set: BTreeMap<String, Value> = [
            ("all".to_string(), Value::map().with("color", "#ffffff").with("intensity", 0.5)),
            ("front".to_string(), Value::map().with("color", "#ff0000")),
            ("left".to_string(), Value::map().with("color", "#00ff00")),
            ("par1".to_string(), Value::map().with("intensity", Value::map().with("value", 0.9).with("fade", "2s"))),
            ("spot1".to_string(), Value::map().with("zoom", 0.3)),
        ]
        .into_iter()
        .collect();
        let pal = Value::map().with("set", set);
        let v = palette_values(&pal, &rig, &["par1".into()]);
        assert_eq!(v.get("color"), Some(&Value::from("#00ff00")), "smaller group `left` beats `front`");
        assert_eq!(v.get("intensity"), Some(&Value::from(0.9)), "fixture entry beats `all`, `{{value}}` unwrapped");
        assert!(!v.contains_key("zoom"), "entries for unselected fixtures are ignored");
        let v = palette_values(&pal, &rig, &["par2".into()]);
        assert_eq!(v.get("color"), Some(&Value::from("#ff0000")));
        assert_eq!(v.get("intensity"), Some(&Value::from(0.5)));
    }

    /// One tap on a look with nothing chosen puts it on every light; with a choice it only sets.
    #[test]
    fn tapping_a_look_with_nothing_chosen_selects_all_lights_first() {
        let rig = Rig::parse(Some(&rig_value()));
        let set: BTreeMap<String, Value> = [("all".to_string(), Value::map().with("color", "#ffb070"))].into_iter().collect();
        let pal = Value::map().with("set", set);
        let every: Vec<String> = rig.heads.iter().map(|h| h.id.clone()).collect();
        let set_color = act("lights.programmer.set", Value::map().with("attr", "color").with("value", "#ffb070"));
        assert_eq!(look_ops(&[], palette_values(&pal, &rig, &every)), vec![select_op("all", false), set_color.clone()]);
        assert_eq!(look_ops(&["par1".into()], palette_values(&pal, &rig, &["par1".into()])), vec![set_color]);
    }

    /// A look shows as "on" only while the programmer holds exactly its values for the light.
    #[test]
    fn look_is_on_when_the_programmer_holds_its_values() {
        let want: BTreeMap<String, Value> = [("color".to_string(), Value::from("#ffb070")), ("intensity".to_string(), Value::from(0.5))].into_iter().collect();
        let held: BTreeMap<String, Value> = [
            ("lights.par1.color".to_string(), Value::from(vec![1.0, 176.0 / 255.0, 112.0 / 255.0, 1.0])),
            ("lights.par1.intensity".to_string(), Value::from(0.5)),
        ]
        .into_iter()
        .collect();
        assert!(look_on(&want, "par1", Some(&held)), "resolved RGBA matches the palette's hex");
        assert!(!look_on(&want, "par2", Some(&held)), "other light holds nothing");
        let mut dimmer = held.clone();
        dimmer.insert("lights.par1.intensity".into(), Value::from(0.3));
        assert!(!look_on(&want, "par1", Some(&dimmer)));
        assert!(!look_on(&BTreeMap::new(), "par1", Some(&held)), "an empty look is never on");
    }

    #[test]
    fn dmx_owner_map_respects_universe_and_clips_at_512() {
        let rig = Rig::parse(Some(&rig_value()));
        let owners = channel_owners(&rig, 1);
        assert_eq!(&owners[0..7], &[Some(0), Some(0), Some(0), Some(1), Some(1), Some(1), None]);
        assert_eq!(owners[508], None);
        assert_eq!(&owners[509..512], &[Some(2), Some(2), Some(2)], "spot1 at 510 with 8 channels is clipped, not overflowing");
        assert!(channel_owners(&rig, 2).iter().all(Option::is_none));
    }

    #[test]
    fn beam_points_downstage_at_rest_and_follows_rotation() {
        let (d, reach) = beam_dir(0.0, 0.5, 1.0);
        assert!(d.y > 0.99 && d.x.abs() < 1e-3, "0° faces downstage (screen down): {d:?}");
        assert!((reach - 1.0).abs() < 1e-6);
        let (d, _) = beam_dir(90.0, 0.5, 1.0);
        assert!(d.x > 0.99, "90° faces stage right: {d:?}");
        let (d, _) = beam_dir(0.0, 0.5, 0.0);
        assert!(d.y < -0.99, "tilt below centre throws the other way: {d:?}");
        let (_, reach) = beam_dir(0.0, 0.5, 0.5);
        assert!(reach > 0.0 && reach < 0.2, "tilt centre = short stub");
    }

    #[test]
    fn durations_and_names_read_plainly() {
        assert_eq!(dur(0.0), "instant");
        assert_eq!(dur(250.0), "0.25 s");
        assert_eq!(dur(2000.0), "2 s");
        assert_eq!(dur(1500.0), "1.5 s");
        assert_eq!(uid_text(Some(&Value::from(0x454E_0405_589Ai64))), "454E:0405589A");
        assert_eq!(slug("  Deep blue! "), "deep_blue");
        assert_eq!(slug("a  b"), "a_b", "runs of spaces collapse");
        assert_eq!(slug("../evil"), "evil", "path characters dropped");
    }
}
