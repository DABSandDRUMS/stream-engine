//! MIDI/deck learn: `midi.learn {target}` arms; the next moved control (or pressed deck key)
//! is written into its `controllers/*.toml` file with `toml_edit` (comments preserved), and
//! the project watcher hot-reloads it into the core and into this subsystem.

use crate::midi::controls::{ControlDef, Kind};
use se_core::Config;
use se_proto::{Meta, ValueType};
use std::collections::HashMap;
use std::time::Instant;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Moved {
    Absolute,
    Relative,
    Button,
}

/// The control that moved.
#[derive(Clone, Debug)]
pub struct Source {
    pub device: String,
    pub control: String,
    pub control_def: ControlDef,
    /// Existing controllers file of the device (None = create `controllers/<device>.toml`).
    pub file: Option<String>,
    pub client_name: String,
    /// Set when identical devices are connected, so the new file pins this unit.
    pub usb: Option<String>,
    pub motor: bool,
    pub moved: Moved,
}

/// Armed learn state.
pub struct Learn {
    pub target: String,
    pub started: Instant,
    /// First value seen per (device, control) while armed, to reject jitter.
    pub baseline: HashMap<(String, usize), f32>,
}

/// Minimum movement of an absolute control (≈5/127) to count as "moved".
pub const MOVE_THRESHOLD: f32 = 0.04;

