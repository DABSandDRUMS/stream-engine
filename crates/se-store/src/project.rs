//! Project directory (§3.3): load, watch (hot reload), round-trip writes with `toml_edit`
//! (comments and formatting preserved, atomic temp+rename), and schema migrations.

use anyhow::{Context, Result, anyhow, bail};
use parking_lot::Mutex;
use se_core::config::{ConfigError, SCHEMA_VERSION, SourceFile};
use se_proto::Value;
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::time::Duration;
use toml_edit::{DocumentMut, Item, Table};

const SKIP_DIRS: &[&str] = &[".git", "sessions", ".backups", "assets", "node_modules", "target", ".cache"];

pub struct Project {
    root: PathBuf,
    /// Content hashes of files we wrote, so the watcher ignores our own writes.
    written: Mutex<HashMap<PathBuf, u64>>,
}

pub struct Loaded {
    pub files: Vec<SourceFile>,
    pub errors: Vec<ConfigError>,
    /// Relative paths that failed to parse (keep their last good version).
    pub failed: Vec<String>,
}

fn hash(bytes: &[u8]) -> u64 {
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    h.finish()
}

impl Project {
    pub fn open(root: impl Into<PathBuf>) -> Result<Project> {
        let root: PathBuf = root.into();
        let root = root.canonicalize().with_context(|| format!("project directory {}", root.display()))?;
        if !root.join("project.toml").exists() {
            bail!("{} has no project.toml", root.display());
        }
        Ok(Project { root, written: Mutex::new(HashMap::new()) })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn rel(&self, p: &Path) -> String {
        p.strip_prefix(&self.root).unwrap_or(p).to_string_lossy().replace('\\', "/")
    }

    /// Classify a project file: `(kind, name)`; `None` for non-config files.
    pub fn classify(&self, p: &Path) -> Option<(String, String)> {
        let rel = p.strip_prefix(&self.root).ok()?;
        if p.extension()? != "toml" {
            return None;
        }
        let parts: Vec<&str> = rel.iter().filter_map(|s| s.to_str()).collect();
        if parts.iter().any(|s| SKIP_DIRS.contains(s) || s.starts_with('.')) {
            return None;
        }
        let stem = p.file_stem()?.to_str()?.to_string();
        match parts.as_slice() {
            ["project.toml"] => Some(("project".into(), "project".into())),
            [_] => None,
            ["patches", id, "patch.toml"] => Some(("patches".into(), id.to_string())),
            ["patches", ..] => None,
            dirs => Some((dirs[..dirs.len() - 1].join("/"), stem)),
        }
    }

    fn walk(&self, dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else { return };
        let mut entries: Vec<_> = rd.flatten().collect();
        entries.sort_by_key(|e| e.file_name());
        for e in entries {
            let p = e.path();
            let name = e.file_name().to_string_lossy().to_string();
            if p.is_dir() {
                if !SKIP_DIRS.contains(&name.as_str()) && !name.starts_with('.') {
                    self.walk(&p, out);
                }
            } else if name.ends_with(".toml") {
                out.push(p);
            }
        }
    }

    /// Parse every config file. Broken files are reported, never fatal.
    pub fn load(&self) -> Loaded {
        let mut paths = Vec::new();
        self.walk(&self.root, &mut paths);
        let mut files = Vec::new();
        let mut errors = Vec::new();
        let mut failed = Vec::new();
        for p in paths {
            let Some((kind, name)) = self.classify(&p) else { continue };
            let rel = self.rel(&p);
            match std::fs::read_to_string(&p).map_err(|e| e.to_string()).and_then(|s| s.parse::<toml::Table>().map_err(|e| e.to_string())) {
                Ok(table) => files.push(SourceFile { kind, name, path: rel, table }),
                Err(msg) => {
                    errors.push(ConfigError { file: rel.clone(), msg });
                    failed.push(rel);
                }
            }
        }
        Loaded { files, errors, failed }
    }

    /// Watch the project; `on_change` receives relative paths of changed files, debounced,
    /// excluding the engine's own writes.
    pub fn watch(self: &std::sync::Arc<Self>, on_change: impl Fn(Vec<String>) + Send + 'static) -> Result<notify::RecommendedWatcher> {
        use notify::{RecursiveMode, Watcher};
        let (tx, rx) = crossbeam_channel::unbounded::<PathBuf>();
        let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res {
                if matches!(ev.kind, notify::EventKind::Access(_)) {
                    return;
                }
                for p in ev.paths {
                    let _ = tx.send(p);
                }
            }
        })?;
        w.watch(&self.root, RecursiveMode::Recursive)?;
        let me = self.clone();
        std::thread::Builder::new().name("se-watch".into()).spawn(move || {
            while let Ok(first) = rx.recv() {
                let mut set = vec![first];
                while let Ok(p) = rx.recv_timeout(Duration::from_millis(120)) {
                    set.push(p);
                }
                set.sort();
                set.dedup();
                let changed: Vec<String> = set
                    .into_iter()
                    .filter(|p| {
                        let rel = me.rel(p);
                        let relevant = rel.ends_with(".toml") || rel.starts_with("patches/") || rel.starts_with("transitions/") || rel.starts_with("assets/");
                        if !relevant || rel.starts_with("sessions/") || rel.starts_with(".") || rel.contains("/.") {
                            return false;
                        }
                        // skip our own writes
                        match std::fs::read(p) {
                            Ok(bytes) => me.written.lock().get(p) != Some(&hash(&bytes)),
                            Err(_) => true, // deleted
                        }
                    })
                    .map(|p| me.rel(&p))
                    .collect();
                if !changed.is_empty() {
                    on_change(changed);
                }
            }
        })?;
        Ok(w)
    }

    /// Atomic write (temp + rename), remembered so the watcher ignores it.
    pub fn write_file(&self, rel: &str, content: &str) -> Result<()> {
        self.write_bytes(rel, content.as_bytes())
    }

    /// [`Project::write_file`] for any bytes (project versions put back binary files too).
    pub fn write_bytes(&self, rel: &str, content: &[u8]) -> Result<()> {
        let p = self.root.join(rel);
        if let Some(dir) = p.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let tmp = p.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::write(&tmp, content)?;
        self.written.lock().insert(p.clone(), hash(content));
        std::fs::rename(&tmp, &p)?;
        Ok(())
    }

    /// Edit a TOML file in place, preserving comments and formatting.
    pub fn edit(&self, rel: &str, f: impl FnOnce(&mut DocumentMut) -> Result<()>) -> Result<()> {
        let p = self.root.join(rel);
        let src = if p.exists() { std::fs::read_to_string(&p)? } else { String::new() };
        let mut doc: DocumentMut = src.parse().with_context(|| format!("parse {rel}"))?;
        f(&mut doc)?;
        let out = doc.to_string();
        if out != src {
            self.write_file(rel, &out)?;
        }
        Ok(())
    }

    /// Persist a base value edited in the UI to the file it belongs to.
    /// Scene node properties go to `scenes/<scene>.toml`; everything else to `project.toml [params]`.
    pub fn write_base(&self, address: &str, value: &Value) -> Result<String> {
        let segs: Vec<&str> = address.split('.').collect();
        if let Some((target, property)) = crate::fx::address_target(address)? {
            let rel = target.path();
            self.edit(&rel, |doc| crate::fx::write_property(doc, &target, &property, value))?;
            return Ok(rel);
        }
        if segs.len() >= 5 && segs[0] == "scene" && segs[2] == "node" {
            let (scene, node, prop) = (segs[1], segs[3], &segs[4..]);
            let rel = format!("scenes/{scene}.toml");
            if self.root.join(&rel).exists() {
                self.edit(&rel, |doc| write_node_prop(doc, node, prop, value))?;
                return Ok(rel);
            }
        }
        let rel = "project.toml".to_string();
        self.edit(&rel, |doc| {
            let params = doc.entry("params").or_insert(Item::Table(Table::new()));
            let t = params.as_table_like_mut().ok_or_else(|| anyhow!("[params] is not a table"))?;
            match to_edit_value(value) {
                Some(v) => {
                    t.insert(address, Item::Value(v));
                }
                None => {
                    t.remove(address);
                }
            }
            Ok(())
        })?;
        Ok(rel)
    }

    /// Upgrade older projects (backing up first). Returns the applied steps.
    pub fn migrate(&self) -> Result<Vec<String>> {
        let src = std::fs::read_to_string(self.root.join("project.toml"))?;
        let doc: DocumentMut = src.parse()?;
        let mut schema = doc.get("schema").and_then(|v| v.as_integer()).unwrap_or(0);
        if schema > SCHEMA_VERSION {
            bail!("project schema {schema} is newer than this engine ({SCHEMA_VERSION})");
        }
        if schema == SCHEMA_VERSION {
            return Ok(vec![]);
        }
        let backup = self.backup()?;
        let mut steps = vec![format!("backup at {}", backup.display())];
        while schema < SCHEMA_VERSION {
            let step = MIGRATIONS.iter().find(|(from, _, _)| *from == schema).ok_or_else(|| anyhow!("no migration from schema {schema}"))?;
            (step.2)(self)?;
            steps.push(format!("schema {} → {}: {}", schema, schema + 1, step.1));
            schema += 1;
            self.edit("project.toml", |d| {
                d["schema"] = toml_edit::value(schema);
                Ok(())
            })?;
        }
        Ok(steps)
    }

    /// Copy config files (not sessions/assets) to `.backups/<unix-time>/`.
    pub fn backup(&self) -> Result<PathBuf> {
        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_secs();
        let dst = self.root.join(".backups").join(ts.to_string());
        let mut paths = Vec::new();
        self.walk(&self.root, &mut paths);
        for p in paths {
            let rel = p.strip_prefix(&self.root)?;
            let d = dst.join(rel);
            std::fs::create_dir_all(d.parent().unwrap())?;
            std::fs::copy(&p, &d)?;
        }
        Ok(dst)
    }
}

