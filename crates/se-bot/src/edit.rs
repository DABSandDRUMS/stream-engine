//! Comment-preserving edits of array-of-tables project files (`commands/*.toml`,
//! `alerts/*.toml`) with `toml_edit`: set/remove fields of `[[command]]` #n, append a table,
//! delete a table. Existing keys keep their position and comments.

use anyhow::{Result, anyhow, bail};
use se_proto::Value;
use std::collections::BTreeMap;
use toml_edit::{DocumentMut, Item, Table, TableLike};

/// One step in a path: a key, or an index into an array of tables.
#[derive(Clone, Debug, PartialEq)]
pub enum Seg {
    Key(String),
    Index(usize),
}

impl Seg {
    pub fn key(k: &str) -> Seg {
        Seg::Key(k.to_string())
    }

    /// Parse `["alert", 2, "variation", 0]` (strings and non-negative ints).
    pub fn parse_path(v: &Value) -> Result<Vec<Seg>> {
        let l = v.as_list().ok_or_else(|| anyhow!("path must be a list"))?;
        l.iter()
            .map(|s| match s {
                Value::Str(k) => Ok(Seg::Key(k.clone())),
                Value::Int(i) if *i >= 0 => Ok(Seg::Index(*i as usize)),
                other => Err(anyhow!("bad path segment {other}")),
            })
            .collect()
    }
}

fn descend<'a>(tl: &'a mut dyn TableLike, path: &[Seg]) -> Result<&'a mut dyn TableLike> {
    match path {
        [] => Ok(tl),
        [Seg::Key(k), Seg::Index(i), rest @ ..] => {
            let item = tl.get_mut(k).ok_or_else(|| anyhow!("no `{k}`"))?;
            let next: &mut dyn TableLike = match item {
                Item::ArrayOfTables(a) => a.get_mut(*i).map(|t| t as &mut dyn TableLike).ok_or_else(|| anyhow!("no `{k}` #{i}"))?,
                Item::Value(toml_edit::Value::Array(arr)) => {
                    arr.get_mut(*i).and_then(|v| v.as_inline_table_mut()).map(|t| t as &mut dyn TableLike).ok_or_else(|| anyhow!("no `{k}` #{i}"))?
                }
                _ => bail!("`{k}` is not an array of tables"),
            };
            descend(next, rest)
        }
        [Seg::Key(k), rest @ ..] => {
            let item = tl.get_mut(k).ok_or_else(|| anyhow!("no `{k}`"))?;
            let next = item.as_table_like_mut().ok_or_else(|| anyhow!("`{k}` is not a table"))?;
            descend(next, rest)
        }
        [Seg::Index(_), ..] => bail!("path cannot start with an index"),
    }
}

/// The table at `path` (e.g. `[command, 3]`).
pub fn table_at<'a>(doc: &'a mut DocumentMut, path: &[Seg]) -> Result<&'a mut dyn TableLike> {
    descend(doc.as_table_mut(), path)
}

/// Set (or with `None`, remove) one field, keeping the line's comments.
pub fn set_field(tl: &mut dyn TableLike, key: &str, value: Option<&Value>) -> Result<()> {
    match value.and_then(se_store::project::to_edit_value) {
        None => {
            tl.remove(key);
        }
        Some(mut v) => match tl.get_mut(key) {
            Some(Item::Value(old)) => {
                let decor = old.decor().clone();
                *old = v;
                *old.decor_mut() = decor;
            }
            Some(other) => *other = Item::Value(v),
            None => {
                v.decor_mut().clear();
                tl.insert(key, Item::Value(v));
            }
        },
    }
    Ok(())
}

/// Set several fields of the table at `path`. `Null` removes a field.
pub fn set_fields(doc: &mut DocumentMut, path: &[Seg], fields: &BTreeMap<String, Value>) -> Result<()> {
    let tl = table_at(doc, path)?;
    for (k, v) in fields {
        set_field(tl, k, (!v.is_null()).then_some(v))?;
    }
    Ok(())
}

/// Append a table to the array `key` inside the table at `parent` (creating the array),
/// with an optional `# comment` line above it. Returns the new index.
pub fn append_table(doc: &mut DocumentMut, parent: &[Seg], key: &str, fields: &BTreeMap<String, Value>, comment: Option<&str>) -> Result<usize> {
    // a file that is only comments keeps them on top instead of after the new table
    let empty = parent.is_empty() && doc.as_table().is_empty();
    let header = if empty {
        let t = doc.trailing().as_str().unwrap_or("").trim_end().to_string();
        doc.set_trailing("");
        t
    } else {
        String::new()
    };
    let tl = table_at(doc, parent)?;
    if tl.get(key).is_none() {
        tl.insert(key, Item::ArrayOfTables(toml_edit::ArrayOfTables::new()));
    }
    let item = tl.get_mut(key).ok_or_else(|| anyhow!("no `{key}`"))?;
    match item {
        Item::ArrayOfTables(a) => {
            let mut t = Table::new();
            for (k, v) in fields {
                if let Some(ev) = se_store::project::to_edit_value(v) {
                    t.insert(k, Item::Value(ev));
                }
            }
            // blank line between tables, none at the very top of a new file
            let mut prefix = if header.is_empty() { String::new() } else { format!("{header}\n") };
            if !(empty && header.is_empty()) {
                prefix.push('\n');
            }
            if let Some(c) = comment {
                prefix.push_str(&format!("# {}\n", c.replace('\n', " ")));
            }
            t.decor_mut().set_prefix(prefix);
            a.push(t);
            Ok(a.len() - 1)
        }
        Item::Value(toml_edit::Value::Array(arr)) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, v) in fields {
                if let Some(ev) = se_store::project::to_edit_value(v) {
                    t.insert(k, ev);
                }
            }
            arr.push(t);
            Ok(arr.len() - 1)
        }
        _ => bail!("`{key}` is not an array of tables"),
    }
}

