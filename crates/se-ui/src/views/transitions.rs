//! Scenes → Transitions: how program changes from one scene to the next. A list on the left,
//! the selected thing on the right:
//! - "How scenes switch": the project-wide default (`[transitions]` in `project.toml`) and the
//!   exceptions: a scene's own choice (`[transitions]` in `scenes/<s>.toml`) or a scene pair's
//!   (`[transitions.from.<scene>]` in the target scene's file);
//! - every transition (the built-ins, `transitions/*.toml`, custom shader transitions), each
//!   with an animated sketch, where it's used (and a switch for the default), its settings, and
//!   "Try it" (off air);
//! - "New custom transition": a shader patch from the transition template plus a
//!   `transitions/<id>.toml` that names it, so scenes can pick it.
//!
//! A choice is "pick at random from …" (a pool) or "always use …" (a fixed `name`). Files are
//! edited as text with `toml_edit` (comments survive) and written with `project.write`. The
//! scene inspector shows one scene's choice through [`scene_choice`].

use crate::app::App;
use crate::frames::Canvas;
use crate::views::live::nice;
use crate::views::{composition, patches, scene_edit, status};
use anyhow::{Context as _, Result, bail};
use egui::{Color32, Painter, Pos2, Rect, RichText, Sense, Stroke, StrokeKind, pos2, vec2};
use se_core::transitions as catalog;
use se_proto::{Ease, Op, Value};
use se_ui_kit::Theme;
use se_ui_kit::theme::{font_medium, font_mono, mix, radius, spacing, type_scale};
use se_ui_kit::widgets::{self, Kind, LedState, Size, Tone, icon};
use std::collections::BTreeMap;
use toml_edit::{Array, DocumentMut, InlineTable, Item as TomlItem, Table, TableLike};

/// Transitions that exist without a file (se_core `BUILTIN_TRANSITIONS`).
const BUILTIN: &[&str] = se_core::config::BUILTIN_TRANSITIONS;
/// Speed of a transition with no `ms` (the core's default).
const DEFAULT_MS: u64 = 700;
/// How things that are only in one of the scenes come and go (engine style names).
const STYLES: [&str; 7] = ["fade", "scale", "slide_left", "slide_right", "slide_up", "slide_down", "none"];
const PROJECT: &str = "project.toml";
/// Files are read again this often while shown (someone else may have changed them).
const REREAD: f64 = 5.0;

#[derive(Default)]
pub struct TransitionsState {
    last_poll: f64,
    files: BTreeMap<String, FileState>,
    try_from: Option<String>,
    try_to: Option<String>,
    adding: Option<Adding>,
    /// What the detail pane shows (`None` = how scenes switch).
    sel: Option<Sel>,
    /// Keep a just-made transition selected until the engine lists it.
    hold_until: f64,
    /// Name typed for a new custom transition.
    new_name: String,
}

/// A project file as read here, with our latest write shown until the engine's copy is read again.
#[derive(Default)]
struct FileState {
    asked: bool,
    asked_at: f64,
    /// Replies received for this file when it was last asked for.
    seq_at_ask: u64,
    /// Our written text, and the reply number that carries it (`None` until re-asked).
    local: Option<(String, Option<u64>)>,
}

/// The "Add an exception" form.
#[derive(Clone, Default)]
struct Adding {
    /// 0 = switching to a scene, 1 = between two scenes.
    pair: usize,
    from: String,
    to: String,
}

// ---- choices -------------------------------------------------------------------------------------

/// How the project default, a scene, or a scene pair picks its transition.
#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    /// Nothing of its own: the level above decides (for the project default: a Crossfade).
    Inherit,
    /// A weighted pick (weights kept as written), skipping the last `avoid_repeat` ones.
    Random {
        pool: Vec<(String, f64)>,
        avoid_repeat: usize,
    },
    Always(String),
}

impl Choice {
    fn mentions(&self, id: &str) -> bool {
        match self {
            Choice::Inherit => false,
            Choice::Random { pool, .. } => pool.iter().any(|(n, _)| n == id),
            Choice::Always(n) => n == id,
        }
    }

    fn first(&self) -> Option<&str> {
        match self {
            Choice::Inherit => None,
            Choice::Random { pool, .. } => pool.first().map(|(n, _)| n.as_str()),
            Choice::Always(n) => Some(n),
        }
    }
}

/// `[transitions]` (`from = None`) or `[transitions.from.<from>]`.
fn table_at<'a>(doc: &'a DocumentMut, from: Option<&str>) -> Option<&'a dyn TableLike> {
    let t = doc.get("transitions")?.as_table_like()?;
    match from {
        None => Some(t),
        Some(f) => t.get("from")?.as_table_like()?.get(f)?.as_table_like(),
    }
}

fn choice_in(t: &dyn TableLike) -> Choice {
    if let Some(n) = t.get("name").and_then(TomlItem::as_str) {
        return Choice::Always(n.to_string());
    }
    let pool: Vec<(String, f64)> = t
        .get("pool")
        .and_then(TomlItem::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| match v {
                    toml_edit::Value::String(s) => Some((s.value().clone(), 1.0)),
                    toml_edit::Value::InlineTable(e) => {
                        let w = e.get("w").and_then(|w| w.as_float().or_else(|| w.as_integer().map(|i| i as f64))).unwrap_or(1.0);
                        e.get("name").and_then(toml_edit::Value::as_str).map(|n| (n.to_string(), w))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    if pool.is_empty() {
        return Choice::Inherit;
    }
    let avoid_repeat = t.get("avoid_repeat").and_then(TomlItem::as_integer).map_or(0, |i| i.max(0) as usize);
    Choice::Random { pool, avoid_repeat }
}

/// The choice written in a project or scene file (`from`: a scene pair's).
pub fn read_choice(text: &str, from: Option<&str>) -> Choice {
    let Ok(doc) = text.parse::<DocumentMut>() else { return Choice::Inherit };
    table_at(&doc, from).map_or(Choice::Inherit, choice_in)
}

/// Scene pairs in a scene file with a choice of their own: (the scene they come from, choice).
pub fn pair_choices(text: &str) -> Vec<(String, Choice)> {
    let Ok(doc) = text.parse::<DocumentMut>() else { return Vec::new() };
    let Some(from) = doc.get("transitions").and_then(TomlItem::as_table_like).and_then(|t| t.get("from")).and_then(TomlItem::as_table_like) else {
        return Vec::new();
    };
    from.iter().filter_map(|(k, v)| Some((k.to_string(), choice_in(v.as_table_like()?)))).filter(|(_, c)| *c != Choice::Inherit).collect()
}

/// The sub-table `key` of `item`, made (as the same kind of table) when missing.
fn sub<'a>(item: &'a mut TomlItem, key: &str, implicit: bool) -> Result<&'a mut TomlItem> {
    let fresh = match item {
        TomlItem::Table(_) => {
            let mut t = Table::new();
            t.set_implicit(implicit);
            TomlItem::Table(t)
        }
        TomlItem::Value(toml_edit::Value::InlineTable(_)) => TomlItem::Value(InlineTable::new().into()),
        _ => bail!("`{key}` sits in something that isn't a table"),
    };
    let t = item.as_table_like_mut().context("not a table")?;
    let it = t.entry(key).or_insert(fresh);
    if it.as_table_like().is_none() {
        bail!("`{key}` isn't a table");
    }
    Ok(it)
}

/// Set a key, keeping the comment after an existing value.
fn put(t: &mut dyn TableLike, key: &str, v: toml_edit::Value) {
    match t.get_mut(key) {
        Some(TomlItem::Value(old)) => {
            let decor = old.decor().clone();
            *old = v;
            *old.decor_mut() = decor;
        }
        _ => {
            t.insert(key, TomlItem::Value(v));
        }
    }
}

fn pool_array(pool: &[(String, f64)]) -> toml_edit::Value {
    let mut a = Array::new();
    for (n, w) in pool {
        let mut e = InlineTable::new();
        e.insert("name", n.as_str().into());
        if (*w - 1.0).abs() > f64::EPSILON {
            e.insert("w", if w.fract() == 0.0 { toml_edit::Value::from(*w as i64) } else { toml_edit::Value::from(*w) });
        }
        a.push(toml_edit::Value::InlineTable(e));
    }
    toml_edit::Value::Array(a)
}

const CHOICE_KEYS: [&str; 3] = ["pool", "name", "avoid_repeat"];

/// Write `c` into a project or scene file (`from`: a scene pair's table). Other keys (speed,
/// lights, other pairs) and comments stay; tables left empty by [`Choice::Inherit`] go away.
pub fn set_choice(text: &str, from: Option<&str>, c: &Choice) -> Result<String> {
    let mut doc = text.parse::<DocumentMut>().context("the file isn't valid TOML")?;
    if *c == Choice::Inherit {
        clear(&mut doc, from);
        return Ok(doc.to_string());
    }
    let fresh = doc.get("transitions").is_none();
    let mut item = sub(doc.as_item_mut(), "transitions", fresh && from.is_some())?;
    if let Some(f) = from {
        item = sub(sub(item, "from", true)?, f, false)?;
    }
    let t = item.as_table_like_mut().context("not a table")?;
    match c {
        Choice::Always(n) => {
            t.remove("pool");
            t.remove("avoid_repeat");
            put(t, "name", n.as_str().into());
        }
        Choice::Random { pool, avoid_repeat } => {
            t.remove("name");
            put(t, "pool", pool_array(pool));
            if *avoid_repeat == 0 {
                t.remove("avoid_repeat");
            } else {
                put(t, "avoid_repeat", (*avoid_repeat as i64).into());
            }
        }
        Choice::Inherit => {}
    }
    Ok(doc.to_string())
}

fn clear(doc: &mut DocumentMut, from: Option<&str>) {
    let Some(tr) = doc.get_mut("transitions").and_then(TomlItem::as_table_like_mut) else { return };
    match from {
        None => {
            for k in CHOICE_KEYS {
                tr.remove(k);
            }
        }
        Some(f) => {
            if let Some(pairs) = tr.get_mut("from").and_then(TomlItem::as_table_like_mut) {
                if let Some(p) = pairs.get_mut(f).and_then(TomlItem::as_table_like_mut) {
                    for k in CHOICE_KEYS {
                        p.remove(k);
                    }
                    if p.is_empty() {
                        pairs.remove(f);
                    }
                }
                if pairs.is_empty() {
                    tr.remove("from");
                }
            }
        }
    }
    if tr.is_empty() {
        doc.remove("transitions");
    }
}

// ---- looks ---------------------------------------------------------------------------------------

/// What a transition looks like, for words and the sketch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    Cut,
    Fade,
    Glide,
    Zoom,
    Glitch,
    GlideGlitch,
    GlideZoom,
    Own,
}

