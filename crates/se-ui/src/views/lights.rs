//! Lights view (§9, §15.6): stage visualizer, cue lists + effects, patch + RDM, palettes,
//! programmer, DMX output monitor. Pure client of the se-dmx queries/actions/state addresses.

use crate::app::App;
use crate::model::Model;
use egui::ecolor::Hsva;
use egui::{Align2, Color32, CornerRadius, FontId, Pos2, Rect, RichText, Sense, Shape, Stroke, StrokeKind, Ui, Vec2};
use se_proto::{Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::widgets::{self, LedState, icon};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

/// Structural queries, refreshed about once a second.
const SLOW_QUERIES: [&str; 6] = ["lights.rig", "lights.cuelists", "lights.palettes", "lights.effects", "lights.programmer", "lights.rdm"];
const SLOW_EVERY: Duration = Duration::from_secs(1);
/// Live output (visualizer, DMX monitor, fps/jitter), refreshed while the view is visible.
const FAST_EVERY: Duration = Duration::from_millis(70);
/// After an action, re-query structure this soon (the engine applies actions asynchronously).
const AFTER_ACTION: Duration = Duration::from_millis(150);
/// Locally edited values win over the (lagging) engine echo for this long.
const EDIT_HOLD: Duration = Duration::from_millis(1500);
/// Pan travel assumed by the visualizer (typical mover: 540°); pan 0.5 = centre.
const PAN_TRAVEL_DEG: f32 = 540.0;
const PALETTE_ICON: &str = "\u{f1fc}";
const GRID_ICON: &str = "\u{f00a}";
const EMPTY: &str = "no lights data — engine offline or lights not configured";
/// Attributes rendered as 0..1 bars in the programmer (intensity/color/pan/tilt/gobo get dedicated controls).
const UNIT_ATTRS: [&str; 12] = ["white", "amber", "uv", "lime", "cto", "zoom", "focus", "iris", "frost", "prism", "gobo_rotate", "strobe"];
const PALETTE_KINDS: [&str; 5] = ["any", "color", "position", "beam", "intensity"];

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Tab {
    #[default]
    Stage,
    Cues,
    Patch,
    Palettes,
    Programmer,
    Dmx,
}

impl Tab {
    const ALL: [(Tab, &'static str, &'static str); 6] = [
        (Tab::Stage, icon::LIGHT, "Stage"),
        (Tab::Cues, icon::TIMELINE, "Cues"),
        (Tab::Patch, icon::DEVICE, "Patch"),
        (Tab::Palettes, PALETTE_ICON, "Palettes"),
        (Tab::Programmer, icon::MIX, "Programmer"),
        (Tab::Dmx, GRID_ICON, "DMX"),
    ];
}

/// Per-view UI state, kept in egui memory.
#[derive(Clone, Default)]
struct LightsState {
    tab: Tab,
    slow_at: Option<Instant>,
    fast_at: Option<Instant>,
    universe: Option<u16>,
    store_name: String,
    store_kind: String,
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
    header(m, t, st, ui, out, output, now);
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        for (tab, ic, label) in Tab::ALL {
            let text = RichText::new(format!("{ic} {label}")).strong().color(if st.tab == tab { t.accent } else { t.fg_dim });
            if ui.selectable_label(st.tab == tab, text).clicked() {
                st.tab = tab;
            }
        }
    });
    ui.separator();
    let selection = selection(m);
    let selected_heads = resolve_heads(&rig, &selection);
    match st.tab {
        Tab::Stage => stage_tab(t, ui, out, &rig, output, &selected_heads),
        Tab::Cues => egui::ScrollArea::both().id_salt("lights_cues").show(ui, |ui| cues_tab(m, t, st, ui, out, now)).inner,
        Tab::Patch => egui::ScrollArea::both().id_salt("lights_patch").show(ui, |ui| patch_tab(m, t, ui, out, &rig)).inner,
        Tab::Palettes => egui::ScrollArea::vertical().id_salt("lights_palettes").show(ui, |ui| palettes_tab(m, t, ui, out, &rig, &selected_heads)).inner,
        Tab::Programmer => {
            egui::ScrollArea::vertical()
                .id_salt("lights_programmer")
                .show(ui, |ui| programmer_tab(m, t, st, ui, out, &rig, &selection, &selected_heads, now))
                .inner
        }
        Tab::Dmx => dmx_tab(t, st, ui, &rig, output),
    }
}

// ---------------------------------------------------------------- data

