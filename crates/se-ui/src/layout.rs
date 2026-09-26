//! Named layouts (`layouts/*.toml`, §15.3).
//!
//! A [`Layout`] says which page the main window starts on and where the main window, pop-out
//! panels, and the (optional) confidence window go. The default, `single`, is one window and
//! nothing else; `show-3disp` / `show-2disp` add the multiview and a program window on other
//! screens for people who want them. Files are human-edited:
//! every key is optional ([`Layout::parse_over`] fills gaps from the built-in of the same name),
//! and [`Layout::to_toml`] rewrites an existing file with `toml_edit`, keeping comments and the
//! order of untouched keys. Parsing and serializing are pure (`&str` in, `String` out); the UI
//! moves the text through the engine (`project.read` / `project.write`).

use crate::monitors::{self, Monitor, MonitorMatch, Placement};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use toml_edit::{DocumentMut, Item, TableLike, Value};

/// Built-in layouts, in the order the switcher lists them.
pub const BUILTIN_NAMES: [&str; 3] = ["single", "show-3disp", "show-2disp"];
/// Layout used when none is chosen.
pub const DEFAULT_LAYOUT: &str = "single";
/// Stem of the shortcuts file that shares the `layouts/` directory (not a layout).
pub const SHORTCUTS_STEM: &str = "shortcuts";
/// App-id of the main window.
pub const MAIN_APP_ID: &str = "stream-engine";
/// App-id of the confidence (program) window.
pub const CONFIDENCE_APP_ID: &str = "stream-engine.program";
/// Page ids a layout can open on (see `app::Page`).
pub const PAGES: [&str; 8] = ["live", "scenes", "lights", "sound", "automation", "community", "recordings", "settings"];
/// Live-page right-rail tabs.
pub const RAIL_TABS: [&str; 4] = ["events", "chat", "queue", "mod"];

/// Which program canvases the confidence window shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Canvases {
    Wide,
    Tall,
    /// Wide and tall side by side.
    #[default]
    Both,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Layout {
    /// Equals the file stem (the stem wins when they differ).
    pub name: String,
    /// Page the main window opens on (`live`, `scenes`, `lights`, …).
    pub page: String,
    /// In-app zoom on top of the Hyprland monitor scale.
    pub zoom: f32,
    /// Layout used instead while none of `confidence.monitors` is present (TV off/unplugged).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_fallback: Option<String>,
    pub main: MainCfg,
    /// Pop-out panels: separate windows with app-id `stream-engine.<panel>`.
    pub windows: Vec<PopOut>,
    pub confidence: ConfidenceCfg,
    pub show: ShowCfg,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MainCfg {
    /// Monitor for the main window; `None` leaves it where Hyprland put it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor: Option<MonitorMatch>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct PopOut {
    /// Panel id (`multiview`, `scopes`, `view.<name>`, …).
    pub panel: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub monitor: Option<MonitorMatch>,
    pub fullscreen: bool,
    /// Initial inner size in logical points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<[f32; 2]>,
}

impl PopOut {
    pub fn app_id(&self) -> String {
        format!("{MAIN_APP_ID}.{}", self.panel)
    }
}

/// The confidence monitor: a borderless `stream-engine.program` window showing program.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ConfidenceCfg {
    pub enabled: bool,
    /// Preferred outputs; the first present, enabled match wins.
    pub monitors: Vec<MonitorMatch>,
    /// Used while none of `monitors` is present.
    pub fallback: Vec<MonitorMatch>,
    pub canvases: Canvases,
    pub fullscreen: bool,
    /// Window size in logical points when not fullscreen.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub size: Option<[f32; 2]>,
}