/// The built-in shader a transition uses (a project copy of one counts: same look).
fn shader_base(shader: Option<&str>) -> Option<&str> {
    let s = shader?;
    if s.starts_with("patch.") {
        return Some(s);
    }
    if s.ends_with(".wgsl") { std::path::Path::new(s).file_stem().and_then(|x| x.to_str()) } else { Some(s) }
}

fn look_of(kind: &str, shader: Option<&str>) -> Look {
    match (kind, shader_base(shader)) {
        ("cut", _) => Look::Cut,
        ("morph" | "glide", _) => Look::Glide,
        ("combined", Some("glitch")) => Look::GlideGlitch,
        ("combined", Some("zoomblur")) => Look::GlideZoom,
        ("combined", _) => Look::Glide,
        (_, None | Some("fade")) => Look::Fade,
        (_, Some("glitch")) => Look::Glitch,
        (_, Some("zoomblur")) => Look::Zoom,
        _ => Look::Own,
    }
}

fn style_text(id: &str) -> (&'static str, &'static str) {
    catalog::style(id).map_or(("", ""), |s| (s.label, s.description))
}

fn look_words(l: Look) -> (&'static str, &'static str) {
    match l {
        Look::Cut => ("Cut", "Switches instantly, no animation."),
        Look::Fade => style_text("fade"),
        Look::Glide => style_text("morph"),
        Look::Zoom => style_text("zoomblur"),
        Look::Glitch => style_text("glitch"),
        Look::GlideGlitch => style_text("morph_glitch"),
        Look::GlideZoom => ("Glide + zoom blur", "A glide with a zoom blur over the move."),
        Look::Own => ("Your own look", "A look made as an overlay or drawn by your own shader file."),
    }
}

/// Built-in names without a file: (kind, shader).
fn builtin_def(id: &str) -> Option<(&'static str, Option<&'static str>)> {
    match id {
        "cut" => Some(("cut", None)),
        "morph" => Some(("morph", None)),
        "fade" => Some(("shader", Some("fade"))),
        _ => None,
    }
}

fn builtin_label(id: &str) -> Option<&'static str> {
    match id {
        "cut" => Some("Cut"),
        "morph" => Some("Glide"),
        "fade" => Some("Crossfade"),
        _ => None,
    }
}

/// Keep `config.transitions` (every transition file, as the engine read it) fresh while a view
/// that shows transitions is open.
pub fn poll(app: &mut App, now: f64) {
    let never = app.m.q_seq("config.transitions") == 0;
    let st = &mut app.build.transitions;
    if app.m.connected && (now - st.last_poll > 2.0 || (never && now - st.last_poll > 0.5)) {
        st.last_poll = now;
        app.m.query("config.transitions", Value::Null);
    }
}

fn defs(app: &App) -> Option<&std::collections::BTreeMap<String, Value>> {
    app.m.q("config.transitions").and_then(Value::as_map)
}

/// A transition's display name.
pub fn label(app: &App, id: &str) -> String {
    defs(app)
        .and_then(|d| d.get(id))
        .and_then(|d| d.get_path("label"))
        .and_then(Value::as_str)
        .filter(|s| !s.trim().is_empty())
        .map(String::from)
        .or_else(|| builtin_label(id).map(String::from))
        .or_else(|| id.strip_prefix("patch.").map(|p| patches::find(app, p).map_or_else(|| nice(p), |v| patches::label(&v))))
        .unwrap_or_else(|| nice(id))
}

fn text<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get_path(k).and_then(Value::as_str).unwrap_or("")
}

// ---- the sketch ----------------------------------------------------------------------------------

/// Everything the sketch needs to move like the real thing.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Motion {
    look: Look,
    ms: u64,
    ease: Ease,
    enter: &'static str,
    exit: &'static str,
    strength: f32,
    block: f32,
}

impl Motion {
    fn new(look: Look, ms: u64, ease: &str, enter: &str, exit: &str, values: impl Fn(&str) -> Option<f64>) -> Motion {
        let known = |s: &str| STYLES.iter().find(|k| **k == s).copied().unwrap_or("fade");
        let default_strength = if matches!(look, Look::Zoom) { 0.4 } else { 1.0 };
        Motion {
            look,
            ms,
            ease: Ease::parse(ease).unwrap_or(Ease::InOutCubic),
            enter: known(enter),
            exit: known(exit),
            strength: values("strength").unwrap_or(default_strength) as f32,
            block: values("block").unwrap_or(16.0) as f32,
        }
    }
}

/// A sketch scene: background tint and boxes (id, rect 0–1, color index).
struct SketchScene {
    tint: usize,
    boxes: &'static [(u8, [f32; 4], usize)],
}

const SCENE_A: SketchScene = SketchScene { tint: 0, boxes: &[(0, [0.05, 0.14, 0.43, 0.72], 0), (1, [0.52, 0.14, 0.43, 0.72], 1)] };
const SCENE_B: SketchScene = SketchScene { tint: 1, boxes: &[(0, [0.06, 0.08, 0.88, 0.84], 0), (2, [0.64, 0.58, 0.27, 0.3], 2)] };
const VIDEO_BLACK: Color32 = Color32::from_rgb(6, 7, 9);

fn hash(a: u32, b: u32) -> f32 {
    let mut x = a.wrapping_mul(0x9E37_79B1) ^ b.wrapping_mul(0x85EB_CA77);
    x ^= x >> 15;
    x = x.wrapping_mul(0x2C1B_3C6D);
    x ^= x >> 12;
    (x & 0xFFFF) as f32 / 65535.0
}

struct Sketch<'a> {
    p: &'a Painter,
    area: Rect,
    colors: [Color32; 3],
    tints: [Color32; 2],
}

impl Sketch<'_> {
    fn to_screen(&self, r: [f32; 4], zoom: f32, center: Pos2) -> Rect {
        let map = |u: Pos2| {
            let z = pos2((u.x - center.x) * zoom + center.x, (u.y - center.y) * zoom + center.y);
            self.area.min + vec2(z.x * self.area.width(), z.y * self.area.height())
        };
        Rect::from_min_max(map(pos2(r[0], r[1])), map(pos2(r[0] + r[2], r[1] + r[3])))
    }

    /// A "camera": a colored box with a head-and-shoulders shape.
    fn cam(&self, r: Rect, c: Color32, alpha: f32) {
        let alpha = alpha.clamp(0.0, 1.0);
        if alpha <= 0.01 || r.width() < 1.0 {
            return;
        }
        let rr = (r.height().min(r.width()) * 0.08).clamp(1.0, 8.0);
        self.p.rect_filled(r, rr, mix(VIDEO_BLACK, c, 0.72).gamma_multiply(alpha));
        let s = r.height().min(r.width());
        let shade = mix(VIDEO_BLACK, c, 0.35).gamma_multiply(alpha);
        let head = pos2(r.center().x, r.center().y - s * 0.08);
        self.p.circle_filled(head, s * 0.14, shade);
        let body = Rect::from_center_size(pos2(r.center().x, r.center().y + s * 0.3), vec2(s * 0.56, s * 0.3));
        self.p.rect_filled(body.intersect(r), s * 0.1, shade);
        self.p.rect_stroke(r, rr, Stroke::new(1.0, c.gamma_multiply(alpha)), StrokeKind::Inside);
    }

    fn scene(&self, s: &SketchScene, alpha: f32, zoom: f32, center: Pos2) {
        let alpha = alpha.clamp(0.0, 1.0);
        let bg = self.to_screen([0.0, 0.0, 1.0, 1.0], zoom, center);
        self.p.rect_filled(bg, 0.0, self.tints[s.tint].gamma_multiply(alpha));
        for (_, r, c) in s.boxes {
            self.cam(self.to_screen(*r, zoom, center), self.colors[*c], alpha);
        }
    }

    /// The engine's glide: shared boxes move, the others come and go in their styles.
    fn glide(&self, a: &SketchScene, b: &SketchScene, e: f32, m: &Motion) {
        let center = pos2(0.5, 0.5);
        self.p.rect_filled(self.area, 0.0, mix(self.tints[a.tint], self.tints[b.tint], e.clamp(0.0, 1.0)));
        for (id, ra, c) in a.boxes {
            match b.boxes.iter().find(|(bid, _, _)| bid == id) {
                Some((_, rb, _)) => {
                    let r = [0, 1, 2, 3].map(|i| ra[i] + (rb[i] - ra[i]) * e);
                    self.cam(self.to_screen(r, 1.0, center), self.colors[*c], 1.0);
                }
                None => {
                    let (r, alpha) = styled(*ra, m.exit, 1.0 - e, false);
                    self.cam(self.to_screen(r, 1.0, center), self.colors[*c], alpha);
                }
            }
        }
        for (_, rb, c) in b.boxes.iter().filter(|(id, _, _)| !a.boxes.iter().any(|(aid, _, _)| aid == id)) {
            let (r, alpha) = styled(*rb, m.enter, e, true);
            self.cam(self.to_screen(r, 1.0, center), self.colors[*c], alpha);
        }
    }

    /// Glitch strips: bands shoved sideways with color fringes, strongest mid-way.
    fn glitch(&self, e: f32, raw: f32, m: &Motion) {
        let env = (4.0 * e * (1.0 - e) * m.strength).clamp(0.0, 2.0);
        if env < 0.02 {
            return;
        }
        let h = self.area.height();
        let band = (m.block / 1080.0 * h * 2.5).clamp(2.0, h / 5.0);
        let seed = (raw * 18.0).floor() as u32;
        let n = (h / band).ceil() as u32;
        for i in 0..n {
            if hash(i, seed) > 0.4 * env.min(1.0) {
                continue;
            }
            let dx = (hash(i + 97, seed) - 0.5) * env * 0.3 * self.area.width();
            let y = self.area.top() + i as f32 * band;
            let strip = Rect::from_min_size(pos2(self.area.left(), y), vec2(self.area.width(), band));
            self.p.rect_filled(strip.translate(vec2(dx, 0.0)), 0.0, self.colors[2].gamma_multiply(0.18));
            self.p.rect_filled(strip.translate(vec2(dx * 0.6, 0.0)).shrink2(vec2(self.area.width() * 0.2, 0.0)), 0.0, self.colors[1].gamma_multiply(0.35));
            self.p.rect_filled(strip.translate(vec2(-dx, 0.0)).shrink2(vec2(self.area.width() * 0.3, 0.0)), 0.0, self.colors[0].gamma_multiply(0.35));
        }
    }
}

