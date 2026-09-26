//! Programmer (§9.2): select fixtures/groups, set values live (overriding playbacks until
//! released), highlight/locate, and store into palettes, cues, or presets (project files,
//! edited with comments preserved).

use crate::palette::{self, Spec};
use crate::rig::{AttrKind, Rig};
use crate::show::Show;
use se_proto::{Command, Op, Origin, PRIORITY_MANUAL, Value};
use std::collections::BTreeMap;

pub const KEY: &str = "programmer";
pub const HIGHLIGHT_KEY: &str = "highlight";
/// Above every playback (the starter `safe` list runs at 1000).
pub const HIGHLIGHT_PRIORITY: u16 = 1100;

#[derive(Default, Clone, Debug)]
pub struct Programmer {
    pub selection: Vec<String>,
    /// address → value held in the programmer.
    pub values: BTreeMap<String, Value>,
    pub highlight: Vec<String>,
}

fn cmd(origin: Origin, op: Op, key: &str, priority: u16) -> Command {
    Command::new(origin, op).with_key(key).with_priority(Some(priority))
}

impl Programmer {
    /// Heads covered by the selection (roots and cells as addressed).
    pub fn heads(&self, rig: &Rig) -> Vec<usize> {
        let mut v = Vec::new();
        for t in &self.selection {
            if let Ok(hs) = rig.leaves_for(t) {
                for h in hs {
                    if !v.contains(&h) {
                        v.push(h);
                    }
                }
            }
        }
        v
    }

    pub fn select(&mut self, rig: &Rig, targets: &[String], add: bool) -> Result<(), String> {
        for t in targets {
            if t != "all" && rig.group(t).is_none() && rig.head(t).is_none() {
                return Err(format!("unknown fixture or group `{t}`"));
            }
        }
        if !add {
            self.selection.clear();
        }
        for t in targets {
            if !self.selection.contains(t) {
                self.selection.push(t.clone());
            }
        }
        Ok(())
    }

    /// Addresses for `attr` on the current selection.
    fn addresses(&self, rig: &Rig, attr: &str) -> Vec<(String, usize, AttrKind)> {
        let mut out: Vec<(String, usize, AttrKind)> = Vec::new();
        for t in &self.selection {
            for h in rig.heads_for(t, attr).unwrap_or_default() {
                if let Some(a) = rig.heads[h].attr(attr) {
                    let addr = rig.heads[h].addr(attr);
                    if !out.iter().any(|(x, _, _)| *x == addr) {
                        out.push((addr, h, a.kind));
                    }
                }
            }
        }
        out
    }

    /// Set `attr` on the selection (value may be a literal or a palette/stream reference).
    pub fn set(&mut self, show: &Show, attr: &str, value: &Value, origin: Origin, get: &dyn Fn(&str) -> Option<Value>) -> Result<Vec<Command>, String> {
        if self.selection.is_empty() {
            return Err("nothing selected".into());
        }
        let spec = Spec::from_value(value)?;
        let attrs: Vec<&str> = match palette::attr_group(attr) {
            Some(g) => g.to_vec(),
            None => vec![attr],
        };
        let mut cmds = Vec::new();
        for a in attrs {
            for (addr, h, kind) in self.addresses(&show.rig, a) {
                let Ok(v) = palette::resolve(&spec, &show.palettes, &show.rig, h, a, kind, get) else { continue };
                cmds.push(cmd(origin, Op::Set { address: addr.clone(), value: v.clone() }, KEY, PRIORITY_MANUAL));
                self.values.insert(addr, v);
            }
        }
        if cmds.is_empty() {
            return Err(format!("the selection has no `{attr}`"));
        }
        Ok(cmds)
    }

    /// Relative change (encoders): numeric attributes only.
    pub fn nudge(&mut self, rig: &Rig, attr: &str, delta: f64, origin: Origin, get: &dyn Fn(&str) -> Option<Value>) -> Result<Vec<Command>, String> {
        let mut cmds = Vec::new();
        for (addr, _, kind) in self.addresses(rig, attr) {
            if kind == AttrKind::Color {
                return Err("colour can't be nudged".into());
            }
            let cur = self.values.get(&addr).cloned().or_else(|| get(&addr)).and_then(|v| v.as_f64()).unwrap_or(0.0);
            let Some(v) = palette::coerce(kind, &Value::Float(cur + delta)) else { continue };
            cmds.push(cmd(origin, Op::Set { address: addr.clone(), value: v.clone() }, KEY, PRIORITY_MANUAL));
            self.values.insert(addr, v);
        }
        if cmds.is_empty() {
            return Err(format!("the selection has no `{attr}`"));
        }
        Ok(cmds)
    }

