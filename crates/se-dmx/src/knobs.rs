//! Knobs on light looks (palettes) and cue lists: the few premade controls a look or cue list
//! exposes to the operator ([`se_core::knob::Knob`], `[[knob]]` tables in its file). A knob's
//! `target` is where its value lives in that same file:
//!
//! - look: `set.<fixture | group | all>.<attribute>`, e.g. `set.all.color`
//! - cue list: `cue.<id>.set.<fixture | group | all>.<attribute>` (a value a cue sets; for a
//!   timed value `{ value, fade, delay }` the knob changes its `value`), or
//!   `cue.<id>.effects.<effect>.rate` / `.size` (what the cue starts that effect with; unset =
//!   the effect's own rate / size)
//!
//! Only plain values can have a knob, not `palette:`, `stream:` or `@` references. Moving a
//! knob (`lights.knob`) changes the loaded look / cue list at once (running ones follow) and
//! writes the value into the file with `toml_edit` (comments and layout kept), where the next
//! reload and every later firing pick it up.

use crate::cuelist::CueList;
use crate::effects::EffectDef;
use crate::palette::{Palette, Spec};
use crate::rig::{AttrKind, Rig};
use se_core::knob::{Knob, KnobKind};
use se_proto::Value;

/// Rate or size of an effect a cue starts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Param {
    Rate,
    Size,
}

/// Where a knob's value lives.
#[derive(Clone, Debug, PartialEq)]
pub enum At {
    /// A value in a look's `[set]`.
    Look { target: String, attr: String },
    /// A value cue `cue` (index) sets.
    CueSet { cue: usize, target: String, attr: String },
    /// The rate / size cue `cue` (index) starts `effect` with.
    CueEffect { cue: usize, effect: String, param: Param },
}

/// A declared knob and where its value lives.
#[derive(Clone, Debug, PartialEq)]
pub struct Bound {
    pub knob: Knob,
    pub at: At,
}

/// `"#rrggbb"` of a colour value.
fn hex(c: [f32; 4]) -> String {
    let b = |x: f32| (x.clamp(0.0, 1.0) * 255.0).round() as u8;
    format!("#{:02x}{:02x}{:02x}", b(c[0]), b(c[1]), b(c[2]))
}

/// A knob value as the queries show it: colours `"#rrggbb"`, numbers as numbers.
fn shown(v: &Value) -> Value {
    match v {
        Value::List(_) => v.as_color().map(|c| Value::Str(hex(c))).unwrap_or_default(),
        other => other.clone(),
    }
}

/// A fitted knob value as the spec a look / cue holds.
fn spec_of(v: &Value) -> Spec {
    match v.as_str().and_then(se_proto::value::parse_hex_color) {
        Some(c) => Spec::Literal(Value::from(c)),
        None => Spec::Literal(Value::Float(v.as_f64().unwrap_or(0.0))),
    }
}

fn is_number(v: &Value) -> bool {
    !matches!(v, Value::Bool(_)) && v.as_f64().is_some()
}

/// The knob's kind suits a colour (`color`) or a number value.
fn kind_fits(k: &Knob, color: bool) -> Result<(), String> {
    let name = &k.label;
    match (k.kind, color) {
        (KnobKind::Color, true) | (KnobKind::Number, false) => Ok(()),
        (KnobKind::Choice, true) if k.options.iter().all(|o| o.value.as_str().is_some_and(se_core::knob::is_hex_color)) => Ok(()),
        (KnobKind::Choice, false) if k.options.iter().all(|o| is_number(&o.value)) => Ok(()),
        (KnobKind::Choice, true) => Err(format!("knob “{name}”: `{}` is a colour, so every option's value is a colour like \"#ff8800\"", k.target)),
        (KnobKind::Choice, false) => Err(format!("knob “{name}”: `{}` is a number, so every option's value is a number", k.target)),
        (_, true) => Err(format!("knob “{name}”: `{}` is a colour, so the knob needs kind = \"color\"", k.target)),
        (_, false) => Err(format!("knob “{name}”: `{}` is a number, not a colour", k.target)),
    }
}

/// The literal behind a knob, or why it can't have one.
fn literal<'a>(k: &Knob, spec: &'a Spec) -> Result<&'a Value, String> {
    match spec {
        Spec::Literal(v) => Ok(v),
        _ => Err(format!(
            "knob “{}”: `{}` follows another value (a look, the stream colours or a live value); only plain values can have a knob",
            k.label, k.target
        )),
    }
}