/// An appearing/disappearing box at presence `p` (0 = gone, 1 = in place), like the renderer.
fn styled(r: [f32; 4], style: &str, p: f32, entering: bool) -> ([f32; 4], f32) {
    let p = p.clamp(0.0, 1.0);
    match style {
        "none" => (r, if (p < 1.0 && !entering) || p <= 0.0 { 0.0 } else { 1.0 }),
        "scale" => {
            let s = 0.85 + 0.15 * p;
            let (cx, cy) = (r[0] + r[2] * 0.5, r[1] + r[3] * 0.5);
            ([cx - r[2] * s * 0.5, cy - r[3] * s * 0.5, r[2] * s, r[3] * s], p)
        }
        "slide_left" | "slide_right" | "slide_up" | "slide_down" => {
            let dir = match style {
                "slide_left" => [-1.0, 0.0],
                "slide_right" => [1.0, 0.0],
                "slide_up" => [0.0, -1.0],
                _ => [0.0, 1.0],
            };
            let k = if entering { -(1.0 - p) } else { 1.0 - p };
            ([r[0] + dir[0] * k, r[1] + dir[1] * k, r[2], r[3]], 1.0)
        }
        _ => (r, p),
    }
}

/// Draw the sketch of `m` into `rect`: two made-up scenes switching back and forth with this
/// transition's look, speed and motion. `time` = None draws one still frame mid-way.
fn sketch(p: &Painter, t: &Theme, rect: Rect, m: &Motion, time: Option<f64>) {
    let p = p.with_clip_rect(rect.intersect(p.clip_rect()));
    p.rect_filled(rect, 6, VIDEO_BLACK);
    let inner = rect.shrink(1.0);
    let s = Sketch { p: &p, area: inner, colors: [t.accent, t.green, t.yellow], tints: [mix(VIDEO_BLACK, t.accent, 0.12), mix(VIDEO_BLACK, t.green, 0.1)] };
    let d = if m.look == Look::Cut { 0.0 } else { (m.ms as f64 / 1000.0).clamp(0.05, 4.0) };
    let hold = 0.9;
    let (forward, raw) = match time {
        None => (true, 0.45),
        Some(now) => {
            let ph = now.rem_euclid(2.0 * (hold + d));
            if ph < hold {
                (true, 0.0)
            } else if ph < hold + d {
                (true, (ph - hold) / d)
            } else if ph < 2.0 * hold + d {
                (true, 1.0)
            } else {
                (false, ((ph - 2.0 * hold - d) / d).min(1.0))
            }
        }
    };
    let (a, b) = if forward { (&SCENE_A, &SCENE_B) } else { (&SCENE_B, &SCENE_A) };
    let raw = raw as f32;
    let e = if m.look == Look::Cut { if raw > 0.0 { 1.0 } else { 0.0 } } else { m.ease.apply(raw as f64) as f32 };
    let flat = e.clamp(0.0, 1.0);
    let c = pos2(0.5, 0.5);
    match m.look {
        Look::Cut => s.scene(if flat >= 1.0 { b } else { a }, 1.0, 1.0, c),
        Look::Fade => {
            s.scene(a, 1.0, 1.0, c);
            s.scene(b, flat, 1.0, c);
        }
        Look::Glide => s.glide(a, b, e, m),
        Look::GlideGlitch => {
            s.glide(a, b, e, m);
            s.glitch(flat, raw, m);
        }
        Look::GlideZoom => {
            s.glide(a, b, e, m);
            let k = ((std::f32::consts::PI * flat).sin() * m.strength).max(0.0);
            for i in 1..=3 {
                let r = inner.scale_from_center(1.0 + 0.06 * i as f32 * k);
                p.rect_stroke(r, 6, Stroke::new(1.5, t.fg.gamma_multiply(0.12 * k)), StrokeKind::Inside);
            }
        }
        Look::Zoom => {
            // like the shader: the zoom point travels across, the pictures dissolve mid-way
            let k = ((std::f32::consts::PI * flat).sin() * m.strength).max(0.0);
            let zoom = 1.0 + 1.4 * k;
            let center = pos2(0.25 + 0.5 * flat, 0.5);
            let dissolve = ((flat - 0.5) * 8.0).tanh() * 0.5 + 0.5;
            s.scene(a, 1.0, zoom, center);
            s.scene(b, dissolve, zoom, center);
            let ghost = if dissolve < 0.5 { a } else { b };
            for i in 1..=3 {
                s.scene(ghost, 0.12 * k.min(1.0), zoom * (1.0 + 0.08 * i as f32 * k), center);
            }
        }
        Look::Glitch => {
            s.scene(a, 1.0, 1.0, c);
            s.scene(b, flat, 1.0, c);
            s.glitch(flat, raw, m);
        }
        Look::Own => {
            // a wipe stands in for looks we can't draw here
            s.scene(a, 1.0, 1.0, c);
            let edge = inner.left() + inner.width() * flat;
            let q = p.with_clip_rect(Rect::from_min_max(inner.min, pos2(edge, inner.bottom())).intersect(p.clip_rect()));
            let sb = Sketch { p: &q, area: inner, colors: s.colors, tints: s.tints };
            sb.scene(b, 1.0, 1.0, c);
            if flat > 0.0 && flat < 1.0 {
                p.line_segment([pos2(edge, inner.top()), pos2(edge, inner.bottom())], Stroke::new(2.0, t.fg.gamma_multiply(0.5)));
            }
        }
    }
    p.rect_stroke(rect, 6, Stroke::new(1.0, t.border), StrokeKind::Inside);
}

// ---- the library ---------------------------------------------------------------------------------

/// One transition as the library shows it.
#[derive(Clone, Debug)]
struct Item {
    id: String,
    label: String,
    look: Look,
    motion: Motion,
    /// Not one of the built-ins: made for this project.
    custom: bool,
    /// Has a `transitions/<id>.toml`.
    file: bool,
    /// Scenes can pick it by name (a built-in or a file; the engine rejects anything else).
    pickable: bool,
    /// The custom shader (patch id) that draws it.
    patch: Option<String>,
}

fn items(app: &App) -> Vec<Item> {
    let defs = defs(app);
    let mut names: Vec<String> = app.m.q_list("transitions").iter().filter_map(|v| v.as_str().map(String::from)).collect();
    for n in BUILTIN.iter().map(|s| s.to_string()).chain(defs.into_iter().flat_map(|d| d.keys().cloned())) {
        if !names.contains(&n) {
            names.push(n);
        }
    }
    // custom shader transitions without a file naming them: the renderer plays them as
    // `patch.<id>`, but scenes can't pick them until they have one
    let named: Vec<&str> = defs
        .into_iter()
        .flat_map(|d| d.values())
        .filter_map(|d| d.get_path("shader").and_then(Value::as_str))
        .filter_map(|s| s.strip_prefix("patch."))
        .collect();
    for p in app.m.q_list("patches").iter().filter(|p| text(p, "kind") == "shader" && text(p, "layer") == "transition") {
        let id = text(p, "id");
        let a = format!("patch.{id}");
        if !id.is_empty() && !named.contains(&id) && !names.contains(&a) {
            names.push(a);
        }
    }
    let mut out: Vec<Item> = names
        .into_iter()
        .filter_map(|id| {
            let def = defs.and_then(|d| d.get(&id));
            let (kind, shader) = match def {
                Some(d) => {
                    (d.get_path("kind").and_then(Value::as_str).unwrap_or("morph").to_string(), d.get_path("shader").and_then(Value::as_str).map(String::from))
                }
                None if id.starts_with("patch.") => ("shader".to_string(), Some(id.clone())),
                None => {
                    let (k, s) = builtin_def(&id)?;
                    (k.to_string(), s.map(String::from))
                }
            };
            let ms = def
                .and_then(|d| d.get_path("ms"))
                .and_then(|v| v.as_i64().map(|i| i.max(0) as u64).or_else(|| v.as_str().and_then(se_proto::parse_duration_ms)));
            let ms = ms.unwrap_or(if kind == "cut" { 0 } else { DEFAULT_MS });
            let s = |k: &str| def.and_then(|d| d.get_path(k)).and_then(Value::as_str).unwrap_or("");
            let look = look_of(&kind, shader.as_deref());
            let motion = Motion::new(look, ms, s("ease"), s("enter"), s("exit"), |k| def.and_then(|d| d.get_path(k)).and_then(Value::as_f64));
            let builtin = BUILTIN.contains(&id.as_str());
            Some(Item {
                label: label(app, &id),
                look,
                motion,
                custom: !builtin,
                file: def.is_some(),
                pickable: builtin || def.is_some(),
                patch: shader.as_deref().and_then(|s| s.strip_prefix("patch.")).map(String::from),
                id,
            })
        })
        .collect();
    out.sort_by_key(|i| (i.id == "cut", i.label.to_lowercase()));
    out
}