impl Default for ConfidenceCfg {
    fn default() -> Self {
        ConfidenceCfg {
            enabled: true,
            monitors: vec![MonitorMatch::parse("desc:Philips FTV"), MonitorMatch::parse("HDMI-A-1")],
            fallback: vec![MonitorMatch::parse("DP-2")],
            canvases: Canvases::Both,
            fullscreen: true,
            size: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ShowCfg {
    /// Initially selected right-rail tab ([`RAIL_TABS`]).
    pub rail_tab: String,
    pub rail_width: f32,
    /// Multiview strip inside the main window (off when multiview is popped out).
    pub multiview: bool,
}

impl Default for ShowCfg {
    fn default() -> Self {
        ShowCfg { rail_tab: "chat".into(), rail_width: 380.0, multiview: true }
    }
}

impl Default for Layout {
    fn default() -> Self {
        Layout {
            name: String::new(),
            page: "live".into(),
            zoom: 1.0,
            auto_fallback: None,
            main: MainCfg::default(),
            windows: Vec::new(),
            confidence: ConfidenceCfg::default(),
            show: ShowCfg::default(),
        }
    }
}

/// Where one window of a layout goes (see [`Layout::plan`]).
#[derive(Clone, Debug, PartialEq)]
pub struct WindowPlan {
    pub app_id: String,
    /// Connector name, or `None` when the layout doesn't say or the monitor is absent.
    pub monitor: Option<String>,
    pub fullscreen: bool,
}

impl Layout {
    /// The built-in layouts (§15.3): `single` (one window, the default) and the multi-screen
    /// setups for this machine (DP-1 main, DP-2 side, Philips TV confidence).
    pub fn builtin(name: &str) -> Option<Layout> {
        let m = |s: &str| Some(MonitorMatch::parse(s));
        let multiview = |size| PopOut { panel: "multiview".into(), monitor: m("DP-2"), fullscreen: false, size };
        let base = Layout { name: name.to_string(), main: MainCfg { monitor: m("DP-1") }, ..Layout::default() };
        Some(match name {
            "single" => Layout { name: name.to_string(), confidence: ConfidenceCfg { enabled: false, ..ConfidenceCfg::default() }, ..Layout::default() },
            "show-3disp" => Layout {
                windows: vec![multiview(None)],
                auto_fallback: Some("show-2disp".into()),
                show: ShowCfg { multiview: false, ..ShowCfg::default() },
                ..base
            },
            "show-2disp" => Layout {
                // DP-2 is shared: multiview and a windowed confidence side by side.
                windows: vec![multiview(Some([1360.0, 1080.0]))],
                confidence: ConfidenceCfg {
                    monitors: vec![MonitorMatch::parse("DP-2")],
                    fallback: Vec::new(),
                    fullscreen: false,
                    size: Some([1360.0, 1080.0]),
                    ..ConfidenceCfg::default()
                },
                show: ShowCfg { multiview: false, ..ShowCfg::default() },
                ..base
            },
            _ => return None,
        })
    }

    /// Parse a layout file; missing keys take [`Layout::default`] values.
    pub fn parse(text: &str) -> Result<Layout> {
        Self::parse_over(&Layout::default(), text)
    }

    /// Parse a layout file over `base`: tables merge key by key, arrays (including `[[windows]]`)
    /// replace. Used so `layouts/show-3disp.toml` with just `zoom = 1.2` keeps the rest of the
    /// built-in `show-3disp`.
    pub fn parse_over(base: &Layout, text: &str) -> Result<Layout> {
        let over: toml::Table = toml::from_str(text).context("invalid TOML")?;
        let mut merged = toml::Table::try_from(base).context("serialize base layout")?;
        deep_merge(&mut merged, over);
        match merged.try_into::<Layout>() {
            Ok(l) => Ok(l),
            // Re-parse the text alone for an error with line/column when it's the text's fault.
            Err(e) => Err(match toml::from_str::<Layout>(text) {
                Err(located) => anyhow::Error::new(located),
                Ok(_) => anyhow::Error::new(e),
            }
            .context("invalid layout")),
        }
    }

    /// Serialize. With `existing` (the file's current text) only changed values are rewritten:
    /// comments, formatting, key order, and unknown keys survive. Without it (or if it no longer
    /// parses as TOML) a fresh file with explanatory comments is produced.
    pub fn to_toml(&self, existing: Option<&str>) -> String {
        let new = self.document();
        match existing.map(str::parse::<DocumentMut>) {
            Some(Ok(mut doc)) => {
                let probe = Self::probe().document();
                merge_table(doc.as_table_mut(), new.as_table(), Some(probe.as_table()));
                doc.to_string()
            }
            _ => fresh_file(new),
        }
    }

    fn document(&self) -> DocumentMut {
        let text = toml::to_string(self).expect("layouts always serialize to TOML");
        let mut doc: DocumentMut = text.parse().expect("toml output parses");
        for (_, item) in doc.as_table_mut().iter_mut() {
            shorten_floats(item);
        }
        doc
    }

    /// A layout with every optional key present: its keys are the schema for [`Self::to_toml`].
    fn probe() -> Layout {
        let m = Some(MonitorMatch::parse("x"));
        Layout {
            auto_fallback: Some(String::new()),
            main: MainCfg { monitor: m.clone() },
            windows: vec![PopOut { monitor: m, size: Some([0.0; 2]), ..PopOut::default() }],
            confidence: ConfidenceCfg { size: Some([0.0; 2]), ..ConfidenceCfg::default() },
            ..Layout::default()
        }
    }

    /// Problems worth a toast; the layout is still usable.
    pub fn warnings(&self) -> Vec<String> {
        let mut w = Vec::new();
        if !(0.5..=3.0).contains(&self.zoom) {
            w.push(format!("zoom {} outside 0.5–3.0", self.zoom));
        }
        if self.auto_fallback.as_deref() == Some(self.name.as_str()) {
            w.push("auto_fallback points at the layout itself".into());
        }
        if !PAGES.contains(&self.page.as_str()) {
            w.push(format!("unknown page `{}`", self.page));
        }
        if !RAIL_TABS.contains(&self.show.rail_tab.as_str()) {
            w.push(format!("show: unknown rail_tab `{}`", self.show.rail_tab));
        }
        for p in &self.windows {
            if p.panel.is_empty() || !p.panel.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) {
                w.push(format!("windows: invalid panel id `{}`", p.panel));
            }
        }
        w
    }

    /// Where every window of this layout goes on `monitors`: the main window, each pop-out, and
    /// (last, when enabled) the confidence window following hotplug from `confidence_prev`. An
    /// empty monitor list means "unknown": nothing is placed and the confidence keeps its spot.
    pub fn plan(&self, monitors: &[Monitor], confidence_prev: Option<&str>) -> (Vec<WindowPlan>, Placement) {
        let resolve = |m: &Option<MonitorMatch>| {
            m.as_ref().and_then(|m| monitors::pick(monitors, std::slice::from_ref(m), &[], None)).filter(|p| !p.is_fallback()).map(|p| p.monitor.name.clone())
        };
        let mut plans = vec![WindowPlan { app_id: MAIN_APP_ID.into(), monitor: resolve(&self.main.monitor), fullscreen: false }];
        plans.extend(self.windows.iter().map(|p| WindowPlan { app_id: p.app_id(), monitor: resolve(&p.monitor), fullscreen: p.fullscreen }));
        let placement = monitors::follow(confidence_prev, monitors, &self.confidence, self.main.monitor.as_ref());
        if self.confidence.enabled {
            plans.push(WindowPlan { app_id: CONFIDENCE_APP_ID.into(), monitor: placement.monitor.clone(), fullscreen: self.confidence.fullscreen });
        }
        (plans, placement)
    }
}

/// The layout to actually use: `chosen`, or its `auto_fallback` while none of
/// `chosen.confidence.monitors` is present and enabled. An empty monitor list (unknown, e.g.
/// before the first fetch) never triggers the fallback.
pub fn effective<'a>(chosen: &'a Layout, all: &'a LayoutSet, monitors: &[Monitor]) -> &'a Layout {
    let Some(fallback) = chosen.auto_fallback.as_deref().and_then(|n| all.get(n)) else { return chosen };
    if monitors.is_empty() {
        return chosen;
    }
    let tv_present = monitors::pick(monitors, &chosen.confidence.monitors, &[], None).is_some_and(|p| !p.is_fallback());
    if tv_present { chosen } else { fallback }
}

/// Named layouts: files (by stem) merged over the built-ins.
#[derive(Clone, Debug, PartialEq)]
pub struct LayoutSet {
    layouts: BTreeMap<String, Layout>,
}

impl Default for LayoutSet {
    fn default() -> Self {
        Self::builtin()
    }
}

impl LayoutSet {
    pub fn builtin() -> LayoutSet {
        LayoutSet { layouts: BUILTIN_NAMES.iter().map(|n| (n.to_string(), Layout::builtin(n).expect("builtin"))).collect() }
    }

    /// Built-ins plus `(path, text)` files from `layouts/`. A file named like a built-in overrides
    /// it (parsed over it); `shortcuts.toml` is skipped. Broken files are reported and skipped.
    pub fn from_files<'a>(files: impl IntoIterator<Item = (&'a str, &'a str)>) -> (LayoutSet, Vec<String>) {
        let mut set = Self::builtin();
        let mut errors = Vec::new();
        for (path, text) in files {
            let p = std::path::Path::new(path);
            if p.extension().and_then(|e| e.to_str()) != Some("toml") {
                continue;
            }
            let Some(stem) = p.file_stem().and_then(|s| s.to_str()).filter(|s| !s.is_empty() && *s != SHORTCUTS_STEM) else { continue };
            let base = Layout::builtin(stem).unwrap_or_default();
            match Layout::parse_over(&base, text) {
                Ok(mut l) => {
                    l.name = stem.to_string();
                    set.layouts.insert(l.name.clone(), l);
                }
                Err(e) => errors.push(format!("{path}: {e:#}")),
            }
        }
        (set, errors)
    }

    pub fn insert(&mut self, layout: Layout) {
        self.layouts.insert(layout.name.clone(), layout);
    }

    pub fn get(&self, name: &str) -> Option<&Layout> {
        self.layouts.get(name)
    }

    /// Built-ins first (in [`BUILTIN_NAMES`] order), then the rest sorted.
    pub fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = BUILTIN_NAMES.iter().copied().filter(|n| self.layouts.contains_key(*n)).collect();
        names.extend(self.layouts.keys().map(String::as_str).filter(|n| !BUILTIN_NAMES.contains(n)));
        names
    }

