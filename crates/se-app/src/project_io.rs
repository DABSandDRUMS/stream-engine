//! Project file access for editors in the UI (§15.5): the `project.write` action (bindings,
//! rules, layouts, shortcuts written with `toml_edit`, comments preserved), the `project.read`
//! and `project.files` queries, and `show.live_since` (uptime shown in the status bar).

mod audio_inputs;
mod sources;
mod fx;

use crate::daemon::Ctx;
use anyhow::{Context, Result, anyhow, bail};
use se_proto::{Meta, Value};
use std::path::{Component, Path};
use std::sync::Arc;
use toml_edit::{ArrayOfTables, DocumentMut, Item, Table, TableLike};

pub(super) static CONFIG_WRITE: parking_lot::Mutex<()> = parking_lot::Mutex::new(());

/// Validate a project-relative path: relative, no `..` or root, nothing hidden. Shared by config
/// files ([`check_path`]) and media files (`assets::check_asset_path`).
pub fn check_relative(rel: &str) -> Result<()> {
    let p = Path::new(rel);
    if rel.is_empty() || p.is_absolute() {
        bail!("`{rel}`: path must be relative to the project");
    }
    for c in p.components() {
        match c {
            Component::Normal(s) => {
                let s = s.to_string_lossy();
                if s.starts_with('.') {
                    bail!("`{rel}`: hidden paths are not allowed");
                }
            }
            _ => bail!("`{rel}`: `..` and root components are not allowed"),
        }
    }
    Ok(())
}

/// Validate a project-relative config path: [`check_relative`], no engine-owned directories,
/// `.toml` only.
pub fn check_path(rel: &str) -> Result<()> {
    check_relative(rel)?;
    let p = Path::new(rel);
    if p.extension().and_then(|e| e.to_str()) != Some("toml") {
        bail!("`{rel}`: only .toml files can be written");
    }
    let first = p.components().next().map(|c| c.as_os_str().to_string_lossy().to_string()).unwrap_or_default();
    if matches!(first.as_str(), "sessions" | "assets" | "target" | "node_modules") {
        bail!("`{rel}`: `{first}/` is not a config directory");
    }
    Ok(())
}

/// What a `project.write` request does to the file.
#[derive(Debug, PartialEq)]
pub enum WriteOutcome {
    Write(String),
    Delete,
    Unchanged,
}

/// Apply a `project.write` request to the current file text (`None` = the file doesn't exist).
///
/// Args (besides `path`):
/// * `text` — replace the whole file (must be valid TOML);
/// * `set` — map of dotted keys → values (`null` removes the key), applied to the document root,
///   or to one entry of the array of tables `table` (`[[rule]]`) chosen by `match` (all
///   key/value pairs equal) or `index` (0-based); with `append = true` a new entry is added;
/// * `delete = true` — remove the selected entry, or the whole file when no `table` is given.
pub fn apply(src: Option<&str>, args: &Value) -> Result<WriteOutcome> {
    if let Some(text) = args.get_path("text").and_then(Value::as_str) {
        text.parse::<DocumentMut>().context("text is not valid TOML")?;
        return Ok(if src == Some(text) { WriteOutcome::Unchanged } else { WriteOutcome::Write(text.to_string()) });
    }
    let table = args.get_path("table").and_then(Value::as_str);
    let delete = args.get_path("delete").is_some_and(Value::truthy);
    if delete && table.is_none() {
        return Ok(if src.is_some() { WriteOutcome::Delete } else { WriteOutcome::Unchanged });
    }
    let src = src.unwrap_or("");
    let mut doc: DocumentMut = src.parse().context("existing file is not valid TOML")?;
    let set = match args.get_path("set") {
        Some(Value::Map(m)) => Some(m),
        None | Some(Value::Null) => None,
        Some(_) => bail!("`set` must be a map of key → value"),
    };
    match table {
        None => {
            let set = set.ok_or_else(|| anyhow!("project.write needs `text`, `set`, or `delete`"))?;
            for (k, v) in set {
                set_dotted(doc.as_table_mut(), k, v)?;
            }
        }
        Some(key) => {
            if args.get_path("append").is_some_and(Value::truthy) {
                let set = set.ok_or_else(|| anyhow!("append needs `set`"))?;
                let mut t = Table::new();
                for (k, v) in set {
                    set_dotted(&mut t, k, v)?;
                }
                append_entry(&mut doc, key, t)?;
            } else {
                let idx = select_entry(&doc, key, args)?;
                if delete {
                    remove_entry(&mut doc, key, idx)?;
                } else {
                    let set = set.ok_or_else(|| anyhow!("project.write needs `set` or `delete`"))?;
                    let entry = entry_mut(&mut doc, key, idx)?;
                    for (k, v) in set {
                        set_dotted_like(entry, k, v)?;
                    }
                }
            }
        }
    }
    let out = doc.to_string();
    Ok(if out == src { WriteOutcome::Unchanged } else { WriteOutcome::Write(out) })
}

