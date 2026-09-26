//! `presets.catalog` (§15.5, Scenes → Quick effects): the plain words the quick effects console
//! needs to say what each quick effect does — built-in screen effects (from the render effect
//! library), overlays, light looks and cue lists, sounds — a summary of each quick effect, and
//! which project files fire each one (reactions, chat commands, buttons and pedals, timelines,
//! …). Also the file half of `preset.knob`: the core turns the knob live, this writes the value
//! into `presets/<id>.toml` with `toml_edit` (comments kept).

use crate::daemon::Ctx;
use se_core::config::{KnobSlot, PresetDef};
use se_proto::Value;
use se_render::effects::{EffectDef, LIBRARY};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use toml_edit::{DocumentMut, Item, TableLike};

/// Plain words for a built-in effect: (effect, label, what it does). Effects missing here fall
/// back to the library's own words.
const WORDS: &[(&str, &str, &str)] = &[
    ("shake", "Screen shake", "Shakes the whole picture."),
    ("zoom_pulse", "Zoom pulse", "Punches in on the picture, pulsing with the beat."),
    ("pixelate", "Pixelate", "Breaks the picture into big blocks."),
    ("blur", "Blur", "Softens the whole picture."),
    ("glitch", "Glitch", "Digital glitches: jumping slices and torn colors."),
    ("rgb_split", "Color split", "Pulls red and blue apart for a trippy edge."),
    ("vhs", "VHS tape", "Old videotape: wobble, scan lines and grain."),
    ("grade", "Color grade", "Changes the colors: warmer, punchier, brighter."),
    ("lut", "Color look", "Puts a color look file over the picture."),
    ("vignette", "Vignette", "Darkens the edges."),
    ("fade_to_black", "Fade to a color", "Covers everything, overlays too, with one color."),
];

/// Library effects that aren't screen effects: the green screen keys one camera.
const NOT_QUICK: &[&str] = &["chroma_key"];

fn effect(e: &EffectDef) -> Value {
    let words = WORDS.iter().find(|w| w.0 == e.name);
    Value::map()
        .with("name", e.name)
        .with("label", words.map(|w| w.1.to_string()).unwrap_or_else(|| e.name.replace('_', " ")))
        .with("description", words.map(|w| w.2).unwrap_or(e.description))
}

fn str_of<'a>(t: &'a toml::Table, k: &str) -> Option<&'a str> {
    t.get(k).and_then(toml::Value::as_str).filter(|s| !s.is_empty())
}

/// `[{name, label}]` of every item of a lights folder (`lights/palettes`, `lights/cuelists`).
fn labelled(cfg: &se_core::Config, kind: &str) -> Vec<Value> {
    cfg.other
        .get(kind)
        .map(|m| m.iter().map(|(n, t)| Value::map().with("name", n.clone()).with("label", str_of(t, "label").unwrap_or(""))).collect())
        .unwrap_or_default()
}

/// Named sounds in `audio/*.toml` `[sounds.<name>]`: name → (file, label).
fn configured_sounds(cfg: &se_core::Config) -> BTreeMap<String, (String, String)> {
    let mut out = BTreeMap::new();
    for t in cfg.other.get("audio").into_iter().flat_map(|m| m.values()) {
        for (name, s) in t.get("sounds").and_then(toml::Value::as_table).into_iter().flatten() {
            let Some(s) = s.as_table() else { continue };
            out.insert(name.clone(), (str_of(s, "file").unwrap_or("").to_string(), str_of(s, "label").unwrap_or("").to_string()));
        }
    }
    out
}