    /// The layout after `current` in [`Self::names`] order, wrapping (Ctrl+L). Unknown → first.
    pub fn next(&self, current: &str) -> &str {
        let names = self.names();
        let i = names.iter().position(|n| *n == current).map_or(0, |i| (i + 1) % names.len());
        names[i]
    }
}

fn deep_merge(base: &mut toml::Table, over: toml::Table) {
    for (k, v) in over {
        match (base.get_mut(&k), v) {
            (Some(toml::Value::Table(b)), toml::Value::Table(o)) => deep_merge(b, o),
            (_, v) => {
                base.insert(k, v);
            }
        }
    }
}

/// `toml` writes an `f32` through `f64` (`1.1` → `1.100000023841858`); write the short form.
fn shorten_floats(item: &mut Item) {
    match item {
        Item::Value(v) => shorten_value(v),
        Item::Table(t) => t.iter_mut().for_each(|(_, i)| shorten_floats(i)),
        Item::ArrayOfTables(a) => a.iter_mut().for_each(|t| t.iter_mut().for_each(|(_, i)| shorten_floats(i))),
        Item::None => {}
    }
}

fn shorten_value(v: &mut Value) {
    match v {
        Value::Float(f) => {
            let short: f64 = format!("{:?}", *f.value() as f32).parse().unwrap_or(*f.value());
            let decor = f.decor().clone();
            *v = Value::from(short);
            *v.decor_mut() = decor;
        }
        Value::Array(a) => a.iter_mut().for_each(shorten_value),
        Value::InlineTable(t) => t.iter_mut().for_each(|(_, v)| shorten_value(v)),
        _ => {}
    }
}