type Migration = (i64, &'static str, fn(&Project) -> Result<()>);

/// Schema migrations; each upgrades from `from` to `from + 1`.
pub const MIGRATIONS: &[Migration] = &[(0, "add schema version and default canvases", migrate_0_to_1)];

fn migrate_0_to_1(p: &Project) -> Result<()> {
    p.edit("project.toml", |d| {
        if !d.contains_key("canvas") {
            let mut c = Table::new();
            c.set_implicit(true);
            let mut wide = Table::new();
            wide["width"] = toml_edit::value(1920);
            wide["height"] = toml_edit::value(1080);
            wide["fps"] = toml_edit::value(60);
            let mut tall = Table::new();
            tall["width"] = toml_edit::value(1080);
            tall["height"] = toml_edit::value(1920);
            tall["fps"] = toml_edit::value(60);
            c.insert("wide", Item::Table(wide));
            c.insert("tall", Item::Table(tall));
            d.insert("canvas", Item::Table(c));
        }
        Ok(())
    })
}

pub fn to_edit_value(v: &Value) -> Option<toml_edit::Value> {
    Some(match v {
        Value::Null => return None,
        Value::Bool(b) => (*b).into(),
        Value::Int(i) => (*i).into(),
        Value::Float(f) => {
            // keep short decimals readable
            let r = (f * 1e6).round() / 1e6;
            r.into()
        }
        Value::Str(s) => s.as_str().into(),
        Value::List(l) => {
            let mut a = toml_edit::Array::new();
            for x in l {
                if let Some(v) = to_edit_value(x) {
                    a.push(v);
                }
            }
            toml_edit::Value::Array(a)
        }
        Value::Map(m) => {
            let mut t = toml_edit::InlineTable::new();
            for (k, x) in m {
                if let Some(v) = to_edit_value(x) {
                    t.insert(k, v);
                }
            }
            toml_edit::Value::InlineTable(t)
        }
    })
}

/// Visit every node table (inline or array-of-tables) whose id/src matches, in all canvases
/// (or only `canvas` when given).
fn for_each_node(doc: &mut DocumentMut, node: &str, canvas: Option<&str>, mut f: impl FnMut(&mut dyn toml_edit::TableLike) -> Result<()>) -> Result<usize> {
    let mut n = 0;
    let Some(canvases) = doc.get_mut("canvas").and_then(|c| c.as_table_like_mut()) else { return Ok(0) };
    for (cname, citem) in canvases.iter_mut() {
        if canvas.is_some_and(|c| c != cname.get()) {
            continue;
        }
        let Some(ct) = citem.as_table_like_mut() else { continue };
        let Some(nodes) = ct.get_mut("nodes") else { continue };
        let matches = |t: &dyn toml_edit::TableLike| {
            let id = t.get("id").and_then(|v| v.as_str()).or_else(|| t.get("src").and_then(|v| v.as_str()));
            id == Some(node)
        };
        match nodes {
            Item::Value(toml_edit::Value::Array(arr)) => {
                for v in arr.iter_mut() {
                    if let Some(t) = v.as_inline_table_mut()
                        && matches(t)
                    {
                        f(t)?;
                        n += 1;
                    }
                }
            }
            Item::ArrayOfTables(aot) => {
                for t in aot.iter_mut() {
                    if matches(t) {
                        f(t)?;
                        n += 1;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(n)
}

const PER_CANVAS: &[&str] = &["rect", "crop", "radius", "z"];

fn write_node_prop(doc: &mut DocumentMut, node: &str, prop: &[&str], value: &Value) -> Result<()> {
    let set = |t: &mut dyn toml_edit::TableLike, key: &str, value: &Value| match to_edit_value(value) {
        Some(v) => match t.get_mut(key) {
            Some(Item::Value(old)) => {
                let decor = old.decor().clone();
                *old = v;
                *old.decor_mut() = decor;
            }
            _ => {
                t.insert(key, Item::Value(v));
            }
        },
        None => {
            t.remove(key);
        }
    };
    let n = match prop {
        [p, canvas] if PER_CANVAS.contains(p) => for_each_node(doc, node, Some(canvas), |t| {
            set(t, p, value);
            Ok(())
        })?,
        [p] => for_each_node(doc, node, None, |t| {
            set(t, p, value);
            Ok(())
        })?,
        _ => bail!("unsupported scene property `{}`", prop.join(".")),
    };
    if n == 0 {
        bail!("node `{node}` not found");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_project(files: &[(&str, &str)]) -> (tempfile::TempDir, Project) {
        let d = tempfile::tempdir().unwrap();
        for (p, c) in files {
            let path = d.path().join(p);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, c).unwrap();
        }
        let p = Project::open(d.path()).unwrap();
        (d, p)
    }

    const SCENE: &str = r#"# The duo scene: face + desk
key = 1 # number key

[canvas.wide]
nodes = [
  { src = "cam_face", rect = [0.02, 0.05, 0.47, 0.90], radius = 24 }, # left
  { src = "cam_desk", rect = [0.51, 0.05, 0.47, 0.90], fx = [{ name = "vhs", amount = 0.3 }] },
]

[[canvas.tall.nodes]]
src = "cam_face"   # top half
rect = [0.0, 0.0, 1.0, 0.55]
"#;

    #[test]
    fn load_classifies_and_reports_errors() {
        let (_d, p) = tmp_project(&[
            ("project.toml", "schema = 1"),
            ("scenes/duo.toml", SCENE),
            ("lights/cuelists/main.toml", "cues = []"),
            ("patches/meteors/patch.toml", "kind = \"script\""),
            ("patches/meteors/extra.toml", "x = 1"),
            ("rules/bad.toml", "when = "),
            ("sessions/x/meta.toml", "a = 1"),
        ]);
        let l = p.load();
        let kinds: Vec<(String, String)> = l.files.iter().map(|f| (f.kind.clone(), f.name.clone())).collect();
        assert!(kinds.contains(&("scenes".into(), "duo".into())));
        assert!(kinds.contains(&("lights/cuelists".into(), "main".into())));
        assert!(kinds.contains(&("patches".into(), "meteors".into())));
        assert!(!kinds.iter().any(|(k, _)| k == "sessions" || k == "patches/meteors"));
        assert_eq!(l.failed, vec!["rules/bad.toml".to_string()]);
    }

    #[test]
    fn write_back_preserves_comments() {
        let (_d, p) = tmp_project(&[("project.toml", "schema = 1\n# my params\n"), ("scenes/duo.toml", SCENE)]);
        p.write_base("scene.duo.node.cam_face.rect.wide", &Value::from([0.1f32, 0.1, 0.4, 0.8])).unwrap();
        p.write_base("scene.duo.node.cam_face.rect.tall", &Value::from([0.0f32, 0.0, 1.0, 0.5])).unwrap();
        p.write_base("scene.duo.node.cam_face.opacity", &Value::Float(0.5)).unwrap();
        p.write_base("scene.duo.node.cam_desk.fx.vhs.amount", &Value::Float(0.7)).unwrap();
        p.write_base("fx.grade.amount", &Value::Float(0.25)).unwrap();
        let s = std::fs::read_to_string(p.root().join("scenes/duo.toml")).unwrap();
        assert!(s.contains("# The duo scene: face + desk"), "{s}");
        assert!(s.contains("# number key"));
        assert!(s.contains("# left"));
        assert!(s.contains("# top half"));
        assert!(s.contains("rect = [0.1, 0.1, 0.4, 0.8]"), "{s}");
        assert!(s.contains("rect = [0.0, 0.0, 1.0, 0.5]"), "{s}");
        assert_eq!(s.matches("opacity = 0.5").count(), 2, "shared prop on both canvases: {s}");
        assert!(s.contains("amount = 0.7"));
        let t: toml::Table = s.parse().unwrap();
        assert!(t.get("canvas").is_some());
        let proj = std::fs::read_to_string(p.root().join("project.toml")).unwrap();
        assert!(proj.contains("# my params"));
        assert!(proj.contains("\"fx.grade.amount\" = 0.25"), "{proj}");
        // round trip through the typed config
        let l = p.load();
        let c = se_core::Config::build(&l.files);
        assert!(c.errors.is_empty(), "{:?}", c.errors);
        assert_eq!(c.scenes["duo"].nodes("wide")[0].rect, [0.1, 0.1, 0.4, 0.8]);
        assert_eq!(c.project.params["fx.grade.amount"], Value::Float(0.25));
    }

    #[test]
    fn migrate_from_schema_zero_with_backup() {
        let (_d, p) = tmp_project(&[("project.toml", "# old project\nname = \"x\"\n")]);
        let steps = p.migrate().unwrap();
        assert!(steps.len() >= 2, "{steps:?}");
        let s = std::fs::read_to_string(p.root().join("project.toml")).unwrap();
        assert!(s.contains("# old project"));
        assert!(s.contains("schema = 1"));
        assert!(s.contains("[canvas.wide]"));
        assert!(p.root().join(".backups").read_dir().unwrap().next().is_some());
        assert!(p.migrate().unwrap().is_empty(), "idempotent");
    }
}
