//! Knobs: the few premade controls a developer-made item exposes to the operator (a quick
//! effect's "Strobe speed", a light look's colour, a cue list's chase speed). Declared in the
//! item's file as `[[knob]]` tables; each maps to one engine address and a value range:
//!
//! ```toml
//! [[knob]]
//! label = "Strobe speed"            # plain words shown in the UI
//! target = "lights.effect.strobe.rate"
//! min = 0.5
//! max = 3.0
//! step = 0.25                       # optional
//! unit = "flashes a second"         # optional, plain words
//! default = 2.5                     # optional ("Reset knobs" goes back to it)
//! # kind = "color" (value "#rrggbb") or kind = "choice" with options = [{ label, value }]
//! ```
//!
//! This module only knows the knob's own shape; where its value lives (a quick effect's `set`
//! map, a look's attribute, a cue list setting) is the owning definition's business.

use se_proto::Value;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KnobKind {
    /// A number between `min` and `max` (a slider).
    #[default]
    Number,
    /// A colour written `"#rrggbb"`.
    Color,
    /// One of `options`.
    Choice,
}

impl KnobKind {
    pub fn as_str(self) -> &'static str {
        match self {
            KnobKind::Number => "number",
            KnobKind::Color => "color",
            KnobKind::Choice => "choice",
        }
    }
}

/// One option of a `kind = "choice"` knob.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct KnobChoice {
    pub label: String,
    pub value: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(default, deny_unknown_fields)]
pub struct Knob {
    pub label: String,
    pub target: String,
    pub kind: KnobKind,
    pub min: Option<f64>,
    pub max: Option<f64>,
    pub step: Option<f64>,
    pub unit: Option<String>,
    pub default: Option<Value>,
    pub options: Vec<KnobChoice>,
}

/// `"#rrggbb"` (either case).
pub fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].chars().all(|c| c.is_ascii_hexdigit())
}

/// Numbers compare by value (`1` = `1.0`), everything else exactly.
fn same(a: &Value, b: &Value) -> bool {
    match (a.as_f64(), b.as_f64()) {
        (Some(x), Some(y)) if !matches!(a, Value::Bool(_)) && !matches!(b, Value::Bool(_)) => (x - y).abs() < 1e-9,
        _ => a == b,
    }
}

impl Knob {
    /// The knob's own shape: a label, an address, and a sensible range, colour or option list.
    /// Messages start with the knob's name so a config error says which one is wrong.
    pub fn check(&self) -> Result<(), String> {
        if self.label.trim().is_empty() {
            return Err(format!(
                "a knob needs a `label` (the words shown next to it){}",
                if self.target.is_empty() { String::new() } else { format!(" — the one for `{}`", self.target) }
            ));
        }
        let name = &self.label;
        if !se_proto::address::is_valid(&self.target, false) {
            return Err(format!("knob “{name}”: `target` must be the engine address it changes (like `fx.shake.strength`), not `{}`", self.target));
        }
        match self.kind {
            KnobKind::Number => {
                let (Some(min), Some(max)) = (self.min, self.max) else {
                    return Err(format!("knob “{name}”: needs `min` and `max` (the slider's two ends)"));
                };
                if !(min.is_finite() && max.is_finite()) || min >= max {
                    return Err(format!("knob “{name}”: `min` ({min}) must be smaller than `max` ({max})"));
                }
                if let Some(step) = self.step
                    && !(step > 0.0 && step <= max - min)
                {
                    return Err(format!("knob “{name}”: `step` ({step}) must be above 0 and at most the range ({})", max - min));
                }
                if let Some(d) = &self.default {
                    match d.as_f64().filter(|_| !matches!(d, Value::Bool(_))) {
                        None => return Err(format!("knob “{name}”: `default` must be a number")),
                        Some(v) if v < min || v > max => return Err(format!("knob “{name}”: `default` ({v}) is outside {min} to {max}")),
                        Some(_) => {}
                    }
                }
            }
            KnobKind::Color => {
                if let Some(d) = &self.default
                    && !d.as_str().is_some_and(is_hex_color)
                {
                    return Err(format!("knob “{name}”: a colour `default` is written \"#rrggbb\" (like \"#ff8800\")"));
                }
            }
            KnobKind::Choice => {
                if self.options.is_empty() {
                    return Err(format!("knob “{name}”: a choice needs `options = [{{ label = \"…\", value = … }}, …]`"));
                }
                if let Some(o) = self.options.iter().find(|o| o.label.trim().is_empty() || o.value.is_null()) {
                    return Err(format!(
                        "knob “{name}”: every option needs a `label` and a `value`{}",
                        if o.label.is_empty() { String::new() } else { format!(" (“{}” has no value)", o.label) }
                    ));
                }
                if let Some(d) = &self.default
                    && !self.options.iter().any(|o| same(&o.value, d))
                {
                    return Err(format!("knob “{name}”: `default` must be one of its options' values"));
                }
            }
        }
        Ok(())
    }