/// "a, b and c".
fn and_list(words: &[String]) -> String {
    match words {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// Scenes: (id, display name).
fn scenes(app: &App) -> Vec<(String, String)> {
    app.m
        .q_list("scenes")
        .iter()
        .filter_map(|s| {
            let n = s.get_path("name").and_then(Value::as_str)?.to_string();
            let l = nice(s.get_path("label").and_then(Value::as_str).unwrap_or(&n));
            Some((n, l))
        })
        .collect()
}

fn scene_path(scene: &str) -> String {
    format!("scenes/{scene}.toml")
}

// ---- files ---------------------------------------------------------------------------------------

fn file_key(path: &str) -> String {
    format!("transitions.file:{path}")
}

/// A project file's text (asked for now and then; our latest write until the engine's copy is
/// read back). `None` until the first reply.
fn file_text(app: &mut App, st: &mut TransitionsState, path: &str, now: f64) -> Option<String> {
    let key = file_key(path);
    let seq = app.m.q_seq(&key);
    let f = st.files.entry(path.to_string()).or_default();
    if app.m.connected && (!f.asked || now - f.asked_at >= REREAD) {
        // a reply still on its way (asked before our write) comes first
        let in_flight = f.asked && seq == f.seq_at_ask;
        if let Some((_, want @ None)) = &mut f.local {
            *want = Some(seq + 1 + in_flight as u64);
        }
        f.asked = true;
        f.asked_at = now;
        f.seq_at_ask = seq;
        app.m.query_as(&key, "project.read", Value::map().with("path", path));
    }
    if let Some((text, want)) = &f.local {
        if want.is_none_or(|w| seq < w) {
            return Some(text.clone());
        }
        f.local = None;
    }
    let reply = app.m.q(&key)?;
    Some(reply.get_path("text").and_then(Value::as_str).unwrap_or("").to_string())
}

/// Write a project file and read it back once the engine has it.
fn write_file(app: &mut App, st: &mut TransitionsState, path: &str, text: String, now: f64) {
    app.m.action("project.write", Value::map().with("path", path).with("text", text.as_str()));
    let f = st.files.entry(path.to_string()).or_default();
    f.local = Some((text, None));
    f.asked_at = now - REREAD + 0.5;
    if let Some(scene) = path.strip_prefix("scenes/").and_then(|p| p.strip_suffix(".toml")) {
        crate::views::composition::scene_file_changed(app, scene);
    }
    app.m.refresh_soon();
}

/// A change made on this page.
enum Edit {
    Default(Choice),
    Scene(String, Choice),
    Pair { to: String, from: String, choice: Choice },
}

fn apply(app: &mut App, st: &mut TransitionsState, edit: Edit, now: f64) {
    let (path, from, choice) = match edit {
        Edit::Default(c) => (PROJECT.to_string(), None, c),
        Edit::Scene(s, c) => (scene_path(&s), None, c),
        Edit::Pair { to, from, choice } => (scene_path(&to), Some(from), choice),
    };
    let Some(text) = file_text(app, st, &path, now) else {
        app.m.toast("Still loading your project. Try again in a moment.", true);
        return;
    };
    match set_choice(&text, from.as_deref(), &choice) {
        Ok(new) if new != text => write_file(app, st, &path, new, now),
        Ok(_) => {}
        Err(e) => app.m.toast(format!("Couldn't change how scenes switch: {e:#}. Fix the file on the Troubleshooting tab, then try again."), true),
    }
}

// ---- words ---------------------------------------------------------------------------------------

fn names_of(pool: &[(String, f64)], names: &[(String, String)]) -> Vec<String> {
    pool.iter().map(|(n, _)| names.iter().find(|(id, _)| id == n).map_or_else(|| nice(n), |(_, l)| l.clone())).collect()
}

fn name_of(id: &str, names: &[(String, String)]) -> String {
    names.iter().find(|(n, _)| n == id).map_or_else(|| nice(id), |(_, l)| l.clone())
}

/// "random from Glide and Crossfade" / "always Cut"; `Inherit` reads as `inherit`.
fn summary(c: &Choice, names: &[(String, String)], inherit: &str) -> String {
    match c {
        Choice::Inherit => inherit.to_string(),
        Choice::Random { pool, .. } => format!("random from {}", and_list(&names_of(pool, names))),
        Choice::Always(n) => format!("always {}", name_of(n, names)),
    }
}

/// The project default in words.
fn default_words(default: &Choice, names: &[(String, String)]) -> String {
    summary(default, names, &format!("always {}", name_of("fade", names)))
}

// ---- choosing ------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Level {
    Default,
    Scene,
    Pair,
}

/// `cur` changed to another way of choosing; `seed` (the project default) fills it in.
fn switch(cur: &Choice, to_random: Option<bool>, seed: &Choice) -> Choice {
    let avoid = match seed {
        Choice::Random { avoid_repeat, .. } => *avoid_repeat,
        _ => 1,
    };
    let fallback = || seed.first().unwrap_or("fade").to_string();
    match to_random {
        None => Choice::Inherit,
        Some(true) => match (cur, seed) {
            (Choice::Always(n), _) => Choice::Random { pool: vec![(n.clone(), 1.0)], avoid_repeat: avoid },
            (Choice::Random { .. }, _) => cur.clone(),
            (Choice::Inherit, Choice::Random { .. }) => seed.clone(),
            (Choice::Inherit, _) => Choice::Random { pool: vec![(fallback(), 1.0)], avoid_repeat: avoid },
        },
        Some(false) => Choice::Always(cur.first().map_or_else(fallback, String::from)),
    }
}

/// The controls for one choice; the new choice when the owner changed it.
fn choice_ui(ui: &mut egui::Ui, t: &Theme, salt: &str, level: Level, cur: &Choice, names: &[(String, String)], seed: &Choice) -> Option<Choice> {
    let labels: &[&str] = match level {
        Level::Default => &["Pick at random", "Always use one"],
        Level::Scene => &["Use the default", "Random", "Always"],
        Level::Pair => &["Random", "Always"],
    };
    let off = usize::from(level == Level::Scene);
    let mut idx = match cur {
        Choice::Inherit => 0,
        Choice::Random { .. } => off,
        Choice::Always(_) => off + 1,
    };
    let mut out = None;
    if widgets::segmented(ui, t, &mut idx, labels) {
        let to = if level == Level::Scene && idx == 0 { None } else { Some(idx == off) };
        let c = switch(cur, to, seed);
        if c != *cur {
            out = Some(c);
        }
    }
    let shown = out.clone().unwrap_or_else(|| cur.clone());
    ui.add_space(spacing::S);
    match &shown {
        Choice::Random { pool, avoid_repeat } => {
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(spacing::XS + 2.0, spacing::XS + 2.0);
                for (id, label) in names {
                    let on = pool.iter().any(|(n, _)| n == id);
                    let last = on && pool.len() == 1 && level != Level::Default;
                    let r = widgets::chip(ui, t, if on { icon::CHECK } else { icon::PLUS }, label, on);
                    let r = if last { r.on_hover_text("Keep at least one, or pick another way above.") } else { r };
                    if r.clicked() && !last {
                        let mut pool = pool.clone();
                        if on {
                            pool.retain(|(n, _)| n != id);
                        } else {
                            pool.push((id.clone(), 1.0));
                        }
                        out = Some(if pool.is_empty() { Choice::Inherit } else { Choice::Random { pool, avoid_repeat: *avoid_repeat } });
                    }
                }
            });
            if level == Level::Default {
                ui.add_space(spacing::S);
                let mut no_repeat = *avoid_repeat > 0;
                ui.horizontal(|ui| {
                    if widgets::toggle(ui, t, &mut no_repeat).changed() {
                        out = Some(Choice::Random { pool: pool.clone(), avoid_repeat: usize::from(no_repeat) });
                    }
                    ui.label(RichText::new("Don't repeat the last one").color(if no_repeat { t.fg } else { t.text_dim }));
                });
            }
        }
        Choice::Always(n) => {
            ui.horizontal(|ui| {
                egui::ComboBox::from_id_salt(("tr-always", salt)).selected_text(name_of(n, names)).width(220.0).show_ui(ui, |ui| {
                    for (id, label) in names {
                        if ui.selectable_label(id == n, label).clicked() && id != n {
                            out = Some(Choice::Always(id.clone()));
                        }
                    }
                });
            });
        }
        Choice::Inherit if level == Level::Default => {
            // nothing picked yet: show the list to pick from
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = vec2(spacing::XS + 2.0, spacing::XS + 2.0);
                for (id, label) in names {
                    if widgets::chip(ui, t, icon::PLUS, label, false).clicked() {
                        out = Some(Choice::Random { pool: vec![(id.clone(), 1.0)], avoid_repeat: 1 });
                    }
                }
            });
            ui.add_space(spacing::XS);
            widgets::hint(ui, t, &format!("Nothing picked yet, so every switch is a {}.", name_of("fade", names)));
        }
        Choice::Inherit => {}
    }
    out
}

/// Scene settings: how switching to `scene` picks its transition (`text`: the scene file).
/// Returns the edited file when the owner changed it.
pub fn scene_choice(app: &mut App, ui: &mut egui::Ui, scene: &str, text: &str) -> Option<Result<String>> {
    let now = ui.input(|i| i.time);
    poll(app, now);
    let mut st = std::mem::take(&mut app.build.transitions);
    let project = file_text(app, &mut st, PROJECT, now);
    app.build.transitions = st;
    let default = project.as_deref().map_or(Choice::Inherit, |t| read_choice(t, None));
    let names: Vec<(String, String)> = items(app).into_iter().filter(|i| i.pickable).map(|i| (i.id, i.label)).collect();
    let t = app.t.clone();
    let cur = read_choice(text, None);
    let new = choice_ui(ui, &t, scene, Level::Scene, &cur, &names, &default);
    let shown = new.as_ref().unwrap_or(&cur);
    if *shown == Choice::Inherit {
        ui.add_space(spacing::XS);
        widgets::hint(ui, &t, &format!("The default: {}.", default_words(&default, &names)));
    }
    let all = scenes(app);
    for (from, c) in pair_choices(text) {
        widgets::hint(ui, &t, &format!("Coming from {}: {}.", name_of(&from, &all), summary(&c, &names, "")));
    }
    widgets::hint(ui, &t, "Set the default and the exceptions on the Transitions tab.");
    new.map(|c| set_choice(text, None, &c))
}

