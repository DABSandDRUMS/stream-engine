//! Text edits for the Scenes editor, made to `scenes/<name>.toml`: add, remove, reorder and
//! duplicate layers; swap what a layer shows; blend, mask, enter/exit animation and conditions;
//! number key, lights, enter/exit actions, background color, transition speed, and new scenes.
//! Pure text → text with `toml_edit`, preserving the owner's comments and formatting, sent
//! through `project.write`. Video FX authoring uses the shared action-based `fx_rack` editor.
//! Also the effect catalog in plain words ([`EFFECTS`]) and normalized slot query decoding.

use anyhow::{Context as _, Result, anyhow, bail};
use toml_edit::{Array, DocumentMut, InlineTable, Item, Table, TableLike, Value, value};

// ---- the effect library ---------------------------------------------------------------------

/// One setting of a built-in effect (default and range as in the engine's library,
/// `se-render/src/effects.rs`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FxParam {
    pub name: &'static str,
    pub label: &'static str,
    pub default: f64,
    pub min: f64,
    pub max: f64,
    /// Shown after the value ("px", "°", "Hz", "EV"); empty for plain amounts.
    pub unit: &'static str,
}

/// A built-in effect in plain words.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FxDef {
    pub id: &'static str,
    pub label: &'static str,
    /// One plain line: what it does.
    pub about: &'static str,
    /// Its settings after the strength every effect has ([`STRENGTH`]).
    pub params: &'static [FxParam],
    /// Can go on a layer or a scene (`fade_to_black` works on the whole output only).
    pub on_layers: bool,
}

const fn fp(name: &'static str, label: &'static str, default: f64, min: f64, max: f64, unit: &'static str) -> FxParam {
    FxParam { name, label, default, min, max, unit }
}

/// How strongly an attached effect shows (`amount`; 1 when the entry doesn't say).
pub const STRENGTH: FxParam = fp("amount", "Strength", 1.0, 0.0, 1.0, "");