    /// `v` made fit for this knob: numbers clamped to the range and snapped to `step` (whole
    /// numbers stay whole when the range and step are), colours normalised to lowercase
    /// `#rrggbb`, choices matched to an option's value. `None` when it can't fit.
    pub fn fit(&self, v: &Value) -> Option<Value> {
        match self.kind {
            KnobKind::Number => {
                if matches!(v, Value::Bool(_)) {
                    return None;
                }
                let (min, max) = (self.min?, self.max?);
                let mut x = v.as_f64().filter(|x| x.is_finite())?.clamp(min, max);
                if let Some(step) = self.step.filter(|s| *s > 0.0) {
                    x = (min + ((x - min) / step).round() * step).clamp(min, max);
                }
                let whole = self.step.is_some_and(|s| s.fract() == 0.0) && min.fract() == 0.0;
                Some(if whole { Value::Int(x.round() as i64) } else { Value::Float((x * 1e6).round() / 1e6) })
            }
            KnobKind::Color => v.as_str().filter(|s| is_hex_color(s)).map(|s| Value::Str(s.to_ascii_lowercase())),
            KnobKind::Choice => self.options.iter().find(|o| same(&o.value, v)).map(|o| o.value.clone()),
        }
    }

    /// The knob as the UI queries show it: `{label, target, kind, min, max, step, unit,
    /// default, options: [{label, value}]}` (absent numbers are null, no unit is "").
    pub fn to_value(&self) -> Value {
        let num = |v: Option<f64>| v.map(Value::Float).unwrap_or_default();
        Value::map()
            .with("label", self.label.clone())
            .with("target", self.target.clone())
            .with("kind", self.kind.as_str())
            .with("min", num(self.min))
            .with("max", num(self.max))
            .with("step", num(self.step))
            .with("unit", self.unit.clone().unwrap_or_default())
            .with("default", self.default.clone().unwrap_or_default())
            .with("options", Value::List(self.options.iter().map(|o| Value::map().with("label", o.label.clone()).with("value", o.value.clone())).collect()))
    }
}

/// The `[[knob]]` tables of a raw item table, each checked ([`Knob::check`]), with no two
/// driving the same address.
pub fn parse_knobs(t: &toml::Table) -> Result<Vec<Knob>, String> {
    let knobs: Vec<Knob> = match t.get("knob") {
        None => return Ok(Vec::new()),
        Some(v @ toml::Value::Array(_)) => v.clone().try_into().map_err(|e: toml::de::Error| format!("knob: {}", e.message()))?,
        Some(_) => return Err("knobs are written as `[[knob]]` tables".into()),
    };
    check_all(&knobs)?;
    Ok(knobs)
}