fn act(name: &str, args: Value) -> Op {
    Op::Action { name: name.into(), args }
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

/// Human duration for cue timing columns.
fn fmt_ms(ms: Option<f64>) -> String {
    match ms {
        None => "—".into(),
        Some(ms) if ms <= 0.0 => "0".into(),
        Some(ms) if ms < 1000.0 => format!("{}ms", ms.round() as i64),
        Some(ms) if (ms % 1000.0).abs() < 0.5 => format!("{}s", (ms / 1000.0).round() as i64),
        Some(ms) => format!("{:.1}s", ms / 1000.0),
    }
}

fn health_led(status: &str) -> LedState {
    match status {
        "pass" | "ok" => LedState::Healthy,
        "warn" => LedState::Armed,
        "fail" | "error" => LedState::Error,
        _ => LedState::Idle,
    }
}

// ---------------------------------------------------------------- local widgets

/// Horizontal 0..1 bar fader with a label and value readout.
fn hbar(ui: &mut Ui, t: &Theme, width: f32, value: &mut f32, label: &str) -> egui::Response {
    let (rect, mut resp) = ui.allocate_exact_size(Vec2::new(width, 20.0), Sense::click_and_drag());
    if let Some(p) = resp.interact_pointer_pos()
        && (resp.dragged() || resp.clicked())
    {
        let v = ((p.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
        if (v - *value).abs() > f32::EPSILON {
            *value = v;
            resp.mark_changed();
        }
    }
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(3), t.bg_darker);
    let fill = Rect::from_min_size(rect.min, Vec2::new(rect.width() * value.clamp(0.0, 1.0), rect.height()));
    p.rect_filled(fill, CornerRadius::same(3), if resp.dragged() { t.accent } else { t.accent.gamma_multiply(0.6) });
    p.rect_stroke(rect, CornerRadius::same(3), Stroke::new(1.0, if resp.hovered() { t.accent } else { t.muted }), StrokeKind::Inside);
    p.text(rect.left_center() + Vec2::new(6.0, 0.0), Align2::LEFT_CENTER, label, FontId::proportional(11.0), t.fg);
    p.text(rect.right_center() - Vec2::new(6.0, 0.0), Align2::RIGHT_CENTER, format!("{:.0}%", *value * 100.0), FontId::monospace(11.0), t.fg);
    resp
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
    p.rect_filled(rect, CornerRadius::same(4), t.bg_darker);
    let grid = Stroke::new(1.0, t.muted.gamma_multiply(0.5));
    for i in 1..4 {
        let f = i as f32 / 4.0;
        p.line_segment([Pos2::new(rect.left() + rect.width() * f, rect.top()), Pos2::new(rect.left() + rect.width() * f, rect.bottom())], grid);
        p.line_segment([Pos2::new(rect.left(), rect.top() + rect.height() * f), Pos2::new(rect.right(), rect.top() + rect.height() * f)], grid);
    }
    let dot = Pos2::new(rect.left() + rect.width() * v[0], rect.bottom() - rect.height() * v[1]);
    let cross = Stroke::new(1.0, t.accent.gamma_multiply(0.5));
    p.line_segment([Pos2::new(dot.x, rect.top()), Pos2::new(dot.x, rect.bottom())], cross);
    p.line_segment([Pos2::new(rect.left(), dot.y), Pos2::new(rect.right(), dot.y)], cross);
    p.circle_filled(dot, 5.0, t.accent);
    p.circle_stroke(dot, 8.0, Stroke::new(1.0, t.accent.gamma_multiply(0.6)));
    p.rect_stroke(rect, CornerRadius::same(4), Stroke::new(1.0, if resp.hovered() { t.accent } else { t.muted }), StrokeKind::Inside);
    resp
}

fn progress_bar(ui: &mut Ui, t: &Theme, width: f32, frac: f32, color: Color32) {
    let (rect, _) = ui.allocate_exact_size(Vec2::new(width, 6.0), Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, CornerRadius::same(2), t.bg_darker);
    p.rect_filled(Rect::from_min_size(rect.min, Vec2::new(rect.width() * frac.clamp(0.0, 1.0), rect.height())), CornerRadius::same(2), color);
}

fn empty(ui: &mut Ui, t: &Theme, msg: &str) {
    ui.label(RichText::new(msg).color(t.fg_dim));
}

// ---------------------------------------------------------------- header

fn header(m: &Model, t: &Theme, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, output: Option<&Value>, now: Instant) {
    ui.horizontal(|ui| {
        widgets::section(ui, t, icon::LIGHT, "Lights");
        let (status, detail) = m.get("health.dmx").map(|h| (s(h, "status"), s(h, "detail"))).unwrap_or(("", ""));
        let text = match (status, detail) {
            ("", _) => "DMX —".to_string(),
            (status, "") => format!("DMX {status}"),
            (_, d) => format!("DMX {d}"),
        };
        widgets::pill(ui, t, icon::DEVICE, &text, health_led(status)).on_hover_text(format!("health.dmx: {status} {detail}"));
        ui.separator();
        let fps = output.and_then(|o| num(o, "fps")).or_else(|| m.get("lights.output.fps").and_then(Value::as_f64));
        let p99 = output.and_then(|o| num(o, "jitter.p99_ms")).or_else(|| m.get("lights.output.jitter_ms").and_then(Value::as_f64));
        match fps {
            Some(fps) => ui.label(RichText::new(format!("{fps:.1} fps")).monospace().color(t.fg)),
            None => ui.label(RichText::new("— fps").monospace().color(t.fg_dim)),
        };
        if let Some(j) = p99 {
            ui.label(RichText::new(format!("p99 {j:.2}ms")).monospace().color(if j > 4.0 { t.yellow } else { t.fg_dim }))
                .on_hover_text("frame jitter, 99th percentile");
        }
        if let Some(l) = output.and_then(|o| o.get_path("limiter")).filter(|l| l.get_path("active").is_some_and(Value::truthy)) {
            let tip = format!("suppressed {} · strobe capped {}", num(l, "suppressed").unwrap_or(0.0), num(l, "strobe_capped").unwrap_or(0.0));
            widgets::pill(ui, t, icon::WARN, "limiter", LedState::Armed).on_hover_text(tip);
        }
        ui.separator();
        ui.label(RichText::new("GM").strong().color(t.fg_dim));
        let key = "@lights.master";
        let mut gm = st.edited(key, now).and_then(Value::as_f32).or_else(|| m.get("lights.master").and_then(Value::as_f32)).unwrap_or(1.0);
        if hbar(ui, t, 140.0, &mut gm, "master").on_hover_text("grand master (lights.master)").changed() {
            st.edit(key, Value::from(gm), now);
            out.push(Op::Set { address: "lights.master".into(), value: Value::from(gm) });
        }
        let bkey = "@lights.blackout";
        let blackout = st.edited(bkey, now).map(Value::truthy).unwrap_or_else(|| m.b("lights.blackout"));
        let label = RichText::new("BLACKOUT").strong().color(if blackout { widgets::contrast(t.bright_red) } else { t.fg });
        let btn = egui::Button::new(label).fill(if blackout { t.bright_red } else { t.bg_light });
        if ui.add(btn).on_hover_text("toggle lights.blackout").clicked() {
            st.edit(bkey, Value::Bool(!blackout), now);
            out.push(Op::Set { address: "lights.blackout".into(), value: Value::Bool(!blackout) });
        }
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if widgets::hold_button(ui, t, "PANIC", t.bright_red, 0.8) {
                out.push(act("lights.panic", Value::Null));
            }
        });
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
    let r = stage.shrink(20.0);
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
    heads.iter().map(|h| (h, to_screen(stage, h.pos).distance(p))).filter(|(_, d)| *d <= 16.0).min_by(|a, b| a.1.total_cmp(&b.1)).map(|(h, _)| h)
}

fn stage_tab(t: &Theme, ui: &mut Ui, out: &mut Vec<Op>, rig: &Rig, output: Option<&Value>, selected: &[String]) {
    let heads = stage_heads(rig, output);
    if heads.is_empty() {
        empty(ui, t, if rig.present { "no fixtures patched — add fixtures to lights/rig.toml" } else { EMPTY });
        return;
    }
    let stage = stage_rect(ui.available_rect_before_wrap());
    let resp = ui.allocate_rect(stage, Sense::click());
    let p = ui.painter_at(stage);
    p.rect_filled(stage, CornerRadius::same(6), t.bg_darker);
    p.rect_stroke(stage, CornerRadius::same(6), Stroke::new(1.0, t.muted), StrokeKind::Inside);
    let small = FontId::proportional(10.0);
    p.text(stage.center_top() + Vec2::new(0.0, 4.0), Align2::CENTER_TOP, "UPSTAGE", small.clone(), t.fg_dim.gamma_multiply(0.6));
    p.text(stage.center_bottom() - Vec2::new(0.0, 4.0), Align2::CENTER_BOTTOM, "DOWNSTAGE · AUDIENCE", small.clone(), t.fg_dim.gamma_multiply(0.6));
    p.text(stage.left_center() + Vec2::new(4.0, 0.0), Align2::LEFT_CENTER, "SL", small.clone(), t.fg_dim.gamma_multiply(0.6));
    p.text(stage.right_center() - Vec2::new(4.0, 0.0), Align2::RIGHT_CENTER, "SR", small.clone(), t.fg_dim.gamma_multiply(0.6));
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
    for h in &heads {
        let c = to_screen(stage, h.pos);
        let lit = rgb(h.color[0] * h.intensity, h.color[1] * h.intensity, h.color[2] * h.intensity);
        if h.intensity > 0.01 {
            let glow = rgb(h.color[0], h.color[1], h.color[2]);
            p.circle_filled(c, 8.0 + 14.0 * h.intensity, glow.gamma_multiply(0.12 * h.intensity));
            p.circle_filled(c, 8.0 + 7.0 * h.intensity, glow.gamma_multiply(0.28 * h.intensity));
        }
        let fill = if h.intensity > 0.01 { lit } else { t.bg_dark };
        let is_sel = selected.contains(&h.id);
        let edge = if is_sel { Stroke::new(2.5, t.accent) } else { Stroke::new(1.0, t.muted) };
        if h.kind == "bar" {
            let r = Rect::from_center_size(c, Vec2::splat(12.0));
            p.rect_filled(r, CornerRadius::same(2), fill);
            p.rect_stroke(r, CornerRadius::same(2), edge, StrokeKind::Outside);
        } else {
            p.circle_filled(c, 7.0, fill);
            p.circle_stroke(c, 7.0, edge);
            if h.moving {
                p.circle_filled(c, 2.0, t.fg_dim);
            }
        }
        if h.limited {
            p.circle_stroke(c, 12.0, Stroke::new(2.0, t.orange));
        }
    }
    if rig.fixtures.len() <= 48 {
        for f in &rig.fixtures {
            let pos = rig.fixture_heads(f).first().and_then(|h| heads.iter().find(|x| &x.id == h)).map(|h| h.pos).unwrap_or(f.position);
            let label = if f.label.is_empty() { &f.id } else { &f.label };
            p.text(to_screen(stage, pos) + Vec2::new(0.0, 14.0), Align2::CENTER_TOP, label, small.clone(), t.fg_dim);
        }
    }
    if resp.clicked()
        && let Some(pos) = resp.interact_pointer_pos()
        && let Some(h) = head_at(&heads, stage, pos)
    {
        let shift = ui.input(|i| i.modifiers.shift);
        out.push(act("lights.programmer.select", Value::map().with("targets", vec![h.fixture.clone()]).with("add", shift)));
    }
    if let Some(pos) = resp.hover_pos()
        && let Some(h) = head_at(&heads, stage, pos)
    {
        let tip = format!(
            "{} ({})\nintensity {:.0}% · {}\npan {:.2} · tilt {:.2}{}",
            h.id,
            h.fixture,
            h.intensity * 100.0,
            hex_of(rgb(h.color[0], h.color[1], h.color[2])),
            h.pan,
            h.tilt,
            if h.limited { "\nlimited by safety limiter" } else { "" }
        );
        resp.on_hover_text_at_pointer(tip);
    }
    ui.add_space(4.0);
    let movers = heads.iter().filter(|h| h.moving).count();
    let limited = heads.iter().filter(|h| h.limited).count();
    ui.horizontal(|ui| {
        ui.label(RichText::new(format!("{} heads · {movers} movers · {} selected", heads.len(), selected.len())).small().color(t.fg_dim));
        if limited > 0 {
            ui.label(RichText::new(format!("{} {limited} limited", icon::WARN)).small().color(t.orange));
        }
        if output.is_none() {
            ui.label(RichText::new("no live output").small().color(t.yellow));
        }
        ui.label(RichText::new("click = select · shift+click = add").small().color(t.fg_dim));
    });
}

// ---------------------------------------------------------------- cues + effects

const CUE_COLS: [(&str, f32); 9] =
    [("id", 44.0), ("label", 170.0), ("fade", 56.0), ("out", 56.0), ("delay", 56.0), ("follow", 56.0), ("wait", 56.0), ("effects", 150.0), ("palettes", 150.0)];

fn cues_tab(m: &Model, t: &Theme, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, now: Instant) {
    effects_section(m, t, ui, out);
    ui.add_space(8.0);
    widgets::section(ui, t, icon::TIMELINE, "Cue lists");
    let lists = m.q_list("lights.cuelists");
    if lists.is_empty() {
        empty(ui, t, if m.q("lights.cuelists").is_some() { "no cue lists — add lights/cuelists/*.toml" } else { EMPTY });
        return;
    }
    for cl in lists {
        cuelist_card(m, t, st, ui, out, cl, now);
        ui.add_space(6.0);
    }
}

fn effects_section(m: &Model, t: &Theme, ui: &mut Ui, out: &mut Vec<Op>) {
    widgets::section(ui, t, icon::EFFECT, "Effects");
    let fx = m.q_list("lights.effects");
    if fx.is_empty() {
        empty(ui, t, if m.q("lights.effects").is_some() { "no effects — add lights/effects/*.toml" } else { EMPTY });
        return;
    }
    egui::Grid::new("lights_effects").striped(true).num_columns(6).show(ui, |ui| {
        for e in fx {
            let name = s(e, "name");
            let active = m.get(&format!("lights.effect.{name}.active")).map(Value::truthy).unwrap_or_else(|| e.get_path("active").is_some_and(Value::truthy));
            widgets::led(ui, t, if active { LedState::Active } else { LedState::Idle });
            let label = s(e, "label");
            ui.label(RichText::new(if label.is_empty() { name } else { label }).strong()).on_hover_text(name);
            ui.label(RichText::new(s(e, "kind")).color(t.fg_dim));
            let rate = m.get(&format!("lights.effect.{name}.rate")).and_then(Value::as_f64).or_else(|| num(e, "rate")).unwrap_or(0.0);
            let size = m.get(&format!("lights.effect.{name}.size")).and_then(Value::as_f64).or_else(|| num(e, "size")).unwrap_or(0.0);
            let unit = if s(e, "unit") == "beats" { "beats/cycle" } else { "Hz" };
            ui.label(RichText::new(format!("{rate:.2} {unit} · size {size:.2}")).monospace().small());
            ui.label(RichText::new(strs(e, "targets").join(", ")).small().color(t.fg_dim));
            let (txt, name_act) = if active { ("stop", "lights.effect.stop") } else { ("start", "lights.effect.start") };
            if ui.button(RichText::new(txt).color(if active { t.bright_red } else { t.green })).clicked() {
                out.push(act(name_act, Value::map().with("name", name)));
            }
            ui.end_row();
        }
    });
}

fn cuelist_card(m: &Model, t: &Theme, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, cl: &Value, now: Instant) {
    let name = s(cl, "name").to_string();
    let base = format!("lights.cuelist.{name}");
    let playing = m.get(&format!("{base}.playing")).map(Value::truthy).unwrap_or_else(|| cl.get_path("playing").is_some_and(Value::truthy));
    let current = m.get(&format!("{base}.cue")).map(text).filter(|c| !c.is_empty()).or_else(|| cl.get_path("current").filter(|v| !v.is_null()).map(text));
    let next = m.get(&format!("{base}.next")).map(text).filter(|c| !c.is_empty()).or_else(|| cl.get_path("next").filter(|v| !v.is_null()).map(text));
    let label = s(cl, "label");
    let title = format!("{} · {name} · prio {}", if label.is_empty() { &name } else { label }, num(cl, "priority").unwrap_or(0.0));
    widgets::card(ui, t, icon::TIMELINE, &title, if playing { LedState::Active } else { LedState::Idle }, |ui| {
        ui.horizontal(|ui| {
            let key = format!("@{base}.master");
            let mut master = st
                .edited(&key, now)
                .and_then(Value::as_f32)
                .or_else(|| m.get(&format!("{base}.master")).and_then(Value::as_f32))
                .or_else(|| num(cl, "master").map(|x| x as f32))
                .unwrap_or(1.0);
            ui.vertical(|ui| {
                if widgets::fader(ui, t, Vec2::new(26.0, 90.0), &mut master, false).on_hover_text(format!("{base}.master")).changed() {
                    st.edit(&key, Value::from(master), now);
                    out.push(Op::Set { address: format!("{base}.master"), value: Value::from(master) });
                }
                ui.label(RichText::new(format!("{:.0}%", master * 100.0)).small().monospace());
            });
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new("cue").color(t.fg_dim));
                    ui.label(RichText::new(current.as_deref().unwrap_or("—")).strong().color(if playing { t.tally_program() } else { t.fg }));
                    ui.label(RichText::new("next").color(t.fg_dim));
                    ui.label(RichText::new(next.as_deref().unwrap_or("—")).strong().color(t.tally_preview()));
                    let (el, tot) = (num(cl, "elapsed_ms").unwrap_or(0.0), num(cl, "total_ms").unwrap_or(0.0));
                    if tot > 0.0 {
                        ui.label(RichText::new(format!("{} / {}", fmt_ms(Some(el)), fmt_ms(Some(tot)))).small().monospace().color(t.fg_dim));
                    }
                });
                let progress = num(cl, "progress").unwrap_or(0.0) as f32;
                progress_bar(ui, t, 320.0, progress, if playing { t.accent } else { t.muted });
                ui.horizontal(|ui| {
                    let args = || Value::map().with("cuelist", name.as_str());
                    if ui.add(egui::Button::new(RichText::new(format!("{} GO", icon::PLAY)).strong().color(widgets::contrast(t.green))).fill(t.green)).clicked()
                    {
                        out.push(act("lights.go", args()));
                    }
                    if ui.button(RichText::new("BACK").strong()).clicked() {
                        out.push(act("lights.back", args()));
                    }
                    if ui.button(RichText::new(format!("{} RELEASE", icon::STOP)).strong().color(t.bright_red)).clicked() {
                        out.push(act("lights.release", args()));
                    }
                });
            });
        });
        ui.add_space(4.0);
        cue_table(t, ui, out, &name, list(cl, "cues"), current.as_deref());
    });
}