/// What gets written.
#[derive(Clone, Debug)]
pub enum Plan {
    /// Core binding: `[[binding]]` target ← `midi.<dev>.<control>`.
    Binding { target: String, signal: String, range: Option<[f64; 2]>, takeover: &'static str, int: bool },
    /// `[[map]]` with key/value pairs.
    Map { control: String, entries: Vec<(String, toml_edit::Value)> },
    /// A CC switch (footswitch) declared as a button (`[control.<name>]`) plus its `[[map]]`.
    Button { control: String, cc: u8, channel: Option<u8>, entries: Vec<(String, toml_edit::Value)> },
}

/// Classify a learn target for a controller of the given kind.
pub fn plan(target: &str, meta: Option<&Meta>, cfg: &Config, src: &Source) -> Result<Plan, String> {
    let action = action_entries(target, meta, cfg);
    match src.moved {
        Moved::Button => {
            let entries = match action {
                Some(e) => e,
                None => vec![("momentary".to_string(), toml_edit::Value::from(target))],
            };
            Ok(Plan::Map { control: src.control.clone(), entries })
        }
        Moved::Relative => {
            if action.is_some() && !matches!(meta.map(|m| m.ty), Some(ValueType::Float | ValueType::Int)) {
                // encoder on a preset/scene target: fire on turn is surprising; step through scenes instead
                return Err(format!("`{target}` is not a continuous parameter; learn it on a button"));
            }
            let mut entries = vec![("target".to_string(), toml_edit::Value::from(target))];
            if let Some([lo, hi]) = meta.and_then(|m| m.range) {
                let step = if matches!(meta.map(|m| m.ty), Some(ValueType::Int)) { 1.0 } else { ((hi - lo) / 100.0 * 1e6).round() / 1e6 };
                entries.push(("step".into(), toml_edit::Value::from(step)));
            }
            entries.push(("feedback".into(), toml_edit::Value::from(true)));
            Ok(Plan::Map { control: src.control.clone(), entries })
        }
        Moved::Absolute => {
            if let Some(entries) = action {
                if let Kind::Cc { cc } = src.control_def.kind {
                    // a footswitch sending CC: declare it as a button, then map it
                    return Ok(Plan::Button { control: src.control.clone(), cc, channel: src.control_def.ch, entries });
                }
                return Err(format!("`{}` ({}) cannot fire `{target}`; learn it on a button or footswitch", src.control, src.control_def.kind.describe()));
            }
            let int = matches!(meta.map(|m| m.ty), Some(ValueType::Int));
            Ok(Plan::Binding {
                target: target.to_string(),
                signal: format!("midi.{}.{}", src.device, src.control),
                range: meta.and_then(|m| m.range),
                takeover: if src.motor { "jump" } else { "pickup" },
                int,
            })
        }
    }
}

/// Button semantics of a target: presets, scenes, commands, toggles, triggers.
fn action_entries(target: &str, meta: Option<&Meta>, cfg: &Config) -> Option<Vec<(String, toml_edit::Value)>> {
    let v = |s: &str| toml_edit::Value::from(s);
    // `do:<cmd>`, one command per line
    if let Some(cmds) = target.strip_prefix("do:") {
        let mut a = Array::new();
        for c in cmds.lines().map(str::trim).filter(|c| !c.is_empty()) {
            a.push(c);
        }
        return Some(vec![("do".into(), toml_edit::Value::Array(a))]);
    }
    if let Some(rest) = target.strip_prefix("preset.") {
        let name = rest.strip_suffix(".active").unwrap_or(rest);
        if cfg.presets.contains_key(name) {
            return Some(vec![("preset".into(), v(name))]);
        }
    }
    if let Some(name) = target.strip_prefix("scene.")
        && cfg.scenes.contains_key(name)
    {
        return Some(vec![("scene".into(), v(name))]);
    }
    if cfg.presets.contains_key(target) {
        return Some(vec![("preset".into(), v(target))]);
    }
    match meta.map(|m| m.ty) {
        Some(ValueType::Bool) => Some(vec![("toggle".into(), v(target))]),
        Some(ValueType::Trigger) => {
            let mut a = Array::new();
            a.push(format!("trigger {target}"));
            Some(vec![("do".into(), toml_edit::Value::Array(a))])
        }
        _ => None,
    }
}

/// Apply a plan to a device file (creating its header when new).
pub fn apply(doc: &mut DocumentMut, p: &Plan, src: &Source, new_file: bool) -> Result<(), String> {
    if new_file {
        doc.decor_mut().set_prefix(format!("# {} — created by MIDI learn\n", src.client_name));
        doc["kind"] = value("midi");
        doc["match"] = value(src.client_name.as_str());
        if let Some(u) = &src.usb {
            doc["usb"] = value(u.as_str());
        }
    }
    match p {
        Plan::Binding { target, signal, range, takeover, int } => {
            // one binding per signal
            remove_where(doc, "binding", |t| t.get("signal").and_then(|v| v.as_str()) == Some(signal));
            remove_where(doc, "map", |t| t.get("control").and_then(|v| v.as_str()) == Some(src.control.as_str()));
            let mut t = Table::new();
            t["target"] = value(target.as_str());
            t["signal"] = value(signal.as_str());
            if let Some([lo, hi]) = range {
                let mut a = Array::new();
                if *int {
                    a.push(*lo as i64);
                    a.push(*hi as i64);
                } else {
                    a.push(*lo);
                    a.push(*hi);
                }
                t["range"] = value(a);
            }
            t["mode"] = value("replace");
            t["takeover"] = value(*takeover);
            t["feedback"] = value(true);
            aot(doc, "binding")?.push(t);
        }
        Plan::Map { control, entries } => push_map(doc, src, control, entries)?,
        Plan::Button { control, cc, channel, entries } => {
            // [control.<a>.<b>] cc = N, switch = "momentary" (fires once per press in every FBV switch mode except "toggle")
            let mut t = doc.entry("control").or_insert(Item::Table(implicit_table())).as_table_mut().ok_or("`control` is not a table")?;
            t.set_implicit(true);
            let segs: Vec<&str> = control.split('.').collect();
            for (i, seg) in segs.iter().enumerate() {
                let last = i + 1 == segs.len();
                let next = t.entry(seg).or_insert(Item::Table(if last { Table::new() } else { implicit_table() }));
                t = next.as_table_mut().ok_or_else(|| format!("`control.{control}` is not a table"))?;
            }
            t.clear();
            t["cc"] = value(*cc as i64);
            if let Some(ch) = channel {
                t["channel"] = value(*ch as i64 + 1);
            }
            t["switch"] = value("momentary");
            push_map(doc, src, control, entries)?;
        }
    }
    Ok(())
}

fn push_map(doc: &mut DocumentMut, src: &Source, control: &str, entries: &[(String, toml_edit::Value)]) -> Result<(), String> {
    remove_where(doc, "map", |t| t.get("control").and_then(|v| v.as_str()) == Some(control));
    let sig = format!("midi.{}.{}", src.device, control);
    remove_where(doc, "binding", |t| t.get("signal").and_then(|v| v.as_str()) == Some(sig.as_str()));
    let mut t = Table::new();
    t["control"] = value(control);
    for (k, v) in entries {
        t[k.as_str()] = Item::Value(v.clone());
    }
    aot(doc, "map")?.push(t);
    Ok(())
}

fn aot<'a>(doc: &'a mut DocumentMut, key: &str) -> Result<&'a mut ArrayOfTables, String> {
    let item = doc.entry(key).or_insert(Item::ArrayOfTables(ArrayOfTables::new()));
    item.as_array_of_tables_mut().ok_or_else(|| format!("`{key}` in the file is not an array of tables ([[{key}]])"))
}