/// Resolve and check a look's knob.
pub fn look_at(p: &Palette, k: &Knob) -> Result<At, String> {
    let name = &k.label;
    let have = || p.set.iter().flat_map(|(t, a)| a.iter().map(move |(x, _)| format!("set.{t}.{x}"))).collect::<Vec<_>>().join(", ");
    let (target, attr) = k
        .target
        .strip_prefix("set.")
        .and_then(|r| r.rsplit_once('.'))
        .ok_or_else(|| format!("knob “{name}”: `target` says which value of this look it changes, like `set.all.color` (this look has {})", have()))?;
    let spec = p
        .set
        .iter()
        .find(|(t, _)| t == target)
        .and_then(|(_, a)| a.iter().find(|(x, _)| x == attr))
        .map(|(_, s)| s)
        .ok_or_else(|| format!("knob “{name}”: this look has no `{}` (it has {})", k.target, have()))?;
    let v = literal(k, spec)?;
    kind_fits(k, matches!(v, Value::List(_)))?;
    Ok(At::Look { target: target.into(), attr: attr.into() })
}

/// Resolve and check a cue list's knob.
pub fn cue_at(l: &CueList, k: &Knob) -> Result<At, String> {
    let name = &k.label;
    let ids = || l.cues.iter().map(|c| c.id.as_str()).collect::<Vec<_>>().join(", ");
    let rest = k.target.strip_prefix("cue.").ok_or_else(|| {
        format!("knob “{name}”: `target` starts with the cue, like `cue.1.effects.chase.rate` or `cue.1.set.all.intensity` (its cues: {})", ids())
    })?;
    let (ci, rest) = l
        .cues
        .iter()
        .enumerate()
        .filter_map(|(i, c)| rest.strip_prefix(c.id.as_str()).and_then(|r| r.strip_prefix('.')).map(|r| (i, c.id.len(), r)))
        .max_by_key(|(_, len, _)| *len)
        .map(|(i, _, r)| (i, r))
        .ok_or_else(|| format!("knob “{name}”: `{}` names no cue of this list (its cues: {})", k.target, ids()))?;
    let cue = &l.cues[ci];
    if let Some(r) = rest.strip_prefix("set.") {
        let (target, attr) =
            r.rsplit_once('.').ok_or_else(|| format!("knob “{name}”: `{}` needs the lights and the attribute, like `set.all.intensity`", k.target))?;
        let entry = cue
            .set
            .iter()
            .find(|(t, _, _)| t == target)
            .and_then(|(_, a, _)| a.iter().find(|(x, _)| x == attr))
            .map(|(_, e)| e)
            .ok_or_else(|| format!("knob “{name}”: cue `{}` sets no `{target}.{attr}`", cue.id))?;
        let v = literal(k, &entry.spec)?;
        kind_fits(k, matches!(v, Value::List(_)))?;
        return Ok(At::CueSet { cue: ci, target: target.into(), attr: attr.into() });
    }
    if let Some(r) = rest.strip_prefix("effects.") {
        let (effect, param) = r.rsplit_once('.').unwrap_or((r, ""));
        let param = match param {
            "rate" => Param::Rate,
            "size" => Param::Size,
            _ => return Err(format!("knob “{name}”: an effect knob changes its `rate` or `size`, like `cue.{}.effects.{effect}.rate`", cue.id)),
        };
        if !cue.effects.iter().any(|(e, _, _)| e == effect) {
            return Err(format!("knob “{name}”: cue `{}` doesn't start the effect `{effect}`", cue.id));
        }
        kind_fits(k, false)?;
        let (lo, hi) = range(k);
        match param {
            Param::Size if lo < 0.0 || hi > 1.0 => return Err(format!("knob “{name}”: an effect's size runs from 0 to 1")),
            Param::Rate if lo <= 0.0 => return Err(format!("knob “{name}”: an effect's rate must stay above 0")),
            _ => {}
        }
        return Ok(At::CueEffect { cue: ci, effect: effect.into(), param });
    }
    Err(format!("knob “{name}”: after the cue, `target` goes on with `set.…` or `effects.…` (not `{}`)", k.target))
}

