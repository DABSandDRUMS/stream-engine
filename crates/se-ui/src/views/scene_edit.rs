//! Edits to `scenes/<name>.toml` made by the Scenes page: add/remove/reorder layers, layer and
//! scene effects, when a layer shows, what the scene does coming on and going off, its
//! background color, the transition speed, new and duplicated scenes. Pure text → text
//! with `toml_edit`, so comments and formatting the owner wrote survive; the page sends the
//! result with `project.write {path, text}` and the engine hot-reloads it.

use anyhow::{Context as _, Result, anyhow};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, Value, value};

/// Built-in effects that can be attached to a layer or a whole scene: (id, friendly name).
pub const EFFECTS: &[(&str, &str)] = &[
    ("grade", "Color grade"),
    ("blur", "Blur"),
    ("pixelate", "Pixelate"),
    ("glitch", "Glitch"),
    ("rgb_split", "Color split"),
    ("vhs", "VHS tape"),
    ("vignette", "Vignette"),
    ("zoom_pulse", "Zoom pulse"),
    ("chroma_key", "Green screen"),
    ("lut", "Color look"),
];

/// Friendly name of an effect id (`patch.aurora` → `Aurora`).
pub fn effect_name(id: &str) -> String {
    if let Some((_, n)) = EFFECTS.iter().find(|(e, _)| *e == id) {
        return n.to_string();
    }
    crate::views::live::nice(id.strip_prefix("patch.").unwrap_or(id))
}

fn parse(text: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>().context("the scene file isn't valid TOML")
}

/// The identity of a node entry: its `id`, else its `src`.
fn node_id(t: &InlineTable) -> Option<&str> {
    t.get("id").and_then(Value::as_str).or_else(|| t.get("src").and_then(Value::as_str))
}

/// Every `canvas.<name>.nodes` array in the document.
fn node_arrays(doc: &mut DocumentMut) -> Vec<&mut Array> {
    let Some(Item::Table(canvases)) = doc.get_mut("canvas") else { return Vec::new() };
    canvases
        .iter_mut()
        .filter_map(|(_, c)| match c {
            Item::Table(t) => t.get_mut("nodes").and_then(Item::as_array_mut),
            Item::Value(Value::InlineTable(t)) => t.get_mut("nodes").and_then(Value::as_array_mut),
            _ => None,
        })
        .collect()
}

fn canvas_table<'a>(doc: &'a mut DocumentMut, canvas: &str) -> &'a mut Table {
    let root = doc.as_table_mut();
    if !root.contains_key("canvas") {
        let mut t = Table::new();
        t.set_implicit(true);
        root.insert("canvas", Item::Table(t));
    }
    let canvases = root["canvas"].as_table_mut().expect("canvas is a table");
    if !canvases.contains_key(canvas) {
        canvases.insert(canvas, Item::Table(Table::new()));
    }
    canvases[canvas].as_table_mut().expect("canvas.<name> is a table")
}

/// Lay an array out one entry per line. Existing entries keep their own line breaks and
/// comments; a comment that trailed the old last entry stays on that entry's line.
fn multiline(a: &mut Array) {
    let trailing = a.trailing().as_str().unwrap_or("").to_string();
    let n = a.len();
    for (i, v) in a.iter_mut().enumerate() {
        let has_break = v.decor().prefix().and_then(|p| p.as_str()).is_some_and(|p| p.contains('\n'));
        if !has_break {
            // the last entry was just added: the old trailing comment belongs to the line above
            let carried = if i + 1 == n && trailing.contains('#') { trailing.trim_end().to_string() } else { String::new() };
            v.decor_mut().set_prefix(format!("{carried}\n  "));
        }
    }
    a.set_trailing("\n");
    a.set_trailing_comma(true);
}

fn rect(r: [f64; 4]) -> Value {
    let mut a = Array::new();
    for x in r {
        a.push((x * 1000.0).round() / 1000.0);
    }
    Value::Array(a)
}

/// Add a layer showing `src`, centered, on the main and vertical canvases.
pub fn add_layer(text: &str, src: &str) -> Result<String> {
    let mut doc = parse(text)?;
    for (canvas, r) in [("wide", [0.25, 0.25, 0.5, 0.5]), ("tall", [0.1, 0.3, 0.8, 0.4])] {
        let t = canvas_table(&mut doc, canvas);
        if !t.contains_key("nodes") {
            t.insert("nodes", value(Array::new()));
        }
        let arr = t["nodes"].as_array_mut().ok_or_else(|| anyhow!("canvas.{canvas}.nodes isn't a list"))?;
        let mut used: Vec<String> = arr.iter().filter_map(|v| v.as_inline_table().and_then(node_id).map(String::from)).collect();
        let mut n = InlineTable::new();
        n.insert("src", src.into());
        if used.iter().any(|u| u == src) {
            // a second layer of the same source needs its own id
            let mut i = 2;
            while used.contains(&format!("{src}_{i}")) {
                i += 1;
            }
            n.insert("id", format!("{src}_{i}").into());
            used.push(format!("{src}_{i}"));
        }
        n.insert("rect", rect(r));
        arr.push(Value::InlineTable(n));
        multiline(arr);
    }
    Ok(doc.to_string())
}