fn cue_table(t: &Theme, ui: &mut Ui, out: &mut Vec<Op>, cuelist: &str, cues: &[Value], current: Option<&str>) {
    if cues.is_empty() {
        empty(ui, t, "empty cue list");
        return;
    }
    let width: f32 = CUE_COLS.iter().map(|(_, w)| w).sum();
    let row = |ui: &mut Ui, cells: &[String], bg: Option<Color32>, fg: Color32, sense: Sense| -> egui::Response {
        let (rect, resp) = ui.allocate_exact_size(Vec2::new(width, 20.0), sense);
        let p = ui.painter_at(rect);
        let fill = if resp.hovered() && sense.senses_click() { Some(t.bg_light) } else { bg };
        if let Some(f) = fill {
            p.rect_filled(rect, CornerRadius::same(3), f);
        }
        let mut x = rect.left() + 4.0;
        for ((_, w), cell) in CUE_COLS.iter().zip(cells) {
            let clip = Rect::from_min_max(Pos2::new(x, rect.top()), Pos2::new(x + w - 6.0, rect.bottom()));
            p.with_clip_rect(clip).text(Pos2::new(x, rect.center().y), Align2::LEFT_CENTER, cell, FontId::proportional(12.0), fg);
            x += w;
        }
        resp
    };
    let head: Vec<String> = CUE_COLS.iter().map(|(h, _)| h.to_uppercase()).collect();
    row(ui, &head, None, t.fg_dim, Sense::hover());
    for c in cues {
        let id = text(c.get_path("id").unwrap_or(&Value::Null));
        let is_cur = current == Some(id.as_str());
        let opt_ms = |k: &str| c.get_path(k).filter(|v| !v.is_null()).and_then(Value::as_f64);
        let mut label = s(c, "label").to_string();
        if c.get_path("block").is_some_and(Value::truthy) {
            label.push_str(" [block]");
        }
        let cells = [
            id.clone(),
            label,
            fmt_ms(opt_ms("fade_ms")),
            fmt_ms(opt_ms("fade_out_ms")),
            fmt_ms(opt_ms("delay_ms")),
            fmt_ms(opt_ms("follow_ms")),
            fmt_ms(opt_ms("wait_ms")),
            strs(c, "effects").join(", "),
            strs(c, "palettes").join(", "),
        ];
        let resp = row(ui, &cells, is_cur.then_some(t.selection), if is_cur { t.fg_bright } else { t.fg }, Sense::click());
        let targets = strs(c, "targets");
        let resp = if targets.is_empty() { resp } else { resp.on_hover_text(format!("targets: {}", targets.join(", "))) };
        if resp.clicked() {
            out.push(act("lights.goto", Value::map().with("cuelist", cuelist).with("cue", id)));
        }
    }
}

