//! Edits to `scenes/<name>.toml` made by the Scenes page: add/remove/reorder layers, layer and
//! scene effects, the transition pool and speed, new and duplicated scenes. Pure text → text
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
    ("lut", "Color look (LUT)"),
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

/// The transition pool: (name, weight).
pub fn pool(text: &str) -> Vec<(String, f64)> {
    let Ok(doc) = parse(text) else { return Vec::new() };
    doc.get("transitions")
        .and_then(|t| t.get("pool"))
        .and_then(Item::as_array)
        .map(|a| {
            a.iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some((s.value().clone(), 1.0)),
                    Value::InlineTable(t) => {
                        let w = t.get("w").and_then(|w| w.as_float().or_else(|| w.as_integer().map(|i| i as f64))).unwrap_or(1.0);
                        t.get("name").and_then(Value::as_str).map(|n| (n.to_string(), w))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn transitions_table(doc: &mut DocumentMut) -> &mut Table {
    if !doc.contains_key("transitions") {
        doc.insert("transitions", Item::Table(Table::new()));
    }
    doc["transitions"].as_table_mut().expect("transitions is a table")
}

/// Replace the transition pool (empty = remove it: the default transition is used).
pub fn set_pool(text: &str, entries: &[(String, f64)]) -> Result<String> {
    let mut doc = parse(text)?;
    let t = transitions_table(&mut doc);
    if entries.is_empty() {
        t.remove("pool");
    } else {
        let mut a = Array::new();
        for (n, w) in entries {
            let mut e = InlineTable::new();
            e.insert("name", n.as_str().into());
            if (*w - 1.0).abs() > f64::EPSILON {
                e.insert("w", if w.fract() == 0.0 { Value::from(*w as i64) } else { Value::from(*w) });
            }
            a.push(Value::InlineTable(e));
        }
        t.insert("pool", value(a));
    }
    Ok(doc.to_string())
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

/// A new, empty scene.
pub fn new_scene(label: &str, key: Option<i64>) -> String {
    let mut s = format!("# Made in Stream Engine.\nlabel = {}\n", Value::from(label));
    if let Some(k) = key {
        s.push_str(&format!("key = {k}\n"));
    }
    s.push_str("\n[canvas.wide]\nnodes = []\n\n[canvas.tall]\nnodes = []\n\n[transitions]\npool = [{ name = \"morph\" }]\n");
    s
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
    fn transition_pool_and_speed() {
        assert_eq!(pool(DUO), [("morph".to_string(), 3.0), ("fade".to_string(), 1.0)]);
        assert_eq!(speed(DUO), Some([500, 900]));
        let p = set_pool(DUO, &[("glitch".into(), 1.0), ("morph".into(), 2.0)]).unwrap();
        assert_eq!(pool(&p), [("glitch".to_string(), 1.0), ("morph".to_string(), 2.0)]);
        assert!(p.contains("{ name = \"morph\", w = 2 }"), "{p}");
        assert!(pool(&set_pool(DUO, &[]).unwrap()).is_empty());
        let s = set_speed(DUO, Some([300, 300])).unwrap();
        assert_eq!(speed(&s), Some([300, 300]));
        assert_eq!(speed(&set_speed(DUO, None).unwrap()), None);
    }

    #[test]
    fn new_duplicate_rename_and_slug() {
        let n = new_scene("Chill \"cam\"", Some(7));
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
}