/// Remove element `index` of the array `key` inside the table at `parent`.
pub fn remove_table(doc: &mut DocumentMut, parent: &[Seg], key: &str, index: usize) -> Result<()> {
    let tl = table_at(doc, parent)?;
    let item = tl.get_mut(key).ok_or_else(|| anyhow!("no `{key}`"))?;
    match item {
        Item::ArrayOfTables(a) if index < a.len() => {
            a.remove(index);
        }
        Item::Value(toml_edit::Value::Array(arr)) if index < arr.len() => {
            arr.remove(index);
        }
        _ => bail!("no `{key}` #{index}"),
    }
    Ok(())
}

/// Index of the table in `[[key]]` whose `name` equals `name` (case-insensitive).
pub fn find_named(doc: &DocumentMut, key: &str, name: &str) -> Option<usize> {
    let item = doc.get(key)?;
    let names: Vec<Option<String>> = match item {
        Item::ArrayOfTables(a) => a.iter().map(|t| t.get("name").and_then(|v| v.as_str()).map(String::from)).collect(),
        Item::Value(toml_edit::Value::Array(arr)) => {
            arr.iter().map(|v| v.as_inline_table().and_then(|t| t.get("name")).and_then(|v| v.as_str()).map(String::from)).collect()
        }
        _ => return None,
    };
    names.iter().position(|n| n.as_deref().is_some_and(|n| n.eq_ignore_ascii_case(name)))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SRC: &str = r#"# Social links — keep these current!
[[command]]
name = "!discord"   # the main one
# where the community hangs out
reply = "Join the Discord: https://discord.gg/xxxx"

[[command]]
name = "!ig"
reply = "old"

[[alert]]
name = "cheer"
[[alert.variation]]
name = "big"
title = "old"
"#;

    fn m(pairs: &[(&str, Value)]) -> BTreeMap<String, Value> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.clone())).collect()
    }

    #[test]
    fn set_keeps_comments_and_order() {
        let mut doc: DocumentMut = SRC.parse().unwrap();
        set_fields(&mut doc, &[Seg::key("command"), Seg::Index(0)], &m(&[("reply", "new link".into()), ("role", "sub".into())])).unwrap();
        let out = doc.to_string();
        assert!(out.contains("# Social links — keep these current!"));
        assert!(out.contains("name = \"!discord\"   # the main one"));
        assert!(out.contains("# where the community hangs out\nreply = \"new link\""));
        assert!(out.contains("role = \"sub\""));
        assert!(out.find("reply = \"new link\"").unwrap() < out.find("[[command]]\nname = \"!ig\"").unwrap());
        // remove a field
        set_fields(&mut doc, &[Seg::key("command"), Seg::Index(0)], &m(&[("role", Value::Null)])).unwrap();
        assert!(!doc.to_string().contains("role"));
    }

    #[test]
    fn nested_paths_append_and_remove() {
        let mut doc: DocumentMut = SRC.parse().unwrap();
        set_fields(&mut doc, &[Seg::key("alert"), Seg::Index(0), Seg::key("variation"), Seg::Index(0)], &m(&[("title", "{user} is huge".into())])).unwrap();
        assert!(doc.to_string().contains("title = \"{user} is huge\""));
        let i = append_table(&mut doc, &[], "command", &m(&[("name", "!new".into()), ("reply", "hello".into())]), Some("added from chat by mod1")).unwrap();
        assert_eq!(i, 2);
        let out = doc.to_string();
        assert!(out.contains("# added from chat by mod1\n[[command]]\nname = \"!new\"\nreply = \"hello\""), "{out}");
        assert_eq!(find_named(&doc, "command", "!NEW"), Some(2));
        remove_table(&mut doc, &[], "command", 1).unwrap();
        let out = doc.to_string();
        assert!(!out.contains("!ig"));
        assert!(out.contains("# the main one"));
        let reparsed: toml::Table = out.parse().unwrap();
        assert_eq!(reparsed["command"].as_array().unwrap().len(), 2);
        assert!(remove_table(&mut doc, &[], "command", 9).is_err());
    }

    #[test]
    fn append_creates_array_in_empty_file() {
        let mut doc: DocumentMut = "# my commands\n".parse().unwrap();
        append_table(&mut doc, &[], "command", &m(&[("name", "!a".into()), ("reply", "b".into())]), None).unwrap();
        let t: toml::Table = doc.to_string().parse().unwrap();
        assert_eq!(t["command"][0]["name"].as_str(), Some("!a"));
        assert!(doc.to_string().starts_with("# my commands\n"));
        let mut fresh = DocumentMut::new();
        append_table(&mut fresh, &[], "command", &m(&[("name", "!a".into())]), Some("added from chat by x")).unwrap();
        assert_eq!(fresh.to_string(), "# added from chat by x\n[[command]]\nname = \"!a\"\n");
    }

    #[test]
    fn parses_paths() {
        let p = Seg::parse_path(&Value::List(vec!["alert".into(), Value::Int(2), "title".into()])).unwrap();
        assert_eq!(p, vec![Seg::key("alert"), Seg::Index(2), Seg::key("title")]);
        assert!(Seg::parse_path(&Value::List(vec![Value::Int(-1)])).is_err());
    }
}