// ---------------------------------------------------------------- patch

fn patch_tab(m: &Model, t: &Theme, ui: &mut Ui, out: &mut Vec<Op>, rig: &Rig) {
    if !rig.errors.is_empty() {
        widgets::section(ui, t, icon::WARN, "Config errors");
        for e in &rig.errors {
            ui.label(RichText::new(e).color(t.bright_red));
        }
        ui.add_space(6.0);
    }
    widgets::section(ui, t, icon::DEVICE, "Fixtures");
    if !rig.present {
        empty(ui, t, EMPTY);
    } else if rig.fixtures.is_empty() {
        empty(ui, t, "no fixtures patched");
    } else {
        egui::Grid::new("lights_fixtures").striped(true).num_columns(7).show(ui, |ui| {
            for h in ["id", "label", "profile", "mode", "univ.addr", "footprint", "heads"] {
                ui.label(RichText::new(h).strong().color(t.fg_dim));
            }
            ui.end_row();
            for f in &rig.fixtures {
                ui.label(RichText::new(&f.id).monospace());
                ui.label(&f.label);
                ui.label(if f.profile_name.is_empty() { &f.profile } else { &f.profile_name }).on_hover_text(&f.profile);
                ui.label(&f.mode);
                let end = f.address + f.footprint.saturating_sub(1);
                ui.label(RichText::new(format!("{}.{:03}", f.universe, f.address)).monospace()).on_hover_text(format!("channels {}–{end}", f.address));
                ui.label(RichText::new(f.footprint.to_string()).monospace());
                let heads = rig.fixture_heads(f);
                if heads.len() == 1 {
                    ui.label(RichText::new("1").monospace());
                } else {
                    ui.label(RichText::new(heads.len().to_string()).monospace()).on_hover_text(heads.join(", "));
                }
                ui.end_row();
            }
        });
    }
    ui.add_space(8.0);
    widgets::section(ui, t, icon::LINK, "Groups");
    if rig.groups.is_empty() {
        empty(ui, t, if rig.present { "no groups (`all` is implicit)" } else { EMPTY });
    } else {
        egui::Grid::new("lights_groups").striped(true).num_columns(2).show(ui, |ui| {
            for (g, heads) in &rig.groups {
                ui.label(RichText::new(g).strong());
                ui.label(RichText::new(heads.join(", ")).small().color(t.fg_dim));
                ui.end_row();
            }
        });
    }
    ui.add_space(8.0);
    widgets::section(ui, t, icon::LIVE, "Outputs");
    if rig.outputs.is_empty() {
        empty(ui, t, if rig.present { "no outputs configured" } else { EMPTY });
    }
    for o in &rig.outputs {
        ui.horizontal(|ui| {
            let status = s(o, "status");
            let enabled = o.get_path("enabled").is_none_or(Value::truthy);
            widgets::pill(
                ui,
                t,
                icon::DEVICE,
                &format!("{} ({}) {status}", s(o, "id"), s(o, "kind")),
                if enabled { health_led(status) } else { LedState::Idle },
            );
            ui.label(RichText::new(s(o, "detail")).color(t.fg_dim));
        });
    }
    ui.add_space(8.0);
    rdm_section(m, t, ui, out);
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

fn rdm_section(m: &Model, t: &Theme, ui: &mut Ui, out: &mut Vec<Op>) {
    widgets::section(ui, t, icon::SEARCH, "RDM");
    let rdm = m.q("lights.rdm");
    ui.horizontal(|ui| {
        match rdm {
            Some(r) => {
                ui.label(RichText::new(format!("status: {}", s(r, "status"))).strong());
                let opt = |k: &str| r.get_path(k).filter(|v| !v.is_null()).map(text).unwrap_or_else(|| "—".into());
                ui.label(RichText::new(format!("firmware {} · serial {}", opt("firmware"), opt("serial"))).color(t.fg_dim));
            }
            None => empty(ui, t, "no RDM data"),
        }
        if ui.button(format!("{} Discover", icon::SEARCH)).on_hover_text("lights.rdm.discover").clicked() {
            out.push(act("lights.rdm.discover", Value::Null));
        }
    });
    let devices = rdm.map(|r| list(r, "devices")).unwrap_or(&[]);
    if devices.is_empty() {
        if rdm.is_some() {
            empty(ui, t, "no RDM devices discovered");
        }
        return;
    }
    egui::Grid::new("lights_rdm").striped(true).num_columns(7).show(ui, |ui| {
        for h in ["uid", "manufacturer", "model", "label", "footprint", "address", "personality"] {
            ui.label(RichText::new(h).strong().color(t.fg_dim));
        }
        ui.end_row();
        for d in devices {
            let o = |k: &str| d.get_path(k).filter(|v| !v.is_null()).map(text).unwrap_or_else(|| "—".into());
            ui.label(RichText::new(uid_text(d.get_path("uid"))).monospace());
            ui.label(o("manufacturer"));
            ui.label(o("model"));
            ui.label(o("label"));
            ui.label(RichText::new(o("footprint")).monospace());
            ui.label(RichText::new(o("start_address")).monospace());
            ui.label(RichText::new(format!("{}/{}", o("personality"), o("personalities"))).monospace());
            ui.end_row();
        }
    });
}

// ---------------------------------------------------------------- palettes

fn palettes_tab(m: &Model, t: &Theme, ui: &mut Ui, out: &mut Vec<Op>, rig: &Rig, heads: &[String]) {
    widgets::section(ui, t, PALETTE_ICON, "Palettes");
    let pals = m.q_list("lights.palettes");
    if pals.is_empty() {
        empty(ui, t, if m.q("lights.palettes").is_some() { "no palettes — add lights/palettes/*.toml or store one from the Programmer" } else { EMPTY });
        return;
    }
    ui.label(
        RichText::new(if heads.is_empty() {
            "select fixtures (Stage or Programmer) to apply a palette"
        } else {
            "click a palette to apply it to the selection"
        })
        .small()
        .color(t.fg_dim),
    );
    ui.horizontal_wrapped(|ui| {
        for pal in pals {
            let name = s(pal, "name");
            let kind = s(pal, "kind");
            let label = if s(pal, "label").is_empty() { name } else { s(pal, "label") };
            let values = palette_values(pal, rig, heads);
            let swatch = pal
                .get_path("set")
                .and_then(Value::as_map)
                .and_then(|set| set.get("all").into_iter().chain(set.values()).find_map(|attrs| attrs.get_path("color").and_then(color_of)));
            let size = Vec2::new(150.0, 92.0);
            let (rect, resp) = ui.allocate_exact_size(size, Sense::click());
            let p = ui.painter_at(rect);
            let usable = !values.is_empty();
            let base = match (kind, swatch) {
                ("color", Some(c)) | ("any", Some(c)) => c,
                _ => t.bg_light,
            };
            p.rect_filled(rect, CornerRadius::same(6), if resp.hovered() && usable { base.gamma_multiply(1.15) } else { base });
            p.rect_stroke(
                rect,
                CornerRadius::same(6),
                Stroke::new(if resp.hovered() && usable { 2.0 } else { 1.0 }, if usable { t.accent } else { t.muted }),
                StrokeKind::Inside,
            );
            let fg = if base == t.bg_light { t.fg } else { widgets::contrast(base) };
            p.text(rect.left_top() + Vec2::new(8.0, 6.0), Align2::LEFT_TOP, label, FontId::proportional(13.0), fg);
            p.text(rect.right_top() + Vec2::new(-8.0, 7.0), Align2::RIGHT_TOP, kind, FontId::proportional(10.0), fg.gamma_multiply(0.7));
            let summary: Vec<String> = pal
                .get_path("set")
                .and_then(Value::as_map)
                .map(|set| {
                    set.iter()
                        .flat_map(|(tg, attrs)| attrs.as_map().into_iter().flatten().map(move |(a, v)| format!("{tg}.{a} = {}", text(plain(v)))))
                        .collect()
                })
                .unwrap_or_default();
            for (i, line) in summary.iter().take(3).enumerate() {
                p.with_clip_rect(rect.shrink(4.0)).text(
                    rect.left_top() + Vec2::new(8.0, 26.0 + 14.0 * i as f32),
                    Align2::LEFT_TOP,
                    line,
                    FontId::monospace(10.0),
                    fg.gamma_multiply(0.85),
                );
            }
            let used = strs(pal, "used_by");
            p.text(
                rect.left_bottom() + Vec2::new(8.0, -5.0),
                Align2::LEFT_BOTTOM,
                format!("used by {}", used.len()),
                FontId::proportional(10.0),
                fg.gamma_multiply(0.7),
            );
            let resp = resp.on_hover_text(format!(
                "{name} ({kind})\n{}\nused by: {}",
                summary.join("\n"),
                if used.is_empty() { "—".to_string() } else { used.join(", ") }
            ));
            if resp.clicked() && usable {
                for (attr, value) in values {
                    out.push(act("lights.programmer.set", Value::map().with("attr", attr).with("value", value)));
                }
            }
        }
    });
    ui.add_space(8.0);
    widgets::section(ui, t, icon::LINK, "Used by");
    egui::Grid::new("lights_palette_usage").striped(true).num_columns(2).show(ui, |ui| {
        for pal in pals {
            let used = strs(pal, "used_by");
            ui.label(RichText::new(s(pal, "name")).strong());
            ui.label(RichText::new(if used.is_empty() { "unused".into() } else { used.join(", ") }).small().color(t.fg_dim));
            ui.end_row();
        }
    });
}

// ---------------------------------------------------------------- programmer

fn prog_set(out: &mut Vec<Op>, attr: &str, value: Value) {
    out.push(act("lights.programmer.set", Value::map().with("attr", attr).with("value", value)));
}

fn programmer_tab(m: &Model, t: &Theme, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>, rig: &Rig, selection: &[String], heads: &[String], now: Instant) {
    widgets::section(ui, t, icon::MIX, "Selection");
    if !rig.present {
        empty(ui, t, EMPTY);
        return;
    }
    let shift = ui.input(|i| i.modifiers.shift);
    let mut chips = |ui: &mut Ui, names: &mut dyn Iterator<Item = (String, String)>| {
        ui.horizontal_wrapped(|ui| {
            for (target, label) in names {
                let on = selection.contains(&target);
                let text = RichText::new(label).color(if on { t.accent } else { t.fg });
                if ui.selectable_label(on, text).on_hover_text("click = select · shift+click = add").clicked() {
                    out.push(act("lights.programmer.select", Value::map().with("targets", vec![target.clone()]).with("add", shift)));
                }
            }
        });
    };
    ui.label(RichText::new("groups").small().color(t.fg_dim));
    chips(ui, &mut rig.group_names().into_iter().map(|g| (g.clone(), g)));
    ui.label(RichText::new("fixtures").small().color(t.fg_dim));
    chips(ui, &mut rig.fixtures.iter().map(|f| (f.id.clone(), if f.label.is_empty() { f.id.clone() } else { format!("{} · {}", f.id, f.label) })));
    ui.add_space(4.0);
    let prog = m.q("lights.programmer");
    let highlight =
        m.get("lights.programmer.highlight").map(Value::truthy).unwrap_or_else(|| prog.and_then(|p| p.get_path("highlight")).is_some_and(Value::truthy));
    ui.horizontal(|ui| {
        let hl = egui::Button::new(RichText::new("Highlight").strong().color(if highlight { widgets::contrast(t.yellow) } else { t.fg })).fill(if highlight {
            t.yellow
        } else {
            t.bg_light
        });
        if ui.add(hl).clicked() {
            out.push(act("lights.programmer.highlight", Value::map().with("on", !highlight)));
        }
        for (label, name) in [("Locate", "lights.programmer.locate"), ("Release", "lights.programmer.release"), ("Clear", "lights.programmer.clear")] {
            if ui.button(label).on_hover_text(name).clicked() {
                out.push(act(name, Value::Null));
            }
        }
        let active = m.b("lights.programmer.active");
        widgets::pill(ui, t, icon::LIVE, if active { "programmer active" } else { "programmer idle" }, if active { LedState::Active } else { LedState::Idle });
        ui.label(RichText::new(format!("{} heads", heads.len())).small().color(t.fg_dim));
    });
    ui.separator();
    if heads.is_empty() {
        empty(ui, t, "select fixtures (chips above or click on Stage) to edit attributes");
    } else {
        attribute_controls(m, t, st, ui, out, rig, prog, heads, now);
    }
    ui.separator();
    store_row(m, t, st, ui, out);
}

/// Current value shown for a programmer attribute: local edit → programmer value → live state.
fn attr_value<'a>(m: &'a Model, st: &'a LightsState, prog: Option<&'a Value>, head: &str, attr: &str, now: Instant) -> Option<&'a Value> {
    let addr = format!("lights.{head}.{attr}");
    st.edited(attr, now)
        .or_else(|| prog.and_then(|p| p.get_path("values")).and_then(Value::as_map).and_then(|v| v.get(&addr)))
        .or_else(|| m.get(&addr))
        .map(plain)
}