/// Lowest and highest value the knob can send.
fn range(k: &Knob) -> (f64, f64) {
    match k.kind {
        KnobKind::Choice => {
            let vals = k.options.iter().filter_map(|o| o.value.as_f64());
            vals.clone().fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| (lo.min(v), hi.max(v)))
        }
        _ => (k.min.unwrap_or(0.0), k.max.unwrap_or(0.0)),
    }
}

/// Knobs on 0–1 attributes (brightness, white, zoom, …) stay inside 0–1.
pub fn check_attr_range(rig: &Rig, k: &Knob, target: &str, attr: &str) -> Option<String> {
    let h = rig.heads_for(target, attr).unwrap_or_default().into_iter().next()?;
    let kind = rig.heads[h].attr(attr)?.kind;
    let (lo, hi) = range(k);
    (matches!(kind, AttrKind::Intensity | AttrKind::Unit) && k.kind != KnobKind::Color && (lo < 0.0 || hi > 1.0))
        .then(|| format!("knob “{}”: `{attr}` runs from 0 to 1 (write min = 0.0, max = 1.0, unit = \"%\")", k.label))
}

/// Current value of a look's knob (`"#rrggbb"` or a number).
pub fn look_value(p: &Palette, at: &At) -> Value {
    let At::Look { target, attr } = at else { return Value::Null };
    match p.set.iter().find(|(t, _)| t == target).and_then(|(_, a)| a.iter().find(|(x, _)| x == attr)) {
        Some((_, Spec::Literal(v))) => shown(v),
        _ => Value::Null,
    }
}

/// Current value of a cue list's knob; an effect rate / size the cue doesn't set is the
/// effect's own.
pub fn cue_value(l: &CueList, at: &At, effects: &[EffectDef]) -> Value {
    match at {
        At::CueSet { cue, target, attr } => {
            match l.cues.get(*cue).and_then(|c| c.set.iter().find(|(t, _, _)| t == target)).and_then(|(_, a, _)| a.iter().find(|(x, _)| x == attr)) {
                Some((_, e)) => match &e.spec {
                    Spec::Literal(v) => shown(v),
                    _ => Value::Null,
                },
                None => Value::Null,
            }
        }
        At::CueEffect { cue, effect, param } => {
            let set = l.cues.get(*cue).and_then(|c| c.effects.iter().find(|(e, _, _)| e == effect)).and_then(|(_, size, rate)| match param {
                Param::Rate => *rate,
                Param::Size => *size,
            });
            let own = || {
                effects.iter().find(|e| &e.name == effect).map(|e| match param {
                    Param::Rate => e.rate,
                    Param::Size => e.size,
                })
            };
            set.or_else(own).map(|x| Value::Float((x as f64 * 1e4).round() / 1e4)).unwrap_or_default()
        }
        At::Look { .. } => Value::Null,
    }
}

/// Put a fitted knob value into a loaded look.
pub fn set_look(p: &mut Palette, at: &At, v: &Value) {
    let At::Look { target, attr } = at else { return };
    if let Some((_, spec)) = p.set.iter_mut().find(|(t, _)| t == target).and_then(|(_, a)| a.iter_mut().find(|(x, _)| x == attr)) {
        *spec = spec_of(v);
    }
}

/// Put a fitted knob value into a loaded cue list.
pub fn set_cue(l: &mut CueList, at: &At, v: &Value) {
    match at {
        At::CueSet { cue, target, attr } => {
            if let Some((_, e)) =
                l.cues.get_mut(*cue).and_then(|c| c.set.iter_mut().find(|(t, _, _)| t == target)).and_then(|(_, a, _)| a.iter_mut().find(|(x, _)| x == attr))
            {
                e.spec = spec_of(v);
            }
        }
        At::CueEffect { cue, effect, param } => {
            if let Some((_, size, rate)) = l.cues.get_mut(*cue).and_then(|c| c.effects.iter_mut().find(|(e, _, _)| e == effect)) {
                let x = v.as_f64().map(|x| x as f32);
                match param {
                    Param::Rate => *rate = x,
                    Param::Size => *size = x,
                }
            }
        }
        At::Look { .. } => {}
    }
}

/// The TOML form of a fitted knob value.
fn toml_value(v: &Value) -> toml_edit::Value {
    match v {
        Value::Str(s) => toml_edit::Value::from(s.as_str()),
        Value::Int(i) => toml_edit::Value::from(*i),
        other => toml_edit::Value::from(other.as_f64().unwrap_or(0.0)),
    }
}