fn set_dotted(t: &mut Table, key: &str, v: &Value) -> Result<()> {
    set_dotted_like(t, key, v)
}

fn set_dotted_like(t: &mut dyn TableLike, key: &str, v: &Value) -> Result<()> {
    let (head, rest) = match key.split_once('.') {
        Some((h, r)) => (h, Some(r)),
        None => (key, None),
    };
    if head.is_empty() {
        bail!("empty key in `{key}`");
    }
    match rest {
        None => {
            match se_store::project::to_edit_value(v) {
                Some(nv) => match t.get_mut(head) {
                    Some(Item::Value(old)) => {
                        let decor = old.decor().clone();
                        *old = nv;
                        *old.decor_mut() = decor;
                    }
                    _ => {
                        t.insert(head, Item::Value(nv));
                    }
                },
                None => {
                    t.remove(head);
                }
            }
            Ok(())
        }
        Some(rest) => {
            if t.get(head).is_none() {
                if v.is_null() {
                    return Ok(());
                }
                let mut nt = Table::new();
                nt.set_implicit(true);
                t.insert(head, Item::Table(nt));
            }
            let child = t.get_mut(head).and_then(Item::as_table_like_mut).ok_or_else(|| anyhow!("`{head}` is not a table"))?;
            set_dotted_like(child, rest, v)
        }
    }
}

fn entries_len(doc: &DocumentMut, key: &str) -> usize {
    match doc.get(key) {
        Some(Item::ArrayOfTables(a)) => a.len(),
        Some(Item::Value(toml_edit::Value::Array(a))) => a.len(),
        _ => 0,
    }
}

fn entry_ref<'a>(doc: &'a DocumentMut, key: &str, i: usize) -> Option<&'a dyn TableLike> {
    match doc.get(key)? {
        Item::ArrayOfTables(a) => a.get(i).map(|t| t as &dyn TableLike),
        Item::Value(toml_edit::Value::Array(a)) => a.get(i)?.as_inline_table().map(|t| t as &dyn TableLike),
        _ => None,
    }
}

fn entry_mut<'a>(doc: &'a mut DocumentMut, key: &str, i: usize) -> Result<&'a mut dyn TableLike> {
    let e: Option<&mut dyn TableLike> = match doc.get_mut(key) {
        Some(Item::ArrayOfTables(a)) => a.get_mut(i).map(|t| t as &mut dyn TableLike),
        Some(Item::Value(toml_edit::Value::Array(a))) => a.get_mut(i).and_then(|v| v.as_inline_table_mut()).map(|t| t as &mut dyn TableLike),
        _ => None,
    };
    e.ok_or_else(|| anyhow!("no `{key}` entry #{i}"))
}