// ---- the view ------------------------------------------------------------------------------------

/// Width of the list pane.
const LIST_W: f32 = 290.0;
/// Room kept for the list's footer line under its scroll area.
const FOOTER_H: f32 = 40.0;

/// What the detail pane shows.
#[derive(Clone, Debug, PartialEq)]
enum Sel {
    /// The project default and the exceptions.
    Switching,
    One(String),
    New,
}

pub fn ui(app: &mut App, ui: &mut egui::Ui) {
    let now = ui.input(|i| i.time);
    poll(app, now);
    patches::refresh(app, ui);
    let mut st = std::mem::take(&mut app.build.transitions);
    view(app, ui, &mut st, now);
    app.build.transitions = st;
}

/// Everything the page shows, read from the files.
struct Uses {
    default: Choice,
    /// Scenes with their own choice: (scene, choice).
    scenes: Vec<(String, Choice)>,
    /// Pairs with their own choice: (to, from, choice).
    pairs: Vec<(String, String, Choice)>,
    loaded: bool,
}

fn uses(app: &mut App, st: &mut TransitionsState, all: &[(String, String)], now: f64) -> Uses {
    let project = file_text(app, st, PROJECT, now);
    let mut u = Uses {
        default: project.as_deref().map_or(Choice::Inherit, |t| read_choice(t, None)),
        scenes: Vec::new(),
        pairs: Vec::new(),
        loaded: project.is_some(),
    };
    for (scene, _) in all {
        let Some(text) = file_text(app, st, &scene_path(scene), now) else {
            u.loaded = false;
            continue;
        };
        let own = read_choice(&text, None);
        if own != Choice::Inherit {
            u.scenes.push((scene.clone(), own));
        }
        u.pairs.extend(pair_choices(&text).into_iter().map(|(from, c)| (scene.clone(), from, c)));
    }
    u
}

/// Where a transition is used, in words.
fn used_words(id: &str, u: &Uses, all: &[(String, String)]) -> String {
    let mut parts = Vec::new();
    if in_default(&u.default, id) {
        parts.push("by default".to_string());
    }
    let to: Vec<String> = u.scenes.iter().filter(|(_, c)| c.mentions(id)).map(|(s, _)| name_of(s, all)).collect();
    if !to.is_empty() {
        parts.push(format!("switching to {}", and_list(&to)));
    }
    for (to, from, _) in u.pairs.iter().filter(|(_, _, c)| c.mentions(id)) {
        parts.push(format!("from {} to {}", name_of(from, all), name_of(to, all)));
    }
    if parts.is_empty() { "Not used right now".to_string() } else { format!("Used {}", and_list(&parts)) }
}

/// Whether the project default picks `id` (nothing picked = always a Crossfade).
fn in_default(default: &Choice, id: &str) -> bool {
    default.mentions(id) || (*default == Choice::Inherit && id == "fade")
}

/// The project default with `id` added to (`on`) or taken out of what it picks. Nothing picked
/// means "always a Crossfade", so adding to that keeps the Crossfade in the mix.
fn toggle_default(cur: &Choice, id: &str, on: bool) -> Choice {
    match (cur, on) {
        (c, true) if in_default(c, id) => c.clone(),
        (Choice::Random { pool, avoid_repeat }, true) => {
            let mut pool = pool.clone();
            pool.push((id.to_string(), 1.0));
            Choice::Random { pool, avoid_repeat: *avoid_repeat }
        }
        (Choice::Always(n), true) => Choice::Random { pool: vec![(n.clone(), 1.0), (id.to_string(), 1.0)], avoid_repeat: 1 },
        (Choice::Inherit, true) => Choice::Random { pool: vec![("fade".into(), 1.0), (id.to_string(), 1.0)], avoid_repeat: 1 },
        (Choice::Random { pool, avoid_repeat }, false) => {
            let pool: Vec<(String, f64)> = pool.iter().filter(|(n, _)| n != id).cloned().collect();
            if pool.is_empty() { Choice::Inherit } else { Choice::Random { pool, avoid_repeat: *avoid_repeat } }
        }
        (Choice::Always(n), false) if n == id => Choice::Inherit,
        (c, false) => c.clone(),
    }
}

/// The project default in a few words, for the list row ("Random pick of 3", "Always Cut").
fn short_words(default: &Choice, names: &[(String, String)]) -> String {
    match default {
        Choice::Inherit => format!("Always {}", name_of("fade", names)),
        Choice::Always(n) => format!("Always {}", name_of(n, names)),
        Choice::Random { pool, .. } if pool.len() == 1 => format!("Random: {}", name_of(&pool[0].0, names)),
        Choice::Random { pool, .. } => format!("Random pick of {}", pool.len()),
    }
}

fn view(app: &mut App, ui: &mut egui::Ui, st: &mut TransitionsState, now: f64) {
    let t = app.t.clone();
    let items = items(app);
    let all = scenes(app);
    let u = uses(app, st, &all, now);
    // what the choosers offer: only names the project files can use
    let names: Vec<(String, String)> = items.iter().filter(|i| i.pickable).map(|i| (i.id.clone(), i.label.clone())).collect();
    if let Some(Sel::One(id)) = &st.sel
        && !items.iter().any(|i| i.id == *id)
        && now > st.hold_until
    {
        st.sel = None;
    }
    let sel = st.sel.clone().unwrap_or(Sel::Switching);
    let mut edits = Vec::new();
    let (clicked, _) = widgets::split(
        ui,
        LIST_W,
        |ui| list_pane(ui, &t, &items, &u, &names, &sel),
        |ui| {
            egui::ScrollArea::vertical().id_salt("tr-detail").auto_shrink([false, false]).show(ui, |ui| match &sel {
                Sel::Switching => switching(ui, &t, st, &u, &all, &names, &mut edits),
                Sel::One(id) => match items.iter().find(|i| i.id == *id) {
                    Some(it) => detail(app, ui, &t, st, it, &u, &all, &mut edits, now),
                    None => {
                        widgets::empty_state(ui, &t, icon::CLOCK, "Making it…", "It shows up here in a moment.", None);
                    }
                },
                Sel::New => new_ui(app, ui, &t, st, &items, now),
            });
        },
    );
    if let Some(s) = clicked {
        if s == Sel::New && sel != Sel::New {
            st.new_name.clear();
        }
        st.sel = Some(s);
    }
    for e in edits {
        apply(app, st, e, now);
    }
    // the sketch moves
    ui.ctx().request_repaint();
}

fn list_pane(ui: &mut egui::Ui, t: &Theme, items: &[Item], u: &Uses, names: &[(String, String)], sel: &Sel) -> Option<Sel> {
    let mut out = None;
    if widgets::pane_header(ui, t, "Transitions", Some(items.len()), Some("New custom transition")) {
        out = Some(Sel::New);
    }
    egui::ScrollArea::vertical().id_salt("tr-list").auto_shrink([false, false]).max_height((ui.available_height() - FOOTER_H).max(80.0)).show(ui, |ui| {
        widgets::group_label(ui, t, "Switching");
        let exceptions = match u.scenes.len() + u.pairs.len() {
            0 => String::new(),
            1 => "1 exception".into(),
            n => format!("{n} exceptions"),
        };
        let sub = if u.loaded || u.default != Choice::Inherit { short_words(&u.default, names) } else { "Loading…".into() };
        if widgets::list_row(ui, t, icon::SLIDERS, "How scenes switch", &sub, &exceptions, *sel == Sel::Switching).clicked() {
            out = Some(Sel::Switching);
        }
        for (label, mine) in [("Built in", false), ("Yours", true)] {
            let group: Vec<&Item> = items.iter().filter(|i| i.custom == mine).collect();
            if group.is_empty() {
                continue;
            }
            widgets::group_label(ui, t, label);
            for it in group {
                let known = u.loaded || u.default != Choice::Inherit;
                let trailing = if known && it.pickable && in_default(&u.default, &it.id) { "Default" } else { "" };
                let ic = if it.look == Look::Own { icon::SPARKLE } else { icon::SHUFFLE };
                let on = matches!(sel, Sel::One(id) if *id == it.id);
                let look = look_words(it.look).0;
                let sub = if look == it.label { speed_words(&it.motion) } else { format!("{look} · {}", speed_words(&it.motion)) };
                if widgets::list_row(ui, t, ic, &it.label, &sub, trailing, on).clicked() {
                    out = Some(Sel::One(it.id.clone()));
                }
            }
        }
    });
    ui.add_space(spacing::S);
    widgets::hint(ui, t, "A scene can pick its own in the scene inspector.");
    out
}

/// "How scenes switch": the project default and the exceptions.
fn switching(ui: &mut egui::Ui, t: &Theme, st: &mut TransitionsState, u: &Uses, all: &[(String, String)], names: &[(String, String)], edits: &mut Vec<Edit>) {
    widgets::detail_header(ui, t, icon::SLIDERS, "How scenes switch", "Every scene switches this way, unless it has an exception.", |_| {});
    widgets::inspector_section(
        ui,
        t,
        "tr-default",
        "Default",
        true,
        |_| {},
        |ui| {
            if !u.loaded && u.default == Choice::Inherit {
                widgets::hint(ui, t, "Loading…");
            } else if let Some(c) = choice_ui(ui, t, "default", Level::Default, &u.default, names, &u.default) {
                edits.push(Edit::Default(c));
            }
        },
    );
    let count = u.scenes.len() + u.pairs.len();
    let title = if count == 0 { "Exceptions".to_string() } else { format!("Exceptions ({count})") };
    let adding = st.adding.is_some();
    let mut add = false;
    widgets::inspector_section(
        ui,
        t,
        "tr-exceptions",
        &title,
        true,
        |ui| {
            if !adding && all.len() >= 2 {
                add = widgets::button_ex(ui, t, Some(icon::PLUS), "Add", Kind::Secondary, Size::Small, 0.0, true)
                    .on_hover_text("A scene, or a move between two scenes, that switches its own way")
                    .clicked();
            }
        },
        |ui| {
            widgets::hint(ui, t, "A scene, or a move between two scenes, can switch its own way.");
            ui.add_space(spacing::S);
            if st.adding.is_some() {
                add_form(ui, t, st, u, all, edits);
                ui.add_space(spacing::M);
            }
            if count == 0 && st.adding.is_none() {
                widgets::hint(
                    ui,
                    t,
                    if all.len() < 2 {
                        "Make a second scene to set how switching between them looks."
                    } else {
                        "None yet: every scene switches the way set above."
                    },
                );
            }
            let rows = u
                .scenes
                .iter()
                .map(|(s, c)| (format!("Switching to {}", name_of(s, all)), s, None, c))
                .chain(u.pairs.iter().map(|(to, from, c)| (format!("From {} to {}", name_of(from, all), name_of(to, all)), to, Some(from), c)));
            for (title, to, from, cur) in rows {
                let (level, salt) = match from {
                    None => (Level::Scene, format!("scene-{to}")),
                    Some(f) => (Level::Pair, format!("pair-{f}-{to}")),
                };
                if let Some(c) = exception_row(ui, t, &title, &salt, level, cur, names, &u.default) {
                    edits.push(match from {
                        None => Edit::Scene(to.clone(), c),
                        Some(f) => Edit::Pair { to: to.clone(), from: f.clone(), choice: c },
                    });
                }
            }
        },
    );
    if add {
        let first = all.first().map(|(s, _)| s.clone()).unwrap_or_default();
        let second = all.get(1).map(|(s, _)| s.clone()).unwrap_or_default();
        st.adding = Some(Adding { pair: 0, from: first, to: second });
    }
}