/// Semantic value of an item (formatting and comments ignored).
fn semantic(item: &Item) -> Option<toml::Value> {
    let mut doc = DocumentMut::new();
    doc.insert("v", item.clone());
    let mut t: toml::Table = toml::from_str(&doc.to_string()).ok()?;
    t.remove("v")
}

fn merge_table(old: &mut dyn TableLike, new: &dyn TableLike, known: Option<&dyn TableLike>) {
    for (key, new_item) in new.iter() {
        match old.get_mut(key) {
            Some(old_item) => merge_item(old_item, new_item, known.and_then(|k| k.get(key))),
            None => {
                let mut item = new_item.clone();
                if let Item::Table(t) = &mut item {
                    t.set_position(None);
                }
                old.insert(key, item);
            }
        }
    }
    // Drop keys the schema knows but the new value omits (an `Option` set back to `None`);
    // keys the schema doesn't know are the user's and stay.
    let stale: Vec<String> = old.iter().map(|(k, _)| k.to_string()).filter(|k| !new.contains_key(k) && known.is_some_and(|t| t.contains_key(k))).collect();
    for k in stale {
        old.remove(&k);
    }
}

/// Semantic equality where `1` and `1.0` are the same (layouts only have float numbers).
fn same_value(a: &toml::Value, b: &toml::Value) -> bool {
    use toml::Value as V;
    match (a, b) {
        (V::Integer(i), V::Float(f)) | (V::Float(f), V::Integer(i)) => *i as f64 == *f,
        (V::Array(x), V::Array(y)) => x.len() == y.len() && x.iter().zip(y).all(|(x, y)| same_value(x, y)),
        (V::Table(x), V::Table(y)) => x.len() == y.len() && x.iter().all(|(k, v)| y.get(k).is_some_and(|w| same_value(v, w))),
        _ => a == b,
    }
}