/// Remove the layer `node` from every canvas.
pub fn remove_layer(text: &str, node: &str) -> Result<String> {
    let mut doc = parse(text)?;
    for arr in node_arrays(&mut doc) {
        arr.retain(|v| v.as_inline_table().and_then(node_id) != Some(node));
    }
    Ok(doc.to_string())
}

/// Move the layer `node` one step toward the front (`forward`) or back, on every canvas.
/// Later entries draw on top.
pub fn move_layer(text: &str, node: &str, forward: bool) -> Result<String> {
    let mut doc = parse(text)?;
    for arr in node_arrays(&mut doc) {
        let Some(i) = arr.iter().position(|v| v.as_inline_table().and_then(node_id) == Some(node)) else { continue };
        let j = if forward { i + 1 } else { i.wrapping_sub(1) };
        if j >= arr.len() {
            continue;
        }
        let a = arr.remove(i);
        arr.insert(j, a);
        multiline(arr);
    }
    Ok(doc.to_string())
}

/// Show `src` in the layer `node` instead (every canvas; place, size and effects stay). A layer
/// without its own `id` is named after what it shows, so it gets a new name unless another
/// layer already shows `src`. Returns the new text and the layer's name afterwards.
pub fn set_layer_source(text: &str, node: &str, src: &str) -> Result<(String, String)> {
    let mut doc = parse(text)?;
    let mut arrays = node_arrays(&mut doc);
    let entries = |a: &Array| a.iter().filter_map(Value::as_inline_table).map(|t| (node_id(t).map(String::from), t.get("id").is_some())).collect::<Vec<_>>();
    let all: Vec<(Option<String>, bool)> = arrays.iter().flat_map(|a| entries(a)).collect();
    let Some(explicit) = all.iter().find(|(id, _)| id.as_deref() == Some(node)).map(|(_, e)| *e) else {
        return Err(anyhow!("this scene has no layer `{node}`"));
    };
    // a layer named after its source follows the new source, unless that name is taken
    let taken = src != node && all.iter().any(|(id, _)| id.as_deref() == Some(src));
    let new_id = if explicit || taken { node.to_string() } else { src.to_string() };
    for arr in arrays.iter_mut() {
        let Some(t) = arr.iter_mut().filter_map(Value::as_inline_table_mut).find(|t| node_id(t) == Some(node)) else { continue };
        t.insert("src", src.into());
        if !explicit && new_id != src {
            t.insert("id", new_id.as_str().into());
        }
    }
    Ok((doc.to_string(), new_id))
}