/// Sounds `audio.play` knows: the named ones, and files in `assets/sounds/` by file name
/// (se-audio's implicit sounds).
fn sounds(root: &Path, mut out: BTreeMap<String, (String, String)>) -> Vec<Value> {
    let mut files: Vec<std::path::PathBuf> =
        std::fs::read_dir(root.join("assets/sounds")).map(|rd| rd.flatten().map(|e| e.path()).collect()).unwrap_or_default();
    files.sort();
    for p in files {
        let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
        let (Some(stem), Some(file)) = (p.file_stem().and_then(|s| s.to_str()), p.file_name().and_then(|s| s.to_str())) else { continue };
        if !se_audio::sounds::AUDIO_EXTS.contains(&ext.as_str()) || stem.contains('.') || !se_proto::address::is_valid(stem, false) {
            continue;
        }
        let path = format!("assets/sounds/{file}");
        if !out.values().any(|(f, _)| *f == path) {
            out.entry(stem.to_string()).or_insert((path, String::new()));
        }
    }
    out.into_iter().map(|(name, (path, label))| Value::map().with("name", name).with("path", path).with("label", label)).collect()
}

/// Overlays a quick effect can burst on cue: visual patches with a trigger.
fn overlays(root: &Path) -> Vec<Value> {
    se_patch::scan(root)
        .into_iter()
        .flatten()
        .filter(|m| {
            m.has_trigger
                && m.kind != se_patch::Kind::Dsp
                && !matches!(m.layer, se_patch::Layer::Transition | se_patch::Layer::AudioEffect | se_patch::Layer::AudioSource)
        })
        .map(|m| Value::map().with("id", m.id.clone()).with("label", m.label.clone()).with("description", m.description.clone()).with("kind", m.kind.as_str()))
        .collect()
}

/// Quick effects a project file fires or depends on, each with the name of the entry that does
/// (`[[rule]] name = "big cheer"`, `do = ["preset.fire hype"]` → `("hype", "big cheer")`).
/// Finds `preset = "<id>"` keys (buttons, timeline regions) and `preset.fire|toggle|release <id>`,
/// `preset.<id>` and `preset.<id>.active` anywhere in the file's strings.
pub fn preset_refs(t: &toml::Table) -> Vec<(String, String)> {
    let mut out = Vec::new();
    walk_table(t, "", "", &mut out);
    out.sort();
    out.dedup();
    out
}

/// The name of an entry: its own name/label/title/id, or where it sits (a Stream Deck key
/// `[page.show.key.5]` → "key 6", a MIDI/deck mapping `control = "btn.1"` → "btn 1").
fn entry_name(t: &toml::Table, key: &str) -> Option<String> {
    for k in ["name", "label", "title", "id"] {
        if let Some(s) = str_of(t, k) {
            return Some(s.to_string());
        }
    }
    if let Ok(n) = key.parse::<u32>() {
        return Some(format!("key {}", n + 1));
    }
    if let Some(c) = str_of(t, "control") {
        return Some(c.replace(['.', '_'], " "));
    }
    match (t.get("key"), t.get("note"), t.get("cc")) {
        (Some(toml::Value::Integer(k)), _, _) => Some(format!("key {}", k + 1)),
        (_, Some(toml::Value::Integer(n)), _) => Some(format!("note {n}")),
        (_, _, Some(toml::Value::Integer(c))) => Some(format!("control {c}")),
        _ => None,
    }
}

fn walk_table(t: &toml::Table, key: &str, ctx: &str, out: &mut Vec<(String, String)>) {
    let here = match entry_name(t, key) {
        // a deck key names itself after its page ("SHOW, key 6")
        Some(own) if own.starts_with("key ") && !ctx.is_empty() => format!("{ctx}, {own}"),
        Some(own) => own,
        None => ctx.to_string(),
    };
    for (k, v) in t {
        if k == "preset"
            && let Some(id) = v.as_str().filter(|s| valid_id(s))
        {
            out.push((id.to_string(), here.clone()));
        }
        walk_value(v, k, &here, out);
    }
}

fn walk_value(v: &toml::Value, key: &str, ctx: &str, out: &mut Vec<(String, String)>) {
    match v {
        toml::Value::String(s) => scan_text(s, ctx, out),
        toml::Value::Array(a) => a.iter().for_each(|x| walk_value(x, "", ctx, out)),
        toml::Value::Table(t) => walk_table(t, key, ctx, out),
        _ => {}
    }
}