/// Every built-in effect of the engine, in menu order.
pub const EFFECTS: &[FxDef] = &[
    FxDef {
        id: "grade",
        label: "Color grade",
        about: "Warmer or cooler, more contrast, richer or paler colors.",
        params: &[
            fp("warmth", "Warmth", 0.0, -1.0, 1.0, ""),
            fp("tint", "Tint", 0.0, -1.0, 1.0, ""),
            fp("contrast", "Contrast", 1.0, 0.0, 2.0, ""),
            fp("saturation", "Saturation", 1.0, 0.0, 2.0, ""),
            fp("lift", "Shadows", 0.0, -0.5, 0.5, ""),
            fp("exposure", "Exposure", 0.0, -3.0, 3.0, "EV"),
        ],
        on_layers: true,
    },
    FxDef { id: "blur", label: "Blur", about: "Softens the picture.", params: &[fp("radius", "Amount", 16.0, 0.0, 96.0, "px")], on_layers: true },
    FxDef {
        id: "pixelate",
        label: "Pixelate",
        about: "Turns the picture into big square blocks.",
        params: &[fp("size", "Block size", 32.0, 1.0, 256.0, "px")],
        on_layers: true,
    },
    FxDef {
        id: "glitch",
        label: "Glitch",
        about: "Digital breakup: slices jump sideways and colors tear.",
        params: &[
            fp("blocks", "Blocks", 0.5, 0.0, 1.0, ""),
            fp("shift", "Shift", 0.5, 0.0, 1.0, ""),
            fp("color", "Color tear", 0.5, 0.0, 1.0, ""),
            fp("speed", "Speed", 1.0, 0.0, 4.0, ""),
        ],
        on_layers: true,
    },
    FxDef {
        id: "rgb_split",
        label: "Color split",
        about: "Pulls the red and blue apart for a trippy edge.",
        params: &[fp("angle", "Direction", 0.0, -180.0, 180.0, "°"), fp("spread", "Spread", 0.012, 0.0, 0.1, "")],
        on_layers: true,
    },
    FxDef {
        id: "vhs",
        label: "VHS tape",
        about: "Old tape look: wobble, color bleed, lines and noise.",
        params: &[
            fp("noise", "Noise", 0.5, 0.0, 1.0, ""),
            fp("jitter", "Wobble", 0.5, 0.0, 1.0, ""),
            fp("scanlines", "Lines", 0.5, 0.0, 1.0, ""),
            fp("bleed", "Color bleed", 0.5, 0.0, 1.0, ""),
        ],
        on_layers: true,
    },
    FxDef {
        id: "vignette",
        label: "Vignette",
        about: "Darkens the edges to pull the eye to the middle.",
        params: &[fp("radius", "Size", 0.75, 0.1, 1.5, ""), fp("softness", "Softness", 0.45, 0.01, 1.0, "")],
        on_layers: true,
    },
    FxDef {
        id: "zoom_pulse",
        label: "Zoom pulse",
        about: "Punches in, and can pulse with the beat.",
        params: &[
            fp("zoom", "Zoom", 0.08, 0.0, 0.5, ""),
            fp("beat", "On the beat", 1.0, 0.0, 1.0, ""),
            fp("center_x", "Center X", 0.5, 0.0, 1.0, ""),
            fp("center_y", "Center Y", 0.5, 0.0, 1.0, ""),
        ],
        on_layers: true,
    },
    FxDef {
        id: "shake",
        label: "Shake",
        about: "Jolts the picture around, like an impact.",
        params: &[fp("strength", "Movement", 0.03, 0.0, 0.15, ""), fp("speed", "Speed", 12.0, 1.0, 40.0, "Hz")],
        on_layers: true,
    },
    FxDef {
        id: "chroma_key",
        label: "Green screen",
        about: "Removes a green (or any one color) background. Layers only.",
        params: &[
            fp("key_r", "Key red", 0.0, 0.0, 1.0, ""),
            fp("key_g", "Key green", 1.0, 0.0, 1.0, ""),
            fp("key_b", "Key blue", 0.0, 0.0, 1.0, ""),
            fp("similarity", "Range", 0.4, 0.0, 1.0, ""),
            fp("smoothness", "Soft edge", 0.08, 0.0, 1.0, ""),
            fp("spill", "Spill fix", 0.1, 0.0, 1.0, ""),
        ],
        on_layers: true,
    },
    FxDef { id: "lut", label: "Color look", about: "Applies a color look file (.cube) from your project.", params: &[], on_layers: true },
    FxDef {
        id: "fade_to_black",
        label: "Fade to black",
        about: "Fades everything, including what's on every scene, to black.",
        params: &[fp("color_r", "Red", 0.0, 0.0, 1.0, ""), fp("color_g", "Green", 0.0, 0.0, 1.0, ""), fp("color_b", "Blue", 0.0, 0.0, 1.0, "")],
        on_layers: false,
    },
];

/// The built-in effect `id`.
pub fn effect_def(id: &str) -> Option<&'static FxDef> {
    EFFECTS.iter().find(|e| e.id == id)
}

/// Friendly name of an effect id (`patch.aurora` → `Aurora`).
pub fn effect_name(id: &str) -> String {
    match effect_def(id) {
        Some(e) => e.label.to_string(),
        None => crate::views::live::nice(id.strip_prefix("patch.").unwrap_or(id)),
    }
}

/// One effect entry as written in a file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FxEntry {
    pub id: String,
    pub name: String,
    /// True bypass gate, independent of the effect-kind trigger envelope.
    pub enabled: bool,
    pub triggered: bool,
    /// Authored settings, retaining their types (including colors, booleans and files).
    pub params: std::collections::BTreeMap<String, se_proto::Value>,
}

impl FxEntry {
    pub fn num(&self, key: &str) -> Option<f64> {
        self.params.get(key).and_then(se_proto::Value::as_f64)
    }
    pub fn text(&self, key: &str) -> Option<&str> {
        self.params.get(key).and_then(se_proto::Value::as_str)
    }
}

