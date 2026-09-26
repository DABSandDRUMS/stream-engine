//! Loading the lights show from the project config (`lights/rig.toml`, `lights/fixtures`,
//! `lights/palettes`, `lights/cuelists`, `lights/effects`), with last-good fallback per item,
//! and the state addresses it declares.

use crate::cuelist::{self, CueList};
use crate::effects::EffectDef;
use crate::palette::Palette;
use crate::rig::{AttrKind, Rig};
use se_proto::{Meta, Value, ValueType};
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone)]
pub struct Show {
    pub rig: Arc<Rig>,
    pub palettes: BTreeMap<String, Palette>,
    pub lists: BTreeMap<String, CueList>,
    pub effects: Vec<EffectDef>,
    /// Problems found while loading (shown in the Lights view and logged).
    pub errors: Vec<String>,
    /// `lights/rig.toml` exists.
    pub has_rig: bool,
}

impl Show {
    pub fn empty() -> Show {
        let mut e = Vec::new();
        let profiles = crate::profile::library(&BTreeMap::new(), &mut e);
        let rig = Rig::compile(&toml::Table::new(), profiles, e).expect("empty rig compiles");
        Show { rig: Arc::new(rig), palettes: BTreeMap::new(), lists: BTreeMap::new(), effects: Vec::new(), errors: Vec::new(), has_rig: false }
    }

    /// Build from the config; items that fail keep their last good version from `prev`.
    pub fn load(cfg: &se_core::Config, prev: &Show) -> Show {
        let kind = |k: &str| cfg.other.get(k).cloned().unwrap_or_default();
        let mut errors: Vec<String> = Vec::new();
        let fixtures = kind("lights/fixtures");
        let mut perr = Vec::new();
        let profiles = crate::profile::library(&fixtures, &mut perr);
        let rig_table = cfg.other.get("lights").and_then(|m| m.get("rig")).cloned();
        let has_rig = rig_table.is_some();
        let rig = match Rig::compile(&rig_table.unwrap_or_default(), profiles, perr) {
            Ok(r) => Arc::new(r),
            Err(e) => {
                errors.push(format!("lights/rig.toml: {e} (keeping the last good rig)"));
                prev.rig.clone()
            }
        };
        errors.extend(rig.errors.iter().cloned());
        let mut palettes = BTreeMap::new();
        for (name, t) in kind("lights/palettes") {
            match Palette::parse(&name, &t) {
                Ok(p) => {
                    errors.extend(p.validate(&rig));
                    palettes.insert(name, p);
                }
                Err(e) => {
                    errors.push(format!("lights/palettes/{name}.toml: {e}"));
                    if let Some(p) = prev.palettes.get(&name) {
                        palettes.insert(name, p.clone());
                    }
                }
            }
        }
        let mut effects = Vec::new();
        for (name, t) in kind("lights/effects") {
            if !se_proto::address::is_valid(&name, false) || name.contains('.') {
                errors.push(format!("lights/effects/{name}.toml: bad effect name"));
                continue;
            }
            match EffectDef::parse(&name, &t) {
                Ok(e) => effects.push(e),
                Err(e) => {
                    errors.push(format!("lights/effects/{name}.toml: {e}"));
                    if let Some(p) = prev.effects.iter().find(|x| x.name == name) {
                        effects.push(p.clone());
                    }
                }
            }
        }
        let effect_names: Vec<String> = effects.iter().map(|e| e.name.clone()).collect();
        let mut lists = BTreeMap::new();
        for (name, t) in kind("lights/cuelists") {
            if !se_proto::address::is_valid(&name, false) || name.contains('.') {
                errors.push(format!("lights/cuelists/{name}.toml: bad cue list name"));
                continue;
            }
            match CueList::parse(&name, &t) {
                Ok(l) => {
                    errors.extend(cuelist::validate(&l, &rig, &palettes, &effect_names));
                    lists.insert(name, l);
                }
                Err(e) => {
                    errors.push(format!("lights/cuelists/{name}.toml: {e}"));
                    if let Some(p) = prev.lists.get(&name) {
                        lists.insert(name, p.clone());
                    }
                }
            }
        }
        Show { rig, palettes, lists, effects, errors, has_rig }
    }