/// The two scenes "Try it" switches between: the picked ones, else program and another.
fn try_pair(st: &TransitionsState, all: &[(String, String)], program: &str) -> (String, String) {
    let valid = |s: &Option<String>| s.as_ref().filter(|s| all.iter().any(|(n, _)| n == *s)).cloned();
    let from = valid(&st.try_from)
        .or_else(|| all.iter().any(|(n, _)| n == program).then(|| program.to_string()))
        .or_else(|| all.first().map(|(n, _)| n.clone()))
        .unwrap_or_default();
    let to = valid(&st.try_to).filter(|s| *s != from).or_else(|| all.iter().find(|(n, _)| *n != from).map(|(n, _)| n.clone())).unwrap_or_default();
    (from, to)
}

/// One transition: its sketch, where it's used, its settings, and "Try it".
#[allow(clippy::too_many_arguments)]
fn detail(
    app: &mut App,
    ui: &mut egui::Ui,
    t: &Theme,
    st: &mut TransitionsState,
    it: &Item,
    u: &Uses,
    all: &[(String, String)],
    edits: &mut Vec<Edit>,
    now: f64,
) {
    let program = app.m.str("show.scene.program").to_string();
    let on_air = status::on_air(app);
    let (from, to) = try_pair(st, all, &program);
    let can_try = !on_air && !to.is_empty();
    let tip = if on_air {
        "You're on air: trying it would switch scenes for your viewers."
    } else if to.is_empty() {
        "Make a second scene to try it."
    } else {
        "Switch between the two scenes under Try it, with this transition."
    };
    let mut tried = false;
    let ic = if it.look == Look::Own { icon::SPARKLE } else { icon::SHUFFLE };
    widgets::detail_header(ui, t, ic, &it.label, look_words(it.look).1, |ui| {
        tried = widgets::button_ex(ui, t, Some(icon::PLAY), "Try it", Kind::Secondary, Size::Small, 0.0, can_try)
            .on_hover_text(tip)
            .on_disabled_hover_text(tip)
            .clicked();
    });
    let w = ui.available_width().min(400.0);
    let (rect, _) = ui.allocate_exact_size(vec2(w, w * 9.0 / 16.0), Sense::hover());
    sketch(ui.painter(), t, rect, &it.motion, Some(now));
    ui.add_space(spacing::XS);
    widgets::hint(ui, t, "A sketch of how it moves. Try it to see it with your own scenes.");
    ui.add_space(spacing::M);
    if !it.pickable {
        let pid = it.patch.clone().unwrap_or_default();
        let free = !pid.is_empty() && !BUILTIN.contains(&pid.as_str()) && !defs(app).is_some_and(|d| d.contains_key(&pid));
        let body = if free {
            "It needs a name file first. Make one and scenes can pick it like any other."
        } else {
            "It needs a name file, but its folder's name is already taken by another transition. Rename its folder, then come back."
        };
        if widgets::callout(ui, t, Tone::Warn, icon::WARN, "Scenes can't pick it yet", body, free.then_some("Make it pickable")) {
            write_name_file(app, &pid, &it.label);
            st.sel = Some(Sel::One(pid));
            st.hold_until = now + 8.0;
        }
        ui.add_space(spacing::M);
    }
    where_used(app, ui, t, st, it, u, all, edits);
    settings(app, ui, t, it);
    try_section(app, ui, t, st, all, (&from, &to), &program, on_air);
    if tried {
        // already on the "to" scene: play it back the other way instead of jumping first
        let (a, b) = if program == to { (to, from) } else { (from, to) };
        try_it(app, &it.id, &a, &b, it.motion.ms);
    }
}

/// "Where it's used": the project default (with a switch), and the scenes and scene pairs that
/// pick it; a row opens the scene.
#[allow(clippy::too_many_arguments)]
fn where_used(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut TransitionsState, it: &Item, u: &Uses, all: &[(String, String)], edits: &mut Vec<Edit>) {
    let id = it.id.as_str();
    let mut open = None;
    let mut to_switching = false;
    widgets::inspector_section(
        ui,
        t,
        ("tr-uses", id),
        "Where it's used",
        true,
        |_| {},
        |ui| {
            widgets::hint(ui, t, &format!("{}.", used_words(id, u, all)));
            ui.add_space(spacing::XS);
            if it.pickable {
                let on = in_default(&u.default, id);
                let next = toggle_default(&u.default, id, !on);
                let can = in_default(&next, id) != on && (u.loaded || u.default != Choice::Inherit);
                widgets::prop_row(ui, t, "By default", |ui| {
                    let tip = match (can, on) {
                        (false, _) => "A Crossfade is used while nothing else is picked.",
                        (true, true) => "Stop picking it by default",
                        (true, false) => "Pick it by default",
                    };
                    let mut v = on;
                    let r = ui.add_enabled_ui(can, |ui| widgets::toggle(ui, t, &mut v)).inner;
                    if r.on_hover_text(tip).on_disabled_hover_text(tip).changed() {
                        edits.push(Edit::Default(next.clone()));
                    }
                    let words = match &u.default {
                        Choice::Random { pool, .. } if on && pool.len() > 1 => "One of the random picks",
                        _ if on => "Every switch uses it",
                        _ => "Not picked by default",
                    };
                    ui.label(RichText::new(words).color(if on { t.fg } else { t.text_dim }));
                });
                if !matches!(&u.default, Choice::Always(n) if n == id) && (u.loaded || u.default != Choice::Inherit) {
                    widgets::prop_row(ui, t, "", |ui| {
                        if widgets::button_ex(ui, t, None, "Use only this one", Kind::Ghost, Size::Small, 0.0, true)
                            .on_hover_text("Every switch uses it")
                            .clicked()
                        {
                            edits.push(Edit::Default(Choice::Always(id.to_string())));
                        }
                    });
                }
                ui.add_space(spacing::S);
            }
            let rows: Vec<(String, &String)> = u
                .scenes
                .iter()
                .filter(|(_, c)| c.mentions(id))
                .map(|(s, _)| (format!("Switching to {}", name_of(s, all)), s))
                .chain(
                    u.pairs.iter().filter(|(_, _, c)| c.mentions(id)).map(|(to, from, _)| (format!("From {} to {}", name_of(from, all), name_of(to, all)), to)),
                )
                .collect();
            for (title, scene) in rows {
                if widgets::list_row(ui, t, icon::SCENE, &title, "", icon::RIGHT, false).on_hover_text("Open this scene").clicked() {
                    open = Some(scene.clone());
                }
            }
            ui.add_space(spacing::XS);
            to_switching = widgets::button_ex(ui, t, Some(icon::SLIDERS), "Change how scenes switch", Kind::Ghost, Size::Small, 0.0, true).clicked();
        },
    );
    if let Some(scene) = open {
        composition::open(app, &scene, None);
    }
    if to_switching {
        st.sel = Some(Sel::Switching);
    }
}

/// How long it takes: "Instant", "0.7 s".
fn speed_words(m: &Motion) -> String {
    if m.look == Look::Cut { "Instant".to_string() } else { format!("{:.1} s", m.ms as f64 / 1000.0) }
}

/// How things that are only in one of the two scenes come and go, in words.
fn style_words(style: &str) -> String {
    match style {
        "scale" => "grow".into(),
        "none" => "cut".into(),
        s => s.replace('_', " "),
    }
}