fn select_entry(doc: &DocumentMut, key: &str, args: &Value) -> Result<usize> {
    let n = entries_len(doc, key);
    if let Some(i) = args.get_path("index").and_then(Value::as_i64) {
        let i = usize::try_from(i).map_err(|_| anyhow!("negative index"))?;
        if i >= n {
            bail!("`{key}` has {n} entries; index {i} is out of range");
        }
        return Ok(i);
    }
    let Some(Value::Map(m)) = args.get_path("match") else { bail!("editing `[[{key}]]` needs `match`, `index`, or `append`") };
    let hits: Vec<usize> = (0..n)
        .filter(|&i| entry_ref(doc, key, i).is_some_and(|t| m.iter().all(|(k, want)| t.get(k).and_then(Item::as_value).is_some_and(|v| value_eq(v, want)))))
        .collect();
    match hits.as_slice() {
        [i] => Ok(*i),
        [] => bail!("no `[[{key}]]` entry matches"),
        _ => bail!("{} `[[{key}]]` entries match; be more specific", hits.len()),
    }
}

/// Semantic equality between a TOML value in the file and a requested value (decor ignored).
fn value_eq(v: &toml_edit::Value, want: &Value) -> bool {
    match (v, want) {
        (toml_edit::Value::String(s), Value::Str(w)) => s.value() == w,
        (toml_edit::Value::Integer(i), w) => w.as_f64() == Some(*i.value() as f64),
        (toml_edit::Value::Float(f), w) => w.as_f64() == Some(*f.value()),
        (toml_edit::Value::Boolean(b), Value::Bool(w)) => b.value() == w,
        (toml_edit::Value::Array(a), Value::List(l)) => a.len() == l.len() && a.iter().zip(l).all(|(x, y)| value_eq(x, y)),
        _ => false,
    }
}

fn append_entry(doc: &mut DocumentMut, key: &str, t: Table) -> Result<()> {
    match doc.get_mut(key) {
        None => {
            let mut a = ArrayOfTables::new();
            a.push(t);
            doc.insert(key, Item::ArrayOfTables(a));
        }
        Some(Item::ArrayOfTables(a)) => a.push(t),
        Some(Item::Value(toml_edit::Value::Array(a))) => a.push(t.into_inline_table()),
        Some(_) => bail!("`{key}` is not an array of tables"),
    }
    Ok(())
}

fn remove_entry(doc: &mut DocumentMut, key: &str, i: usize) -> Result<()> {
    match doc.get_mut(key) {
        Some(Item::ArrayOfTables(a)) => {
            a.remove(i);
        }
        Some(Item::Value(toml_edit::Value::Array(a))) => {
            a.remove(i);
        }
        _ => bail!("`{key}` is not an array of tables"),
    }
    if entries_len(doc, key) == 0 {
        doc.remove(key);
    }
    Ok(())
}

/// Handle one `project.write` action; returns the relative path when the file changed.
pub fn write(ctx: &Ctx, args: &Value) -> Result<Option<String>> {
    let _write_guard = CONFIG_WRITE.lock();
    let rel = args.get_path("path").and_then(Value::as_str).ok_or_else(|| anyhow!("project.write needs `path`"))?;
    check_path(rel)?;
    let abs = ctx.project.root().join(rel);
    let src = match std::fs::read_to_string(&abs) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(e).with_context(|| format!("read {rel}")),
    };
    match apply(src.as_deref(), args).with_context(|| rel.to_string())? {
        WriteOutcome::Unchanged => Ok(None),
        WriteOutcome::Write(text) => {
            ctx.project.write_file(rel, &text)?;
            Ok(Some(rel.to_string()))
        }
        WriteOutcome::Delete => {
            std::fs::remove_file(&abs).with_context(|| format!("delete {rel}"))?;
            Ok(Some(rel.to_string()))
        }
    }
}