fn remove_where(doc: &mut DocumentMut, key: &str, pred: impl Fn(&Table) -> bool) {
    if let Some(a) = doc.get_mut(key).and_then(|i| i.as_array_of_tables_mut()) {
        a.retain(|t| !pred(t));
    }
}

/// Deck key assignment: replaces `[page.<page>.key.<key>]` (or removes it).
pub fn assign_key(doc: &mut DocumentMut, page: &str, key: u8, entries: Option<&[(String, toml_edit::Value)]>) -> Result<(), String> {
    let pages = doc.entry("page").or_insert(Item::Table(implicit_table())).as_table_mut().ok_or("`page` is not a table")?;
    pages.set_implicit(true);
    let p = pages.entry(page).or_insert(Item::Table(Table::new())).as_table_mut().ok_or_else(|| format!("`page.{page}` is not a table"))?;
    // keep `[page.<name>]` even when its last key is cleared (the page stays in `pages = [...]`)
    p.set_implicit(false);
    let keys = p.entry("key").or_insert(Item::Table(implicit_table())).as_table_mut().ok_or_else(|| format!("`page.{page}.key` is not a table"))?;
    keys.set_implicit(true);
    let k = key.to_string();
    match entries {
        None => {
            keys.remove(&k);
        }
        Some(e) => {
            let mut t = Table::new();
            for (name, v) in e {
                t[name.as_str()] = Item::Value(v.clone());
            }
            keys.insert(&k, Item::Table(t));
        }
    }
    Ok(())
}

fn implicit_table() -> Table {
    let mut t = Table::new();
    t.set_implicit(true);
    t
}