fn fx_names(a: Option<&Array>) -> Vec<String> {
    a.map(|a| {
        a.iter()
            .filter_map(|v| match v {
                Value::String(s) => Some(s.value().clone()),
                Value::InlineTable(t) => t.get("name").and_then(Value::as_str).map(String::from),
                _ => None,
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Effects attached to the layer `node` (first canvas that has the node).
pub fn layer_effects(text: &str, node: &str) -> Vec<String> {
    let Ok(mut doc) = parse(text) else { return Vec::new() };
    for arr in node_arrays(&mut doc) {
        if let Some(t) = arr.iter().filter_map(Value::as_inline_table).find(|t| node_id(t) == Some(node)) {
            return fx_names(t.get("fx").and_then(Value::as_array));
        }
    }
    Vec::new()
}

/// Add (`on`) or remove the effect `fx` on the layer `node` (every canvas).
pub fn set_layer_effect(text: &str, node: &str, fx: &str, on: bool) -> Result<String> {
    let mut doc = parse(text)?;
    for arr in node_arrays(&mut doc) {
        for v in arr.iter_mut() {
            let Some(t) = v.as_inline_table_mut() else { continue };
            if node_id(t) != Some(node) {
                continue;
            }
            toggle_fx(t.get_or_insert("fx", Array::new()).as_array_mut().ok_or_else(|| anyhow!("fx isn't a list"))?, fx, on);
            if t.get("fx").and_then(Value::as_array).is_some_and(Array::is_empty) {
                t.remove("fx");
            }
        }
    }
    Ok(doc.to_string())
}

fn toggle_fx(a: &mut Array, fx: &str, on: bool) {
    let has = fx_names(Some(a)).iter().any(|n| n == fx);
    if on && !has {
        let mut e = InlineTable::new();
        e.insert("name", fx.into());
        a.push(Value::InlineTable(e));
    } else if !on {
        a.retain(|v| match v {
            Value::String(s) => s.value() != fx,
            Value::InlineTable(t) => t.get("name").and_then(Value::as_str) != Some(fx),
            _ => true,
        });
    }
}

/// Effects on the whole scene (`fx = [...]` at the top of the file).
pub fn scene_effects(text: &str) -> Vec<String> {
    parse(text).ok().map(|d| fx_names(d.get("fx").and_then(Item::as_array))).unwrap_or_default()
}

pub fn set_scene_effect(text: &str, fx: &str, on: bool) -> Result<String> {
    let mut doc = parse(text)?;
    if !doc.contains_key("fx") {
        doc.insert("fx", value(Array::new()));
    }
    let a = doc["fx"].as_array_mut().ok_or_else(|| anyhow!("fx isn't a list"))?;
    toggle_fx(a, fx, on);
    if a.is_empty() {
        doc.remove("fx");
    }
    Ok(doc.to_string())
}

/// The show condition of the layer `node` (`when = "…"`; first canvas that has the node).
pub fn layer_when(text: &str, node: &str) -> Option<String> {
    let mut doc = parse(text).ok()?;
    node_arrays(&mut doc)
        .into_iter()
        .find_map(|arr| {
            arr.iter().filter_map(Value::as_inline_table).find(|t| node_id(t) == Some(node)).map(|t| t.get("when").and_then(Value::as_str).map(String::from))
        })
        .flatten()
        .filter(|w| !w.trim().is_empty())
}

/// Show the layer `node` only while `when` is true (`None` or empty = always), on every canvas.
pub fn set_layer_when(text: &str, node: &str, when: Option<&str>) -> Result<String> {
    let mut doc = parse(text)?;
    let when = when.map(str::trim).filter(|w| !w.is_empty());
    let mut found = false;
    for arr in node_arrays(&mut doc) {
        for t in arr.iter_mut().filter_map(Value::as_inline_table_mut).filter(|t| node_id(t) == Some(node)) {
            found = true;
            match when {
                Some(w) => {
                    t.insert("when", w.into());
                }
                None => {
                    t.remove("when");
                }
            }
        }
    }
    if !found {
        return Err(anyhow!("this scene has no layer `{node}`"));
    }
    Ok(doc.to_string())
}

/// One plain check of a layer's show condition. Anything else is edited as text.
#[derive(Clone, Debug, PartialEq)]
pub enum LayerCheck {
    /// `mode == 'x'` (`not`: `mode != 'x'`); an empty mode is an unfinished row.
    Mode { mode: String, not: bool },
    /// `queue.now.id` (a song request is playing) or `!queue.now.id`.
    Song { playing: bool },
    /// `patch.<id>.active` (the overlay is playing) or `!patch.<id>.active`.
    Overlay { id: String, playing: bool },
}

impl LayerCheck {
    /// Expression text; `None` while the row is unfinished.
    pub fn text(&self) -> Option<String> {
        let bang = |yes: bool| if yes { "" } else { "!" };
        match self {
            LayerCheck::Mode { mode, .. } if mode.is_empty() || mode.contains(['\'', '"']) => None,
            LayerCheck::Mode { mode, not } => Some(format!("mode {} '{mode}'", if *not { "!=" } else { "==" })),
            LayerCheck::Song { playing } => Some(format!("{}queue.now.id", bang(*playing))),
            LayerCheck::Overlay { id, .. } if id.is_empty() => None,
            LayerCheck::Overlay { id, playing } => Some(format!("{}patch.{id}.active", bang(*playing))),
        }
    }
}

fn simple_name(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn parse_check(p: &str) -> Option<LayerCheck> {
    let p = p.trim();
    for (op, not) in [("==", false), ("!=", true)] {
        if let Some((l, r)) = p.split_once(op) {
            let r = r.trim();
            let mode = ['\'', '"'].into_iter().find_map(|q| r.strip_prefix(q)?.strip_suffix(q))?;
            return (l.trim() == "mode" && simple_name(mode)).then(|| LayerCheck::Mode { mode: mode.to_string(), not });
        }
    }
    let (yes, path) = match p.strip_prefix('!') {
        Some(rest) => (false, rest.trim()),
        None => (true, p),
    };
    if path == "queue.now.id" {
        return Some(LayerCheck::Song { playing: yes });
    }
    let id = path.strip_prefix("patch.")?.strip_suffix(".active")?;
    simple_name(id).then(|| LayerCheck::Overlay { id: id.to_string(), playing: yes })
}

/// The plain checks of a show condition (all must be true); `None` when it needs the text editor.
pub fn parse_layer_when(src: &str) -> Option<Vec<LayerCheck>> {
    let src = src.trim();
    if src.is_empty() {
        return Some(Vec::new());
    }
    if src.contains("||") || src.contains('(') {
        return None;
    }
    src.split("&&").map(parse_check).collect()
}

/// The condition for `checks` (finished rows only), empty when there are none.
pub fn layer_when_text(checks: &[LayerCheck]) -> String {
    checks.iter().filter_map(LayerCheck::text).collect::<Vec<_>>().join(" && ")
}

/// The random duration range in ms (`transitions.ms = [lo, hi]` or a single number).
pub fn speed(text: &str) -> Option<[i64; 2]> {
    let doc = parse(text).ok()?;
    match doc.get("transitions")?.get("ms")? {
        Item::Value(Value::Array(a)) if a.len() == 2 => Some([a.get(0)?.as_integer()?, a.get(1)?.as_integer()?]),
        Item::Value(Value::Integer(i)) => Some([*i.value(), *i.value()]),
        _ => None,
    }
}

fn transitions_table(doc: &mut DocumentMut) -> &mut Table {
    if !doc.contains_key("transitions") {
        doc.insert("transitions", Item::Table(Table::new()));
    }
    doc["transitions"].as_table_mut().expect("transitions is a table")
}

pub fn set_speed(text: &str, ms: Option<[i64; 2]>) -> Result<String> {
    let mut doc = parse(text)?;
    let t = transitions_table(&mut doc);
    match ms {
        None => {
            t.remove("ms");
        }
        Some([lo, hi]) => {
            let mut a = Array::new();
            a.push(lo);
            a.push(hi);
            t.insert("ms", value(a));
        }
    }
    Ok(doc.to_string())
}

/// A scene's command list: `on_enter` ("when this scene comes on") or `on_exit`.
pub fn scene_commands(text: &str, key: &str) -> Vec<String> {
    let Ok(doc) = parse(text) else { return Vec::new() };
    doc.get(key).and_then(Item::as_array).map(|a| a.iter().filter_map(Value::as_str).map(String::from).collect()).unwrap_or_default()
}

/// Replace the command list `key` (`on_enter` / `on_exit`); empty removes it.
pub fn set_scene_commands(text: &str, key: &str, cmds: &[String]) -> Result<String> {
    let mut doc = parse(text)?;
    if cmds.is_empty() {
        doc.remove(key);
    } else {
        let mut a = Array::new();
        for c in cmds {
            a.push(c.as_str());
        }
        match doc.get_mut(key) {
            // keep the key's own comments and place
            Some(Item::Value(v)) => {
                let decor = v.decor().clone();
                *v = Value::Array(a);
                *v.decor_mut() = decor;
            }
            _ => {
                doc.insert(key, value(a));
            }
        }
    }
    Ok(doc.to_string())
}

/// The scene's background color (`"#rrggbb"`) on the main video (else any video that has one).
pub fn background(text: &str) -> Option<String> {
    let doc = parse(text).ok()?;
    let canvases = doc.get("canvas")?;
    let of = |c: &str| canvases.get(c).and_then(|c| c.get("background")).and_then(Item::as_str).map(String::from);
    of("wide").or_else(|| canvases.as_table_like()?.iter().find_map(|(_, c)| c.get("background").and_then(Item::as_str).map(String::from)))
}

/// Set (or with `None` remove) the background color on every video the scene lays out (the main
/// one when it has none yet).
pub fn set_background(text: &str, color: Option<&str>) -> Result<String> {
    let mut doc = parse(text)?;
    if doc.get("canvas").and_then(Item::as_table_like).is_none_or(|c| c.is_empty()) {
        if color.is_none() {
            return Ok(doc.to_string());
        }
        canvas_table(&mut doc, "wide");
    }
    let canvases = doc["canvas"].as_table_like_mut().ok_or_else(|| anyhow!("canvas isn't a table"))?;
    for (_, c) in canvases.iter_mut() {
        let Some(t) = c.as_table_like_mut() else { continue };
        match (color, t.get_mut("background")) {
            // keep the line's own comment
            (Some(col), Some(Item::Value(v))) => {
                let decor = v.decor().clone();
                *v = col.into();
                *v.decor_mut() = decor;
            }
            (Some(col), _) => {
                t.insert("background", value(col));
            }
            (None, _) => {
                t.remove("background");
            }
        }
    }
    Ok(doc.to_string())
}

/// A generic starting layout for a new scene: camera boxes on the main (16:9) and vertical
/// (9:16) videos, later boxes in front.
pub struct StartLayout {
    pub id: &'static str,
    pub label: &'static str,
    /// Boxes `[x, y, w, h]` (0–1 of the canvas) on the main video.
    pub wide: &'static [[f64; 4]],
    /// The same boxes on the vertical video.
    pub tall: &'static [[f64; 4]],
    /// Rounded corners in canvas pixels, per box.
    pub radius: &'static [f64],
}

impl StartLayout {
    /// How many cameras it shows.
    pub fn slots(&self) -> usize {
        self.wide.len()
    }
}

/// "New scene → Start from", in menu order.
pub const START_LAYOUTS: &[StartLayout] = &[
    StartLayout { id: "one", label: "One camera", wide: &[[0.0, 0.0, 1.0, 1.0]], tall: &[[0.0, 0.0, 1.0, 1.0]], radius: &[0.0] },
    StartLayout {
        id: "side_by_side",
        label: "Side by side",
        wide: &[[0.01, 0.02, 0.485, 0.96], [0.505, 0.02, 0.485, 0.96]],
        tall: &[[0.02, 0.01, 0.96, 0.485], [0.02, 0.505, 0.96, 0.485]],
        radius: &[16.0, 16.0],
    },
    StartLayout {
        id: "pip",
        label: "Picture in picture",
        wide: &[[0.0, 0.0, 1.0, 1.0], [0.68, 0.64, 0.3, 0.3]],
        tall: &[[0.0, 0.0, 1.0, 1.0], [0.54, 0.05, 0.42, 0.13]],
        radius: &[0.0, 12.0],
    },
    StartLayout {
        id: "three",
        label: "Three across",
        wide: &[[0.01, 0.02, 0.32, 0.96], [0.34, 0.02, 0.32, 0.96], [0.67, 0.02, 0.32, 0.96]],
        tall: &[[0.02, 0.01, 0.96, 0.32], [0.02, 0.34, 0.96, 0.32], [0.02, 0.67, 0.96, 0.32]],
        radius: &[16.0, 16.0, 16.0],
    },
    StartLayout {
        id: "grid",
        label: "2×2 grid",
        wide: &[[0.01, 0.02, 0.485, 0.47], [0.505, 0.02, 0.485, 0.47], [0.01, 0.51, 0.485, 0.47], [0.505, 0.51, 0.485, 0.47]],
        tall: &[[0.02, 0.01, 0.96, 0.2375], [0.02, 0.2575, 0.96, 0.2375], [0.02, 0.505, 0.96, 0.2375], [0.02, 0.7525, 0.96, 0.2375]],
        radius: &[12.0, 12.0, 12.0, 12.0],
    },
    StartLayout {
        id: "big_two",
        label: "Big + two small",
        wide: &[[0.01, 0.02, 0.66, 0.96], [0.68, 0.02, 0.31, 0.47], [0.68, 0.51, 0.31, 0.47]],
        tall: &[[0.02, 0.01, 0.96, 0.55], [0.02, 0.57, 0.47, 0.42], [0.51, 0.57, 0.47, 0.42]],
        radius: &[16.0, 12.0, 12.0],
    },
    StartLayout { id: "blank", label: "Blank", wide: &[], tall: &[], radius: &[] },
];

/// Crop insets `[l, t, r, b]` so a `src_aspect` picture fills `rect` on a `canvas_px` canvas
/// without being squashed (the middle stays, the edges are cut).
fn cover_crop(rect: [f64; 4], canvas_px: [f64; 2], src_aspect: f64) -> [f64; 4] {
    let box_aspect = (rect[2] * canvas_px[0]) / (rect[3] * canvas_px[1]).max(1e-6);
    let round = |x: f64| (x * 1000.0).round() / 1000.0;
    if box_aspect < src_aspect {
        let side = round((1.0 - box_aspect / src_aspect) / 2.0);
        [side, 0.0, side, 0.0]
    } else {
        let side = round((1.0 - src_aspect / box_aspect) / 2.0);
        [0.0, side, 0.0, side]
    }
}

/// A new scene laid out as `layout`, its boxes showing `cams` (name, picture aspect) in order
/// (repeated when there are fewer cameras than boxes). `canvases` = main and vertical video
/// sizes in pixels.
pub fn new_scene(label: &str, key: Option<i64>, layout: &StartLayout, cams: &[(String, f64)], canvases: [[f64; 2]; 2]) -> Result<String> {
    let mut s = format!("# Made in Stream Engine.\nlabel = {}\n", Value::from(label));
    if let Some(k) = key {
        s.push_str(&format!("key = {k}\n"));
    }
    s.push_str("\n[canvas.wide]\nnodes = []\n\n[canvas.tall]\nnodes = []\n\n[transitions]\npool = [{ name = \"morph\" }]\n");
    if layout.slots() == 0 {
        return Ok(s);
    }
    if cams.is_empty() {
        return Err(anyhow!("there are no cameras to put in it yet"));
    }
    // one name per box, the same on both videos (so a glide matches them up)
    let mut ids: Vec<(String, Option<String>)> = Vec::new();
    for i in 0..layout.slots() {
        let src = cams[i % cams.len()].0.clone();
        let mut id = src.clone();
        let mut n = 2;
        while ids.iter().any(|(s, x)| x.as_deref().unwrap_or(s) == id) {
            id = format!("{src}_{n}");
            n += 1;
        }
        ids.push((src.clone(), (id != src).then_some(id)));
    }
    let mut doc = parse(&s)?;
    for (canvas, rects, px) in [("wide", layout.wide, canvases[0]), ("tall", layout.tall, canvases[1])] {
        let arr = canvas_table(&mut doc, canvas)["nodes"].as_array_mut().ok_or_else(|| anyhow!("canvas.{canvas}.nodes isn't a list"))?;
        for (i, r) in rects.iter().enumerate() {
            let (src, id) = &ids[i];
            let mut n = InlineTable::new();
            n.insert("src", src.as_str().into());
            if let Some(id) = id {
                n.insert("id", id.as_str().into());
            }
            n.insert("rect", rect(*r));
            let aspect = cams[i % cams.len()].1;
            let crop = cover_crop(*r, px, aspect);
            if crop.iter().any(|c| *c >= 0.005) {
                n.insert("crop", rect(crop));
            }
            let radius = layout.radius.get(i).copied().unwrap_or(0.0);
            if radius > 0.0 {
                n.insert("radius", (radius as i64).into());
            }
            arr.push(Value::InlineTable(n));
        }
        multiline(arr);
    }
    Ok(doc.to_string())
}

/// A copy of a scene with a new label and number key.
pub fn duplicate(text: &str, label: &str, key: Option<i64>) -> Result<String> {
    let mut doc = parse(text)?;
    doc.insert("label", value(label));
    match key {
        Some(k) => {
            doc.insert("key", value(k));
        }
        None => {
            doc.remove("key");
        }
    }
    Ok(doc.to_string())
}

/// Rename a scene (its display label; the file name stays).
pub fn set_label(text: &str, label: &str) -> Result<String> {
    let mut doc = parse(text)?;
    doc.insert("label", value(label));
    Ok(doc.to_string())
}

/// A file-name-safe scene name from a label (`My Scene!` → `my_scene`).
pub fn slug(label: &str) -> String {
    let mut s: String = label.trim().to_lowercase().chars().map(|c| if c.is_ascii_alphanumeric() { c } else { '_' }).collect();
    while s.contains("__") {
        s = s.replace("__", "_");
    }
    s.trim_matches('_').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const DUO: &str = r#"# Kit front + high corner wide, side by side.
label = "duo"
key = 1

[canvas.wide]
nodes = [
  { src = "cam_kit",  rect = [0.02, 0.05, 0.47, 0.90], radius = 24 },
  { src = "cam_wide", rect = [0.51, 0.05, 0.47, 0.90], radius = 24, fx = [{ name = "vhs", when = "mode == 'chill'" }] },
]

[canvas.tall]
nodes = [
  { src = "cam_kit",  rect = [0.0, 0.0, 1.0, 0.55] },   # top
]

[transitions]
pool = [{ name = "morph", w = 3 }, { name = "fade", w = 1 }]
ms = [500, 900]
"#;

    fn nodes(text: &str, canvas: &str) -> Vec<String> {
        let doc: DocumentMut = text.parse().unwrap();
        doc["canvas"][canvas]["nodes"].as_array().unwrap().iter().filter_map(|v| v.as_inline_table().and_then(node_id).map(String::from)).collect()
    }

    #[test]
    fn add_layer_on_both_canvases_keeps_comments_and_gives_duplicates_ids() {
        let out = add_layer(DUO, "youtube").unwrap();
        assert!(out.starts_with("# Kit front + high corner wide"), "{out}");
        assert!(out.contains("# top"), "{out}");
        assert_eq!(nodes(&out, "wide"), ["cam_kit", "cam_wide", "youtube"]);
        assert_eq!(nodes(&out, "tall"), ["cam_kit", "youtube"]);
        let twice = add_layer(&out, "youtube").unwrap();
        assert_eq!(nodes(&twice, "wide"), ["cam_kit", "cam_wide", "youtube", "youtube_2"]);
        // a scene without canvases gets them
        let fresh = add_layer("label = \"x\"\n", "cam_kit").unwrap();
        assert_eq!(nodes(&fresh, "wide"), ["cam_kit"]);
        assert_eq!(nodes(&fresh, "tall"), ["cam_kit"]);
        assert!(fresh.parse::<DocumentMut>().is_ok());
    }

    #[test]
    fn remove_and_reorder_layers() {
        let out = remove_layer(DUO, "cam_kit").unwrap();
        assert_eq!(nodes(&out, "wide"), ["cam_wide"]);
        assert!(nodes(&out, "tall").is_empty());
        let fwd = move_layer(DUO, "cam_kit", true).unwrap();
        assert_eq!(nodes(&fwd, "wide"), ["cam_wide", "cam_kit"]);
        assert_eq!(nodes(&fwd, "tall"), ["cam_kit"], "single-entry canvas unchanged");
        // already at the back: no change
        assert_eq!(nodes(&move_layer(DUO, "cam_kit", false).unwrap(), "wide"), ["cam_kit", "cam_wide"]);
    }

    #[test]
    fn layer_and_scene_effects_toggle() {
        assert_eq!(layer_effects(DUO, "cam_wide"), ["vhs"]);
        let on = set_layer_effect(DUO, "cam_kit", "blur", true).unwrap();
        assert_eq!(layer_effects(&on, "cam_kit"), ["blur"]);
        let again = set_layer_effect(&on, "cam_kit", "blur", true).unwrap();
        assert_eq!(layer_effects(&again, "cam_kit"), ["blur"], "no duplicates");
        let off = set_layer_effect(&on, "cam_wide", "vhs", false).unwrap();
        assert!(layer_effects(&off, "cam_wide").is_empty());
        assert!(!off.contains("when = \"mode"), "{off}");
        let s = set_scene_effect(DUO, "grade", true).unwrap();
        assert_eq!(scene_effects(&s), ["grade"]);
        assert!(scene_effects(&set_scene_effect(&s, "grade", false).unwrap()).is_empty());
    }

    #[test]
    fn transition_speed() {
        assert_eq!(speed(DUO), Some([500, 900]));
        let s = set_speed(DUO, Some([300, 300])).unwrap();
        assert_eq!(speed(&s), Some([300, 300]));
        assert_eq!(speed(&set_speed(DUO, None).unwrap()), None);
    }

    #[test]
    fn new_duplicate_rename_and_slug() {
        let blank = START_LAYOUTS.iter().find(|l| l.id == "blank").unwrap();
        let n = new_scene("Chill \"cam\"", Some(7), blank, &[], [[1920.0, 1080.0], [1080.0, 1920.0]]).unwrap();
        let doc: DocumentMut = n.parse().unwrap();
        assert_eq!(doc["label"].as_str(), Some("Chill \"cam\""));
        assert_eq!(doc["key"].as_integer(), Some(7));
        let d = duplicate(DUO, "Duo copy", None).unwrap();
        assert!(d.contains("label = \"Duo copy\"") && !d.contains("key = 1"), "{d}");
        assert!(set_label(DUO, "Two cams").unwrap().contains("label = \"Two cams\""));
        assert_eq!(slug("  My Scene! 2 "), "my_scene_2");
        assert_eq!(slug("--"), "");
        assert!(set_label("not [toml", "x").is_err());
    }

    fn node_entries(text: &str, canvas: &str) -> Vec<(String, String, [f64; 4], [f64; 4])> {
        let doc: DocumentMut = text.parse().unwrap();
        let four = |v: Option<&Value>| {
            let a = v.and_then(Value::as_array);
            let mut out = [0.0; 4];
            for (i, x) in a.into_iter().flat_map(|a| a.iter()).enumerate().take(4) {
                out[i] = x.as_float().or_else(|| x.as_integer().map(|i| i as f64)).unwrap();
            }
            out
        };
        doc["canvas"][canvas]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| {
                let t = v.as_inline_table().unwrap();
                (node_id(t).unwrap().to_string(), t.get("src").and_then(Value::as_str).unwrap().to_string(), four(t.get("rect")), four(t.get("crop")))
            })
            .collect()
    }

    #[test]
    fn start_layouts_fill_both_videos_with_unsquashed_cameras() {
        let cams = [("cam_kit".to_string(), 16.0 / 9.0), ("cam_wide".to_string(), 16.0 / 9.0)];
        let sizes = [[1920.0, 1080.0], [1080.0, 1920.0]];
        for l in START_LAYOUTS {
            let text = new_scene(l.label, Some(3), l, &cams, sizes).unwrap();
            let def: se_core::config::SceneDef = toml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}\n{text}", l.id));
            assert_eq!(def.key, Some(3));
            let (wide, tall) = (node_entries(&text, "wide"), node_entries(&text, "tall"));
            assert_eq!((wide.len(), tall.len()), (l.slots(), l.slots()), "{}", l.id);
            let ids = |n: &[(String, String, [f64; 4], [f64; 4])]| n.iter().map(|e| e.0.clone()).collect::<Vec<_>>();
            assert_eq!(ids(&wide), ids(&tall), "{}: the same layers on both videos", l.id);
            let mut unique = ids(&wide);
            unique.sort();
            unique.dedup();
            assert_eq!(unique.len(), l.slots(), "{}: every box has its own name", l.id);
            for (canvas, nodes, [cw, ch]) in [("wide", &wide, sizes[0]), ("tall", &tall, sizes[1])] {
                for (id, src, r, c) in nodes {
                    assert!(cams.iter().any(|(n, _)| n == src), "{}: {src}", l.id);
                    assert!(r[0] >= 0.0 && r[1] >= 0.0 && r[0] + r[2] <= 1.0001 && r[1] + r[3] <= 1.0001, "{} {canvas} {id}: {r:?} off screen", l.id);
                    // what's left after the crop has the camera's shape, so the box shows it unsquashed
                    let shown = (r[2] * cw) / (r[3] * ch) * (1.0 - c[1] - c[3]) / (1.0 - c[0] - c[2]);
                    assert!((shown / (16.0 / 9.0) - 1.0).abs() < 0.02, "{} {canvas} {id}: box {r:?} crop {c:?}", l.id);
                }
            }
        }
        // more boxes than cameras: cameras repeat under their own names
        let grid = START_LAYOUTS.iter().find(|l| l.id == "grid").unwrap();
        let text = new_scene("Grid", None, grid, &cams, sizes).unwrap();
        let wide = node_entries(&text, "wide");
        let srcs: Vec<&str> = wide.iter().map(|e| e.1.as_str()).collect();
        assert_eq!(srcs, ["cam_kit", "cam_wide", "cam_kit", "cam_wide"]);
        assert_eq!(wide.iter().map(|e| e.0.as_str()).collect::<Vec<_>>(), ["cam_kit", "cam_wide", "cam_kit_2", "cam_wide_2"]);
        assert!(new_scene("x", None, grid, &[], sizes).is_err(), "a camera layout needs cameras");
    }

    #[test]
    fn swapping_a_layers_camera_keeps_its_place_and_names_it_once() {
        let (out, id) = set_layer_source(DUO, "cam_kit", "cam_room").unwrap();
        assert_eq!(id, "cam_room");
        assert_eq!(nodes(&out, "wide"), ["cam_room", "cam_wide"]);
        assert_eq!(nodes(&out, "tall"), ["cam_room"]);
        assert_eq!(node_entries(&out, "wide")[0].2, [0.02, 0.05, 0.47, 0.90], "the box stays where it was");
        assert!(out.contains("# top"), "comments stay: {out}");
        // the camera is already in the scene: the layer keeps its name on every video
        let (out, id) = set_layer_source(DUO, "cam_kit", "cam_wide").unwrap();
        assert_eq!(id, "cam_kit");
        let wide = node_entries(&out, "wide");
        assert_eq!((wide[0].0.as_str(), wide[0].1.as_str()), ("cam_kit", "cam_wide"));
        let tall = node_entries(&out, "tall");
        assert_eq!((tall[0].0.as_str(), tall[0].1.as_str()), ("cam_kit", "cam_wide"));
        assert!(set_layer_source(DUO, "nope", "cam_room").is_err());
    }

    const STARTING_SOON: &str = r##"# Starting soon: boot, then title + chat.
label = "Starting soon"
on_enter = ["patch.terminal_boot.trigger", "patch.terminal_title.trigger"]

# 80 columns across the full width.
[canvas.wide]
background = "#000000" # black like a DOS screen
nodes = [
  { src = "patch.terminal_title", rect = [0.0, 0.0, 1.0, 0.266667], when = "!patch.terminal_boot.active" },
  { src = "patch.terminal_boot",  rect = [0.0, 0.0, 1.0, 1.0], when = "patch.terminal_boot.active" },
]

[canvas.tall]
background = "#000000"
nodes = [
  { src = "patch.terminal_title", rect = [0.0, 0.3, 1.0, 0.084375], when = "!patch.terminal_boot.active" },
  { src = "patch.terminal_boot",  rect = [0.0, 0.341797, 1.0, 0.316406], when = "patch.terminal_boot.active" },
]

[transitions]
name = "cut"
"##;

    fn engine_reads(text: &str) -> se_core::config::SceneDef {
        toml::from_str(text).unwrap_or_else(|e| panic!("{e}\n{text}"))
    }

    #[test]
    fn layer_show_condition_round_trips_on_every_video() {
        assert_eq!(layer_when(STARTING_SOON, "patch.terminal_title").as_deref(), Some("!patch.terminal_boot.active"));
        assert_eq!(layer_when(DUO, "cam_kit"), None);
        let out = set_layer_when(STARTING_SOON, "patch.terminal_title", Some("mode == 'live'")).unwrap();
        assert_eq!(layer_when(&out, "patch.terminal_title").as_deref(), Some("mode == 'live'"));
        let def = engine_reads(&out);
        for c in ["wide", "tall"] {
            assert_eq!(def.nodes(c)[0].when.as_deref(), Some("mode == 'live'"), "{c}");
            assert_eq!(def.nodes(c)[1].when.as_deref(), Some("patch.terminal_boot.active"), "{c}: other layers untouched");
        }
        assert!(out.contains("# black like a DOS screen") && out.contains("# 80 columns"), "{out}");
        let always = set_layer_when(&out, "patch.terminal_title", None).unwrap();
        assert_eq!(layer_when(&always, "patch.terminal_title"), None);
        assert!(engine_reads(&always).nodes("tall")[0].when.is_none());
        // an empty condition means always too
        assert_eq!(layer_when(&set_layer_when(DUO, "cam_kit", Some("  ")).unwrap(), "cam_kit"), None);
        assert!(set_layer_when(DUO, "nope", Some("mode == 'x'")).is_err());
    }

    #[test]
    fn plain_show_checks_read_and_write_the_same_condition() {
        use LayerCheck::*;
        let cases: &[(&str, Vec<LayerCheck>)] = &[
            ("!patch.terminal_boot.active", vec![Overlay { id: "terminal_boot".into(), playing: false }]),
            ("queue.now.id", vec![Song { playing: true }]),
            ("mode == 'chill' && !queue.now.id", vec![Mode { mode: "chill".into(), not: false }, Song { playing: false }]),
            ("mode != \"brb\"", vec![Mode { mode: "brb".into(), not: true }]),
        ];
        for (src, checks) in cases {
            assert_eq!(parse_layer_when(src).as_ref(), Some(checks), "{src}");
            let text = layer_when_text(checks);
            assert_eq!(parse_layer_when(&text).as_ref(), Some(checks), "{text}");
            se_expr::Expr::parse(&text).unwrap();
        }
        // what the plain editor can't show stays text
        for src in ["scene == 'duo'", "mode == 'a' || mode == 'b'", "(queue.now.id)", "patch.x.progress > 0.5", "mode == 'it''s'"] {
            assert_eq!(parse_layer_when(src), None, "{src}");
        }
        // unfinished rows are left out
        assert_eq!(layer_when_text(&[Mode { mode: String::new(), not: false }, Song { playing: true }]), "queue.now.id");
        assert_eq!(parse_layer_when(""), Some(Vec::new()));
    }

    #[test]
    fn scene_on_and_off_steps_round_trip() {
        let on = scene_commands(STARTING_SOON, "on_enter");
        assert_eq!(on, ["patch.terminal_boot.trigger", "patch.terminal_title.trigger"]);
        assert!(scene_commands(STARTING_SOON, "on_exit").is_empty());
        let mut more = on.clone();
        more.push("preset.fire hype".into());
        let out = set_scene_commands(STARTING_SOON, "on_enter", &more).unwrap();
        assert_eq!(engine_reads(&out).on_enter, more);
        assert!(out.starts_with("# Starting soon: boot"), "{out}");
        let exit = set_scene_commands(&out, "on_exit", &["lights.cue look=warm".to_string()]).unwrap();
        let def = engine_reads(&exit);
        assert_eq!(def.on_exit, ["lights.cue look=warm"]);
        assert_eq!(def.nodes("wide").len(), 2, "the layout is untouched");
        let none = set_scene_commands(&exit, "on_enter", &[]).unwrap();
        assert!(!none.contains("on_enter"), "{none}");
        assert!(engine_reads(&none).on_enter.is_empty());
    }

    #[test]
    fn background_color_on_every_video() {
        assert_eq!(background(STARTING_SOON).as_deref(), Some("#000000"));
        assert_eq!(background(DUO), None);
        let out = set_background(STARTING_SOON, Some("#102030")).unwrap();
        assert_eq!(background(&out).as_deref(), Some("#102030"));
        assert_eq!(out.matches("background = \"#102030\"").count(), 2, "{out}");
        assert!(out.contains("# black like a DOS screen"), "the line keeps its comment: {out}");
        let cleared = set_background(&out, None).unwrap();
        assert!(!cleared.contains("background"), "{cleared}");
        engine_reads(&cleared);
        // no videos yet: the main one gets it; clearing nothing adds nothing
        let fresh = set_background("label = \"x\"\n", Some("#ffffff")).unwrap();
        assert_eq!(background(&fresh).as_deref(), Some("#ffffff"));
        assert_eq!(set_background("label = \"x\"\n", None).unwrap(), "label = \"x\"\n");
    }
}