pub fn register_queries(ctx: &Ctx) {
    audio_inputs::start(ctx);
    sources::start(ctx);
    fx::start(ctx);
    let root = ctx.project.root().to_path_buf();
    ctx.hub.register_query(
        "project.read",
        Arc::new(move |_, args| {
            let root = root.clone();
            Box::pin(async move {
                let rel = args.get_path("path").and_then(Value::as_str).ok_or("project.read needs `path`")?.to_string();
                check_path(&rel).map_err(|e| e.to_string())?;
                match std::fs::read_to_string(root.join(&rel)) {
                    Ok(text) => Ok(Value::map().with("path", rel).with("exists", true).with("text", text)),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Value::map().with("path", rel).with("exists", false).with("text", "")),
                    Err(e) => Err(format!("{rel}: {e}")),
                }
            })
        }),
    );
    let config = ctx.config.clone();
    ctx.hub.register_query(
        "project.files",
        Arc::new(move |_, args| {
            let kind = args.get_path("kind").and_then(Value::as_str).map(String::from);
            let files: Vec<Value> = config
                .lock()
                .files
                .iter()
                .filter_map(|(key, path)| {
                    let (k, name) = key.rsplit_once('/')?;
                    kind.as_deref()
                        .is_none_or(|want| k == want || k.starts_with(&format!("{want}/")))
                        .then(|| Value::map().with("kind", k).with("name", name).with("path", path.clone()))
                })
                .collect();
            Box::pin(async move { Ok(Value::List(files)) })
        }),
    );
}