fn merge_item(old: &mut Item, new: &Item, known: Option<&Item>) {
    if let (Some(a), Some(b)) = (semantic(old), semantic(new))
        && same_value(&a, &b)
    {
        return;
    }
    let known_table = known.and_then(Item::as_table_like);
    match (old, new) {
        (Item::Table(o), Item::Table(n)) => merge_table(o, n, known_table),
        (Item::Value(Value::InlineTable(o)), Item::Table(n)) => merge_table(o, n, known_table),
        (Item::ArrayOfTables(o), Item::ArrayOfTables(n)) => {
            let known = known.and_then(Item::as_array_of_tables).and_then(|a| a.get(0)).map(|t| t as &dyn TableLike);
            for (i, nt) in n.iter().enumerate() {
                match o.get_mut(i) {
                    Some(ot) => merge_table(ot, nt, known),
                    None => o.push(nt.clone()),
                }
            }
            while o.len() > n.len() {
                o.remove(o.len() - 1);
            }
        }
        (Item::Value(ov), Item::Value(nv)) => {
            let decor = ov.decor().clone();
            *ov = nv.clone();
            *ov.decor_mut() = decor;
        }
        (old, new) => *old = new.clone(),
    }
}

const FILE_HEADER: &str = "\
# stream-engine layout (PLAN §15.3). Switch layouts with Ctrl+L or the command palette.
# The UI rewrites changed values only: comments and key order survive saves.
#
# Monitor matches: a connector name (\"DP-1\"), or \"desc:<text>\" / \"make:<text>\" /
# \"model:<text>\": case-insensitive substrings of what `hyprctl monitors all` reports.

";

/// Comments for a freshly written file: (table path, key or "" for the table header, comment).
const FILE_COMMENTS: &[(&str, &str, &str)] = &[
    ("", "name", "# must match the file name"),
    ("", "page", "# page to open on: live | scenes | lights | sound | automation | community | recordings | settings"),
    ("", "zoom", "# in-app zoom on top of the Hyprland monitor scale"),
    ("", "auto_fallback", "# layout used while no confidence monitor is present (TV off or unplugged)"),
    ("main", "", "# main window (app-id stream-engine)"),
    ("windows", "", "# pop-out panel: its own window with app-id stream-engine.<panel>"),
    ("confidence", "", "# confidence monitor: borderless stream-engine.program window showing program"),
    ("confidence", "monitors", "# first present and enabled match wins"),
    ("confidence", "fallback", "# used while none of `monitors` is present"),
    ("confidence", "canvases", "# wide | tall | both"),
    ("show", "", "# Live page"),
    ("show", "rail_tab", "# events | chat | queue | mod"),
];

