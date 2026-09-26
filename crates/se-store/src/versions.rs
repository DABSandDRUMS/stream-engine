//! Project versions: automatic, content-addressed snapshots of the project directory under
//! `<data_dir>/versions/`, so any edit (from the UI or a text editor) can be undone and any
//! earlier state brought back.
//!
//! Layout:
//! * `objects/<2 hex>/<62 hex>[.zst]`: file contents by blake3 hash, stored once however many
//!   versions use them. Text is zstd-compressed (`.zst`); binary media is stored as is.
//! * `manifests/<id>.json`: one per version (when, what kind, every file's hash, and which
//!   paths changed since the version before).
//! * `stat-cache.json`: size + modification time → hash of the last scan, so unchanged files
//!   (big media) aren't read again.
//!
//! Tracked: everything in the project except `sessions/`, hidden files and folders,
//! `node_modules/` and `target/` (build output), symlinks, and editor scratch files.
//!
//! Retention ([`retained`]): everything from the last 7 days, then the newest per day for 90
//! days; named versions and the newest version are kept forever.

use crate::Project;
use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

const DAY_MS: i64 = 86_400_000;
/// Files up to this size are read whole (text among them is compressed); bigger ones are
/// hashed and copied as a stream.
const WHOLE_LIMIT: u64 = 32 << 20;
/// Leading bytes checked for a NUL to tell text from binary.
const SNIFF: usize = 8192;
const ZSTD_LEVEL: i32 = 9;

/// One file of a version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    pub hash: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub exec: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// Saved automatically after edits.
    Edit,
    /// Saved by the user with a name (kept forever).
    Named,
    /// "Go back to this": the project was put back to `restored_from`.
    Restore,
    /// `project.undo`: put back the state before the latest change.
    Undo,
    /// `project.redo`: took back the undo right before it.
    Redo,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Edit => "edit",
            Kind::Named => "named",
            Kind::Restore => "restore",
            Kind::Undo => "undo",
            Kind::Redo => "redo",
        }
    }

    /// Undo/redo entries stand for the state they put back: undoing again keeps stepping back
    /// from there instead of undoing the undo.
    fn replays(self) -> bool {
        matches!(self, Kind::Undo | Kind::Redo)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Version {
    pub id: String,
    pub at_ms: i64,
    pub kind: Kind,
    /// The user's name for a named version.
    #[serde(default)]
    pub label: String,
    /// Project paths added, changed, or removed since the version before it.
    #[serde(default)]
    pub changed: Vec<String>,
    /// The very first version (nothing before it to compare with).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub first: bool,
    /// Restore/undo/redo: the version whose files were put back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub restored_from: Option<String>,
    /// Undo: the version that was current before it (what redo brings back).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub undo_of: Option<String>,
    /// Hash of the whole file list: equal trees have equal hashes.
    pub tree: String,
    pub files: BTreeMap<String, FileEntry>,
}

impl Version {
    /// Saved by the engine rather than named by the user.
    pub fn auto(&self) -> bool {
        self.kind != Kind::Named
    }
}

/// What undo (or redo) would change.
#[derive(Clone, Debug, PartialEq)]
pub struct Preview {
    /// The change being taken back (or brought back): its kind and time.
    pub kind: Kind,
    pub at_ms: i64,
    /// Paths that would change on disk.
    pub changed: Vec<String>,
}

#[derive(Default, Serialize, Deserialize)]
struct StatCache {
    /// Wall clock (ns) when the files were looked at.
    scanned_ns: i64,
    /// path → (size, modification time ns, hash)
    files: HashMap<String, (u64, i64, String)>,
}

/// A directory the versions never look into (`top`: directly in the project folder).
fn skip_dir(name: &str, top: bool) -> bool {
    name.starts_with('.') || name == "node_modules" || name == "target" || (top && name == "sessions")
}

/// Hidden files, editor backups/swap files and in-flight atomic writes (`x.tmp.<pid>`).
fn skip_file(name: &str) -> bool {
    name.starts_with('.') || name.ends_with('~') || name.ends_with(".swp") || name.ends_with(".swo") || name.contains(".tmp.") || name == "4913"
}

/// Is the project-relative path `rel` part of project versions?
pub fn tracked(rel: &str) -> bool {
    let n = rel.split('/').count();
    rel.split('/').enumerate().all(|(i, c)| !c.is_empty() && !skip_dir(c, i == 0) && (i + 1 < n || !skip_file(c)))
}

/// Watch the project for changes to tracked paths from anyone (the engine, a text editor, a
/// file manager). `on_change` runs once per batch of filesystem events that touches one.
pub fn watch(root: &Path, on_change: impl Fn() + Send + 'static) -> Result<notify::RecommendedWatcher> {
    use notify::{RecursiveMode, Watcher};
    let base = root.to_path_buf();
    let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        let Ok(ev) = res else { return };
        if matches!(ev.kind, notify::EventKind::Access(_)) {
            return;
        }
        if ev.paths.iter().any(|p| p.strip_prefix(&base).ok().and_then(Path::to_str).is_some_and(tracked)) {
            on_change();
        }
    })?;
    w.watch(root, RecursiveMode::Recursive)?;
    Ok(w)
}