/// "Settings": what it's made of; a custom shader's own settings and files.
fn settings(app: &mut App, ui: &mut egui::Ui, t: &Theme, it: &Item) {
    let p = it.patch.as_deref().and_then(|id| patches::find(app, id));
    widgets::inspector_section(
        ui,
        t,
        ("tr-settings", &it.id),
        "Settings",
        true,
        |_| {},
        |ui| {
            let m = &it.motion;
            let value = |ui: &mut egui::Ui, s: String| ui.label(RichText::new(s).color(t.fg));
            widgets::prop_row(ui, t, "Looks like", |ui| value(ui, look_words(it.look).0.to_string()));
            widgets::prop_row(ui, t, "Speed", |ui| value(ui, speed_words(m)));
            if matches!(it.look, Look::Glide | Look::GlideGlitch | Look::GlideZoom) {
                widgets::prop_row(ui, t, "Only in one scene", |ui| value(ui, format!("{} in, {} out", style_words(m.enter), style_words(m.exit))));
            }
            let made = match (&it.patch, it.file) {
                (Some(pid), _) => format!("patches/{pid}/"),
                (None, true) => format!("transitions/{}.toml", it.id),
                (None, false) => String::new(),
            };
            widgets::prop_row(ui, t, "Made by", |ui| {
                if made.is_empty() { value(ui, "Built in".into()) } else { ui.label(RichText::new(&made).font(font_mono(type_scale::SMALL + 0.5)).color(t.fg)) }
            });
            ui.add_space(spacing::XS);
            widgets::hint(ui, t, "How long a switch takes can also be set per scene, in the scene inspector.");
            let Some(p) = &p else { return };
            let id = text(p, "id").to_string();
            ui.add_space(spacing::M);
            if let Some(pr) = patches::problem(app, p) {
                let title = if pr.still_showing { "Its last change has a mistake" } else { "It has a mistake and can't play" };
                let body = if pr.location.is_empty() { pr.message.clone() } else { format!("{} ({})", pr.message, pr.location) };
                if widgets::callout(ui, t, Tone::Danger, icon::WARN, title, &body, Some("Open files")) {
                    patches::open_files(app, &id);
                }
                ui.add_space(spacing::S);
            }
            if p.get_path("params").and_then(Value::as_list).is_some_and(|l| !l.is_empty()) {
                patches::params_ui(app, ui, p);
                ui.add_space(spacing::S);
            }
            ui.horizontal(|ui| {
                if widgets::button_ex(ui, t, Some(icon::EDIT), "Edit files", Kind::Secondary, Size::Small, 0.0, true).clicked() {
                    patches::open_files(app, &id);
                }
                if widgets::button_ex(ui, t, Some(icon::UNDO), "Reload", Kind::Ghost, Size::Small, 0.0, true).on_hover_text("Read its files again").clicked() {
                    patches::reload(app, &id);
                }
            });
        },
    );
}

/// "Try it": the two scenes it switches between, and the main video to watch it on.
#[allow(clippy::too_many_arguments)]
fn try_section(
    app: &mut App,
    ui: &mut egui::Ui,
    t: &Theme,
    st: &mut TransitionsState,
    all: &[(String, String)],
    pair: (&str, &str),
    program: &str,
    on_air: bool,
) {
    widgets::inspector_section(
        ui,
        t,
        "tr-try",
        "Try it",
        true,
        |_| {},
        |ui| {
            if all.len() < 2 {
                widgets::hint(ui, t, "Make a second scene to try transitions between two scenes.");
                return;
            }
            widgets::prop_row(ui, t, "Between", |ui| {
                for (salt, cur, which) in [("tr-try-from", pair.0, 0), ("tr-try-to", pair.1, 1)] {
                    if which == 1 {
                        ui.label(RichText::new("and").color(t.text_dim));
                    }
                    egui::ComboBox::from_id_salt(salt).selected_text(name_of(cur, all)).width(160.0).show_ui(ui, |ui| {
                        for (n, l) in all {
                            if ui.selectable_label(n == cur, l).clicked() {
                                *(if which == 0 { &mut st.try_from } else { &mut st.try_to }) = Some(n.clone());
                            }
                        }
                    });
                }
            });
            ui.add_space(spacing::XS);
            widgets::hint(
                ui,
                t,
                if on_air {
                    "You're on air: trying one would switch scenes for your viewers."
                } else {
                    "\"Try it\" switches between these two, so you can watch it here."
                },
            );
            ui.add_space(spacing::S);
            let w = ui.available_width().min(640.0);
            let tally = if on_air { LedState::Active } else { LedState::Idle };
            crate::views::monitor::monitor(app, ui, Canvas::Wide, program, if on_air { "ON AIR" } else { "Main video" }, vec2(w, w * 9.0 / 16.0), tally, 30.0);
        },
    );
}

/// A `transitions/<name>.toml` that names the custom shader `patch` (so scenes can pick it).
fn name_file(label: &str, patch: &str) -> String {
    format!(
        "# Made in Stream Engine: the custom transition drawn by patches/{patch}/.\nlabel = {}\nkind = \"shader\"\nshader = \"patch.{patch}\"\n",
        toml_edit::Value::from(label)
    )
}

fn write_name_file(app: &mut App, patch: &str, label: &str) {
    app.m.action("project.write", Value::map().with("path", format!("transitions/{patch}.toml")).with("text", name_file(label, patch)));
    app.m.refresh_soon();
}

/// "New custom transition": a shader from the transition template, named so scenes can pick it.
fn new_ui(app: &mut App, ui: &mut egui::Ui, t: &Theme, st: &mut TransitionsState, items: &[Item], now: f64) {
    widgets::detail_header(ui, t, icon::PLUS, "New custom transition", "Starts from a soft diagonal wipe you can change in its files.", |_| {});
    let template = patches::templates(app, "shader").into_iter().find(|x| x.layer == "transition").map(|x| x.name);
    widgets::inspector_section(
        ui,
        t,
        "tr-new-name",
        "Name",
        true,
        |_| {},
        |ui| {
            widgets::prop_row(ui, t, "Name", |ui| {
                ui.add(widgets::field(&mut st.new_name).hint_text("e.g. Wipe").desired_width(260.0));
            });
        },
    );
    let id = scene_edit::slug(&st.new_name);
    let taken = !id.is_empty()
        && (BUILTIN.contains(&id.as_str())
            || items.iter().any(|i| i.id == id || i.patch.as_deref() == Some(id.as_str()))
            || defs(app).is_some_and(|d| d.contains_key(&id))
            || patches::find(app, &id).is_some());
    let ready = !id.is_empty() && !taken && template.is_some();
    ui.add_space(spacing::S);
    ui.horizontal(|ui| {
        if widgets::button_ex(ui, t, Some(icon::CHECK), "Create", Kind::Primary, Size::Medium, 0.0, ready).clicked()
            && let Some(tpl) = &template
        {
            let label = st.new_name.trim().to_string();
            patches::create(app, &id, "shader", tpl);
            patches::open_files(app, &id);
            write_name_file(app, &id, &label);
            app.m.toast(format!("Made \"{label}\". Its files open in your editor."), false);
            st.sel = Some(Sel::One(id.clone()));
            st.hold_until = now + 8.0;
            st.new_name.clear();
        }
        if widgets::button_ex(ui, t, None, "Cancel", Kind::Ghost, Size::Medium, 0.0, true).clicked() {
            st.sel = None;
        }
        if taken {
            widgets::hint(ui, t, "That name is taken.");
        } else if template.is_none() {
            widgets::hint(ui, t, "Loading the starting point…");
        }
    });
    ui.add_space(spacing::S);
    widgets::hint(ui, t, "Its files open in your editor. Once made, scenes can pick it like any other transition.");
}

/// One exception: its title, a remove button, and its choice.
#[allow(clippy::too_many_arguments)]
fn exception_row(
    ui: &mut egui::Ui,
    t: &Theme,
    title: &str,
    salt: &str,
    level: Level,
    cur: &Choice,
    names: &[(String, String)],
    seed: &Choice,
) -> Option<Choice> {
    let mut out = None;
    egui::Frame::new().fill(t.surface_hi).corner_radius(radius::CONTROL).inner_margin(spacing::M).show(ui, |ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.add(egui::Label::new(RichText::new(title).font(font_medium(type_scale::BODY)).color(t.fg)).truncate());
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if widgets::icon_button(ui, t, icon::TRASH, "Remove this exception (back to the default)").clicked() {
                    out = Some(Choice::Inherit);
                }
            });
        });
        ui.add_space(spacing::XS);
        if let Some(c) = choice_ui(ui, t, salt, level, cur, names, seed) {
            out = Some(c);
        }
    });
    ui.add_space(spacing::S);
    out
}

fn add_form(ui: &mut egui::Ui, t: &Theme, st: &mut TransitionsState, u: &Uses, all: &[(String, String)], edits: &mut Vec<Edit>) {
    let Some(mut a) = st.adding.clone() else { return };
    let mut close = false;
    egui::Frame::new().stroke(Stroke::new(1.0, t.accent)).corner_radius(radius::CONTROL).inner_margin(spacing::M).show(ui, |ui| {
        ui.set_width(ui.available_width());
        widgets::segmented(ui, t, &mut a.pair, &["Switching to a scene", "Between two scenes"]);
        ui.add_space(spacing::S);
        let combo = |ui: &mut egui::Ui, salt: &str, cur: &mut String| {
            egui::ComboBox::from_id_salt(salt).selected_text(name_of(cur, all)).width(180.0).show_ui(ui, |ui| {
                for (s, l) in all {
                    if ui.selectable_label(s == cur, l).clicked() {
                        *cur = s.clone();
                    }
                }
            });
        };
        ui.horizontal(|ui| {
            if a.pair == 1 {
                ui.label(RichText::new("From").color(t.text_dim));
                combo(ui, "tr-add-from", &mut a.from);
                ui.label(RichText::new("to").color(t.text_dim));
            } else {
                ui.label(RichText::new("Switching to").color(t.text_dim));
            }
            combo(ui, "tr-add-to", &mut a.to);
        });
        let exists = if a.pair == 1 { u.pairs.iter().any(|(to, from, _)| *to == a.to && *from == a.from) } else { u.scenes.iter().any(|(s, _)| *s == a.to) };
        let same = a.pair == 1 && a.from == a.to;
        ui.add_space(spacing::S);
        ui.horizontal(|ui| {
            if widgets::button_ex(ui, t, Some(icon::PLUS), "Add", Kind::Primary, Size::Small, 0.0, !exists && !same && !a.to.is_empty()).clicked() {
                // start from what it does today: the default (or a Crossfade)
                let seed = if u.default == Choice::Inherit { Choice::Always("fade".into()) } else { u.default.clone() };
                edits.push(if a.pair == 1 { Edit::Pair { to: a.to.clone(), from: a.from.clone(), choice: seed } } else { Edit::Scene(a.to.clone(), seed) });
                close = true;
            }
            if widgets::button_ex(ui, t, None, "Cancel", Kind::Ghost, Size::Small, 0.0, true).clicked() {
                close = true;
            }
            if exists {
                widgets::hint(ui, t, "That one is already below.");
            } else if same {
                widgets::hint(ui, t, "Pick two different scenes.");
            }
        });
    });
    st.adding = if close { None } else { Some(a) };
}