fn fresh_file(mut doc: DocumentMut) -> String {
    for (table, key, comment) in FILE_COMMENTS {
        let comment = format!("{comment}\n");
        let root = doc.as_table_mut();
        match (*table, *key) {
            ("", key) => {
                if let Some(mut k) = root.key_mut(key) {
                    k.leaf_decor_mut().set_prefix(comment);
                }
            }
            (table, "") => match root.get_mut(table) {
                Some(Item::Table(t)) => t.decor_mut().set_prefix(format!("\n{comment}")),
                Some(Item::ArrayOfTables(a)) => a.iter_mut().for_each(|t| t.decor_mut().set_prefix(format!("\n{comment}"))),
                _ => {}
            },
            (table, key) => {
                if let Some(Item::Table(t)) = root.get_mut(table)
                    && let Some(mut k) = t.key_mut(key)
                {
                    k.leaf_decor_mut().set_prefix(comment);
                }
            }
        }
    }
    format!("{FILE_HEADER}{doc}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<Monitor> {
        let path = format!("{}/tests/fixtures/hyprctl/{name}.json", env!("CARGO_MANIFEST_DIR"));
        monitors::parse_monitors(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    fn example(name: &str) -> String {
        let path = format!("{}/../../project-example/layouts/{name}.toml", env!("CARGO_MANIFEST_DIR"));
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"))
    }

    #[test]
    fn builtins_match_the_plan() {
        let one = Layout::builtin("single").unwrap();
        assert!(!one.confidence.enabled && one.windows.is_empty() && one.main.monitor.is_none());
        assert_eq!(one.page, "live");
        let s3 = Layout::builtin("show-3disp").unwrap();
        assert_eq!(s3.auto_fallback.as_deref(), Some("show-2disp"));
        assert_eq!(s3.main.monitor, Some(MonitorMatch::Name("DP-1".into())));
        assert_eq!(s3.windows.len(), 1);
        assert_eq!((s3.windows[0].app_id().as_str(), s3.windows[0].monitor.as_ref().map(|m| m.to_string())), ("stream-engine.multiview", Some("DP-2".into())));
        assert!(s3.confidence.enabled && s3.confidence.fullscreen);
        assert!(!s3.show.multiview);

        let s2 = Layout::builtin("show-2disp").unwrap();
        assert_eq!(s2.auto_fallback, None);
        assert!(!s2.confidence.fullscreen && s2.confidence.size.is_some());
        assert_eq!(s2.confidence.monitors, vec![MonitorMatch::Name("DP-2".into())]);

        for n in BUILTIN_NAMES {
            assert!(Layout::builtin(n).unwrap().warnings().is_empty(), "{n}");
        }
        assert_eq!(Layout::builtin("nope"), None);
    }

    #[test]
    fn serialize_parse_round_trip_for_builtins() {
        for n in BUILTIN_NAMES {
            let l = Layout::builtin(n).unwrap();
            let fresh = l.to_toml(None);
            assert_eq!(Layout::parse(&fresh).unwrap(), l, "{n}:\n{fresh}");
            // No f64 noise from f32 fields.
            assert!(!fresh.contains("0000000"), "{fresh}");
            assert!(fresh.starts_with("# stream-engine layout"));
        }
    }

    #[test]
    fn to_toml_keeps_comments_order_and_unknown_keys() {
        let text = r#"# my show layout
name = "mine"   # the name
zoom = 1.25
page = "live"   # trailing comment stays
my_note = "not a layout key"

[main]
# main goes left
monitor = "desc:MSI"

[[windows]]
# multiview on the side
panel = "multiview"
monitor = "DP-2"
fullscreen = false
size = [1000, 800]   # ints are fine

[confidence]
enabled = true
monitors = ["desc:Philips FTV"]   # the TV
"#;
        let mut l = Layout::parse(text).unwrap();
        assert_eq!(l.windows[0].size, Some([1000.0, 800.0]));
        // Unchanged values stay byte-identical (`1000` isn't rewritten as `1000.0`); defaults the
        // file omits get appended to their tables.
        let same = l.to_toml(Some(text));
        assert!(same.starts_with(text.split("[confidence]").next().unwrap()), "{same}");
        l.zoom = 1.5;
        l.windows[0].size = None;
        l.confidence.monitors.push(MonitorMatch::parse("HDMI-A-1"));
        let out = l.to_toml(Some(text));
        assert!(
            out.starts_with(
                "# my show layout\nname = \"mine\"   # the name\nzoom = 1.5\npage = \"live\"   # trailing comment stays\nmy_note = \"not a layout key\"\n"
            ),
            "{out}"
        );
        assert!(out.contains("[main]\n# main goes left\nmonitor = \"desc:MSI\"\n"), "{out}");
        assert!(out.contains("# multiview on the side\npanel = \"multiview\""), "{out}");
        assert!(!out.contains("size"), "{out}");
        assert!(out.contains("monitors = [\"desc:Philips FTV\", \"HDMI-A-1\"]   # the TV"), "{out}");
        assert_eq!(Layout::parse(&out).unwrap(), l);
    }

    #[test]
    fn to_toml_handles_shape_changes() {
        let text = "name = \"x\"\n\n[[windows]]\npanel = \"a\"\n\n[[windows]]\n# second\npanel = \"b\"\n";
        let mut l = Layout::parse(text).unwrap();
        l.windows.truncate(1);
        let out = l.to_toml(Some(text));
        assert_eq!(Layout::parse(&out).unwrap(), l);
        assert!(!out.contains("# second"));
        l.windows.clear();
        let out = l.to_toml(Some(text));
        assert_eq!(Layout::parse(&out).unwrap().windows, vec![]);
        l.windows = vec![PopOut { panel: "scopes".into(), ..PopOut::default() }, PopOut { panel: "trace".into(), ..PopOut::default() }];
        let out2 = l.to_toml(Some(&out));
        assert_eq!(Layout::parse(&out2).unwrap(), l);
        // Inline tables written by hand are updated in place.
        let inline = "main = { monitor = \"DP-1\" } # inline\n";
        let mut l = Layout::parse(inline).unwrap();
        l.main.monitor = Some(MonitorMatch::parse("DP-2"));
        let out = l.to_toml(Some(inline));
        assert!(out.starts_with("main = { monitor = \"DP-2\" } # inline\n"), "{out}");
        // Garbage existing text → fresh commented file.
        let out = l.to_toml(Some("this is [not toml"));
        assert!(out.starts_with("# stream-engine layout"));
        assert_eq!(Layout::parse(&out).unwrap(), l);
    }

    #[test]
    fn parse_over_builtin_and_errors() {
        let (set, errors) = LayoutSet::from_files([("layouts/show-3disp.toml", "zoom = 1.2\n[show]\nrail_width = 400\n")]);
        assert!(errors.is_empty(), "{errors:?}");
        let l = set.get("show-3disp").unwrap();
        assert_eq!((l.zoom, l.show.rail_width, l.show.rail_tab.as_str()), (1.2, 400.0, "chat"));
        assert_eq!(l.auto_fallback.as_deref(), Some("show-2disp"));
        assert_eq!(l.windows, Layout::builtin("show-3disp").unwrap().windows);

        let err = format!("{:#}", Layout::parse("zoom = \"big\"\n").unwrap_err());
        assert!(err.contains("big") && err.contains("line 1"), "{err}");
        assert_eq!(Layout::parse("page = \"lghts\"\n").unwrap().warnings(), ["unknown page `lghts`"]);
        assert!(Layout::parse("[confidence]\ncanvases = 3\n").is_err());
        assert!(Layout::parse("zoom = ").is_err());
        let (set, errors) = LayoutSet::from_files([
            ("a/bad.toml", "zoom = \"big\""),
            ("a/good.toml", "page = \"lights\""),
            ("a/shortcuts.toml", "[keys]\n"),
            ("a/readme.md", ""),
        ]);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].starts_with("a/bad.toml: "));
        assert_eq!(set.names(), ["single", "show-3disp", "show-2disp", "good"]);
        assert_eq!(set.get("good").unwrap().name, "good");
    }

    #[test]
    fn names_and_cycling() {
        let mut set = LayoutSet::builtin();
        for n in ["zeta", "alpha"] {
            set.insert(Layout { name: n.into(), ..Layout::default() });
        }
        assert_eq!(set.names(), ["single", "show-3disp", "show-2disp", "alpha", "zeta"]);
        assert_eq!(set.next("show-3disp"), "show-2disp");
        assert_eq!(set.next("show-2disp"), "alpha");
        assert_eq!(set.next("zeta"), "single");
        assert_eq!(set.next("missing"), "single");
    }

    #[test]
    fn effective_switches_to_2disp_while_tv_absent() {
        let set = LayoutSet::builtin();
        let s3 = set.get("show-3disp").unwrap();
        let eff = |f: &str| effective(s3, &set, &fixture(f)).name.clone();
        assert_eq!(eff("three-monitors"), "show-3disp");
        assert_eq!(eff("two-monitors-tv-off"), "show-2disp");
        assert_eq!(eff("tv-disabled"), "show-2disp");
        assert_eq!(eff("three-monitors"), "show-3disp");
        // Unknown monitor list: stay.
        assert_eq!(effective(s3, &set, &[]).name, "show-3disp");
        // Layouts without auto_fallback, or with a dangling one, stay.
        assert_eq!(effective(set.get("single").unwrap(), &set, &fixture("tv-disabled")).name, "single");
        let dangling = Layout { auto_fallback: Some("gone".into()), ..s3.clone() };
        assert_eq!(effective(&dangling, &set, &fixture("tv-disabled")).name, "show-3disp");
    }

    #[test]
    fn plan_places_every_window() {
        let s3 = Layout::builtin("show-3disp").unwrap();
        let (plans, placement) = s3.plan(&fixture("three-monitors"), None);
        let got: Vec<_> = plans.iter().map(|p| (p.app_id.as_str(), p.monitor.as_deref(), p.fullscreen)).collect();
        assert_eq!(
            got,
            [("stream-engine", Some("DP-1"), false), ("stream-engine.multiview", Some("DP-2"), false), ("stream-engine.program", Some("HDMI-A-1"), true)]
        );
        assert!(placement.changed);
        // DP-2 gone: the pop-out has no monitor (it is not moved elsewhere), confidence finds none.
        let (plans, placement) = s3.plan(&fixture("tv-only-dp1"), Some("HDMI-A-1"));
        assert_eq!(plans[1].monitor, None);
        assert_eq!((plans[2].monitor.as_deref(), placement.reason), (None, monitors::Reason::NoMonitor));
        // Confidence disabled: no program window planned.
        let off = Layout { confidence: ConfidenceCfg { enabled: false, ..ConfidenceCfg::default() }, ..s3 };
        assert_eq!(off.plan(&fixture("three-monitors"), None).0.len(), 2);
    }

    #[test]
    fn warnings_flag_bad_values() {
        let l = Layout { zoom: 9.0, page: "nope".into(), windows: vec![PopOut { panel: "has space".into(), ..PopOut::default() }], ..Layout::default() };
        let w = l.warnings().join("\n");
        for needle in ["zoom 9", "unknown page `nope`", "`has space`"] {
            assert!(w.contains(needle), "missing {needle:?} in\n{w}");
        }
    }

    #[test]
    fn example_layout_files_parse_and_round_trip_unchanged() {
        for n in BUILTIN_NAMES {
            let text = example(n);
            let (set, errors) = LayoutSet::from_files([(format!("layouts/{n}.toml").as_str(), text.as_str())]);
            assert!(errors.is_empty(), "{errors:?}");
            let l = set.get(n).unwrap();
            assert!(l.warnings().is_empty(), "{n}: {:?}", l.warnings());
            // The shipped files spell out the built-ins exactly.
            assert_eq!(l, &Layout::builtin(n).unwrap(), "{n}");
            // Saving an unchanged layout rewrites nothing.
            assert_eq!(l.to_toml(Some(&text)), text, "{n}");
        }
    }
}