fn valid_id(s: &str) -> bool {
    !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn scan_text(s: &str, ctx: &str, out: &mut Vec<(String, String)>) {
    let mut toks = s
        .split(|c: char| c.is_whitespace() || matches!(c, ';' | '"' | '\'' | '(' | ')' | ',' | '!' | '&' | '|' | '{' | '}' | '[' | ']'))
        .filter(|t| !t.is_empty());
    while let Some(tok) = toks.next() {
        let Some(rest) = tok.strip_prefix("preset.") else { continue };
        let id = match rest {
            "fire" | "toggle" | "release" => match toks.next() {
                Some(n) => n.strip_prefix("name=").unwrap_or(n),
                None => continue,
            },
            r => r.split('.').next().unwrap_or(""),
        };
        if valid_id(id) {
            out.push((id.to_string(), ctx.to_string()));
        }
    }
}

/// Every project file that uses a quick effect: preset → `[{kind, file, file_label, name}]`
/// (`kind` = the file's folder: rules, commands, controllers, timelines, rewards, presets, …).
fn used_by(root: &Path, mut paths: Vec<String>) -> BTreeMap<String, Value> {
    paths.sort();
    paths.dedup();
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for rel in &paths {
        let Ok(text) = std::fs::read_to_string(root.join(rel)) else { continue };
        let Ok(t) = text.parse::<toml::Table>() else { continue };
        let kind = rel.split('/').next().unwrap_or("");
        let file_label = match str_of(&t, "kind") {
            Some("deck") if kind == "controllers" => "Stream Deck".to_string(),
            _ => entry_name(&t, "").unwrap_or_default(),
        };
        for (id, name) in preset_refs(&t) {
            if kind == "presets" && Path::new(rel).file_stem().and_then(|s| s.to_str()) == Some(id.as_str()) {
                continue;
            }
            let file_label = if name == file_label { String::new() } else { file_label.clone() };
            out.entry(id).or_default().push(Value::map().with("kind", kind).with("file", rel.clone()).with("file_label", file_label).with("name", name));
        }
    }
    out.into_iter().map(|(k, v)| (k, Value::List(v))).collect()
}

/// What a quick effect does, for its one-line description and its "What it does" words: the
/// screen effects and overlays it fires, the light look or cue list, the sound, other things it
/// runs (sound effects on a channel, a sound mix, a scene, a mode) and the settings it holds.
fn summary(p: &se_core::config::PresetDef) -> Value {
    let strs = |v: Vec<String>| Value::List(v.into_iter().map(Value::Str).collect());
    let effects = p.fx.iter().filter(|f| !f.name.contains('.')).map(|f| f.name.clone()).collect();
    let overlays = p.fx.iter().filter_map(|f| f.name.strip_prefix("patch.")).map(String::from).collect();
    let triggers = p.fx.iter().filter(|f| f.name.contains('.') && !f.name.starts_with("patch.")).map(|f| f.name.clone()).collect();
    let (lights, lights_name) = match &p.lights {
        Some(l) if l.look.is_some() => ("look", l.look.clone().unwrap_or_default()),
        Some(l) => ("cuelist", l.cuelist.clone().or_else(|| l.cue.clone()).unwrap_or_default()),
        None => ("", String::new()),
    };
    Value::map()
        .with("effects", strs(effects))
        .with("overlays", strs(overlays))
        .with("triggers", strs(triggers))
        .with("lights", lights)
        .with("lights_name", lights_name)
        .with("sound", p.sound.clone().map(Value::Str).unwrap_or_default())
        .with("mix", p.mix.as_ref().map(|m| Value::Str(m.snapshot.clone())).unwrap_or_default())
        .with("scene", p.scene.clone().map(Value::Str).unwrap_or_default())
        .with("mode", p.mode.clone().map(Value::Str).unwrap_or_default())
        .with("hold_ms", p.hold.map(|h| Value::Int(h.ms() as i64)).unwrap_or_default())
        .with("toggle", p.toggle)
        .with("settings", p.set.len() as i64)
        .with("set", strs(p.set.keys().cloned().collect()))
        .with("steps", (p.commands.len() + p.on_release.len()) as i64)
}

/// A quick effect's file with a knob's value written where firing picks it up (its `set` entry
/// or its effect entry's key, see [`KnobSlot`]), comments and layout kept.
pub fn write_knob(text: &str, target: &str, v: &Value) -> Result<String, String> {
    let def: PresetDef = toml::from_str(text).map_err(|e| format!("the file isn't readable: {}", e.message()))?;
    let slot = def.knobs.iter().any(|k| k.target == target).then(|| def.knob_slot(target)).flatten().ok_or_else(|| format!("no knob changes `{target}`"))?;
    let nv = se_store::project::to_edit_value(v).ok_or("a knob needs a value")?;
    let mut doc: DocumentMut = text.parse().map_err(|e| format!("the file isn't readable: {e}"))?;
    let missing = || format!("can't find where `{target}` is kept");
    match &slot {
        KnobSlot::Set => put(doc.get_mut("set").and_then(Item::as_table_like_mut).ok_or_else(missing)?, target, nv),
        KnobSlot::Fx { index, key } => match doc.get_mut("fx") {
            Some(Item::ArrayOfTables(a)) => put(a.get_mut(*index).ok_or_else(missing)?, key, nv),
            Some(Item::Value(toml_edit::Value::Array(a))) => {
                let t = a.get_mut(*index).and_then(toml_edit::Value::as_inline_table_mut).ok_or_else(missing)?;
                if t.contains_key(key) {
                    put(t, key, nv);
                } else {
                    // a new key: lay the one-line table out again (`{ name = "shake", strength = 0.7 }`)
                    t.insert(key, nv);
                    t.fmt();
                }
            }
            _ => return Err(missing()),
        },
    }
    Ok(doc.to_string())
}

/// Set `key`, keeping the old value's comment and spacing.
fn put(table: &mut dyn TableLike, key: &str, nv: toml_edit::Value) {
    match table.get_mut(key) {
        Some(Item::Value(old)) => {
            let decor = old.decor().clone();
            *old = nv;
            *old.decor_mut() = decor;
        }
        _ => {
            table.insert(key, Item::Value(nv));
        }
    }
}

/// `preset.knob {name, target, value}` after the core took it: write the value into
/// `presets/<name>.toml` (the core already fitted it to the knob and updated what's running).
fn save_knob(ctx: &Ctx, args: &Value) -> Result<Option<String>, String> {
    let name = args.get_path("name").and_then(Value::as_str).filter(|n| valid_id(n)).ok_or("needs a quick effect's `name`")?;
    let target = args.get_path("target").and_then(Value::as_str).ok_or("needs the knob's `target`")?;
    let value = args.get_path("value").ok_or("needs a `value`")?;
    let rel = format!("presets/{name}.toml");
    let text = std::fs::read_to_string(ctx.project.root().join(&rel)).map_err(|e| format!("{rel}: {e}"))?;
    let next = write_knob(&text, target, value).map_err(|e| format!("{rel}: {e}"))?;
    if next == text {
        return Ok(None);
    }
    ctx.project.write_file(&rel, &next).map_err(|e| format!("{rel}: {e:#}"))?;
    Ok(Some(rel))
}

pub fn register(ctx: &Ctx) {
    let root = ctx.project.root().to_path_buf();
    let config = ctx.config.clone();
    ctx.hub.register_query(
        "presets.catalog",
        Arc::new(move |_, _| {
            let root = root.clone();
            // take what's needed under the lock; file reads happen after
            let (looks, cuelists, named_sounds, files, presets) = {
                let cfg = config.lock();
                (
                    labelled(&cfg, "lights/palettes"),
                    labelled(&cfg, "lights/cuelists"),
                    configured_sounds(&cfg),
                    cfg.files.values().cloned().collect::<Vec<String>>(),
                    cfg.presets.iter().map(|(n, p)| (n.clone(), summary(p))).collect::<BTreeMap<String, Value>>(),
                )
            };
            Box::pin(async move {
                let effects: Vec<Value> = LIBRARY.iter().filter(|e| !NOT_QUICK.contains(&e.name)).map(effect).collect();
                Ok(Value::map()
                    .with("effects", Value::List(effects))
                    .with("overlays", Value::List(overlays(&root)))
                    .with("looks", Value::List(looks))
                    .with("cuelists", Value::List(cuelists))
                    .with("sounds", Value::List(sounds(&root, named_sounds)))
                    .with("presets", Value::Map(presets))
                    .with("used_by", Value::Map(used_by(&root, files))))
            })
        }),
    );
    let ctx = ctx.clone();
    let mut knobs = ctx.hub.route_actions("preset.knob");
    tokio::spawn(async move {
        while let Some(c) = knobs.recv().await {
            let se_proto::Op::Action { args, .. } = &c.op else { continue };
            match save_knob(&ctx, args) {
                Ok(Some(rel)) => crate::daemon::reload(&ctx, &[rel]),
                Ok(None) => {}
                Err(e) => ctx.hub.log("error", "presets", format!("preset.knob: {e}")),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use se_core::{Config, SourceFile};

    fn refs(src: &str) -> Vec<(String, String)> {
        preset_refs(&src.parse::<toml::Table>().unwrap())
    }

    fn preset(src: &str) -> Result<PresetDef, String> {
        let c = Config::build(&[SourceFile { kind: "presets".into(), name: "p".into(), path: "presets/p.toml".into(), table: src.parse().unwrap() }]);
        match c.errors.first() {
            Some(e) => Err(e.msg.clone()),
            None => Ok(c.presets["p"].clone()),
        }
    }

    #[test]
    fn finds_every_way_a_file_fires_a_quick_effect() {
        let rules = "[[rule]]\nname = \"big cheer\"\nwhen = \"twitch.cheer\"\ndo = [\"preset.fire hype\", \"bot.say 'thanks'\"]\n\
                     [[rule]]\nname = \"calm\"\nwhen = \"x\"\nif = \"preset.chill.active && mode == 'live'\"\ndo = [\"preset.release name=strobe\"]";
        assert_eq!(
            refs(rules),
            [("chill".into(), "calm".into()), ("hype".into(), "big cheer".into()), ("strobe".into(), "calm".into())] as [(String, String); 3]
        );
        let deck = "label = \"Main deck\"\n[[key]]\nkey = 3\npreset = \"confetti\"\n[[key]]\nkey = 4\npress = [\"preset.toggle chill; scene.take\"]";
        assert_eq!(refs(deck), [("chill".into(), "Main deck, key 5".into()), ("confetti".into(), "Main deck, key 4".into())] as [(String, String); 2]);
        let pages = "[page.show]\nlabel = \"SHOW\"\n[page.show.key.5]\npreset = \"hype\"\n[[map]]\ncontrol = \"btn.1\"\npreset = \"chill\"";
        assert_eq!(refs(pages), [("chill".into(), "btn 1".into()), ("hype".into(), "SHOW, key 6".into())] as [(String, String); 2]);
        assert_eq!(refs("title = \"Hype\"\nfires = \"preset.hype\""), [("hype".to_string(), "Hype".to_string())]);
        assert_eq!(refs("target = \"x\"\nscope = \"preset.hype\""), [("hype".to_string(), String::new())]);
        // other words that merely contain "preset" are not references
        assert!(refs("note = \"presets/hype.toml and preset.fire\"\nname = \"presetting\"").is_empty());
    }

    #[test]
    fn every_screen_effect_has_plain_words_and_the_green_screen_is_left_out() {
        let fx: Vec<Value> = LIBRARY.iter().filter(|e| !NOT_QUICK.contains(&e.name)).map(effect).collect();
        assert!(!fx.iter().any(|e| e.get_path("name").and_then(Value::as_str) == Some("chroma_key")));
        for e in LIBRARY.iter().filter(|e| !NOT_QUICK.contains(&e.name)) {
            assert!(WORDS.iter().any(|w| w.0 == e.name), "no plain words for effect `{}`", e.name);
        }
    }

    #[test]
    fn a_knob_is_written_where_firing_picks_it_up_and_comments_stay() {
        let knob = |t: &str| format!("\n[[knob]]\nlabel = \"K\"\ntarget = \"{t}\"\nmin = 0\nmax = 20\n");
        // `set` entry, a comment on the line and above
        let src =
            format!("# Strobe look.\nhold = \"3s\"\n# speed\nset = {{ \"lights.effect.strobe.rate\" = 2.5 }} # flashes\n{}", knob("lights.effect.strobe.rate"));
        let out = write_knob(&src, "lights.effect.strobe.rate", &Value::Float(1.5)).unwrap();
        assert!(out.starts_with("# Strobe look.\nhold = \"3s\"\n# speed\nset = { \"lights.effect.strobe.rate\" = 1.5 } # flashes\n"), "{out}");
        assert_eq!(preset(&out).unwrap().set["lights.effect.strobe.rate"], Value::Float(1.5));
        // a built-in effect's setting that isn't in its entry yet, and an overlay's burst size
        let src = format!(
            "fx = [{{ name = \"shake\" }}, {{ name = \"patch.confetti\", count = 160 }}] # both\n{}{}",
            knob("fx.shake.strength"),
            knob("patch.confetti.count")
        );
        let out = write_knob(&src, "fx.shake.strength", &Value::Float(0.7)).unwrap();
        let out = write_knob(&out, "patch.confetti.count", &Value::Int(300)).unwrap();
        assert!(out.starts_with("fx = [{ name = \"shake\", strength = 0.7 }, { name = \"patch.confetti\", count = 300 }] # both\n"), "{out}");
        let p = preset(&out).unwrap();
        assert_eq!(p.knob_value("fx.shake.strength"), Some(&Value::Float(0.7)));
        assert_eq!(p.fx[1].params["count"], Value::Int(300));
        // `[[fx]]` tables work the same
        let src = format!("[[fx]]\nname = \"vhs\"\nnoise = 0.2 # grain\n{}", knob("fx.vhs.noise"));
        let out = write_knob(&src, "fx.vhs.noise", &Value::Float(0.4)).unwrap();
        assert!(out.contains("noise = 0.4 # grain\n"), "{out}");
        // only knobs are written
        assert!(write_knob("set = { \"a.b\" = 1.0 }", "a.b", &Value::Float(2.0)).unwrap_err().contains("no knob"));
    }

    #[test]
    fn knobs_must_drive_something_the_quick_effect_changes() {
        let err = preset("set = { \"fx.vhs.amount\" = 0.5 }\n[[knob]]\nlabel = \"Warmth\"\ntarget = \"fx.grade.warmth\"\nmin = -1\nmax = 1").unwrap_err();
        assert!(err.contains("knob “Warmth”") && err.contains("doesn't change `fx.grade.warmth`"), "{err}");
        // timings of an effect entry aren't knobs
        assert!(preset("fx = [{ name = \"shake\" }]\n[[knob]]\nlabel = \"Length\"\ntarget = \"fx.shake.hold\"\nmin = 0\nmax = 9").is_err());
        let err = preset("set = { \"a.b\" = \"fast\" }\n[[knob]]\nlabel = \"Speed\"\ntarget = \"a.b\"\nmin = 0\nmax = 9").unwrap_err();
        assert!(err.contains("must be a number"), "{err}");
        let err = preset("set = { \"a.b\" = 1.0 }\n[[knob]]\nlabel = \"Speed\"\ntarget = \"a.b\"\nmin = 9\nmax = 1").unwrap_err();
        assert!(err.contains("`min` (9) must be smaller than `max` (1)"), "{err}");
    }
}