    /// Locate: full, open white, centred, default beam — into the programmer.
    pub fn locate(&mut self, show: &Show, origin: Origin) -> Result<Vec<Command>, String> {
        if self.selection.is_empty() {
            return Err("nothing selected".into());
        }
        let mut cmds = Vec::new();
        let look: &[(&str, Value)] = &[
            ("intensity", Value::Float(1.0)),
            ("color", Value::from([1.0f32, 1.0, 1.0, 1.0])),
            ("white", Value::Float(0.0)),
            ("pan", Value::Float(0.5)),
            ("tilt", Value::Float(0.5)),
            ("zoom", Value::Float(0.5)),
            ("focus", Value::Float(0.5)),
            ("iris", Value::Float(0.0)),
            ("frost", Value::Float(0.0)),
            ("prism", Value::Float(0.0)),
            ("gobo", Value::Int(0)),
            ("gobo_rotate", Value::Float(0.0)),
            ("strobe", Value::Float(0.0)),
            ("cto", Value::Float(0.0)),
        ];
        for (attr, v) in look {
            for (addr, _, _) in self.addresses(&show.rig, attr) {
                cmds.push(cmd(origin, Op::Set { address: addr.clone(), value: v.clone() }, KEY, PRIORITY_MANUAL));
                self.values.insert(addr, v.clone());
            }
        }
        Ok(cmds)
    }

    /// Highlight the selection (open white at full, above everything) or turn it off.
    pub fn highlight(&mut self, rig: &Rig, on: bool, origin: Origin) -> Vec<Command> {
        let mut cmds: Vec<Command> = self.highlight.drain(..).map(|a| cmd(origin, Op::Release { address: a }, HIGHLIGHT_KEY, HIGHLIGHT_PRIORITY)).collect();
        if on {
            for (attr, v) in [("intensity", Value::Float(1.0)), ("color", Value::from([1.0f32, 1.0, 1.0, 1.0])), ("strobe", Value::Float(0.0))] {
                for (addr, _, _) in self.addresses(rig, attr) {
                    cmds.push(cmd(origin, Op::Set { address: addr.clone(), value: v.clone() }, HIGHLIGHT_KEY, HIGHLIGHT_PRIORITY));
                    self.highlight.push(addr);
                }
            }
        }
        cmds
    }

    /// Release every programmer value (playbacks show again).
    pub fn release(&mut self, origin: Origin) -> Vec<Command> {
        std::mem::take(&mut self.values).into_keys().map(|a| cmd(origin, Op::Release { address: a }, KEY, PRIORITY_MANUAL)).collect()
    }

    /// Group programmer values by head id and attribute.
    fn by_head(&self, rig: &Rig) -> BTreeMap<String, Vec<(String, Value, AttrKind)>> {
        let mut m: BTreeMap<String, Vec<(String, Value, AttrKind)>> = BTreeMap::new();
        for (addr, v) in &self.values {
            let Some(rest) = addr.strip_prefix("lights.") else { continue };
            let Some((head, attr)) = rest.rsplit_once('.') else { continue };
            let Some(h) = rig.head(head) else { continue };
            let Some(a) = rig.heads[h].attr(attr) else { continue };
            m.entry(head.to_string()).or_default().push((attr.to_string(), v.clone(), a.kind));
        }
        m
    }
}

/// Palette kind an attribute belongs to.
pub fn kind_of(attr: &str) -> &'static str {
    match attr {
        "intensity" => "intensity",
        "color" | "white" | "amber" | "uv" | "lime" | "cto" => "color",
        "pan" | "tilt" => "position",
        "zoom" | "focus" | "iris" | "frost" | "prism" | "gobo" | "gobo_rotate" => "beam",
        _ => "any",
    }
}

/// TOML value for an attribute value (colours as `#rrggbb`, 3-decimal floats).
pub fn toml_value(kind: AttrKind, v: &Value) -> toml_edit::Value {
    match kind {
        AttrKind::Color => {
            let c = v.as_color().unwrap_or([1.0; 4]);
            let b = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
            toml_edit::Value::from(format!("#{:02x}{:02x}{:02x}", b(c[0]), b(c[1]), b(c[2])))
        }
        AttrKind::Index | AttrKind::Raw => toml_edit::Value::from(v.as_i64().unwrap_or(0)),
        _ => toml_edit::Value::from((v.as_f64().unwrap_or(0.0) * 1000.0).round() / 1000.0),
    }
}

fn head_table(rows: &[(String, Value, AttrKind)]) -> toml_edit::InlineTable {
    let mut t = toml_edit::InlineTable::new();
    for (attr, v, kind) in rows {
        t.insert(attr, toml_value(*kind, v));
    }
    t
}