/// Modes that are not "on air" for the uptime counter (every other mode counts as live).
const OFF_AIR: &[&str] = &["offline", "preshow", "rehearsal"];
const LIVE_SINCE: &str = "show.live_since";

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Publish `show.live_since` (unix ms, 0 when off air), surviving engine restarts via the kv store.
pub fn start_uptime(ctx: &Ctx) {
    ctx.hub.declare(
        LIVE_SINCE,
        Meta::int(0, [0.0, 9.0e15]).readonly().owner("engine").unit("unix_ms").describe("wall-clock time the show went on air (0 = off air)"),
    );
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let mut since = ctx.db.kv_get("show", "live_since").ok().flatten().and_then(|v| v.as_i64()).unwrap_or(0);
        let mut published = -1i64;
        let mut bus = ctx.hub.subscribe();
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(2));
        loop {
            tokio::select! {
                m = bus.recv() => match m {
                    Ok(b) => {
                        if !matches!(&*b, se_hub::Bus::Event(e) if e.ty == "mode.changed") {
                            continue;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => return,
                },
                _ = tick.tick() => {}
            }
            let snap = ctx.hub.snapshot.load();
            let Some(mode) = snap.str("show.mode") else { continue };
            let on_air = !OFF_AIR.contains(&mode);
            let next = match (on_air, since) {
                (true, 0) => now_ms(),
                (false, _) => 0,
                (true, s) => s,
            };
            if next != since {
                since = next;
                if let Err(e) = ctx.db.kv_set("show", "live_since", &Value::Int(since)) {
                    tracing::warn!("persist live_since: {e:#}");
                }
            }
            if since != published {
                published = since;
                ctx.hub.publish(LIVE_SINCE, Value::Int(since));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(pairs: Vec<(&str, Value)>) -> Value {
        let mut m = Value::map();
        for (k, v) in pairs {
            m = m.with(k, v);
        }
        m
    }

    #[test]
    fn paths_are_confined_to_config_files() {
        assert!(check_path("bindings/bass.toml").is_ok());
        assert!(check_path("layouts/show-3disp.toml").is_ok());
        for bad in ["", "/etc/passwd.toml", "../x.toml", "bindings/../../x.toml", "sessions/a.toml", ".git/x.toml", "rules/x.lua", "a/.hidden/x.toml"] {
            assert!(check_path(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn set_keeps_comments_and_creates_files() {
        let src = "# bass → shake\ntarget = \"fx.x.amount\" # the target\nsignal = \"band.bass\"\ngain = 1.0\n";
        let out = apply(Some(src), &args(vec![("set", Value::map().with("gain", 2.5).with("curve", "exp").with("signal", Value::Null))])).unwrap();
        let WriteOutcome::Write(t) = out else { panic!("{out:?}") };
        assert!(t.contains("# bass → shake"));
        assert!(t.contains("target = \"fx.x.amount\" # the target"));
        assert!(t.contains("gain = 2.5"));
        assert!(t.contains("curve = \"exp\""));
        assert!(!t.contains("signal"));
        let fresh = apply(None, &args(vec![("set", Value::map().with("target", "a.b").with("signal", "lfo.slow").with("range", vec![0.0, 1.0]))])).unwrap();
        let WriteOutcome::Write(t) = fresh else { panic!() };
        let parsed: toml::Table = t.parse().unwrap();
        assert_eq!(parsed["signal"].as_str(), Some("lfo.slow"));
        assert_eq!(parsed["range"].as_array().unwrap().len(), 2);
        // dotted keys create nested tables
        let WriteOutcome::Write(t) = apply(None, &args(vec![("set", Value::map().with("keys.take", vec!["Enter"]))])).unwrap() else { panic!() };
        assert_eq!(t.parse::<toml::Table>().unwrap()["keys"]["take"][0].as_str(), Some("Enter"));
    }

    #[test]
    fn edits_rule_entries_by_match_index_and_append() {
        let src = "# cheers\n[[rule]]\nname = \"big cheer\" # keep\nwhen = \"twitch.cheer\"\ndo = [\"preset.fire hype\"]\n\n[[rule]]\nname = \"small\"\nwhen = \"twitch.cheer\"\n";
        let WriteOutcome::Write(t) = apply(
            Some(src),
            &args(vec![("table", "rule".into()), ("match", Value::map().with("name", "small")), ("set", Value::map().with("if", "event.bits < 100"))]),
        )
        .unwrap() else {
            panic!()
        };
        assert!(t.contains("name = \"big cheer\" # keep"));
        let parsed: toml::Table = t.parse().unwrap();
        assert_eq!(parsed["rule"][1]["if"].as_str(), Some("event.bits < 100"));
        assert!(parsed["rule"][0].get("if").is_none());
        let WriteOutcome::Write(t2) = apply(
            Some(&t),
            &args(vec![("table", "rule".into()), ("append", true.into()), ("set", Value::map().with("name", "new").with("when", "twitch.raid"))]),
        )
        .unwrap() else {
            panic!()
        };
        assert_eq!(t2.parse::<toml::Table>().unwrap()["rule"].as_array().unwrap().len(), 3);
        assert!(t2.starts_with("# cheers"));
        let WriteOutcome::Write(t3) = apply(Some(&t2), &args(vec![("table", "rule".into()), ("index", Value::Int(0)), ("delete", true.into())])).unwrap()
        else {
            panic!()
        };
        let p3: toml::Table = t3.parse().unwrap();
        assert_eq!(p3["rule"].as_array().unwrap().len(), 2);
        assert_eq!(p3["rule"][0]["name"].as_str(), Some("small"));
        // ambiguous and missing selectors are errors, not guesses
        assert!(
            apply(Some(src), &args(vec![("table", "rule".into()), ("match", Value::map().with("when", "twitch.cheer")), ("set", Value::map().with("x", 1))]))
                .is_err()
        );
        assert!(
            apply(Some(src), &args(vec![("table", "rule".into()), ("match", Value::map().with("name", "nope")), ("set", Value::map().with("x", 1))])).is_err()
        );
        assert!(apply(Some(src), &args(vec![("table", "rule".into()), ("index", Value::Int(5)), ("set", Value::map().with("x", 1))])).is_err());
    }

    #[test]
    fn text_delete_and_noop() {
        assert!(apply(None, &args(vec![("text", "not = [toml".into())])).is_err());
        assert_eq!(apply(Some("a = 1\n"), &args(vec![("text", "a = 1\n".into())])).unwrap(), WriteOutcome::Unchanged);
        assert_eq!(apply(Some("a = 1\n"), &args(vec![("delete", true.into())])).unwrap(), WriteOutcome::Delete);
        assert_eq!(apply(None, &args(vec![("delete", true.into())])).unwrap(), WriteOutcome::Unchanged);
        assert_eq!(apply(Some("a = 1\n"), &args(vec![("set", Value::map().with("a", 1))])).unwrap(), WriteOutcome::Unchanged);
        assert!(apply(Some("a = 1\n"), &args(vec![])).is_err());
    }
}