/// Deck key entries for a learn target.
pub fn deck_entries(target: &str, meta: Option<&Meta>, cfg: &Config) -> Vec<(String, toml_edit::Value)> {
    action_entries(target, meta, cfg).unwrap_or_else(|| vec![("momentary".into(), toml_edit::Value::from(target))])
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_core::config::{PresetDef, SceneDef};

    fn cfg() -> Config {
        let mut c = Config::default();
        c.presets.insert("hype".into(), PresetDef { name: "hype".into(), ..Default::default() });
        c.scenes.insert("duo".into(), SceneDef { name: "duo".into(), ..Default::default() });
        c
    }

    fn src(moved: Moved, kind: Kind) -> Source {
        Source {
            device: "xtouch".into(),
            control: "fader".into(),
            control_def: ControlDef::new("fader", Some(8), kind),
            file: Some("controllers/xtouch.toml".into()),
            client_name: "X-TOUCH MINI".into(),
            usb: None,
            motor: false,
            moved,
        }
    }

    #[test]
    fn fader_learns_a_pickup_binding_and_replaces_previous() {
        let meta = Meta::float(0.0, [0.0, 1.0]);
        let s = src(Moved::Absolute, Kind::PitchBend);
        let p = plan("fx.rgb_split.amount", Some(&meta), &cfg(), &s).unwrap();
        let mut doc: DocumentMut =
            "# my surface\nkind = \"midi\"\nmatch = \"X-TOUCH MINI*\"\n\n[[binding]]\ntarget = \"fx.old\"\nsignal = \"midi.xtouch.fader\"\n".parse().unwrap();
        apply(&mut doc, &p, &s, false).unwrap();
        let out = doc.to_string();
        assert!(out.starts_with("# my surface"));
        assert!(!out.contains("fx.old"));
        let t: toml::Table = out.parse().unwrap();
        let b = &t["binding"].as_array().unwrap()[0];
        assert_eq!(b["target"].as_str(), Some("fx.rgb_split.amount"));
        assert_eq!(b["takeover"].as_str(), Some("pickup"));
        assert_eq!(b["mode"].as_str(), Some("replace"));
        assert_eq!(b["feedback"].as_bool(), Some(true));
    }

    #[test]
    fn button_and_encoder_targets() {
        let c = cfg();
        let mut s = src(Moved::Button, Kind::Note { note: 89 });
        s.control = "btn.1".into();
        let Plan::Map { control, entries } = plan("preset.hype.active", None, &c, &s).unwrap() else { panic!() };
        assert_eq!((control.as_str(), entries[0].0.as_str(), entries[0].1.as_str()), ("btn.1", "preset", Some("hype")));
        let bool_meta = Meta::boolean(false);
        let Plan::Map { entries, .. } = plan("fx.vhs.enabled", Some(&bool_meta), &c, &s).unwrap() else { panic!() };
        assert_eq!(entries[0].0, "toggle");
        let Plan::Map { entries, .. } = plan("do:preset.fire hype\n\nscene.take", None, &c, &s).unwrap() else { panic!() };
        let cmds: Vec<&str> = entries[0].1.as_array().unwrap().iter().filter_map(|v| v.as_str()).collect();
        assert_eq!((entries[0].0.as_str(), cmds), ("do", vec!["preset.fire hype", "scene.take"]), "one command per line");
        let mut e = src(Moved::Relative, Kind::Rel { cc: 16, enc: crate::midi::controls::RelEnc::SignMag });
        e.control = "enc.1".into();
        let meta = Meta::float(0.2, [0.0, 2.0]);
        let Plan::Map { entries, .. } = plan("fx.vignette.amount", Some(&meta), &c, &e).unwrap() else { panic!() };
        assert_eq!((entries[0].0.as_str(), entries[0].1.as_str()), ("target", Some("fx.vignette.amount")));
        assert_eq!(entries[1].1.as_float(), Some(0.02));
        assert!(plan("preset.hype", None, &c, &e).is_err());
    }

    #[test]
    fn new_file_gets_a_header_and_deck_keys_are_replaced() {
        let s = Source { file: None, usb: Some("usb-0000:0a:00.0-2.2".into()), ..src(Moved::Absolute, Kind::Cc { cc: 7 }) };
        let p = plan("audio.bus.music.gain", Some(&Meta::float(1.0, [0.0, 2.0])), &cfg(), &s).unwrap();
        let mut doc = DocumentMut::new();
        apply(&mut doc, &p, &s, true).unwrap();
        let t: toml::Table = doc.to_string().parse().unwrap();
        assert_eq!(t["kind"].as_str(), Some("midi"));
        assert_eq!(t["match"].as_str(), Some("X-TOUCH MINI"));
        assert_eq!(t["binding"].as_array().unwrap()[0]["signal"].as_str(), Some("midi.xtouch.fader"));

        // a footswitch CC firing a preset becomes a declared button + map
        let mut fs = src(Moved::Absolute, Kind::Cc { cc: 81 });
        fs.control = "cc.81".into();
        fs.control_def.ch = Some(0);
        let p = plan("preset.hype", None, &cfg(), &fs).unwrap();
        let mut doc: DocumentMut = "kind = \"midi\"\nmatch = \"FBV*\"\n".parse().unwrap();
        apply(&mut doc, &p, &fs, false).unwrap();
        let parsed = crate::config::parse_file("fbv", "controllers/fbv.toml", &doc.to_string().parse().unwrap()).unwrap();
        let crate::config::Parsed::Midi(m) = parsed else { panic!() };
        let c = m.controls.iter().find(|c| c.name == "cc.81").unwrap();
        assert!(c.button && c.ch == Some(0));
        assert_eq!(m.maps[0].action, crate::config::Action::Preset("hype".into()));
        assert_eq!(t["usb"].as_str(), Some("usb-0000:0a:00.0-2.2"));

        let mut deck: DocumentMut = "kind = \"deck\"\n[page.show.key.0]\npreset = \"old\"\n".parse().unwrap();
        assign_key(&mut deck, "show", 0, Some(&deck_entries("scene.duo", None, &cfg()))).unwrap();
        assign_key(&mut deck, "fx", 3, Some(&[("preset".into(), toml_edit::Value::from("hype"))])).unwrap();
        let t: toml::Table = deck.to_string().parse().unwrap();
        assert_eq!(t["page"]["show"]["key"]["0"]["scene"].as_str(), Some("duo"));
        assert!(t["page"]["show"]["key"]["0"].get("preset").is_none());
        assert_eq!(t["page"]["fx"]["key"]["3"]["preset"].as_str(), Some("hype"));
        assign_key(&mut deck, "show", 0, None).unwrap();
        let t: toml::Table = deck.to_string().parse().unwrap();
        assert!(t["page"]["show"].get("key").and_then(|k| k.get("0")).is_none());
    }
}