/// Decode either raw TOML query entries or the normalized scene attachment shape.
/// Legacy names receive the same deterministic identity as the core config loader.
pub fn fx_values(values: &[se_proto::Value]) -> Vec<FxEntry> {
    use se_proto::Value as V;
    let mut used: std::collections::BTreeSet<String> = values.iter()
        .filter_map(|v| v.get_path("id").and_then(V::as_str).filter(|id| !id.is_empty()).map(String::from)).collect();
    let mut out = Vec::new();
    for value in values {
        let Some(name) = value.as_str().or_else(|| value.get_path("name").and_then(V::as_str)) else { continue };
        let explicit = value.get_path("id").and_then(V::as_str).filter(|id| !id.is_empty());
        let id = match explicit {
            Some(id) => id.to_string(),
            None => {
                let mut id = name.to_string();
                let mut suffix = 2;
                while used.contains(&id) {
                    id = format!("{name}_{suffix}");
                    suffix += 1;
                }
                used.insert(id.clone());
                id
            }
        };
        let mut e = FxEntry {
            id,
            name: name.to_string(),
            enabled: value.get_path("enabled").and_then(|v| if let V::Bool(b) = v { Some(*b) } else { None }).unwrap_or(true),
            triggered: value.get_path("triggered").and_then(|v| if let V::Bool(b) = v { Some(*b) } else { None }).unwrap_or(false),
            ..FxEntry::default()
        };
        if let Some(params) = value.get_path("params").and_then(V::as_map).or_else(|| value.as_map()) {
            for (key, v) in params {
                if ["id", "name", "enabled", "triggered", "when", "group", "hold"].contains(&key.as_str()) { continue; }
                e.params.insert(key.clone(), v.clone());
            }
        }
        out.push(e);
    }
    out
}


fn parse(text: &str) -> Result<DocumentMut> {
    text.parse::<DocumentMut>().context("the scene file isn't valid TOML")
}

/// The identity of a node entry: its `id`, else its `src`.
fn node_id(t: &InlineTable) -> Option<&str> {
    t.get("id").and_then(Value::as_str).or_else(|| t.get("src").and_then(Value::as_str))
}

fn table_id(t: &dyn TableLike) -> Option<&str> {
    t.get("id").and_then(Item::as_str).or_else(|| t.get("src").and_then(Item::as_str))
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

/// Run `f` on the entry of the layer `node` on every canvas (inline `nodes = [...]` or
/// `[[canvas.x.nodes]]` tables). Returns how many entries it ran on.
fn each_node(doc: &mut DocumentMut, node: &str, mut f: impl FnMut(&mut dyn TableLike) -> Result<()>) -> Result<usize> {
    let Some(canvases) = doc.get_mut("canvas").and_then(Item::as_table_like_mut) else { return Ok(0) };
    let mut n = 0;
    for (_, c) in canvases.iter_mut() {
        let Some(nodes) = c.as_table_like_mut().and_then(|c| c.get_mut("nodes")) else { continue };
        match nodes {
            Item::Value(Value::Array(arr)) => {
                for t in arr.iter_mut().filter_map(Value::as_inline_table_mut).filter(|t| node_id(t) == Some(node)) {
                    f(t)?;
                    n += 1;
                }
            }
            Item::ArrayOfTables(aot) => {
                for t in aot.iter_mut().filter(|t| table_id(&**t) == Some(node)) {
                    f(t)?;
                    n += 1;
                }
            }
            _ => {}
        }
    }
    Ok(n)
}

/// The first entry of the layer `node` (the main canvas first).
fn first_node<'a>(doc: &'a DocumentMut, node: &str) -> Option<&'a dyn TableLike> {
    let canvases = doc.get("canvas")?.as_table_like()?;
    let mut order: Vec<(&str, &Item)> = canvases.iter().collect();
    order.sort_by_key(|(k, _)| *k != "wide");
    order.into_iter().find_map(|(_, c)| match c.as_table_like()?.get("nodes")? {
        Item::Value(Value::Array(arr)) => arr.iter().filter_map(Value::as_inline_table).find(|t| node_id(t) == Some(node)).map(|t| t as &dyn TableLike),
        Item::ArrayOfTables(aot) => aot.iter().find(|t| table_id(*t) == Some(node)).map(|t| t as &dyn TableLike),
        _ => None,
    })
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

/// A layer name that is safe inside an address (`scene.<s>.node.<id>.…`): `patch.aurora` →
/// `aurora`, `color:#ff0000` → `color`; names of letters, digits, `_` and `-` stay as they are.
fn plain_id(src: &str) -> String {
    let base = src.strip_prefix("patch.").unwrap_or(src);
    let base = if base.starts_with("color:") || base.starts_with('#') { "color" } else { base };
    let s: String = base.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
    if s.is_empty() { "layer".into() } else { s }
}

