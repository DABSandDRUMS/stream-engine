//! Dynamic value type shared by the state tree, events, commands, and the wire.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;

/// A JSON-like value. Semantic types (color, vec2, enum, …) are carried by [`crate::Meta`],
/// not by the value itself, so every value round-trips through TOML, JSON, and MessagePack.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(untagged)]
pub enum Value {
    #[default]
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    List(Vec<Value>),
    Map(BTreeMap<String, Value>),
}

impl Value {
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Value::Float(f) => Some(*f),
            Value::Int(i) => Some(*i as f64),
            Value::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            _ => None,
        }
    }

    pub fn as_f32(&self) -> Option<f32> {
        self.as_f64().map(|v| v as f32)
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Value::Int(i) => Some(*i),
            Value::Float(f) => Some(*f as i64),
            Value::Bool(b) => Some(*b as i64),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_list(&self) -> Option<&[Value]> {
        match self {
            Value::List(l) => Some(l),
            _ => None,
        }
    }

    pub fn as_map(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Value::Map(m) => Some(m),
            _ => None,
        }
    }

    pub fn is_null(&self) -> bool {
        matches!(self, Value::Null)
    }

    /// Truthiness used by expressions and `when` clauses.
    pub fn truthy(&self) -> bool {
        match self {
            Value::Null => false,
            Value::Bool(b) => *b,
            Value::Int(i) => *i != 0,
            Value::Float(f) => *f != 0.0 && !f.is_nan(),
            Value::Str(s) => !s.is_empty(),
            Value::List(l) => !l.is_empty(),
            Value::Map(m) => !m.is_empty(),
        }
    }

    /// Dotted lookup into nested maps (`"user.name"`), also indexing lists by number.
    pub fn get_path(&self, path: &str) -> Option<&Value> {
        let mut cur = self;
        if path.is_empty() {
            return Some(cur);
        }
        for seg in path.split('.') {
            cur = match cur {
                Value::Map(m) => m.get(seg)?,
                Value::List(l) => l.get(seg.parse::<usize>().ok()?)?,
                _ => return None,
            };
        }
        Some(cur)
    }

    pub fn map() -> Self {
        Value::Map(BTreeMap::new())
    }

    /// Insert into a map value (no-op for non-maps).
    pub fn with(mut self, key: impl Into<String>, v: impl Into<Value>) -> Self {
        if let Value::Map(m) = &mut self {
            m.insert(key.into(), v.into());
        }
        self
    }

    /// Numeric floats as 4-component RGBA; accepts `[r,g,b]`, `[r,g,b,a]`, or `"#rrggbb[aa]"`.
    pub fn as_color(&self) -> Option<[f32; 4]> {
        match self {
            Value::List(l) if l.len() == 3 || l.len() == 4 => {
                let mut c = [1.0f32; 4];
                for (i, v) in l.iter().enumerate() {
                    c[i] = v.as_f32()?;
                }
                Some(c)
            }
            Value::Str(s) => parse_hex_color(s),
            _ => None,
        }
    }

    pub fn as_vec2(&self) -> Option<[f32; 2]> {
        match self {
            Value::List(l) if l.len() == 2 => Some([l[0].as_f32()?, l[1].as_f32()?]),
            _ => None,
        }
    }

    pub fn as_vec4(&self) -> Option<[f32; 4]> {
        match self {
            Value::List(l) if l.len() == 4 => Some([l[0].as_f32()?, l[1].as_f32()?, l[2].as_f32()?, l[3].as_f32()?]),
            _ => None,
        }
    }

    /// Parse a scalar written in command text or CLI args: bool, int, float, TOML inline
    /// arrays/tables, quoted string, or a bare string.
    pub fn parse_text(s: &str) -> Value {
        let t = s.trim();
        match t {
            "true" => return Value::Bool(true),
            "false" => return Value::Bool(false),
            "null" | "none" => return Value::Null,
            _ => {}
        }
        if let Ok(i) = t.parse::<i64>() {
            return Value::Int(i);
        }
        if let Ok(f) = t.parse::<f64>() {
            return Value::Float(f);
        }
        if (t.starts_with('[') || t.starts_with('{') || t.starts_with('"') || t.starts_with('\''))
            && let Ok(v) = toml::from_str::<toml::Table>(&format!("v = {t}"))
            && let Some(v) = v.get("v")
        {
            return Value::from(v.clone());
        }
        Value::Str(t.to_string())
    }
}

pub fn parse_hex_color(s: &str) -> Option<[f32; 4]> {
    let h = s.strip_prefix('#')?;
    let byte = |i: usize| u8::from_str_radix(h.get(i..i + 2)?, 16).ok().map(|b| b as f32 / 255.0);
    match h.len() {
        6 => Some([byte(0)?, byte(2)?, byte(4)?, 1.0]),
        8 => Some([byte(0)?, byte(2)?, byte(4)?, byte(6)?]),
        _ => None,
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Null => write!(f, "null"),
            Value::Bool(b) => write!(f, "{b}"),
            Value::Int(i) => write!(f, "{i}"),
            Value::Float(x) => {
                if x.fract() == 0.0 && x.abs() < 1e15 {
                    write!(f, "{x:.1}")
                } else {
                    write!(f, "{}", (x * 10000.0).round() / 10000.0)
                }
            }
            Value::Str(s) => write!(f, "{s}"),
            other => write!(f, "{}", serde_json::to_string(other).unwrap_or_default()),
        }
    }
}