fn mtime_ns(m: &std::fs::Metadata) -> i64 {
    m.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos() as i64)
}

fn wall_ns() -> i64 {
    std::time::SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64)
}

fn not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound)
}

/// A hidden temp file next to `dst` (hidden, so neither the versions nor the reload watcher
/// pick it up).
fn temp_beside(dst: &Path) -> PathBuf {
    let name = dst.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    dst.with_file_name(format!(".{name}.se-tmp.{}", std::process::id()))
}

fn write_atomic(dst: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(d) = dst.parent() {
        std::fs::create_dir_all(d)?;
    }
    let tmp = temp_beside(dst);
    std::fs::write(&tmp, bytes).with_context(|| format!("write {}", tmp.display()))?;
    std::fs::rename(&tmp, dst).with_context(|| format!("rename to {}", dst.display()))?;
    Ok(())
}

fn tree_hash(files: &BTreeMap<String, FileEntry>) -> String {
    let mut h = blake3::Hasher::new();
    for (rel, f) in files {
        h.update(rel.as_bytes());
        h.update(&[0]);
        h.update(f.hash.as_bytes());
        h.update(&[u8::from(f.exec), b'\n']);
    }
    h.finalize().to_hex().to_string()
}

/// Paths that differ between two file lists (added, changed, or removed), sorted.
pub fn diff(a: &BTreeMap<String, FileEntry>, b: &BTreeMap<String, FileEntry>) -> Vec<String> {
    let mut out: Vec<String> = b.iter().filter(|(k, f)| a.get(*k).is_none_or(|g| g.hash != f.hash || g.exec != f.exec)).map(|(k, _)| k.clone()).collect();
    out.extend(a.keys().filter(|k| !b.contains_key(*k)).cloned());
    out.sort();
    out
}

/// Which versions the retention policy keeps (same order as `list`, oldest first): every
/// version from the last 7 days, the newest per (UTC) day for 90 days, named versions and the
/// newest version forever.
pub fn retained(list: &[Version], now_ms: i64) -> Vec<bool> {
    let mut keep = vec![false; list.len()];
    let mut days = HashSet::new();
    for (i, v) in list.iter().enumerate().rev() {
        let age = now_ms - v.at_ms;
        keep[i] = if i + 1 == list.len() || v.kind == Kind::Named || age <= 7 * DAY_MS {
            true
        } else if age <= 90 * DAY_MS {
            days.insert(v.at_ms.div_euclid(DAY_MS))
        } else {
            false
        };
    }
    keep
}

/// The project's version history (oldest first).
pub struct Versions {
    dir: PathBuf,
    list: Vec<Version>,
    cache: StatCache,
    whole_limit: u64,
}