/// `base`, else `base_2`, `base_3`, … whichever isn't in `used`.
fn unique(base: &str, used: &[String]) -> String {
    if !used.iter().any(|u| u == base) {
        return base.to_string();
    }
    (2..).map(|i| format!("{base}_{i}")).find(|c| !used.contains(c)).expect("a free name")
}

/// Every layer name in the scene (all canvases).
fn layer_names(doc: &mut DocumentMut) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for arr in node_arrays(doc) {
        for id in arr.iter().filter_map(|v| v.as_inline_table().and_then(node_id)) {
            if !out.iter().any(|o| o == id) {
                out.push(id.to_string());
            }
        }
    }
    out
}

fn is_color(src: &str) -> bool {
    src.starts_with("color:") || src.starts_with('#')
}

// ---- layers ---------------------------------------------------------------------------------

/// Add a layer showing `src` on the main and vertical canvases: centered in front, or a solid
/// color filling the canvas at the back. Returns the new text and the new layer's name.
pub fn add_layer(text: &str, src: &str) -> Result<(String, String)> {
    let mut doc = parse(text)?;
    let used = layer_names(&mut doc);
    let base = plain_id(src);
    // a layer without an `id` is named after its source; that only works for a plain,
    // unused name
    let id = if base == src && !used.iter().any(|u| u == src) { None } else { Some(unique(&base, &used)) };
    let color = is_color(src);
    for (canvas, r) in [("wide", [0.25, 0.25, 0.5, 0.5]), ("tall", [0.1, 0.3, 0.8, 0.4])] {
        let t = canvas_table(&mut doc, canvas);
        if !t.contains_key("nodes") {
            t.insert("nodes", value(Array::new()));
        }
        let arr = t["nodes"].as_array_mut().ok_or_else(|| anyhow!("canvas.{canvas}.nodes isn't a list"))?;
        let mut n = InlineTable::new();
        n.insert("src", src.into());
        if let Some(id) = &id {
            n.insert("id", id.as_str().into());
        }
        n.insert("rect", rect(if color { [0.0, 0.0, 1.0, 1.0] } else { r }));
        if color {
            arr.insert(0, Value::InlineTable(n));
        } else {
            arr.push(Value::InlineTable(n));
        }
        multiline(arr);
    }
    let name = id.unwrap_or_else(|| src.to_string());
    Ok((doc.to_string(), name))
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

/// Move the layer `node` right next to `target`: just in front of it (`in_front`) or just
/// behind it, on every canvas that has both.
pub fn place_layer(text: &str, node: &str, target: &str, in_front: bool) -> Result<String> {
    let mut doc = parse(text)?;
    if node == target {
        return Ok(doc.to_string());
    }
    let pos = |a: &Array, id: &str| a.iter().position(|v| v.as_inline_table().and_then(node_id) == Some(id));
    for arr in node_arrays(&mut doc) {
        let Some(i) = pos(arr, node) else { continue };
        if pos(arr, target).is_none() {
            continue;
        }
        let v = arr.remove(i);
        let j = pos(arr, target).expect("target is still there") + usize::from(in_front);
        arr.insert(j, v);
        multiline(arr);
    }
    Ok(doc.to_string())
}

/// A copy of the layer `node` just in front of it, on every canvas that has it. Returns the
/// new text and the copy's name.
pub fn duplicate_layer(text: &str, node: &str) -> Result<(String, String)> {
    let mut doc = parse(text)?;
    let used = layer_names(&mut doc);
    if !used.iter().any(|u| u == node) {
        bail!("this scene has no layer `{node}`");
    }
    let id = unique(&plain_id(node), &used);
    for arr in node_arrays(&mut doc) {
        let Some(i) = arr.iter().position(|v| v.as_inline_table().and_then(node_id) == Some(node)) else { continue };
        let mut copy = arr.get(i).expect("found").clone();
        if let Some(t) = copy.as_inline_table_mut() {
            t.insert("id", id.as_str().into());
        }
        copy.decor_mut().clear();
        arr.insert(i + 1, copy);
        multiline(arr);
    }
    Ok((doc.to_string(), id))
}

/// Show `src` in the layer `node` instead (every canvas; place, size and effects stay). A layer
/// without its own `id` is named after what it shows, so it follows the new source when that
/// is a plain, unused name; otherwise it gets an `id`. Returns the new text and the layer's
/// name afterwards.
pub fn set_layer_source(text: &str, node: &str, src: &str) -> Result<(String, String)> {
    let mut doc = parse(text)?;
    let used = layer_names(&mut doc);
    let mut explicit = None;
    each_node(&mut doc, node, |t| {
        explicit = Some(explicit.unwrap_or(false) || t.contains_key("id"));
        Ok(())
    })?;
    let Some(explicit) = explicit else { return Err(anyhow!("this scene has no layer `{node}`")) };
    let taken = src != node && used.iter().any(|u| u == src);
    let new_id = if explicit {
        node.to_string()
    } else if plain_id(src) == src && !taken {
        src.to_string()
    } else if plain_id(node) == node {
        node.to_string()
    } else {
        unique(&plain_id(src), &used)
    };
    each_node(&mut doc, node, |t| {
        t.insert("src", value(src));
        if !explicit && new_id != src {
            t.insert("id", value(new_id.as_str()));
        }
        Ok(())
    })?;
    Ok((doc.to_string(), new_id))
}

/// One layer as the Layers list shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct LayerInfo {
    pub id: String,
    pub src: String,
    /// Effects on it.
    pub fx: usize,
}

