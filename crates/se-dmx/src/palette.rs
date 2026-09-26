//! Palettes (§9.2) and value references used by cues and palettes:
//! literals, `palette:<name>`, `stream:<slot>` (the stream palette `palette.<slot>`, §16.2),
//! and `@<address>` (any live state value).

use crate::rig::{AttrKind, Rig};
use se_proto::Value;
use serde::Deserialize;
use std::collections::BTreeMap;

/// Stream palette slots published by the renderer as `palette.<slot>`.
pub const STREAM_SLOTS: &[&str] = &["accent", "background", "foreground", "red", "yellow", "green", "cyan", "magenta"];

/// Attribute groups a palette reference can expand to.
pub fn attr_group(name: &str) -> Option<&'static [&'static str]> {
    match name {
        "position" => Some(&["pan", "tilt"]),
        "beam" => Some(&["zoom", "focus", "iris", "frost", "prism", "gobo", "gobo_rotate"]),
        _ => None,
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Spec {
    Literal(Value),
    Palette(String),
    /// Live state address (`stream:<slot>` = `palette.<slot>`).
    Address(String),
}

impl Spec {
    pub fn parse(v: &toml::Value) -> Result<Spec, String> {
        Ok(match v {
            toml::Value::String(s) => {
                if let Some(p) = s.strip_prefix("palette:") {
                    Spec::Palette(p.to_string())
                } else if let Some(slot) = s.strip_prefix("stream:") {
                    if !STREAM_SLOTS.contains(&slot) {
                        return Err(format!("unknown stream palette slot `{slot}` ({})", STREAM_SLOTS.join(", ")));
                    }
                    Spec::Address(format!("palette.{slot}"))
                } else if let Some(a) = s.strip_prefix('@') {
                    if !se_proto::address::is_valid(a, false) {
                        return Err(format!("bad address `{a}`"));
                    }
                    Spec::Address(a.to_string())
                } else if s.starts_with('#') {
                    let c = se_proto::value::parse_hex_color(s).ok_or_else(|| format!("bad colour `{s}`"))?;
                    Spec::Literal(Value::from(c))
                } else {
                    Spec::Literal(Value::Str(s.clone()))
                }
            }
            toml::Value::Integer(i) => Spec::Literal(Value::Float(*i as f64)),
            toml::Value::Float(f) => Spec::Literal(Value::Float(*f)),
            toml::Value::Boolean(b) => Spec::Literal(Value::Bool(*b)),
            toml::Value::Array(a) => {
                let v = Value::from(toml::Value::Array(a.clone()));
                let c = v.as_color().ok_or("arrays must be colours [r, g, b] or [r, g, b, a]")?;
                Spec::Literal(Value::from(c))
            }
            other => return Err(format!("unsupported value `{other}`")),
        })
    }

    /// Parse from an API/command value.
    pub fn from_value(v: &Value) -> Result<Spec, String> {
        let t = se_proto::value::to_toml(v).ok_or("unsupported value")?;
        Spec::parse(&t)
    }

    /// The live address this value follows, if any (after palette indirection).
    pub fn live<'a>(&'a self, palettes: &'a BTreeMap<String, Palette>, rig: &Rig, head: usize, attr: &str) -> Option<&'a str> {
        match self {
            Spec::Address(a) => Some(a),
            Spec::Palette(p) => match palettes.get(p)?.value_for(rig, head, attr)? {
                Spec::Address(a) => Some(a),
                _ => None,
            },
            Spec::Literal(_) => None,
        }
    }
}

/// Coerce a value for an attribute kind: colours → `[r,g,b,1]`, numbers clamped.
pub fn coerce(kind: AttrKind, v: &Value) -> Option<Value> {
    match kind {
        AttrKind::Color => v.as_color().map(|c| Value::from([c[0].clamp(0.0, 1.0), c[1].clamp(0.0, 1.0), c[2].clamp(0.0, 1.0), 1.0])),
        AttrKind::Intensity | AttrKind::Unit => v.as_f64().map(|x| Value::Float(x.clamp(0.0, 1.0))),
        AttrKind::Index => v.as_f64().map(|x| Value::Int(x.round().max(0.0) as i64)),
        AttrKind::Raw => v.as_f64().map(|x| Value::Int(x.round().clamp(0.0, 255.0) as i64)),
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Palette {
    pub name: String,
    pub label: String,
    pub kind: String,
    /// target → attribute → value (no nested `palette:` references).
    pub set: Vec<(String, Vec<(String, Spec)>)>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    kind: Option<String>,
    label: Option<String>,
    #[serde(default)]
    set: toml::map::Map<String, toml::Value>,
    #[serde(default)]
    notes: Option<String>,
}

impl Palette {
    pub fn parse(name: &str, t: &toml::Table) -> Result<Palette, String> {
        let r: Raw = toml::Value::Table(t.clone()).try_into().map_err(|e: toml::de::Error| e.message().to_string())?;
        let _ = r.notes;
        let kind = r.kind.unwrap_or_else(|| "any".into());
        if !["color", "position", "beam", "intensity", "any"].contains(&kind.as_str()) {
            return Err(format!("unknown palette kind `{kind}`"));
        }
        let mut set = Vec::new();
        for (target, attrs) in r.set {
            let attrs = attrs.as_table().ok_or_else(|| format!("`set.{target}` must be a table of attributes"))?;
            let mut out = Vec::new();
            for (a, v) in attrs {
                let spec = Spec::parse(v).map_err(|e| format!("set.{target}.{a}: {e}"))?;
                if matches!(spec, Spec::Palette(_)) {
                    return Err(format!("set.{target}.{a}: palettes cannot reference other palettes"));
                }
                if attr_group(a).is_some() {
                    return Err(format!("set.{target}.{a}: write the attributes (e.g. pan, tilt) explicitly"));
                }
                out.push((a.clone(), spec));
            }
            set.push((target, out));
        }
        Ok(Palette { name: name.into(), label: r.label.unwrap_or_else(|| name.to_string()), kind, set })
    }

    /// Attributes this palette defines anywhere.
    pub fn attrs(&self) -> Vec<&str> {
        let mut v: Vec<&str> = Vec::new();
        for (_, attrs) in &self.set {
            for (a, _) in attrs {
                if !v.contains(&a.as_str()) {
                    v.push(a);
                }
            }
        }
        v
    }

    /// The most specific entry covering `head` for `attr` (fixture > smaller group > larger
    /// group > `all`).
    pub fn value_for(&self, rig: &Rig, head: usize, attr: &str) -> Option<&Spec> {
        let mut best: Option<(usize, &Spec)> = None;
        for (target, attrs) in &self.set {
            let Some((_, spec)) = attrs.iter().find(|(a, _)| a == attr) else { continue };
            if !covers(rig, target, head) {
                continue;
            }
            let s = rig.specificity(target);
            if best.is_none_or(|(b, _)| s < b) {
                best = Some((s, spec));
            }
        }
        best.map(|(_, s)| s)
    }

    /// Check targets against the rig. Attributes no patched fixture has are fine (a position
    /// palette stays valid while no movers are patched).
    pub fn validate(&self, rig: &Rig) -> Vec<String> {
        self.set
            .iter()
            .filter(|(t, _)| t != "all" && rig.group(t).is_none() && rig.head(t).is_none())
            .map(|(t, _)| format!("palette `{}`: unknown fixture or group `{t}`", self.name))
            .collect()
    }
}

/// Does `target` (fixture, cell, group, `all`) include `head`?
pub fn covers(rig: &Rig, target: &str, head: usize) -> bool {
    let h = &rig.heads[head];
    if target == h.id || h.parent.is_some_and(|p| rig.heads[p].id == target) {
        return true;
    }
    match rig.group(target) {
        Some(g) => {
            let g = &rig.groups[g];
            g.roots.contains(&head) || g.leaves.contains(&head) || h.parent.is_some_and(|p| g.roots.contains(&p))
        }
        None => false,
    }
}

/// Resolve a value for `head.attr` now: palette indirection, then live addresses via `get`.
pub fn resolve(
    spec: &Spec,
    palettes: &BTreeMap<String, Palette>,
    rig: &Rig,
    head: usize,
    attr: &str,
    kind: AttrKind,
    get: &dyn Fn(&str) -> Option<Value>,
) -> Result<Value, String> {
    let s = match spec {
        Spec::Palette(p) => {
            let pal = palettes.get(p).ok_or_else(|| format!("unknown palette `{p}`"))?;
            pal.value_for(rig, head, attr).ok_or_else(|| format!("palette `{p}` has no `{attr}` for `{}`", rig.heads[head].id))?
        }
        other => other,
    };
    let v = match s {
        Spec::Literal(v) => v.clone(),
        Spec::Address(a) => get(a).ok_or_else(|| format!("`{a}` has no value"))?,
        Spec::Palette(_) => unreachable!("palettes cannot nest"),
    };
    coerce(kind, &v).ok_or_else(|| format!("`{v}` is not a valid {attr}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::rig;

    #[test]
    fn specificity_and_references() {
        let r = rig(
            "[fixtures.a]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\n[fixtures.b]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 4\n[groups]\nfront = [\"a\", \"b\"]\nsolo = [\"b\"]",
        );
        let p = Palette::parse(
            "warm",
            &toml::from_str("kind = \"color\"\n[set]\nall = { color = \"#ff0000\" }\nfront = { color = \"#00ff00\" }\nsolo = { color = \"stream:accent\" }")
                .unwrap(),
        )
        .unwrap();
        let a = r.head("a").unwrap();
        let b = r.head("b").unwrap();
        assert_eq!(p.value_for(&r, a, "color"), Some(&Spec::Literal(Value::from([0.0f32, 1.0, 0.0, 1.0]))));
        assert_eq!(p.value_for(&r, b, "color"), Some(&Spec::Address("palette.accent".into())));
        let mut pals = BTreeMap::new();
        pals.insert("warm".to_string(), p);
        let get = |a: &str| (a == "palette.accent").then(|| Value::from([0.1f32, 0.2, 0.3, 1.0]));
        let v = resolve(&Spec::Palette("warm".into()), &pals, &r, b, "color", AttrKind::Color, &get).unwrap();
        assert_eq!(v, Value::from([0.1f32, 0.2, 0.3, 1.0]));
        assert_eq!(Spec::Palette("warm".into()).live(&pals, &r, b, "color"), Some("palette.accent"));
        assert!(resolve(&Spec::Palette("nope".into()), &pals, &r, b, "color", AttrKind::Color, &get).is_err());
        assert!(Spec::parse(&toml::Value::String("stream:purple".into())).is_err());
        assert!(pals["warm"].validate(&r).is_empty());
    }
}