    /// Every address this show declares, with metadata.
    pub fn declarations(&self) -> Vec<(String, Meta)> {
        let mut out = Vec::new();
        let rig = &self.rig;
        for h in &rig.heads {
            for a in &h.attrs {
                out.push((h.addr(&a.name), a.meta(&format!("{} ({})", h.id, rig.fixtures[h.fixture].label))));
            }
        }
        for g in &rig.groups {
            for a in &g.attrs {
                let m = match a.kind {
                    AttrKind::Intensity => Meta::float(0.0, [0.0, 1.0]).htp(),
                    AttrKind::Color => Meta { ty: ValueType::Color, default: Value::Null, ..Default::default() },
                    AttrKind::Index => Meta { ty: ValueType::Int, range: Some([0.0, (a.slots.max(1) - 1) as f64]), default: Value::Null, ..Default::default() },
                    _ => Meta { ty: ValueType::Float, range: Some([0.0, 1.0]), default: Value::Null, ..Default::default() },
                };
                out.push((
                    format!("lights.group.{}.{}", g.name, a.name),
                    m.owner("lights").describe(&format!("group {} {} (unset = members keep their own)", g.name, a.name)),
                ));
            }
            out.push((
                format!("lights.group.{}.master", g.name),
                Meta::float(1.0, [0.0, 1.0]).owner("lights").describe(&format!("group {} submaster", g.name)),
            ));
        }
        out.push(("lights.master".into(), Meta::float(1.0, [0.0, 1.0]).owner("lights").describe("grand master")));
        out.push(("lights.blackout".into(), Meta::boolean(false).owner("lights").describe("blackout (grand master off)")));
        for (name, l) in &self.lists {
            let p = format!("lights.cuelist.{name}");
            out.push((format!("{p}.cue"), Meta::string("").readonly().owner("lights").describe(&format!("{}: current cue", l.label))));
            out.push((format!("{p}.next"), Meta::string("").readonly().owner("lights").describe(&format!("{}: next cue", l.label))));
            out.push((format!("{p}.playing"), Meta::boolean(false).readonly().owner("lights").describe(&format!("{}: running", l.label))));
            out.push((format!("{p}.master"), Meta::float(1.0, [0.0, 1.0]).owner("lights").describe(&format!("{}: playback master", l.label))));
        }
        for e in &self.effects {
            let p = format!("lights.effect.{}", e.name);
            out.push((format!("{p}.active"), Meta::boolean(false).owner("lights").describe(&format!("effect {}", e.label))));
            let unit = if e.unit == crate::effects::Unit::Beats { "beats/cycle" } else { "Hz" };
            out.push((format!("{p}.rate"), Meta::float(e.rate as f64, [0.01, 64.0]).unit(unit).owner("lights").describe(&format!("effect {} rate", e.label))));
            out.push((format!("{p}.size"), Meta::float(e.size as f64, [0.0, 1.0]).owner("lights").describe(&format!("effect {} size", e.label))));
        }
        out.push((
            "lights.programmer.selection".into(),
            Meta { ty: ValueType::List, default: Value::List(vec![]), readonly: true, ..Default::default() }.owner("lights"),
        ));
        out.push(("lights.programmer.highlight".into(), Meta::boolean(false).readonly().owner("lights")));
        out.push(("lights.programmer.active".into(), Meta::boolean(false).readonly().owner("lights").describe("programmer holds values")));
        out.push(("lights.output.fps".into(), Meta::float(0.0, [0.0, 100.0]).readonly().unit("Hz").owner("lights")));
        out.push((
            "lights.output.jitter_ms".into(),
            Meta::float(0.0, [0.0, 1000.0]).readonly().unit("ms").owner("lights").describe("DMX frame interval jitter p99"),
        ));
        out.push(("lights.output.frames".into(), Meta::int(0, [0.0, 1e15]).readonly().owner("lights")));
        out.push(("lights.output.limited".into(), Meta::int(0, [0.0, 1e6]).readonly().owner("lights").describe("heads clamped by the flash limiter")));
        out
    }

    /// Address prefixes owned per item (stale ones are removed on reload).
    pub fn prefixes(&self) -> Vec<String> {
        let mut v: Vec<String> = self.rig.heads.iter().map(|h| format!("lights.{}", h.id)).collect();
        v.extend(self.rig.groups.iter().map(|g| format!("lights.group.{}", g.name)));
        v.extend(self.lists.keys().map(|n| format!("lights.cuelist.{n}")));
        v.extend(self.effects.iter().map(|e| format!("lights.effect.{}", e.name)));
        v
    }

    /// Which cues use a palette (`list/cue`).
    pub fn palette_users(&self, palette: &str) -> Vec<String> {
        let mut v = Vec::new();
        for (n, l) in &self.lists {
            for c in &l.cues {
                if c.palettes().iter().any(|p| p == palette) {
                    v.push(format!("{n}/{}", c.id));
                }
            }
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starter_preserves_setup_without_programming_lights() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../project-example");
        let project = se_store::Project::open(&root).unwrap();
        let loaded = project.load();
        let cfg = se_core::Config::build(&loaded.files);
        let show = Show::load(&cfg, &Show::empty());
        assert!(show.errors.is_empty(), "{:#?}", show.errors);
        assert!(show.rig.fixtures.is_empty(), "setup must not patch imaginary fixtures");
        assert!(show.palettes.is_empty() && show.lists.is_empty() && show.effects.is_empty(), "lighting programming is opt-in");
        let declarations = show.declarations();
        assert!(declarations.iter().any(|(a, _)| a == "lights.blackout"), "blank projects retain safety controls");
        assert!(declarations.iter().any(|(a, _)| a == "lights.master"), "blank projects retain master control");
    }
}