/// The scene's layers in drawing order on `canvas` (back first), then those only on other
/// canvases; each with what it shows and how many effects it has (from its first entry).
pub fn layers(text: &str, canvas: &str) -> Vec<LayerInfo> {
    let Ok(doc) = parse(text) else { return Vec::new() };
    let Some(canvases) = doc.get("canvas").and_then(Item::as_table_like) else { return Vec::new() };
    let mut order: Vec<(&str, &Item)> = canvases.iter().collect();
    order.sort_by_key(|(k, _)| *k != canvas);
    let mut out: Vec<LayerInfo> = Vec::new();
    let mut add = |t: &dyn TableLike| {
        let Some(id) = table_id(t) else { return };
        if out.iter().any(|l| l.id == id) {
            return;
        }
        let src = t.get("src").and_then(Item::as_str).unwrap_or(id).to_string();
        let fx = t.get("fx").and_then(Item::as_array).map_or(0, Array::len);
        out.push(LayerInfo { id: id.to_string(), src, fx });
    };
    for (_, c) in order {
        match c.as_table_like().and_then(|c| c.get("nodes")) {
            Some(Item::Value(Value::Array(arr))) => arr.iter().filter_map(Value::as_inline_table).for_each(|t| add(t)),
            Some(Item::ArrayOfTables(aot)) => aot.iter().for_each(|t| add(t)),
            _ => {}
        }
    }
    out
}

/// Layer names in drawing order on `canvas` (back first), then those only on other canvases.
pub fn layer_order(text: &str, canvas: &str) -> Vec<String> {
    layers(text, canvas).into_iter().map(|l| l.id).collect()
}

/// A text property of the layer `node` (`blend`, `mask`, `enter`, `exit`, `src`; the main
/// canvas's entry first).
pub fn layer_prop(text: &str, node: &str, key: &str) -> Option<String> {
    let doc = parse(text).ok()?;
    first_node(&doc, node)?.get(key)?.as_str().filter(|s| !s.is_empty()).map(String::from)
}

/// Set (or with `None` remove) a text property of the layer `node` on every canvas.
pub fn set_layer_prop(text: &str, node: &str, key: &str, v: Option<&str>) -> Result<String> {
    let mut doc = parse(text)?;
    let n = each_node(&mut doc, node, |t| {
        match v.filter(|s| !s.is_empty()) {
            Some(s) => match t.get_mut(key) {
                // keep the value's own comment
                Some(Item::Value(old)) => {
                    let decor = old.decor().clone();
                    *old = s.into();
                    *old.decor_mut() = decor;
                }
                _ => {
                    t.insert(key, value(s));
                }
            },
            None => {
                t.remove(key);
            }
        }
        Ok(())
    })?;
    if n == 0 {
        bail!("this scene has no layer `{node}`");
    }
    Ok(doc.to_string())
}

/// The show condition of the layer `node` (`when = "…"`; the main canvas's entry first).
pub fn layer_when(text: &str, node: &str) -> Option<String> {
    layer_prop(text, node, "when").filter(|w| !w.trim().is_empty())
}