fn attribute_controls(
    m: &Model,
    t: &Theme,
    st: &mut LightsState,
    ui: &mut Ui,
    out: &mut Vec<Op>,
    rig: &Rig,
    prog: Option<&Value>,
    heads: &[String],
    now: Instant,
) {
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
        // intensity: every head has one (virtual dimmer when unpatched)
        ui.vertical(|ui| {
            ui.label(RichText::new("intensity").small().color(t.fg_dim));
            let mut v = f32_of(st, "intensity", 0.0);
            if widgets::fader(ui, t, Vec2::new(36.0, 160.0), &mut v, false).changed() {
                st.edit("intensity", Value::from(v), now);
                prog_set(out, "intensity", Value::from(v));
            }
            ui.label(RichText::new(format!("{:.0}%", v * 100.0)).monospace());
            ui.horizontal(|ui| {
                for (l, v) in [("0", 0.0f32), ("50", 0.5), ("FL", 1.0)] {
                    if ui.small_button(l).clicked() {
                        st.edit("intensity", Value::from(v), now);
                        prog_set(out, "intensity", Value::from(v));
                    }
                }
            });
        });
        if has("color") {
            ui.separator();
            ui.vertical(|ui| {
                ui.label(RichText::new("color").small().color(t.fg_dim));
                let cur = attr_value(m, st, prog, first, "color", now).and_then(color_of).unwrap_or(Color32::WHITE);
                let mut hsva = st.hsva.filter(|(_, at)| now.saturating_duration_since(*at) < EDIT_HOLD).map(|(h, _)| h).unwrap_or_else(|| Hsva::from(cur));
                ui.spacing_mut().slider_width = 160.0;
                if egui::color_picker::color_picker_hsva_2d(ui, &mut hsva, egui::color_picker::Alpha::Opaque) {
                    let c = Color32::from(hsva);
                    st.hsva = Some((hsva, now));
                    st.edit("color", Value::from(hex_of(c)), now);
                    prog_set(out, "color", Value::from(hex_of(c)));
                }
                ui.horizontal_wrapped(|ui| {
                    for c in [Color32::WHITE, t.red, t.orange, t.yellow, t.green, t.cyan, t.blue, t.magenta, t.accent] {
                        let (r, resp) = ui.allocate_exact_size(Vec2::splat(18.0), Sense::click());
                        ui.painter().rect_filled(r, CornerRadius::same(3), c);
                        ui.painter().rect_stroke(
                            r,
                            CornerRadius::same(3),
                            Stroke::new(1.0, if resp.hovered() { t.accent } else { t.muted }),
                            StrokeKind::Inside,
                        );
                        if resp.on_hover_text(hex_of(c)).clicked() {
                            st.hsva = None;
                            st.edit("color", Value::from(hex_of(c)), now);
                            prog_set(out, "color", Value::from(hex_of(c)));
                        }
                    }
                });
            });
        }
        if has("pan") || has("tilt") {
            ui.separator();
            ui.vertical(|ui| {
                ui.label(RichText::new("pan / tilt").small().color(t.fg_dim));
                let before = [f32_of(st, "pan", 0.5), f32_of(st, "tilt", 0.5)];
                let mut v = before;
                if xy_pad(ui, t, 160.0, &mut v).changed() {
                    for (i, a) in ["pan", "tilt"].into_iter().enumerate() {
                        if has(a) && (v[i] - before[i]).abs() > f32::EPSILON {
                            st.edit(a, Value::from(v[i]), now);
                            prog_set(out, a, Value::from(v[i]));
                        }
                    }
                }
                ui.label(RichText::new(format!("pan {:.3} · tilt {:.3}", v[0], v[1])).monospace().small());
                if ui.small_button("centre").clicked() {
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
            ui.separator();
            ui.vertical(|ui| {
                for a in unit {
                    let mut v = f32_of(st, a, 0.0);
                    if hbar(ui, t, 220.0, &mut v, a).changed() {
                        st.edit(a, Value::from(v), now);
                        prog_set(out, a, Value::from(v));
                    }
                }
                for a in ints {
                    ui.horizontal(|ui| {
                        let max = if a == "gobo" { 63 } else { 255 };
                        let mut v = attr_value(m, st, prog, first, a, now).and_then(Value::as_i64).unwrap_or(0).clamp(0, max);
                        ui.label(RichText::new(a.as_str()).small());
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

fn store_row(m: &Model, t: &Theme, st: &mut LightsState, ui: &mut Ui, out: &mut Vec<Op>) {
    widgets::section(ui, t, icon::CHECK, "Store");
    let lists: Vec<String> = m.q_list("lights.cuelists").iter().map(|c| s(c, "name").to_string()).filter(|n| !n.is_empty()).collect();
    if st.store_cuelist.is_empty() || !lists.contains(&st.store_cuelist) {
        st.store_cuelist = lists.first().cloned().unwrap_or_default();
    }
    if st.store_kind.is_empty() {
        st.store_kind = "any".into();
    }
    ui.horizontal_wrapped(|ui| {
        ui.add(egui::TextEdit::singleline(&mut st.store_name).desired_width(160.0).hint_text("palette / preset name"));
        let name = st.store_name.trim().to_string();
        egui::ComboBox::from_id_salt("lights_store_kind").selected_text(format!("kind: {}", st.store_kind)).show_ui(ui, |ui| {
            for k in PALETTE_KINDS {
                ui.selectable_value(&mut st.store_kind, k.to_string(), k);
            }
        });
        if ui.add_enabled(!name.is_empty(), egui::Button::new("Store palette")).clicked() {
            out.push(act("lights.programmer.store", Value::map().with("palette", name.as_str()).with("kind", st.store_kind.as_str())));
        }
        if ui.add_enabled(!name.is_empty(), egui::Button::new("Store preset")).clicked() {
            out.push(act("lights.programmer.store", Value::map().with("preset", name.as_str())));
        }
        ui.separator();
        egui::ComboBox::from_id_salt("lights_store_cuelist")
            .selected_text(if st.store_cuelist.is_empty() { "cue list".to_string() } else { st.store_cuelist.clone() })
            .show_ui(ui, |ui| {
                for l in &lists {
                    ui.selectable_value(&mut st.store_cuelist, l.clone(), l);
                }
            });
        ui.add(egui::TextEdit::singleline(&mut st.store_cue).desired_width(60.0).hint_text("cue id"));
        let cue = st.store_cue.trim().to_string();
        if ui.add_enabled(!st.store_cuelist.is_empty() && !cue.is_empty(), egui::Button::new("Store cue")).clicked() {
            out.push(act("lights.programmer.store", Value::map().with("cuelist", st.store_cuelist.as_str()).with("cue", cue.as_str())));
        }
    });
}

// ---------------------------------------------------------------- DMX monitor

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

fn dmx_tab(t: &Theme, st: &mut LightsState, ui: &mut Ui, rig: &Rig, output: Option<&Value>) {
    let us = universes(rig, output);
    if us.is_empty() {
        empty(ui, t, EMPTY);
        return;
    }
    let universe = st.universe.filter(|u| us.contains(u)).unwrap_or(us[0]);
    st.universe = Some(universe);
    ui.horizontal(|ui| {
        ui.label(RichText::new("universe").color(t.fg_dim));
        for u in &us {
            if ui.selectable_label(*u == universe, RichText::new(u.to_string()).strong()).clicked() {
                st.universe = Some(*u);
            }
        }
        if output.is_none() {
            ui.label(RichText::new("no live output").small().color(t.yellow));
        }
    });
    let data = output.and_then(|o| o.get_path("universes")).and_then(|m| m.get_path(&universe.to_string())).and_then(Value::as_list).unwrap_or(&[]);
    let value = |ch: usize| data.get(ch).and_then(Value::as_i64).unwrap_or(0).clamp(0, 255) as u8;
    let owners = channel_owners(rig, universe);
    let tint = tints(t);
    let avail = ui.available_width().max(320.0);
    let cell = Vec2::new((avail / 32.0).floor(), 22.0);
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(cell.x * 32.0, cell.y * 16.0), Sense::hover());
    let p = ui.painter_at(rect);
    let font = FontId::monospace(if cell.x >= 26.0 { 10.0 } else { 8.0 });
    for ch in 0..512 {
        let (col, row) = (ch % 32, ch / 32);
        let r = Rect::from_min_size(rect.min + Vec2::new(col as f32 * cell.x, row as f32 * cell.y), cell).shrink(0.5);
        let v = value(ch);
        let owner = owners[ch];
        let fix_tint = owner.map(|i| tint[i as usize % tint.len()]);
        p.rect_filled(r, CornerRadius::same(2), fix_tint.map(|c| c.gamma_multiply(0.22)).unwrap_or(t.bg_darker));
        if v > 0 {
            let h = r.height() * v as f32 / 255.0;
            p.rect_filled(
                Rect::from_min_max(Pos2::new(r.left(), r.bottom() - h), r.right_bottom()),
                CornerRadius::same(2),
                fix_tint.unwrap_or(t.fg).gamma_multiply(0.35 + 0.5 * v as f32 / 255.0),
            );
        }
        if let Some(i) = owner
            && (ch == 0 || owners[ch - 1] != Some(i))
        {
            p.line_segment([r.left_top(), r.left_bottom()], Stroke::new(2.0, tint[i as usize % tint.len()]));
        }
        if cell.x >= 18.0 {
            p.text(r.center(), Align2::CENTER_CENTER, v.to_string(), font.clone(), if v > 140 { t.fg_bright } else { t.fg_dim });
        }
    }
    if let Some(pos) = resp.hover_pos() {
        let col = ((pos.x - rect.left()) / cell.x).floor() as i64;
        let row = ((pos.y - rect.top()) / cell.y).floor() as i64;
        if (0..32).contains(&col) && (0..16).contains(&row) {
            let ch = (row * 32 + col) as usize;
            let who = owners[ch].and_then(|i| rig.fixtures.get(i as usize)).map(|f| {
                let off = ch + 1 - f.address as usize;
                let role =
                    rig.channels.get(&f.profile).and_then(|modes| modes.get(&f.mode)).and_then(|c| c.get(off)).map(|r| format!(" · {r}")).unwrap_or_default();
                format!("{}{role}", if f.label.is_empty() { &f.id } else { &f.label })
            });
            resp.on_hover_text_at_pointer(format!("ch {} = {} ({})", ch + 1, value(ch), who.as_deref().unwrap_or("unpatched")));
        }
    }
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        for (i, f) in rig.fixtures.iter().enumerate().filter(|(_, f)| f.universe == universe) {
            let c = tint[i % tint.len()];
            ui.label(RichText::new(format!("■ {} {}–{}", f.id, f.address, f.address + f.footprint.saturating_sub(1))).small().color(c));
        }
    });
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
        let set: BTreeMap<String, Value> = [("all".to_string(), Value::map().with("color", "#ffb070"))].into_iter().collect();
        let palette = Value::map().with("name", "warm").with("label", "Warm").with("kind", "color").with("set", set).with("used_by", vec!["main/1"]);
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
        q.insert("lights.cuelists".into(), Value::from(vec![cl]));
        q.insert("lights.palettes".into(), Value::from(vec![palette]));
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

    fn frame(ctx: &egui::Context, m: &Model, st: &mut LightsState, raw: egui::RawInput) -> Vec<Op> {
        let t = Theme::default();
        let mut out = Vec::new();
        ctx.run_ui(raw, |ui| view(m, &t, st, ui, &mut out, Instant::now())).drop_without_applying_deltas();
        out
    }

    fn click(p: Pos2, pressed: bool, shift: bool) -> egui::Event {
        egui::Event::PointerButton { pos: p, button: egui::PointerButton::Primary, pressed, modifiers: egui::Modifiers { shift, ..Default::default() } }
    }

    /// Run `ui()` on a real (headless) App with the given tab selected.
    fn run_app_tab(ctx: &egui::Context, app: &mut App, tab: Tab) {
        ctx.run_ui(input(Vec::new(), false), |ui| {
            let sid = ui.make_persistent_id("lights_view");
            ui.data_mut(|d| d.get_temp_mut_or_default::<LightsState>(sid).tab = tab);
            super::ui(app, ui);
            assert_eq!(ui.data_mut(|d| d.get_temp_mut_or_default::<LightsState>(sid).tab), tab, "view state survives the frame");
        })
        .drop_without_applying_deltas();
    }

    /// `ui()` runs headless on a real App for every tab: no data, full data, and malformed data.
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
            for (tab, _, _) in Tab::ALL {
                run_app_tab(&ctx, &mut app, tab);
                run_app_tab(&ctx, &mut app, tab);
            }
        }
        for q in ["lights.rig", "lights.cuelists", "lights.palettes", "lights.effects", "lights.programmer", "lights.output", "lights.rdm"] {
            app.m.queries.insert(q.into(), Value::from("garbage"));
        }
        app.m.state.insert("lights.programmer.selection".into(), Value::from(42));
        app.m.state.insert("health.dmx".into(), Value::from(vec![1, 2]));
        for (tab, _, _) in Tab::ALL {
            run_app_tab(&ctx, &mut app, tab);
        }
    }

    /// Press + release on a head of the stage emits `lights.programmer.select` for its fixture.
    #[test]
    fn stage_click_selects_fixture_and_shift_adds() {
        let mut m = model();
        payload(&mut m);
        let rig = Rig::parse(m.q("lights.rig"));
        let output = m.q("lights.output");
        let heads = stage_heads(&rig, output);
        let t = Theme::default();
        let ctx = egui::Context::default();
        let screen = Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 900.0));
        let par2 = heads.iter().find(|h| h.id == "par2").map(|h| h.pos).expect("par2 on stage");
        let p = to_screen(stage_rect(screen), par2);
        let stage_frame = |events: Vec<egui::Event>, shift: bool| {
            let mut out = Vec::new();
            ctx.run_ui(input(events, shift), |ui| stage_tab(&t, ui, &mut out, &rig, output, &[])).drop_without_applying_deltas();
            out
        };
        assert!(stage_frame(Vec::new(), false).is_empty());
        let mut got = Vec::new();
        for shift in [false, true] {
            assert!(stage_frame(vec![egui::Event::PointerMoved(p), click(p, true, shift)], shift).is_empty());
            got.extend(stage_frame(vec![click(p, false, shift)], shift));
        }
        assert_eq!(
            got,
            vec![
                act("lights.programmer.select", Value::map().with("targets", vec!["par2"]).with("add", false)),
                act("lights.programmer.select", Value::map().with("targets", vec!["par2"]).with("add", true)),
            ]
        );
        // clicking empty floor selects nothing
        let floor = to_screen(stage_rect(screen), [0.5, 0.5]);
        stage_frame(vec![egui::Event::PointerMoved(floor), click(floor, true, false)], false);
        assert!(stage_frame(vec![click(floor, false, false)], false).is_empty());
    }

    /// Rendering alone (every tab, full data, fader values ≠ defaults) never sends commands.
    #[test]
    fn view_emits_nothing_without_interaction() {
        let mut m = model();
        payload(&mut m);
        let ctx = egui::Context::default();
        let mut st = LightsState::default();
        for (tab, _, _) in Tab::ALL {
            st.tab = tab;
            assert!(frame(&ctx, &m, &mut st, input(Vec::new(), false)).is_empty(), "{tab:?} sent a command without input");
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
    fn durations_format_for_cue_columns() {
        assert_eq!(fmt_ms(None), "—");
        assert_eq!(fmt_ms(Some(0.0)), "0");
        assert_eq!(fmt_ms(Some(250.0)), "250ms");
        assert_eq!(fmt_ms(Some(2000.0)), "2s");
        assert_eq!(fmt_ms(Some(1500.0)), "1.5s");
        assert_eq!(uid_text(Some(&Value::from(0x454E_0405_589Ai64))), "454E:0405589A");
    }
}