macro_rules! from_num {
    ($($t:ty => $v:ident as $c:ty),*) => {$(
        impl From<$t> for Value { fn from(x: $t) -> Self { Value::$v(x as $c) } }
    )*};
}
from_num!(i32 => Int as i64, i64 => Int as i64, u32 => Int as i64, u64 => Int as i64, usize => Int as i64, f32 => Float as f64, f64 => Float as f64);

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Value::Bool(b)
    }
}
impl From<&str> for Value {
    fn from(s: &str) -> Self {
        Value::Str(s.to_string())
    }
}
impl From<String> for Value {
    fn from(s: String) -> Self {
        Value::Str(s)
    }
}
impl<T: Into<Value>> From<Vec<T>> for Value {
    fn from(v: Vec<T>) -> Self {
        Value::List(v.into_iter().map(Into::into).collect())
    }
}
impl From<[f32; 4]> for Value {
    fn from(c: [f32; 4]) -> Self {
        Value::List(c.iter().map(|x| Value::Float(*x as f64)).collect())
    }
}
impl From<[f32; 2]> for Value {
    fn from(c: [f32; 2]) -> Self {
        Value::List(c.iter().map(|x| Value::Float(*x as f64)).collect())
    }
}
impl From<BTreeMap<String, Value>> for Value {
    fn from(m: BTreeMap<String, Value>) -> Self {
        Value::Map(m)
    }
}

impl From<toml::Value> for Value {
    fn from(v: toml::Value) -> Self {
        match v {
            toml::Value::String(s) => Value::Str(s),
            toml::Value::Integer(i) => Value::Int(i),
            toml::Value::Float(f) => Value::Float(f),
            toml::Value::Boolean(b) => Value::Bool(b),
            toml::Value::Datetime(d) => Value::Str(d.to_string()),
            toml::Value::Array(a) => Value::List(a.into_iter().map(Value::from).collect()),
            toml::Value::Table(t) => Value::Map(t.into_iter().map(|(k, v)| (k, Value::from(v))).collect()),
        }
    }
}

impl From<serde_json::Value> for Value {
    fn from(v: serde_json::Value) -> Self {
        match v {
            serde_json::Value::Null => Value::Null,
            serde_json::Value::Bool(b) => Value::Bool(b),
            serde_json::Value::Number(n) => n.as_i64().map(Value::Int).unwrap_or_else(|| Value::Float(n.as_f64().unwrap_or(0.0))),
            serde_json::Value::String(s) => Value::Str(s),
            serde_json::Value::Array(a) => Value::List(a.into_iter().map(Value::from).collect()),
            serde_json::Value::Object(o) => Value::Map(o.into_iter().map(|(k, v)| (k, Value::from(v))).collect()),
        }
    }
}

impl From<&Value> for serde_json::Value {
    fn from(v: &Value) -> Self {
        serde_json::to_value(v).unwrap_or(serde_json::Value::Null)
    }
}

/// Convert back to a TOML value for project file writes. `Null` has no TOML form → `None`.
pub fn to_toml(v: &Value) -> Option<toml::Value> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => toml::Value::Boolean(*b),
        Value::Int(i) => toml::Value::Integer(*i),
        Value::Float(f) => toml::Value::Float(*f),
        Value::Str(s) => toml::Value::String(s.clone()),
        Value::List(l) => toml::Value::Array(l.iter().filter_map(to_toml).collect()),
        Value::Map(m) => toml::Value::Table(m.iter().filter_map(|(k, v)| Some((k.clone(), to_toml(v)?))).collect()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_text_types() {
        assert_eq!(Value::parse_text("1000"), Value::Int(1000));
        assert_eq!(Value::parse_text("0.5"), Value::Float(0.5));
        assert_eq!(Value::parse_text("true"), Value::Bool(true));
        assert_eq!(Value::parse_text("hello"), Value::Str("hello".into()));
        assert_eq!(Value::parse_text("[1, 2]"), Value::List(vec![Value::Int(1), Value::Int(2)]));
        assert_eq!(Value::parse_text("\"a b\""), Value::Str("a b".into()));
    }

    #[test]
    fn wire_roundtrip_msgpack_and_json() {
        let v = Value::map().with("bits", 1000).with("msg", "hi").with("x", 0.25).with("n", Value::Null);
        let mp = rmp_serde::to_vec_named(&v).unwrap();
        assert_eq!(rmp_serde::from_slice::<Value>(&mp).unwrap(), v);
        let js = serde_json::to_string(&v).unwrap();
        assert_eq!(serde_json::from_str::<Value>(&js).unwrap(), v);
    }

    #[test]
    fn colors() {
        assert_eq!(Value::from("#ff0000").as_color(), Some([1.0, 0.0, 0.0, 1.0]));
        assert_eq!(Value::parse_text("[0.5, 0.5, 0.5]").as_color(), Some([0.5, 0.5, 0.5, 1.0]));
    }
}