/// Show the layer `node` only while `when` is true (`None` or empty = always), on every canvas.
pub fn set_layer_when(text: &str, node: &str, when: Option<&str>) -> Result<String> {
    set_layer_prop(text, node, "when", when.map(str::trim))
}

/// One plain check of a layer's show condition. Anything else is edited as text.
#[derive(Clone, Debug, PartialEq)]
pub enum LayerCheck {
    /// `mode == 'x'` (`not`: `mode != 'x'`); an empty mode is an unfinished row.
    Mode { mode: String, not: bool },
    /// `queue.now.id` (a song request is playing) or `!queue.now.id`.
    Song { playing: bool },
    /// `patch.<id>.active` (a source that plays on cue is playing) or `!patch.<id>.active`.
    Source { id: String, playing: bool },
}

impl LayerCheck {
    /// Expression text; `None` while the row is unfinished.
    pub fn text(&self) -> Option<String> {
        let bang = |yes: bool| if yes { "" } else { "!" };
        match self {
            LayerCheck::Mode { mode, .. } if mode.is_empty() || mode.contains(['\'', '"']) => None,
            LayerCheck::Mode { mode, not } => Some(format!("mode {} '{mode}'", if *not { "!=" } else { "==" })),
            LayerCheck::Song { playing } => Some(format!("{}queue.now.id", bang(*playing))),
            LayerCheck::Source { id, .. } if id.is_empty() => None,
            LayerCheck::Source { id, playing } => Some(format!("{}patch.{id}.active", bang(*playing))),
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
    simple_name(id).then(|| LayerCheck::Source { id: id.to_string(), playing: yes })
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

/// The scene's number key (1–9).
pub fn key(text: &str) -> Option<i64> {
    parse(text).ok()?.get("key")?.as_integer()
}

/// Set (or with `None` remove) the scene's number key.
pub fn set_key(text: &str, key: Option<i64>) -> Result<String> {
    let mut doc = parse(text)?;
    match key {
        Some(k) => match doc.get_mut("key") {
            // keep the line's own comment
            Some(Item::Value(v)) => {
                let decor = v.decor().clone();
                *v = k.into();
                *v.decor_mut() = decor;
            }
            _ => {
                doc.insert("key", value(k));
            }
        },
        None => {
            doc.remove("key");
        }
    }
    Ok(doc.to_string())
}

/// What the scene does with the lights when it comes on: (`look`, `cue`); `cue` is a cue list
/// to run (a `cuelist` + `cue` pair names one cue of a list and reads as that list).
pub fn lights(text: &str) -> (Option<String>, Option<String>) {
    let Ok(doc) = parse(text) else { return (None, None) };
    let Some(l) = doc.get("lights").and_then(Item::as_table_like) else { return (None, None) };
    let s = |k: &str| l.get(k).and_then(Item::as_str).filter(|s| !s.is_empty()).map(String::from);
    (s("look"), s("cuelist").or_else(|| s("cue")))
}

/// Set (or with `None` remove) the scene's light look (`key` = `look`) or cue list (`cue`).
/// Other keys (`hold`) stay; an empty `[lights]` goes away.
pub fn set_lights(text: &str, key: &str, v: Option<&str>) -> Result<String> {
    if !["look", "cue"].contains(&key) {
        bail!("scenes set a light `look` or `cue`, not `{key}`");
    }
    let mut doc = parse(text)?;
    if v.is_some() && !doc.contains_key("lights") {
        doc.insert("lights", Item::Table(Table::new()));
    }
    let Some(l) = doc.get_mut("lights").and_then(Item::as_table_like_mut) else { return Ok(doc.to_string()) };
    if key == "cue" {
        // a whole cue list now, not one cue of it
        l.remove("cuelist");
    }
    match v.filter(|s| !s.is_empty()) {
        Some(s) => {
            l.insert(key, value(s));
        }
        None => {
            l.remove(key);
        }
    }
    let empty = l.is_empty();
    if empty {
        doc.remove("lights");
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
        let (out, id) = add_layer(DUO, "youtube").unwrap();
        assert_eq!(id, "youtube");
        assert!(out.starts_with("# Kit front + high corner wide"), "{out}");
        assert!(out.contains("# top"), "{out}");
        assert_eq!(nodes(&out, "wide"), ["cam_kit", "cam_wide", "youtube"]);
        assert_eq!(nodes(&out, "tall"), ["cam_kit", "youtube"]);
        let (twice, id) = add_layer(&out, "youtube").unwrap();
        assert_eq!(id, "youtube_2");
        assert_eq!(nodes(&twice, "wide"), ["cam_kit", "cam_wide", "youtube", "youtube_2"]);
        assert_eq!(nodes(&twice, "tall"), ["cam_kit", "youtube", "youtube_2"], "the same name on both canvases");
        // a scene without canvases gets them
        let (fresh, _) = add_layer("label = \"x\"\n", "cam_kit").unwrap();
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
    fn layer_names_stay_usable_in_addresses() {
        // sources whose names aren't plain get a plain layer name, so
        // `scene.<s>.node.<id>.<prop>` stays one segment per part
        let (out, id) = add_layer(DUO, "patch.aurora").unwrap();
        assert_eq!(id, "aurora");
        assert_eq!(nodes(&out, "wide"), ["cam_kit", "cam_wide", "aurora"]);
        let (out, id) = add_layer(&out, "patch.aurora").unwrap();
        assert_eq!(id, "aurora_2");
        // a solid color fills the canvas at the back
        let (out, id) = add_layer(&out, "color:#102030").unwrap();
        assert_eq!(id, "color");
        assert_eq!(nodes(&out, "wide")[0], "color");
        assert_eq!(node_entries(&out, "tall")[0].2, [0.0, 0.0, 1.0, 1.0]);
        let def = engine_reads(&out);
        assert_eq!(def.nodes("wide")[0].src, "color:#102030");
        assert_eq!(def.nodes("wide").len(), 5);
    }

    #[test]
    fn duplicate_and_drop_layers_in_place() {
        let (out, id) = duplicate_layer(DUO, "cam_kit").unwrap();
        assert_eq!(id, "cam_kit_2");
        assert_eq!(nodes(&out, "wide"), ["cam_kit", "cam_kit_2", "cam_wide"]);
        assert_eq!(nodes(&out, "tall"), ["cam_kit", "cam_kit_2"]);
        let def = engine_reads(&out);
        assert_eq!(def.nodes("wide")[1].src, "cam_kit");
        assert_eq!(def.nodes("wide")[1].radius, 24.0, "the copy keeps the look");
        assert!(duplicate_layer(DUO, "nope").is_err());
        // drop cam_kit in front of cam_wide, then behind it again
        let front = place_layer(DUO, "cam_kit", "cam_wide", true).unwrap();
        assert_eq!(nodes(&front, "wide"), ["cam_wide", "cam_kit"]);
        assert_eq!(nodes(&front, "tall"), ["cam_kit"], "a canvas without the target is left alone");
        let back = place_layer(&front, "cam_kit", "cam_wide", false).unwrap();
        assert_eq!(nodes(&back, "wide"), ["cam_kit", "cam_wide"]);
        assert_eq!(layer_order(&front, "wide"), ["cam_wide", "cam_kit"]);
        assert_eq!(layer_order(&front, "tall"), ["cam_kit", "cam_wide"], "layers only on the other canvas come last");
    }

    #[test]
    fn layer_effects_keep_independent_identity_and_settings() {
        use se_proto::Value as V;
        let values = vec![
            V::map().with("id", "soft").with("name", "blur").with("enabled", false).with("triggered", true).with("params", V::map().with("radius", 8.0)),
            V::map().with("id", "strong").with("name", "blur").with("enabled", true).with("triggered", false).with("params", V::map().with("radius", 40.0)),
        ];
        let entries = fx_values(&values);
        assert_eq!((entries[0].id.as_str(), entries[1].id.as_str()), ("soft", "strong"));
        assert_eq!((entries[0].num("radius"), entries[1].num("radius")), (Some(8.0), Some(40.0)));
        assert!(!entries[0].enabled && entries[0].triggered);
        assert!(entries[1].enabled && !entries[1].triggered);
        let reversed = fx_values(&[values[1].clone(), values[0].clone()]);
        assert_eq!((reversed[0].id.as_str(), reversed[1].id.as_str()), ("strong", "soft"));
    }

    #[test]
    fn scene_effects_preserve_legacy_addresses_and_typed_settings() {
        use se_proto::Value as V;
        let values = vec![
            V::from("patch.win31_video"),
            V::map().with("name", "patch.win31_video").with("id", "patch.win31_video_2").with("file", "assets/effect.png")
                .with("invert", true).with("color", V::List(vec![0.2.into(), 0.4.into(), 0.8.into(), 1.0.into()])),
            V::map().with("name", "patch.win31_video"),
            V::map().with("name", "lut").with("file", "assets/luts/warm.cube").with("enabled", false),
        ];
        let entries = fx_values(&values);
        assert_eq!(entries.iter().map(|e| e.id.as_str()).collect::<Vec<_>>(), ["patch.win31_video", "patch.win31_video_2", "patch.win31_video_3", "lut"]);
        assert_eq!(entries[1].text("file"), Some("assets/effect.png"));
        assert_eq!(entries[1].params.get("invert"), Some(&V::Bool(true)));
        assert_eq!(crate::views::patches::rgba(entries[1].params.get("color").unwrap()).unwrap().to_srgba_unmultiplied(), [51, 102, 204, 255]);
        assert_eq!(entries[3].text("file"), Some("assets/luts/warm.cube"));
        assert!(!entries[3].enabled && !entries[3].triggered);
    }


    #[test]
    fn blend_mask_enter_and_exit_are_written_on_every_canvas() {
        let out = set_layer_prop(DUO, "cam_kit", "blend", Some("screen")).unwrap();
        let out = set_layer_prop(&out, "cam_kit", "enter", Some("slide_left")).unwrap();
        let out = set_layer_prop(&out, "cam_kit", "exit", Some("scale")).unwrap();
        let out = set_layer_prop(&out, "cam_kit", "mask", Some("assets/images/round.png")).unwrap();
        assert_eq!(layer_prop(&out, "cam_kit", "blend").as_deref(), Some("screen"));
        let def = engine_reads(&out);
        for c in ["wide", "tall"] {
            let n = &def.nodes(c)[0];
            assert_eq!((n.blend.as_str(), n.enter.as_deref(), n.exit.as_deref()), ("screen", Some("slide_left"), Some("scale")), "{c}");
            assert_eq!(n.mask.as_deref(), Some("assets/images/round.png"), "{c}");
        }
        assert!(out.contains("# top"), "{out}");
        let plain = set_layer_prop(&out, "cam_kit", "blend", None).unwrap();
        assert_eq!(layer_prop(&plain, "cam_kit", "blend"), None);
        assert_eq!(engine_reads(&plain).nodes("tall")[0].blend, "normal");
        assert!(set_layer_prop(DUO, "nope", "blend", Some("add")).is_err());
        // `[[canvas.x.nodes]]` tables are edited too
        let aot = "[[canvas.wide.nodes]]\nsrc = \"cam_kit\" # me\nrect = [0.0, 0.0, 1.0, 1.0]\n";
        let out = set_layer_prop(aot, "cam_kit", "exit", Some("fade")).unwrap();
        assert_eq!(engine_reads(&out).nodes("wide")[0].exit.as_deref(), Some("fade"));
        assert!(out.contains("# me"), "{out}");
    }

    #[test]
    fn number_key_and_lights() {
        assert_eq!(key(DUO), Some(1));
        assert_eq!(key(&set_key(DUO, Some(4)).unwrap()), Some(4));
        assert_eq!(key(&set_key(DUO, None).unwrap()), None);
        let l = set_lights(DUO, "look", Some("warm")).unwrap();
        let l = set_lights(&l, "cue", Some("intro")).unwrap();
        assert_eq!(lights(&l), (Some("warm".into()), Some("intro".into())));
        let def = engine_reads(&l);
        let lr = def.lights.unwrap();
        assert_eq!((lr.look.as_deref(), lr.cue.as_deref()), (Some("warm"), Some("intro")));
        let none = set_lights(&set_lights(&l, "look", None).unwrap(), "cue", None).unwrap();
        assert_eq!(lights(&none), (None, None));
        assert!(!none.contains("[lights]"), "{none}");
        assert!(set_lights(DUO, "hold", Some("1s")).is_err());
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
            ("!patch.terminal_boot.active", vec![Source { id: "terminal_boot".into(), playing: false }]),
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