/// [`Knob::check`] for each, plus no two knobs driving the same address.
pub fn check_all(knobs: &[Knob]) -> Result<(), String> {
    for (i, k) in knobs.iter().enumerate() {
        k.check()?;
        if let Some(other) = knobs[..i].iter().find(|o| o.target == k.target) {
            return Err(format!("knobs “{}” and “{}” both change `{}`; keep one", other.label, k.label, k.target));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn knobs(src: &str) -> Result<Vec<Knob>, String> {
        parse_knobs(&src.parse::<toml::Table>().unwrap())
    }

    #[test]
    fn knobs_parse_and_bad_shapes_say_what_is_wrong() {
        let k = knobs("[[knob]]\nlabel = \"Strobe speed\"\ntarget = \"lights.effect.strobe.rate\"\nmin = 1\nmax = 12\nstep = 0.5\nunit = \"flashes a second\"\ndefault = 6")
            .unwrap();
        assert_eq!(k[0].min, Some(1.0));
        assert_eq!(k[0].default, Some(Value::Int(6)));
        assert!(knobs("").unwrap().is_empty());
        let err = |src: &str| knobs(src).unwrap_err();
        assert!(err("[[knob]]\ntarget = \"a.b\"\nmin = 0\nmax = 1").contains("needs a `label`"));
        assert!(err("[[knob]]\nlabel = \"Speed\"\ntarget = \"a b\"\nmin = 0\nmax = 1").contains("engine address"));
        assert!(err("[[knob]]\nlabel = \"Speed\"\ntarget = \"a.b\"\nmin = 12\nmax = 1").contains("`min` (12) must be smaller than `max` (1)"));
        assert!(err("[[knob]]\nlabel = \"Speed\"\ntarget = \"a.b\"\nmax = 1").contains("needs `min` and `max`"));
        assert!(err("[[knob]]\nlabel = \"Speed\"\ntarget = \"a.b\"\nmin = 0\nmax = 1\ndefault = 3").contains("outside 0 to 1"));
        assert!(err("[[knob]]\nlabel = \"Speed\"\ntarget = \"a.b\"\nmin = 0\nmax = 1\nstep = 0").contains("`step`"));
        assert!(err("[[knob]]\nlabel = \"Tint\"\ntarget = \"a.b\"\nkind = \"color\"\ndefault = \"orange\"").contains("#rrggbb"));
        assert!(err("[[knob]]\nlabel = \"Mode\"\ntarget = \"a.b\"\nkind = \"choice\"").contains("needs `options"));
        assert!(
            err("[[knob]]\nlabel = \"Mode\"\ntarget = \"a.b\"\nkind = \"choice\"\noptions = [{ label = \"On\", value = 1 }]\ndefault = 2")
                .contains("one of its options")
        );
        assert!(err("[[knob]]\nlabel = \"Speed\"\ntarget = \"a.b\"\nmin = 0\nmaxx = 1").contains("unknown field `maxx`"));
        assert!(
            err("[[knob]]\nlabel = \"A\"\ntarget = \"a.b\"\nmin = 0\nmax = 1\n[[knob]]\nlabel = \"B\"\ntarget = \"a.b\"\nmin = 0\nmax = 1")
                .contains("both change `a.b`")
        );
        assert!(err("knob = 3").contains("[[knob]]"));
    }

    #[test]
    fn values_fit_the_knob() {
        let n = Knob { label: "Speed".into(), target: "a.b".into(), min: Some(1.0), max: Some(12.0), step: Some(0.5), ..Default::default() };
        assert_eq!(n.fit(&Value::Float(6.3)), Some(Value::Float(6.5)));
        assert_eq!(n.fit(&Value::Int(40)), Some(Value::Float(12.0)));
        assert_eq!(n.fit(&Value::Str("fast".into())), None);
        let whole = Knob { min: Some(10.0), max: Some(1500.0), step: Some(10.0), ..n.clone() };
        assert_eq!(whole.fit(&Value::Float(304.0)), Some(Value::Int(300)));
        let c = Knob { kind: KnobKind::Color, min: None, max: None, step: None, ..n.clone() };
        assert_eq!(c.fit(&Value::Str("#FF8800".into())), Some(Value::Str("#ff8800".into())));
        assert_eq!(c.fit(&Value::Str("ff8800".into())), None);
        let ch = Knob {
            kind: KnobKind::Choice,
            options: vec![KnobChoice { label: "Slow".into(), value: Value::Float(1.0) }, KnobChoice { label: "Fast".into(), value: Value::Float(4.0) }],
            ..c
        };
        assert_eq!(ch.fit(&Value::Int(4)), Some(Value::Float(4.0)));
        assert_eq!(ch.fit(&Value::Int(3)), None);
    }
}