/// Show `a`, then switch to `b` with transition `id` at its own speed (off air only).
fn try_it(app: &mut App, id: &str, a: &str, b: &str, ms: u64) {
    if app.m.str("show.scene.program") != a {
        app.m.command(Op::SceneCut { scene: a.to_string(), transition: Some("cut".into()) });
    }
    if app.m.b("show.direct") {
        // "up next" switches straight away in direct mode: cut with the transition instead
        app.m.command(Op::SceneCut { scene: b.to_string(), transition: Some(id.to_string()) });
    } else {
        app.m.command(Op::SceneGo { scene: b.to_string() });
        app.m.command(Op::SceneTake { transition: Some(id.to_string()), ms: Some(ms.min(u32::MAX as u64) as u32) });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pool(names: &[&str]) -> Vec<(String, f64)> {
        names.iter().map(|n| (n.to_string(), 1.0)).collect()
    }

    const PROJECT_TOML: &str = r#"# The drummer's stream.
schema = 1
name = "stream"   # shown in the title bar

[transition_vote]
window = "30s"    # votes older than this don't count
"#;

    #[test]
    fn project_default_round_trips_and_keeps_comments() {
        let three = Choice::Random { pool: pool(&["morph", "fade", "zoomblur"]), avoid_repeat: 1 };
        let text = set_choice(PROJECT_TOML, None, &three).unwrap();
        assert_eq!(read_choice(&text, None), three);
        assert!(text.contains("name = \"stream\"   # shown in the title bar") && text.contains("window = \"30s\"    # votes older"), "{text}");
        // the engine reads the same default
        let p: se_core::config::ProjectDef = toml::from_str(&text).unwrap();
        assert_eq!(p.transitions.pool.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["morph", "fade", "zoomblur"]);
        assert_eq!((p.transitions.avoid_repeat, p.transitions.name), (1, None));
        // "always use" replaces the pool; a comment after the value stays
        let commented = text.replace("avoid_repeat = 1", "avoid_repeat = 1 # keep it fresh").replace("\npool = ", "\nname = \"morph\" # for now\npool = ");
        let always = set_choice(&commented, None, &Choice::Always("cut".into())).unwrap();
        assert!(always.contains("name = \"cut\" # for now"), "{always}");
        let p: se_core::config::ProjectDef = toml::from_str(&always).unwrap();
        assert_eq!((p.transitions.name.as_deref(), p.transitions.pool.len(), p.transitions.avoid_repeat), (Some("cut"), 0, 0));
        // back to nothing: the file is what it was
        assert_eq!(set_choice(&text, None, &Choice::Inherit).unwrap(), PROJECT_TOML);
    }

    const WIDE: &str = r#"label = "Wide"

[canvas.wide]
nodes = [{ src = "cam_room" }]

[transitions]
pool = [{ name = "morph", w = 2 }, { name = "zoomblur", w = 1 }]   # mostly glides
avoid_repeat = 1
ms = [500, 800]
# Scene pairs: settings for takes coming from one scene.
[transitions.from.kit]
pool = [{ name = "glitch" }]
ms = 450

[transitions.from.brb]
ms = 1200                      # back from the break: slower
"#;

    #[test]
    fn scene_exceptions_round_trip() {
        assert_eq!(read_choice(WIDE, None), Choice::Random { pool: vec![("morph".into(), 2.0), ("zoomblur".into(), 1.0)], avoid_repeat: 1 });
        assert_eq!(
            pair_choices(WIDE),
            vec![("kit".to_string(), Choice::Random { pool: pool(&["glitch"]), avoid_repeat: 0 })],
            "a pair with only a speed isn't a choice"
        );
        // pairs: "always cut" from brb keeps its speed and comment; kit's pool changes, weights of kept ones stay
        let text = set_choice(WIDE, Some("brb"), &Choice::Always("cut".into())).unwrap();
        let text = set_choice(&text, Some("kit"), &Choice::Random { pool: pool(&["glitch", "fade"]), avoid_repeat: 0 }).unwrap();
        assert!(text.contains("ms = 1200                      # back from the break: slower"), "{text}");
        assert!(text.contains("# Scene pairs: settings for takes coming from one scene.\n[transitions.from.kit]"), "{text}");
        let s: se_core::config::SceneDef = toml::from_str(&text).unwrap();
        let brb = s.transitions.for_pair("brb");
        assert_eq!((brb.name.as_deref(), brb.ms_range()), (Some("cut"), Some((1200, 1200))));
        assert_eq!(s.transitions.for_pair("kit").pool.iter().map(|e| e.name.as_str()).collect::<Vec<_>>(), ["glitch", "fade"]);
        // the scene's own choice: weights kept for what stays, the speed and the pairs untouched
        let text = set_choice(&text, None, &Choice::Always("fade".into())).unwrap();
        let s: se_core::config::SceneDef = toml::from_str(&text).unwrap();
        assert_eq!(
            (s.transitions.name.as_deref(), s.transitions.pool.len(), s.transitions.avoid_repeat, s.transitions.ms_range()),
            (Some("fade"), 0, 0, Some((500, 800)))
        );
        assert_eq!(s.transitions.from.len(), 2);
        // removing: brb's pair keeps its speed, the scene goes back to the default
        let text = set_choice(&text, Some("brb"), &Choice::Inherit).unwrap();
        let text = set_choice(&text, None, &Choice::Inherit).unwrap();
        let s: se_core::config::SceneDef = toml::from_str(&text).unwrap();
        assert_eq!((s.transitions.name, s.transitions.pool.len()), (None, 0));
        assert_eq!(s.transitions.from["brb"].ms_range(), Some((1200, 1200)));
        assert!(text.starts_with("label = \"Wide\"\n\n[canvas.wide]"), "{text}");
    }

    #[test]
    fn a_new_pair_in_a_scene_without_transitions() {
        let src = "# Just the kit.\nlabel = \"Kit\"\n\n[canvas.wide]\nnodes = [{ src = \"cam_kit\" }]\n";
        let text = set_choice(src, Some("duo"), &Choice::Always("cut".into())).unwrap();
        assert!(text.contains("[transitions.from.duo]\nname = \"cut\""), "{text}");
        assert!(!text.contains("[transitions]\n") && !text.contains("[transitions.from]\n"), "no empty headers: {text}");
        assert_eq!(pair_choices(&text), vec![("duo".to_string(), Choice::Always("cut".into()))]);
        assert_eq!(read_choice(&text, None), Choice::Inherit, "the scene itself still uses the default");
        // removing the only exception gives the file back as it was
        assert_eq!(set_choice(&text, Some("duo"), &Choice::Inherit).unwrap(), src);
        assert!(set_choice("not [toml", None, &Choice::Always("cut".into())).is_err());
    }

    #[test]
    fn switching_ways_starts_from_the_default() {
        let default = Choice::Random { pool: pool(&["morph", "fade"]), avoid_repeat: 1 };
        assert_eq!(switch(&Choice::Inherit, Some(true), &default), default);
        assert_eq!(switch(&Choice::Inherit, Some(false), &default), Choice::Always("morph".into()));
        assert_eq!(switch(&Choice::Always("cut".into()), Some(true), &default), Choice::Random { pool: pool(&["cut"]), avoid_repeat: 1 });
        assert_eq!(switch(&default, Some(false), &Choice::Inherit), Choice::Always("morph".into()));
        assert_eq!(switch(&Choice::Inherit, Some(false), &Choice::Inherit), Choice::Always("fade".into()));
        assert_eq!(switch(&default, None, &default), Choice::Inherit);
    }

    #[test]
    fn looks_of_files_and_copies() {
        assert_eq!(look_of("morph", None), Look::Glide);
        assert_eq!(look_of("combined", Some("glitch")), Look::GlideGlitch);
        assert_eq!(look_of("shader", Some("transitions/zoomblur.wgsl")), Look::Zoom, "a project copy of a built-in looks the same");
        assert_eq!(look_of("shader", Some("transitions/swirl.wgsl")), Look::Own);
        assert_eq!(look_of("shader", Some("patch.wipe")), Look::Own);
        assert_eq!(look_of("shader", None), Look::Fade);
        assert_eq!(look_of("cut", None), Look::Cut);
    }

    #[test]
    fn default_switch_adds_and_removes_one() {
        let two = Choice::Random { pool: pool(&["morph", "zoomblur"]), avoid_repeat: 1 };
        assert_eq!(toggle_default(&two, "cut", true), Choice::Random { pool: pool(&["morph", "zoomblur", "cut"]), avoid_repeat: 1 });
        assert_eq!(toggle_default(&two, "morph", false), Choice::Random { pool: pool(&["zoomblur"]), avoid_repeat: 1 });
        assert_eq!(
            toggle_default(&Choice::Random { pool: pool(&["morph"]), avoid_repeat: 1 }, "morph", false),
            Choice::Inherit,
            "the last one out: nothing picked"
        );
        // nothing picked is "always a Crossfade": adding keeps the Crossfade in the mix
        assert_eq!(toggle_default(&Choice::Inherit, "morph", true), Choice::Random { pool: pool(&["fade", "morph"]), avoid_repeat: 1 });
        assert!(in_default(&toggle_default(&Choice::Inherit, "fade", false), "fade"), "the Crossfade can't be switched off by itself");
        assert_eq!(toggle_default(&Choice::Always("cut".into()), "morph", true), Choice::Random { pool: pool(&["cut", "morph"]), avoid_repeat: 1 });
        assert_eq!(toggle_default(&Choice::Always("cut".into()), "cut", false), Choice::Inherit);
        assert_eq!(toggle_default(&Choice::Always("cut".into()), "morph", false), Choice::Always("cut".into()));
    }

    #[test]
    fn name_file_makes_a_custom_shader_pickable() {
        let text = name_file("Neon \"wipe\"", "neon_wipe");
        let d: se_core::config::TransitionDef = toml::from_str(&text).unwrap();
        assert_eq!((d.label.as_deref(), d.kind.as_str(), d.shader.as_deref()), (Some("Neon \"wipe\""), "shader", Some("patch.neon_wipe")));
        assert_eq!(look_of(&d.kind, d.shader.as_deref()), Look::Own);
    }
}