fn table<'a>(item: Option<&'a mut toml_edit::Item>, what: &str) -> anyhow::Result<&'a mut dyn toml_edit::TableLike> {
    item.and_then(toml_edit::Item::as_table_like_mut).ok_or_else(|| anyhow::anyhow!("`{what}` is missing from the file"))
}

/// Replace (keeping its spacing) or add `key = v`; a timed value `{ value, … }` changes its
/// `value`.
fn put(t: &mut dyn toml_edit::TableLike, key: &str, v: toml_edit::Value) {
    match t.get_mut(key) {
        Some(item) if item.as_table_like().is_some_and(|x| x.contains_key("value")) => {
            if let Some(inner) = item.as_table_like_mut() {
                put(inner, "value", v);
            }
        }
        Some(toml_edit::Item::Value(old)) => {
            let decor = old.decor().clone();
            *old = v;
            *old.decor_mut() = decor;
        }
        _ => {
            t.insert(key, toml_edit::Item::Value(v));
        }
    }
}

/// Write a fitted knob value into the item's file.
pub fn write(doc: &mut toml_edit::DocumentMut, at: &At, v: &Value) -> anyhow::Result<()> {
    let v = toml_value(v);
    match at {
        At::Look { target, attr } => {
            let set = table(doc.get_mut("set"), "set")?;
            put(table(set.get_mut(target), &format!("set.{target}"))?, attr, v);
        }
        At::CueSet { cue, target, attr } => {
            let c = doc
                .get_mut("cue")
                .and_then(toml_edit::Item::as_array_of_tables_mut)
                .and_then(|a| a.get_mut(*cue))
                .ok_or_else(|| anyhow::anyhow!("cue {} is missing from the file", cue + 1))?;
            let set = table(c.get_mut("set"), "set")?;
            put(table(set.get_mut(target), &format!("set.{target}"))?, attr, v);
        }
        At::CueEffect { cue, effect, param } => {
            let c = doc
                .get_mut("cue")
                .and_then(toml_edit::Item::as_array_of_tables_mut)
                .and_then(|a| a.get_mut(*cue))
                .ok_or_else(|| anyhow::anyhow!("cue {} is missing from the file", cue + 1))?;
            let fx = table(c.get_mut("effects"), "effects")?;
            put(table(fx.get_mut(effect), &format!("effects.{effect}"))?, if *param == Param::Rate { "rate" } else { "size" }, v);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn knob(src: &str) -> Knob {
        let t: toml::Table = toml::from_str(src).unwrap();
        toml::Value::Table(t).try_into().unwrap()
    }

    #[test]
    fn targets_resolve_to_plain_values_of_the_right_kind() {
        let p =
            Palette::parse("warm", &toml::from_str("[set]\nall = { color = \"#ffb070\", intensity = 0.5 }\nfront = { color = \"stream:accent\" }").unwrap())
                .unwrap();
        let ok = look_at(&p, &knob("label = \"Color\"\ntarget = \"set.all.color\"\nkind = \"color\"")).unwrap();
        assert_eq!(look_value(&p, &ok), Value::Str("#ffb070".into()));
        let e = |src: &str| look_at(&p, &knob(src)).unwrap_err();
        assert!(e("label = \"C\"\ntarget = \"set.all.colour\"\nkind = \"color\"").contains("no `set.all.colour` (it has set.all.color, set.all.intensity"));
        assert!(e("label = \"C\"\ntarget = \"set.front.color\"\nkind = \"color\"").contains("only plain values"));
        assert!(e("label = \"C\"\ntarget = \"set.all.intensity\"\nkind = \"color\"").contains("is a number, not a colour"));
        assert!(e("label = \"C\"\ntarget = \"set.all.color\"\nmin = 0.0\nmax = 1.0").contains("needs kind = \"color\""));

        let l = CueList::parse(
            "chase",
            &toml::from_str("[[cue]]\nid = \"1\"\neffects = { chase = {} }\n[cue.set]\nfront = { intensity = { value = 1.0, fade = \"4s\" } }\n[[cue]]\nid = \"10\"\n[cue.set]\nall = { intensity = 0.2 }").unwrap(),
        )
        .unwrap();
        let fx = EffectDef::parse("chase", &toml::from_str("kind = \"dimmer_sine\"\nrate = 2.0").unwrap()).unwrap();
        let rate = cue_at(&l, &knob("label = \"Speed\"\ntarget = \"cue.1.effects.chase.rate\"\nmin = 0.5\nmax = 6.0")).unwrap();
        assert_eq!(cue_value(&l, &rate, std::slice::from_ref(&fx)), Value::Float(2.0), "unset rate = the effect's own");
        let timed = cue_at(&l, &knob("label = \"Front\"\ntarget = \"cue.1.set.front.intensity\"\nmin = 0.0\nmax = 1.0")).unwrap();
        assert_eq!(timed, At::CueSet { cue: 0, target: "front".into(), attr: "intensity".into() });
        assert_eq!(
            cue_at(&l, &knob("label = \"Dim\"\ntarget = \"cue.10.set.all.intensity\"\nmin = 0.0\nmax = 1.0")).unwrap(),
            At::CueSet { cue: 1, target: "all".into(), attr: "intensity".into() },
            "cue ids match whole"
        );
        let e = |src: &str| cue_at(&l, &knob(src)).unwrap_err();
        assert!(e("label = \"S\"\ntarget = \"cue.3.effects.chase.rate\"\nmin = 1.0\nmax = 2.0").contains("names no cue of this list (its cues: 1, 10)"));
        assert!(e("label = \"S\"\ntarget = \"cue.10.effects.chase.rate\"\nmin = 1.0\nmax = 2.0").contains("doesn't start the effect `chase`"));
        assert!(e("label = \"S\"\ntarget = \"cue.1.effects.chase.size\"\nmin = 0.0\nmax = 2.0").contains("size runs from 0 to 1"));
        assert!(e("label = \"S\"\ntarget = \"cue.1.effects.chase.rate\"\nmin = 0.0\nmax = 2.0").contains("above 0"));
        assert!(e("label = \"S\"\ntarget = \"cue.1.effects.chase.speed\"\nmin = 1.0\nmax = 2.0").contains("`rate` or `size`"));
    }

    #[test]
    fn writing_keeps_comments_and_changes_what_the_parser_reads() {
        let src = "# my look\nlabel = \"Warm\"\n\n[set]\nall = { color = \"#ffb070\" } # the wash\n";
        let mut doc: toml_edit::DocumentMut = src.parse().unwrap();
        write(&mut doc, &At::Look { target: "all".into(), attr: "color".into() }, &Value::Str("#ff0000".into())).unwrap();
        let out = doc.to_string();
        assert_eq!(out, "# my look\nlabel = \"Warm\"\n\n[set]\nall = { color = \"#ff0000\" } # the wash\n");

        let src = "# chase\n[[cue]]\nid = \"1\"\neffects = { chase = {} } # beat chase\n[cue.set]\nfront = { intensity = { value = 1.0, fade = \"4s\" } }\n";
        let mut doc: toml_edit::DocumentMut = src.parse().unwrap();
        write(&mut doc, &At::CueEffect { cue: 0, effect: "chase".into(), param: Param::Rate }, &Value::Float(3.5)).unwrap();
        write(&mut doc, &At::CueSet { cue: 0, target: "front".into(), attr: "intensity".into() }, &Value::Float(0.4)).unwrap();
        let out = doc.to_string();
        assert!(out.starts_with("# chase\n") && out.contains("# beat chase"), "{out}");
        let l = CueList::parse("chase", &toml::from_str(&out).unwrap()).unwrap();
        assert_eq!(l.cues[0].effects, vec![("chase".to_string(), None, Some(3.5))]);
        let e = &l.cues[0].set[0].1[0].1;
        assert_eq!((e.spec.clone(), e.fade), (Spec::Literal(Value::Float(0.4)), Some(4000)), "timed values keep their timing");
    }

    #[test]
    fn brightness_knobs_stay_inside_zero_to_one() {
        let r = crate::engine::tests::rig("[fixtures.a]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1");
        let src = |max: f64| {
            format!("[set]\nall = {{ intensity = 0.5 }}\n[[knob]]\nlabel = \"Brightness\"\ntarget = \"set.all.intensity\"\nmin = 0.0\nmax = {max:.1}")
        };
        let p = Palette::parse("half", &toml::from_str(&src(100.0)).unwrap()).unwrap();
        assert_eq!(p.validate(&r), ["palette `half`: knob “Brightness”: `intensity` runs from 0 to 1 (write min = 0.0, max = 1.0, unit = \"%\")"]);
        assert!(Palette::parse("half", &toml::from_str(&src(1.0)).unwrap()).unwrap().validate(&r).is_empty());
    }
}
