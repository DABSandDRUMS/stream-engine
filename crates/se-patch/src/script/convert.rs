//! Conversion between engine [`Value`]s and Lua values.

use mlua::{Lua, Table, Value as Lv};
use se_proto::Value;
use std::collections::BTreeMap;

const MAX_DEPTH: usize = 32;

/// Engine value → Lua. Maps and lists become tables; lists are 1-based arrays.
pub fn to_lua(lua: &Lua, v: &Value) -> mlua::Result<Lv> {
    to_lua_depth(lua, v, 0)
}

fn to_lua_depth(lua: &Lua, v: &Value, depth: usize) -> mlua::Result<Lv> {
    if depth > MAX_DEPTH {
        return Err(mlua::Error::runtime("value nested too deeply"));
    }
    Ok(match v {
        Value::Null => Lv::Nil,
        Value::Bool(b) => Lv::Boolean(*b),
        Value::Int(i) => Lv::Number(*i as f64),
        Value::Float(f) => Lv::Number(*f),
        Value::Str(s) => Lv::String(lua.create_string(s)?),
        Value::List(l) => {
            let t = lua.create_table_with_capacity(l.len(), 0)?;
            for (i, x) in l.iter().enumerate() {
                t.raw_set(i + 1, to_lua_depth(lua, x, depth + 1)?)?;
            }
            Lv::Table(t)
        }
        Value::Map(m) => {
            let t = lua.create_table_with_capacity(0, m.len())?;
            for (k, x) in m {
                t.raw_set(k.as_str(), to_lua_depth(lua, x, depth + 1)?)?;
            }
            Lv::Table(t)
        }
    })
}

/// Lua → engine value. Integral numbers become `Int`; tables with keys `1..n` become lists,
/// other tables maps (non-string keys are stringified). Functions, userdata, and cycles are
/// rejected.
pub fn from_lua(v: &Lv) -> Result<Value, String> {
    from_lua_depth(v, 0)
}

fn number(f: f64) -> Value {
    if f.fract() == 0.0 && f.abs() < 9.007_199_254_740_992e15 { Value::Int(f as i64) } else { Value::Float(f) }
}

fn from_lua_depth(v: &Lv, depth: usize) -> Result<Value, String> {
    if depth > MAX_DEPTH {
        return Err("table nested too deeply (or cyclic)".into());
    }
    Ok(match v {
        Lv::Nil => Value::Null,
        Lv::Boolean(b) => Value::Bool(*b),
        Lv::Integer(i) => Value::Int(*i),
        Lv::Number(f) => number(*f),
        Lv::String(s) => Value::Str(s.to_string_lossy()),
        Lv::Table(t) => table(t, depth)?,
        other => return Err(format!("cannot convert a {} to an engine value", other.type_name())),
    })
}

fn table(t: &Table, depth: usize) -> Result<Value, String> {
    let n = t.raw_len();
    let mut count = 0usize;
    let mut map = BTreeMap::new();
    for pair in t.pairs::<Lv, Lv>() {
        let (k, v) = pair.map_err(|e| e.to_string())?;
        count += 1;
        let key = match &k {
            Lv::String(s) => s.to_string_lossy(),
            Lv::Integer(i) => i.to_string(),
            Lv::Number(f) => match number(*f) {
                Value::Int(i) => i.to_string(),
                _ => f.to_string(),
            },
            Lv::Boolean(b) => b.to_string(),
            other => return Err(format!("unsupported table key type {}", other.type_name())),
        };
        map.insert(key, from_lua_depth(&v, depth + 1)?);
    }
    if n > 0 && count == n {
        let mut list = Vec::with_capacity(n);
        for i in 1..=n {
            list.push(map.remove(&i.to_string()).unwrap_or(Value::Null));
        }
        return Ok(Value::List(list));
    }
    Ok(Value::Map(map))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let lua = Lua::new();
        let v = Value::map().with("tier", 2).with("user", "ann").with("xs", vec![Value::Float(0.5), Value::Int(3)]).with("ok", true);
        let l = to_lua(&lua, &v).unwrap();
        assert_eq!(from_lua(&l).unwrap(), v);
    }

    #[test]
    fn tables_from_lua() {
        let lua = Lua::new();
        let v: Lv = lua.load("return { 1, 2.5, 'x', nested = { a = true } }").eval().unwrap();
        // mixed table → map with stringified indices
        let m = from_lua(&v).unwrap();
        assert_eq!(m.get_path("1"), Some(&Value::Int(1)));
        assert_eq!(m.get_path("nested.a"), Some(&Value::Bool(true)));
        let arr: Lv = lua.load("return { 'a', 'b' }").eval().unwrap();
        assert_eq!(from_lua(&arr).unwrap(), Value::List(vec!["a".into(), "b".into()]));
        let empty: Lv = lua.load("return {}").eval().unwrap();
        assert_eq!(from_lua(&empty).unwrap(), Value::map());
    }

    #[test]
    fn rejects_functions_and_cycles() {
        let lua = Lua::new();
        let f: Lv = lua.load("return { f = function() end }").eval().unwrap();
        assert!(from_lua(&f).is_err());
        let c: Lv = lua.load("local t = {} t.self = t return t").eval().unwrap();
        assert!(from_lua(&c).is_err());
    }
}