fn valid_name(n: &str) -> Result<(), String> {
    if se_proto::address::is_valid(n, false) && !n.contains('.') { Ok(()) } else { Err(format!("bad name `{n}` (letters, digits, `_`, `-`)")) }
}

/// Store the programmer into `lights/palettes/<name>.toml` (replacing its `[set]`).
pub fn store_palette(project: &se_store::Project, prog: &Programmer, rig: &Rig, name: &str, kind: Option<&str>) -> Result<String, String> {
    valid_name(name)?;
    if let Some(k) = kind
        && !["color", "position", "beam", "intensity", "any"].contains(&k)
    {
        return Err(format!("unknown palette kind `{k}` (color | position | beam | intensity | any)"));
    }
    let mut rows = prog.by_head(rig);
    // a typed palette keeps only its kind of attributes
    if let Some(k) = kind.filter(|k| *k != "any") {
        for r in rows.values_mut() {
            r.retain(|(attr, _, _)| kind_of(attr) == k);
        }
        rows.retain(|_, r| !r.is_empty());
    }
    if rows.is_empty() {
        return Err(match kind {
            Some(k) if k != "any" => format!("the programmer holds no {k} values"),
            _ => "the programmer is empty".into(),
        });
    }
    let rel = format!("lights/palettes/{name}.toml");
    project
        .edit(&rel, |doc| {
            if !doc.contains_key("label") {
                doc["label"] = toml_edit::value(name);
            }
            if let Some(k) = kind {
                doc["kind"] = toml_edit::value(k);
            } else if !doc.contains_key("kind") {
                doc["kind"] = toml_edit::value("any");
            }
            let mut set = toml_edit::Table::new();
            for (head, r) in &rows {
                set.insert(head, toml_edit::Item::Value(toml_edit::Value::InlineTable(head_table(r))));
            }
            doc["set"] = toml_edit::Item::Table(set);
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    Ok(rel)
}

/// Store the programmer as cue `cue` of `lights/cuelists/<list>.toml`: replaces that cue's
/// `[cue.set]` or appends a new cue (tracking: only these values change).
pub fn store_cue(project: &se_store::Project, prog: &Programmer, rig: &Rig, list: &str, cue: &str) -> Result<String, String> {
    valid_name(list)?;
    if cue.is_empty() {
        return Err("cue id is empty".into());
    }
    let rows = prog.by_head(rig);
    if rows.is_empty() {
        return Err("the programmer is empty".into());
    }
    let rel = format!("lights/cuelists/{list}.toml");
    project
        .edit(&rel, |doc| {
            if !doc.contains_key("label") {
                doc["label"] = toml_edit::value(list);
            }
            let mut set = toml_edit::Table::new();
            for (head, r) in &rows {
                set.insert(head, toml_edit::Item::Value(toml_edit::Value::InlineTable(head_table(r))));
            }
            let cues = doc.entry("cue").or_insert(toml_edit::Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
            let arr = cues.as_array_of_tables_mut().ok_or_else(|| anyhow::anyhow!("`cue` is not an array of tables"))?;
            let pos = arr.iter().enumerate().position(|(i, t)| match t.get("id") {
                Some(id) => id.as_str().map(String::from).or_else(|| id.as_integer().map(|n| n.to_string())).as_deref() == Some(cue),
                None => (i + 1).to_string() == cue,
            });
            match pos {
                Some(i) => {
                    let t = arr.get_mut(i).expect("index from position");
                    t["set"] = toml_edit::Item::Table(set);
                }
                None => {
                    let mut t = toml_edit::Table::new();
                    t["id"] = toml_edit::value(cue);
                    t["set"] = toml_edit::Item::Table(set);
                    arr.push(t);
                }
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    Ok(rel)
}

/// Store the programmer as `presets/<name>.toml` with a `set` of addresses.
pub fn store_preset(project: &se_store::Project, prog: &Programmer, rig: &Rig, name: &str) -> Result<String, String> {
    valid_name(name)?;
    if prog.values.is_empty() {
        return Err("the programmer is empty".into());
    }
    let rel = format!("presets/{name}.toml");
    project
        .edit(&rel, |doc| {
            if !doc.contains_key("label") {
                doc["label"] = toml_edit::value(name.to_uppercase());
            }
            let mut set = toml_edit::InlineTable::new();
            for (addr, v) in &prog.values {
                let kind = addr
                    .strip_prefix("lights.")
                    .and_then(|r| r.rsplit_once('.'))
                    .and_then(|(h, a)| rig.head(h).and_then(|h| rig.heads[h].attr(a)).map(|a| a.kind))
                    .unwrap_or(AttrKind::Unit);
                set.insert(addr, toml_value(kind, v));
            }
            doc["set"] = toml_edit::value(set);
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    Ok(rel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::rig;

    fn show() -> Show {
        let mut s = Show::empty();
        s.rig = rig(
            "[fixtures.a]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\n[fixtures.b]\nprofile = \"generic_moving_head\"\naddress = 10\n[groups]\nfront = [\"a\", \"b\"]",
        );
        s
    }

    #[test]
    fn set_locate_highlight_release() {
        let s = show();
        let mut p = Programmer::default();
        let get = |_: &str| None;
        assert!(p.set(&s, "intensity", &Value::Float(1.0), Origin::Ui, &get).unwrap_err().contains("nothing selected"));
        p.select(&s.rig, &["front".into()], false).unwrap();
        assert!(p.select(&s.rig, &["ghost".into()], true).is_err());
        let c = p.set(&s, "color", &Value::Str("#ff0000".into()), Origin::Ui, &get).unwrap();
        assert_eq!(c.len(), 2);
        assert!(c.iter().all(|c| c.key.as_deref() == Some(KEY) && c.priority == Some(PRIORITY_MANUAL)));
        let c = p.set(&s, "pan", &Value::Float(0.25), Origin::Ui, &get).unwrap();
        assert_eq!(c.len(), 1, "only the mover has pan");
        let c = p.nudge(&s.rig, "pan", 0.1, Origin::Midi, &get).unwrap();
        let Op::Set { address, value } = &c[0].op else { panic!() };
        assert_eq!(address, "lights.b.pan");
        assert!((value.as_f64().unwrap() - 0.35).abs() < 1e-9);
        assert!(p.locate(&s, Origin::Ui).unwrap().len() > 4);
        let h = p.highlight(&s.rig, true, Origin::Ui);
        assert!(h.iter().all(|c| c.priority == Some(HIGHLIGHT_PRIORITY)));
        let off = p.highlight(&s.rig, false, Origin::Ui);
        assert_eq!(off.len(), h.len());
        assert!(off.iter().all(|c| matches!(c.op, Op::Release { .. })));
        let n = p.values.len();
        assert_eq!(p.release(Origin::Ui).len(), n);
        assert!(p.values.is_empty());
    }

    #[test]
    fn store_palette_cue_and_preset_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("project.toml"), "schema = 1\n").unwrap();
        std::fs::create_dir_all(dir.path().join("lights/cuelists")).unwrap();
        std::fs::write(
            dir.path().join("lights/cuelists/main.toml"),
            "# my show\nlabel = \"Main\"\n\n[[cue]]\nid = \"1\"\nfade = \"2s\" # keep\n[cue.set]\nall = { intensity = 1.0 }\n",
        )
        .unwrap();
        let project = se_store::Project::open(dir.path()).unwrap();
        let s = show();
        let mut p = Programmer::default();
        let get = |_: &str| None;
        p.select(&s.rig, &["a".into()], false).unwrap();
        p.set(&s, "color", &Value::Str("#ff8000".into()), Origin::Ui, &get).unwrap();
        p.set(&s, "intensity", &Value::Float(0.5), Origin::Ui, &get).unwrap();
        store_palette(&project, &p, &s.rig, "amber", Some("color")).unwrap();
        let pal: toml::Table = toml::from_str(&std::fs::read_to_string(dir.path().join("lights/palettes/amber.toml")).unwrap()).unwrap();
        let parsed = crate::palette::Palette::parse("amber", &pal).unwrap();
        assert_eq!(parsed.set[0].0, "a");
        assert_eq!(parsed.attrs(), vec!["color"], "a colour palette keeps only colour attributes");
        assert!(store_palette(&project, &p, &s.rig, "pos", Some("position")).unwrap_err().contains("no position values"));
        store_cue(&project, &p, &s.rig, "main", "1").unwrap();
        store_cue(&project, &p, &s.rig, "main", "2").unwrap();
        let src = std::fs::read_to_string(dir.path().join("lights/cuelists/main.toml")).unwrap();
        assert!(src.contains("# my show") && src.contains("# keep"), "comments preserved:\n{src}");
        let l = crate::cuelist::CueList::parse("main", &toml::from_str(&src).unwrap()).unwrap();
        assert_eq!(l.cues.len(), 2);
        assert_eq!(l.cues[0].fade_ms, 2000, "existing cue timing kept");
        assert_eq!(l.cues[1].id, "2");
        store_preset(&project, &p, &s.rig, "amber_look").unwrap();
        let pre = std::fs::read_to_string(dir.path().join("presets/amber_look.toml")).unwrap();
        assert!(pre.contains("\"lights.a.color\" = \"#ff8000\""), "{pre}");
        assert!(store_palette(&project, &Programmer::default(), &s.rig, "x", None).is_err());
        assert!(store_palette(&project, &p, &s.rig, "../evil", None).is_err());
    }
}