impl Versions {
    /// Open (or create) the store in `dir` and load every version.
    pub fn open(dir: &Path) -> Result<Versions> {
        for sub in ["objects", "manifests"] {
            std::fs::create_dir_all(dir.join(sub)).with_context(|| format!("create {}", dir.join(sub).display()))?;
        }
        let mut list = Vec::new();
        for e in std::fs::read_dir(dir.join("manifests"))?.flatten() {
            let p = e.path();
            if p.extension().is_none_or(|x| x != "json") {
                continue;
            }
            match std::fs::read(&p).map_err(anyhow::Error::from).and_then(|b| Ok(serde_json::from_slice::<Version>(&b)?)) {
                Ok(v) => list.push(v),
                Err(e) => tracing::warn!("skipping unreadable version {}: {e:#}", p.display()),
            }
        }
        list.sort_by_key(|v| v.at_ms);
        let cache = std::fs::read(dir.join("stat-cache.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Ok(Versions { dir: dir.to_path_buf(), list, cache, whole_limit: WHOLE_LIMIT })
    }

    /// Every version, oldest first.
    pub fn list(&self) -> &[Version] {
        &self.list
    }

    pub fn get(&self, id: &str) -> Option<&Version> {
        self.index(id).map(|i| &self.list[i])
    }

    fn index(&self, id: &str) -> Option<usize> {
        self.list.iter().position(|v| v.id == id)
    }

    fn object(&self, hash: &str, compressed: bool) -> PathBuf {
        let (a, b) = hash.split_at(2.min(hash.len()));
        self.dir.join("objects").join(a).join(if compressed { format!("{b}.zst") } else { b.to_string() })
    }

    /// Where the contents with `hash` are stored, and whether they are compressed.
    fn locate(&self, hash: &str) -> Option<(PathBuf, bool)> {
        [true, false].into_iter().map(|z| (self.object(hash, z), z)).find(|(p, _)| p.is_file())
    }

    /// Store one file's contents; returns its hash.
    fn store(&self, path: &Path, size: u64) -> Result<String> {
        let mut f = std::fs::File::open(path)?;
        if size <= self.whole_limit {
            let mut buf = Vec::with_capacity(size as usize);
            f.read_to_end(&mut buf)?;
            let hash = blake3::hash(&buf).to_hex().to_string();
            let text = !buf[..buf.len().min(SNIFF)].contains(&0);
            let dst = self.object(&hash, text);
            if !dst.exists() {
                if text {
                    write_atomic(&dst, &zstd::encode_all(&buf[..], ZSTD_LEVEL)?)?;
                } else {
                    write_atomic(&dst, &buf)?;
                }
            }
            return Ok(hash);
        }
        let mut h = blake3::Hasher::new();
        h.update_reader(&mut f)?;
        let hash = h.finalize().to_hex().to_string();
        let dst = self.object(&hash, false);
        if !dst.exists() {
            if let Some(d) = dst.parent() {
                std::fs::create_dir_all(d)?;
            }
            let tmp = temp_beside(&dst);
            std::fs::copy(path, &tmp).with_context(|| format!("copy {}", path.display()))?;
            std::fs::rename(&tmp, &dst)?;
        }
        Ok(hash)
    }

    /// Every tracked file of the project as it is now (contents stored as needed).
    fn scan(&mut self, root: &Path) -> Result<BTreeMap<String, FileEntry>> {
        let started = wall_ns();
        let mut found = Vec::new();
        walk(root, "", &mut found)?;
        // A cached hash is trusted only for files that hadn't changed for a second before the
        // last scan (a same-size rewrite within the clock's resolution would look unchanged).
        let settled = self.cache.scanned_ns - 1_000_000_000;
        let mut files = BTreeMap::new();
        let mut cache = HashMap::with_capacity(found.len());
        for (rel, path, meta) in found {
            let (size, mtime) = (meta.len(), mtime_ns(&meta));
            let cached =
                self.cache.files.get(&rel).filter(|(s, m, h)| *s == size && *m == mtime && *m < settled && self.locate(h).is_some()).map(|(_, _, h)| h.clone());
            let hash = match cached {
                Some(h) => h,
                None => match self.store(&path, size) {
                    Ok(h) => h,
                    Err(e) if not_found(&e) => continue, // deleted while we looked
                    Err(e) => return Err(e.context(format!("save {rel}"))),
                },
            };
            cache.insert(rel.clone(), (size, mtime, hash.clone()));
            files.insert(rel, FileEntry { hash, size, exec: meta.permissions().mode() & 0o111 != 0 });
        }
        self.cache = StatCache { scanned_ns: started, files: cache };
        if let Err(e) = serde_json::to_vec(&self.cache).map_err(anyhow::Error::from).and_then(|b| write_atomic(&self.dir.join("stat-cache.json"), &b)) {
            tracing::warn!("versions: could not save the file cache: {e:#}");
        }
        Ok(files)
    }

    fn push(&mut self, v: Version) -> Result<()> {
        write_atomic(&self.dir.join("manifests").join(format!("{}.json", v.id)), &serde_json::to_vec(&v)?)?;
        self.list.push(v);
        Ok(())
    }

    /// A time for a new version: `now_ms`, but always after the newest one (ids are unique).
    fn next_at(&self, now_ms: i64) -> i64 {
        self.list.last().map_or(now_ms, |p| now_ms.max(p.at_ms + 1))
    }

    /// Record the project as it is now. An automatic snapshot ([`Kind::Edit`]) that finds
    /// nothing changed since the newest version records nothing and returns `None`.
    pub fn snapshot(&mut self, project: &Project, kind: Kind, label: &str, now_ms: i64) -> Result<Option<&Version>> {
        let files = self.scan(project.root())?;
        let tree = tree_hash(&files);
        let prev = self.list.last();
        if kind == Kind::Edit && prev.is_some_and(|p| p.tree == tree) {
            return Ok(None);
        }
        let changed = prev.map_or_else(|| files.keys().cloned().collect(), |p| diff(&p.files, &files));
        let at_ms = self.next_at(now_ms);
        let first = prev.is_none();
        self.push(Version { id: at_ms.to_string(), at_ms, kind, label: label.to_string(), changed, first, restored_from: None, undo_of: None, tree, files })?;
        Ok(self.list.last())
    }

    /// Put the project back to version `id`, after recording the current state so this can be
    /// undone. Files the version didn't have are removed (never anything untracked, like
    /// `sessions/`). Returns the paths that changed on disk.
    pub fn restore(&mut self, project: &Project, id: &str, now_ms: i64) -> Result<Vec<String>> {
        if self.index(id).is_none() {
            bail!("That version isn't there any more.");
        }
        self.snapshot(project, Kind::Edit, "", now_ms)?;
        self.put_back(project, id, Kind::Restore, None, now_ms)
    }

    /// Take back the latest change (again and again: each undo steps further back).
    pub fn undo(&mut self, project: &Project, now_ms: i64) -> Result<Vec<String>> {
        self.snapshot(project, Kind::Edit, "", now_ms)?;
        let (_, target) = self.undo_step().ok_or_else(|| anyhow!("There's nothing to undo yet."))?;
        let latest = self.list.last().map(|v| v.id.clone());
        let target = self.list[target].id.clone();
        self.put_back(project, &target, Kind::Undo, latest, now_ms)
    }

    /// Take back the undo that was just done (only while nothing changed since).
    pub fn redo(&mut self, project: &Project, now_ms: i64) -> Result<Vec<String>> {
        self.snapshot(project, Kind::Edit, "", now_ms)?;
        let target = self.redo_target().ok_or_else(|| anyhow!("There's nothing to redo."))?;
        let target = self.list[target].id.clone();
        self.put_back(project, &target, Kind::Redo, None, now_ms)
    }

    /// For undo/redo entries, the version whose state they stand for.
    fn resolve(&self, i: usize) -> usize {
        let v = &self.list[i];
        match &v.restored_from {
            Some(src) if v.kind.replays() => self.index(src).unwrap_or(i),
            _ => i,
        }
    }

    /// `(position, target)`: the version the project is at and the one undo goes back to (the
    /// newest older version that differs).
    fn undo_step(&self) -> Option<(usize, usize)> {
        let pos = self.resolve(self.list.len().checked_sub(1)?);
        let tree = &self.list[pos].tree;
        let target = self.list[..pos].iter().rposition(|v| &v.tree != tree)?;
        Some((pos, target))
    }

    fn redo_target(&self) -> Option<usize> {
        let v = self.list.last()?;
        let i = self.index(v.undo_of.as_deref().filter(|_| v.kind == Kind::Undo)?)?;
        Some(self.resolve(i))
    }

    /// What `undo` would take back (as of the newest version).
    pub fn undo_preview(&self) -> Option<Preview> {
        let (pos, target) = self.undo_step()?;
        // everything after `target` up to `pos` has the same files: the change happened first
        let when = &self.list[target + 1];
        Some(Preview { kind: when.kind, at_ms: when.at_ms, changed: diff(&self.list[target].files, &self.list[pos].files) })
    }

    /// What `redo` would bring back, if the newest version is an undo.
    pub fn redo_preview(&self) -> Option<Preview> {
        let t = &self.list[self.redo_target()?];
        let cur = self.list.last()?;
        Some(Preview { kind: t.kind, at_ms: t.at_ms, changed: diff(&cur.files, &t.files) })
    }

    /// Write version `id`'s files over the project (which matches the newest version) and
    /// record the result as a new version of `kind`.
    fn put_back(&mut self, project: &Project, id: &str, kind: Kind, undo_of: Option<String>, now_ms: i64) -> Result<Vec<String>> {
        let src = self.resolve(self.index(id).ok_or_else(|| anyhow!("That version isn't there any more."))?);
        let cur = self.list.last().ok_or_else(|| anyhow!("There are no versions yet."))?;
        let want = &self.list[src];
        let writes: Vec<(&String, &FileEntry)> =
            want.files.iter().filter(|(k, f)| cur.files.get(*k).is_none_or(|c| c.hash != f.hash || c.exec != f.exec)).collect();
        let removes: Vec<&String> = cur.files.keys().filter(|k| !want.files.contains_key(*k)).collect();
        // every file must be readable before anything on disk changes
        let mut sources = Vec::with_capacity(writes.len());
        for (rel, f) in &writes {
            sources.push(self.locate(&f.hash).ok_or_else(|| anyhow!("The saved copy of {rel} is missing, so this version can't be brought back."))?);
        }
        let root = project.root();
        for ((rel, f), (obj, compressed)) in writes.iter().zip(&sources) {
            put_file(project, rel, f, obj, *compressed, self.whole_limit).with_context(|| format!("put back {rel}"))?;
        }
        for rel in &removes {
            remove_file(root, rel).with_context(|| format!("remove {rel}"))?;
        }
        let mut changed: Vec<String> = writes.iter().map(|(k, _)| (*k).clone()).chain(removes.iter().map(|k| (*k).clone())).collect();
        changed.sort();
        let at_ms = self.next_at(now_ms);
        let v = Version {
            id: at_ms.to_string(),
            at_ms,
            kind,
            label: String::new(),
            changed: changed.clone(),
            first: false,
            restored_from: Some(want.id.clone()),
            undo_of,
            tree: want.tree.clone(),
            files: want.files.clone(),
        };
        self.push(v)?;
        Ok(changed)
    }

    /// Drop versions the retention policy no longer keeps, and their stored files. Returns how
    /// many versions went.
    pub fn prune(&mut self, now_ms: i64) -> Result<usize> {
        let keep = retained(&self.list, now_ms);
        let mut dropped = 0;
        let mut kept = Vec::with_capacity(self.list.len());
        for (v, k) in std::mem::take(&mut self.list).into_iter().zip(keep) {
            if k {
                kept.push(v);
                continue;
            }
            match std::fs::remove_file(self.dir.join("manifests").join(format!("{}.json", v.id))) {
                Ok(()) => dropped += 1,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => dropped += 1,
                Err(e) => {
                    tracing::warn!("versions: could not remove version {}: {e}", v.id);
                    kept.push(v);
                }
            }
        }
        self.list = kept;
        if dropped > 0 {
            self.collect_garbage()?;
        }
        Ok(dropped)
    }

    /// Remove stored files no version refers to (and leftovers of interrupted writes).
    fn collect_garbage(&self) -> Result<()> {
        let used: HashSet<&str> = self.list.iter().flat_map(|v| v.files.values().map(|f| f.hash.as_str())).collect();
        let objects = self.dir.join("objects");
        for d in std::fs::read_dir(&objects)?.flatten() {
            let prefix = d.file_name().to_string_lossy().to_string();
            let Ok(rd) = std::fs::read_dir(d.path()) else { continue };
            for f in rd.flatten() {
                let name = f.file_name().to_string_lossy().to_string();
                let hash = format!("{prefix}{}", name.trim_end_matches(".zst"));
                if name.starts_with('.') || !used.contains(hash.as_str()) {
                    let _ = std::fs::remove_file(f.path());
                }
            }
            let _ = std::fs::remove_dir(d.path()); // only when empty
        }
        Ok(())
    }

    /// Bytes stored (objects + manifests).
    pub fn disk_bytes(&self) -> u64 {
        fn size(p: &Path) -> u64 {
            std::fs::read_dir(p).map_or(0, |rd| rd.flatten().map(|e| e.metadata().map_or(0, |m| if m.is_dir() { size(&e.path()) } else { m.len() })).sum())
        }
        size(&self.dir)
    }
}

/// Collect tracked files under `dir` (`rel` is its project-relative path).
fn walk(dir: &Path, rel: &str, out: &mut Vec<(String, PathBuf, std::fs::Metadata)>) -> Result<()> {
    let rd = match std::fs::read_dir(dir) {
        Ok(rd) => rd,
        Err(e) if !rel.is_empty() && e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", dir.display())),
    };
    for e in rd.flatten() {
        let Some(name) = e.file_name().to_str().map(String::from) else { continue };
        let Ok(ft) = e.file_type() else { continue };
        let r = if rel.is_empty() { name.clone() } else { format!("{rel}/{name}") };
        if ft.is_dir() {
            if !skip_dir(&name, rel.is_empty()) {
                walk(&e.path(), &r, out)?;
            }
        } else if ft.is_file()
            && !skip_file(&name)
            && let Ok(m) = e.metadata()
        {
            out.push((r, e.path(), m));
        }
    }
    Ok(())
}

/// Write one stored file back into the project atomically.
fn put_file(project: &Project, rel: &str, f: &FileEntry, obj: &Path, compressed: bool, whole_limit: u64) -> Result<()> {
    let dst = project.root().join(rel);
    if compressed {
        project.write_bytes(rel, &zstd::decode_all(std::fs::File::open(obj)?)?)?;
    } else if f.size <= whole_limit {
        project.write_bytes(rel, &std::fs::read(obj)?)?;
    } else {
        if let Some(d) = dst.parent() {
            std::fs::create_dir_all(d)?;
        }
        let tmp = temp_beside(&dst);
        std::fs::copy(obj, &tmp)?;
        std::fs::rename(&tmp, &dst)?;
    }
    let mut perm = std::fs::metadata(&dst)?.permissions();
    let mode = perm.mode();
    let want = if f.exec { mode | ((mode & 0o444) >> 2) | 0o100 } else { mode & !0o111 };
    if want != mode {
        perm.set_mode(want);
        std::fs::set_permissions(&dst, perm)?;
    }
    Ok(())
}

/// Remove a project file and any folders that leaves empty.
fn remove_file(root: &Path, rel: &str) -> Result<()> {
    let p = root.join(rel);
    match std::fs::remove_file(&p) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    let mut dir = p.parent();
    while let Some(d) = dir {
        if d == root || !d.starts_with(root) || std::fs::remove_dir(d).is_err() {
            break;
        }
        dir = d.parent();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every tracked file's bytes (the "exact tree").
    fn tree(root: &Path) -> BTreeMap<String, Vec<u8>> {
        let mut found = Vec::new();
        walk(root, "", &mut found).unwrap();
        found.into_iter().map(|(rel, p, _)| (rel, std::fs::read(p).unwrap())).collect()
    }

    fn put(root: &Path, rel: &str, bytes: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }

    fn setup() -> (tempfile::TempDir, Project, Versions) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("project");
        put(&root, "project.toml", b"schema = 1\n");
        let project = Project::open(&root).unwrap();
        let store = Versions::open(&tmp.path().join("data/versions")).unwrap();
        (tmp, project, store)
    }

    fn objects(store: &Versions) -> Vec<PathBuf> {
        let mut out = Vec::new();
        for d in std::fs::read_dir(store.dir.join("objects")).unwrap().flatten() {
            out.extend(std::fs::read_dir(d.path()).unwrap().flatten().map(|f| f.path()));
        }
        out
    }

    #[test]
    fn tracked_paths() {
        assert!(tracked("scenes/duo.toml"));
        assert!(tracked("assets/video/intro.mp4"));
        assert!(tracked("patches/x/sessions/a.txt"), "only the top-level sessions folder is skipped");
        assert!(!tracked("sessions/2026/log.bin"));
        assert!(!tracked(".git/HEAD"));
        assert!(!tracked("patches/ringmod/target/release/x"));
        assert!(!tracked("scenes/.duo.toml.swp"));
        assert!(!tracked("scenes/duo.tmp.4242"));
        assert!(!tracked("scenes/duo.toml~"));
    }

    #[test]
    fn restore_round_trip_brings_back_the_exact_tree() {
        let (_tmp, project, mut store) = setup();
        let root = project.root().to_path_buf();
        put(&root, "scenes/duo.toml", b"# my comment\nname = \"Duo\"\n");
        put(&root, "patches/banner/patch.toml", b"kind = \"web\"\n");
        put(&root, "patches/banner/index.html", b"<h1>hi</h1>\n");
        put(&root, "assets/images/logo.png", b"\x89PNG\r\n\x1a\n\0\0\0binary");
        put(&root, "sessions/s1/log.bin", b"session one");
        put(&root, ".notes", b"hidden");
        let v1 = store.snapshot(&project, Kind::Edit, "", 1_000).unwrap().unwrap().clone();
        assert!(v1.first);
        let before = tree(&root);
        assert!(!before.contains_key("sessions/s1/log.bin") && !before.contains_key(".notes"));

        // edit, add, delete (a whole overlay folder), change a binary, add a session
        put(&root, "scenes/duo.toml", b"# my comment\nname = \"Duo 2\"\n");
        put(&root, "scenes/wide.toml", b"name = \"Wide\"\n");
        std::fs::remove_dir_all(root.join("patches/banner")).unwrap();
        put(&root, "assets/images/logo.png", b"\x89PNG\r\n\x1a\n\0\0\0other");
        put(&root, "presets/hype/deep/nested.toml", b"x = 1\n");
        put(&root, "sessions/s2/log.bin", b"session two");
        let v2 = store.snapshot(&project, Kind::Edit, "", 2_000).unwrap().unwrap().clone();
        assert_eq!(
            v2.changed,
            [
                "assets/images/logo.png",
                "patches/banner/index.html",
                "patches/banner/patch.toml",
                "presets/hype/deep/nested.toml",
                "scenes/duo.toml",
                "scenes/wide.toml"
            ]
        );
        assert!(store.snapshot(&project, Kind::Edit, "", 2_500).unwrap().is_none(), "nothing changed: no new version");

        let changed = store.restore(&project, &v1.id, 3_000).unwrap();
        assert_eq!(changed, v2.changed);
        assert_eq!(tree(&root), before);
        assert!(!root.join("presets").exists(), "folders the version didn't have are gone");
        assert_eq!(std::fs::read(root.join("sessions/s2/log.bin")).unwrap(), b"session two", "sessions are never touched");
        assert_eq!(std::fs::read(root.join(".notes")).unwrap(), b"hidden");
        let last = store.list().last().unwrap();
        assert_eq!((last.kind, last.restored_from.as_deref()), (Kind::Restore, Some(v1.id.as_str())));

        // going back is itself undoable: the v2 state comes back exactly
        let after_v2: BTreeMap<String, FileEntry> = v2.files.clone();
        store.restore(&project, &v2.id, 4_000).unwrap();
        let files = store.scan(&root).unwrap();
        assert_eq!(files, after_v2);
    }

    #[test]
    fn undo_steps_back_twice_and_redo_takes_back_one() {
        let (_tmp, project, mut store) = setup();
        let root = project.root().to_path_buf();
        let state = |s: &str| put(&root, "presets/hype.toml", s.as_bytes());
        let read = || std::fs::read_to_string(root.join("presets/hype.toml")).unwrap_or_default();
        state("a");
        store.snapshot(&project, Kind::Edit, "", 1_000).unwrap();
        state("b");
        store.snapshot(&project, Kind::Edit, "", 2_000).unwrap();
        store.snapshot(&project, Kind::Named, "Before the show", 2_500).unwrap();
        state("c"); // not snapshotted yet: undo records it first
        assert_eq!(store.undo(&project, 3_000).unwrap(), ["presets/hype.toml"]);
        assert_eq!(read(), "b");
        let p = store.undo_preview().unwrap();
        assert_eq!((p.at_ms, p.changed.as_slice()), (2_000, &["presets/hype.toml".to_string()][..]));
        store.undo(&project, 4_000).unwrap();
        assert_eq!(read(), "a");
        assert!(store.undo(&project, 5_000).is_err(), "nothing older");
        assert!(store.redo_preview().is_some());
        store.redo(&project, 6_000).unwrap();
        assert_eq!(read(), "b");
        assert!(store.redo_preview().is_none(), "one step of redo");
        // a new edit after undo: undo takes back that edit
        store.undo(&project, 7_000).unwrap();
        assert_eq!(read(), "a");
        state("d");
        store.undo(&project, 8_000).unwrap();
        assert_eq!(read(), "a");
        assert!(store.redo_preview().is_some());
    }

    #[test]
    fn unchanged_large_file_is_stored_once() {
        let (_tmp, project, mut store) = setup();
        store.whole_limit = 64 * 1024; // exercise the streamed path with a small file
        let root = project.root().to_path_buf();
        let big: Vec<u8> = (0..1_000_000u32).map(|i| (i % 251) as u8).collect();
        put(&root, "assets/video/intro.mp4", &big);
        put(&root, "scenes/duo.toml", b"v = 0\n");
        store.snapshot(&project, Kind::Edit, "", 1_000).unwrap();
        for i in 1..5 {
            put(&root, "scenes/duo.toml", format!("v = {i}\n").as_bytes());
            store.snapshot(&project, Kind::Edit, "", 1_000 + i * 1_000).unwrap().unwrap();
        }
        let hash = store.list()[0].files["assets/video/intro.mp4"].hash.clone();
        let copies: Vec<PathBuf> = objects(&store).into_iter().filter(|p| std::fs::metadata(p).unwrap().len() == big.len() as u64).collect();
        assert_eq!(copies, [store.object(&hash, false)], "one copy of the video for five versions");
        assert_eq!(objects(&store).len(), 1 + 5 + 1, "video + five scene texts + project.toml");
        assert!(store.list().iter().all(|v| v.files["assets/video/intro.mp4"].hash == hash));
        // and it comes back byte for byte
        std::fs::remove_file(root.join("assets/video/intro.mp4")).unwrap();
        let first = store.list()[0].id.clone();
        store.restore(&project, &first, 9_000).unwrap();
        assert_eq!(std::fs::read(root.join("assets/video/intro.mp4")).unwrap(), big);
    }

    #[test]
    fn retention_keeps_named_versions_and_prunes_their_files_only() {
        let (_tmp, project, mut store) = setup();
        let root = project.root().to_path_buf();
        let now = 20_000 * DAY_MS + 15 * 3_600_000;
        let day30 = (20_000 - 30) * DAY_MS;
        let snap = |store: &mut Versions, text: &str, at: i64, kind: Kind| {
            put(&root, "scenes/duo.toml", text.as_bytes());
            store.snapshot(&project, kind, if kind == Kind::Named { "Old show" } else { "" }, at).unwrap().unwrap().id.clone()
        };
        let old_auto = snap(&mut store, "old auto", now - 200 * DAY_MS, Kind::Edit);
        let old_named = snap(&mut store, "old named", now - 200 * DAY_MS + 1_000, Kind::Named);
        let morning = snap(&mut store, "30 days ago, morning", day30 + 9 * 3_600_000, Kind::Edit);
        let evening = snap(&mut store, "30 days ago, evening", day30 + 20 * 3_600_000, Kind::Edit);
        let recent = snap(&mut store, "3 days ago", now - 3 * DAY_MS, Kind::Edit);
        let recent2 = snap(&mut store, "2 days ago", now - 2 * DAY_MS, Kind::Edit);
        let hash_of = |store: &Versions, id: &str| store.get(id).unwrap().files["scenes/duo.toml"].hash.clone();
        let dropped_hashes = [hash_of(&store, &old_auto), hash_of(&store, &morning)];

        assert_eq!(store.prune(now).unwrap(), 2);
        let ids: Vec<&str> = store.list().iter().map(|v| v.id.as_str()).collect();
        assert_eq!(ids, [old_named.as_str(), evening.as_str(), recent.as_str(), recent2.as_str()]);
        for h in &dropped_hashes {
            assert!(store.locate(h).is_none(), "files only the dropped versions used are gone");
        }
        for id in &ids {
            assert!(store.locate(&hash_of(&store, id)).is_some());
        }
        // survives a reopen, and the named version still restores
        let mut store = Versions::open(&store.dir.clone()).unwrap();
        assert_eq!(store.list().len(), 4);
        store.restore(&project, &old_named, now + 1_000).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("scenes/duo.toml")).unwrap(), "old named");
        // 100 days later only the named one and the newest remain
        let before = store.list().len();
        assert_eq!(store.prune(now + 100 * DAY_MS).unwrap(), before - 2);
        assert_eq!(store.list().first().unwrap().id, old_named);
    }
}
