//! The patch loader (§6, §3.4): scans and watches `project/patches/*`, declares every patch's
//! addresses, runs script patches, publishes the live [`PatchSet`] to the other hosts, and
//! serves the `patches` query and `patch.*` actions.
//!
//! Hot reload is debounced per patch folder. A patch whose newest `patch.toml` or script
//! fails keeps its last good version live and reports the error (`file:line: message`) in
//! `patch.<id>.error` and `patch.<id>.state = "error"`.

use crate::manifest::{Kind, Manifest, ManifestError};
use crate::registry::{PatchInfo, PatchSet, Patches, state};
use crate::script::vm::split_location;
use crate::script::{Ctrl, Timing, Worker};
use crate::templates;
use parking_lot::Mutex;
use se_core::{Config, Input};
use se_hub::{Bus, EngineCtx, Hub};
use se_proto::{Meta, Op, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

/// Quiet period before a changed patch folder is reloaded.
pub const DEBOUNCE: Duration = Duration::from_millis(150);
const KV_NS: &str = "patches";
const KV_DISABLED: &str = "disabled";
const TARGET: &str = "patches";

struct Entry {
    dir: PathBuf,
    /// Last manifest that parsed (stays live when a newer one fails).
    manifest: Option<Arc<Manifest>>,
    /// Newest `patch.toml` failure, `patch.toml:<line>: message`.
    manifest_error: Option<String>,
    generation: u64,
    declared: BTreeSet<String>,
    worker: Option<Worker>,
    timing: Option<Arc<Timing>>,
    /// Latest `patch.<id>.error` published by the hosting runtime (non-script kinds).
    host_error: String,
    /// What the loader last published for `(state, error)` (loader-owned writes only).
    published: Option<(String, Option<String>)>,
    /// Sizes and modification times of the folder's files at the last reload.
    fingerprint: u64,
}

pub(crate) struct Loader {
    hub: Arc<Hub>,
    db: se_store::Db,
    root: PathBuf,
    share: PathBuf,
    entries: BTreeMap<String, Entry>,
    disabled: BTreeSet<String>,
    set_tx: watch::Sender<Arc<PatchSet>>,
    fps: u32,
    canvas: [u32; 2],
}

fn kind_of(e: &Entry) -> Option<Kind> {
    e.manifest.as_ref().map(|m| m.kind)
}

/// Frame rate and default resolution for script layers from the canvases.
fn canvas_timing(cfg: &Config) -> (u32, [u32; 2]) {
    let fps = cfg.project.canvas.values().map(|c| c.fps).max().unwrap_or(60).clamp(1, 240);
    let res = cfg.project.canvas.get("wide").or_else(|| cfg.project.canvas.values().next()).map(|c| [c.width.max(1), c.height.max(1)]).unwrap_or([1920, 1080]);
    (fps, res)
}

impl Loader {
    fn patches_dir(&self) -> PathBuf {
        self.root.join("patches")
    }

    fn publish_set(&self) {
        let set: PatchSet = self
            .entries
            .iter()
            .filter_map(|(id, e)| {
                e.manifest.as_ref().map(|m| (id.clone(), PatchInfo { manifest: m.clone(), enabled: !self.disabled.contains(id), generation: e.generation }))
            })
            .collect();
        self.set_tx.send_replace(Arc::new(set));
    }

    fn persist_disabled(&self) {
        let v = Value::List(self.disabled.iter().map(|s| Value::Str(s.clone())).collect());
        if let Err(e) = self.db.kv_set(KV_NS, KV_DISABLED, &v) {
            self.hub.log("error", TARGET, format!("could not save disabled patches: {e:#}"));
        }
    }

    fn declare(&mut self, id: &str) {
        let hub = self.hub.clone();
        let Some(e) = self.entries.get_mut(id) else { return };
        let a = format!("patch.{id}");
        let owner = a.clone();
        hub.declare(
            &format!("{a}.error"),
            Meta { description: Some("Last error (file:line: message); empty when healthy".into()), ..Meta::string("").readonly().owner(&owner) },
        );
        hub.declare(
            &format!("{a}.state"),
            Meta {
                description: Some("Patch status".into()),
                ..Meta::enumeration(state::LOADED, &[state::LOADED, state::ERROR, state::SUSPENDED, state::DISABLED]).readonly().owner(&owner)
            },
        );
        let Some(m) = e.manifest.clone() else { return };
        let mut t = Meta::trigger().owner(&owner);
        t.description = Some(if m.description.is_empty() { m.label.clone() } else { format!("{} — {}", m.label, m.description) });
        hub.declare(&a, t);
        hub.submit(Input::DeclareTrigger { address: a.clone(), spec: m.trigger });
        let now: BTreeSet<String> = m.params.keys().cloned().collect();
        for gone in e.declared.difference(&now) {
            hub.submit(Input::Remove { prefix: format!("{a}.{gone}") });
        }
        for (name, p) in &m.params {
            hub.declare(&format!("{a}.{name}"), p.meta(id));
        }
        e.declared = now;
    }

    /// Loader-owned `(state, error)` for non-script kinds (and scripts without a worker).
    fn publish_loader_status(&mut self, id: &str) {
        let enabled = !self.disabled.contains(id);
        let hub = self.hub.clone();
        let Some(e) = self.entries.get_mut(id) else { return };
        if e.worker.is_some() {
            return;
        }
        let st = if !enabled {
            state::DISABLED
        } else if e.manifest_error.is_some() || !e.host_error.is_empty() {
            state::ERROR
        } else {
            state::LOADED
        };
        // the loader writes `.error` only for manifest failures; once the manifest parses again
        // it restores what the hosting runtime last reported (the host overwrites it on reload)
        let err = match &e.manifest_error {
            Some(m) => Some(m.clone()),
            None if e.published.as_ref().is_some_and(|(_, er)| er.as_ref().is_some_and(|s| !s.is_empty())) => Some(e.host_error.clone()),
            None => None,
        };
        let next = (st.to_string(), err.clone());
        let prev_state = e.published.as_ref().map(|(s, _)| s.clone());
        if prev_state.as_deref() != Some(st) {
            hub.publish(&format!("patch.{id}.state"), Value::Str(st.into()));
        }
        if let Some(er) = &err
            && e.published.as_ref().and_then(|(_, x)| x.as_ref()) != Some(er)
        {
            hub.publish(&format!("patch.{id}.error"), Value::Str(er.clone()));
        }
        e.published = Some((next.0, err.filter(|s| !s.is_empty())));
    }

    fn remove(&mut self, id: &str) {
        if let Some(e) = self.entries.remove(id) {
            drop(e.worker);
            self.hub.submit(Input::Remove { prefix: format!("patch.{id}") });
            self.hub.log("info", TARGET, format!("patch `{id}` removed"));
        }
    }

    fn script_timing(&self, m: &Manifest) -> (u32, [u32; 2]) {
        (self.fps, m.size.unwrap_or(self.canvas))
    }

    /// Re-read one patch folder and apply the result. Without `force`, a folder whose files
    /// did not change (same sizes and mtimes) is left alone (watchers report touches too).
    pub(crate) fn reload(&mut self, id: &str, force: bool) {
        let dir = self.patches_dir().join(id);
        if id.starts_with('.') || !dir.join("patch.toml").is_file() {
            if self.entries.contains_key(id) {
                self.remove(id);
                self.publish_set();
            }
            return;
        }
        let enabled = !self.disabled.contains(id);
        let is_new = !self.entries.contains_key(id);
        let e = self.entries.entry(id.to_string()).or_insert_with(|| Entry {
            dir: dir.clone(),
            manifest: None,
            manifest_error: None,
            generation: 0,
            declared: BTreeSet::new(),
            worker: None,
            timing: None,
            host_error: String::new(),
            published: None,
            fingerprint: 0,
        });
        let fp = fingerprint(&dir);
        if !force && !is_new && e.fingerprint == fp {
            return;
        }
        e.fingerprint = fp;
        match Manifest::load(&dir) {
            Ok(m) => {
                let m = Arc::new(m);
                e.manifest = Some(m.clone());
                e.manifest_error = None;
                e.generation += 1;
                let generation = e.generation;
                if m.kind == Kind::Script {
                    let (fps, res) = (self.fps, m.size.unwrap_or(self.canvas));
                    let e = self.entries.get_mut(id).expect("inserted above");
                    match (&e.worker, &e.timing) {
                        (Some(w), Some(t)) if !w.is_finished() => {
                            t.set(fps, res);
                            w.send(Ctrl::Load { manifest: m.clone(), generation });
                        }
                        _ => {
                            let timing = Arc::new(Timing::new(fps, res));
                            match Worker::spawn(id, self.hub.clone(), timing.clone(), m.clone(), generation, enabled) {
                                Ok(w) => {
                                    e.worker = Some(w);
                                    e.timing = Some(timing);
                                }
                                Err(err) => {
                                    e.manifest_error = Some(format!("{}:0: cannot start script thread: {err}", m.entry));
                                }
                            }
                        }
                    }
                } else {
                    let e = self.entries.get_mut(id).expect("inserted above");
                    if e.worker.take().is_some() {
                        // was a script: its runtime error no longer applies; the new host reports
                        self.hub.publish(&format!("patch.{id}.error"), Value::Str(String::new()));
                        e.published = None;
                        e.host_error.clear();
                    }
                    e.timing = None;
                }
                self.declare(id);
                self.publish_loader_status(id);
                let e = &self.entries[id];
                let what = if is_new { "loaded" } else { "reloaded" };
                self.hub.log("info", TARGET, format!("patch `{id}` ({}) {what}, generation {}", m.kind.as_str(), e.generation));
            }
            Err(err) => {
                let msg = err.located();
                e.manifest_error = Some(msg.clone());
                let live = e.manifest.is_some();
                if let Some(w) = &e.worker {
                    w.send(Ctrl::ManifestError(msg.clone()));
                }
                self.declare(id);
                self.publish_loader_status(id);
                let keep = if live { "; the last good version stays live" } else { "" };
                self.hub.log("error", TARGET, format!("patch `{id}`: {msg}{keep}"));
                log_manifest_error(&err);
            }
        }
        self.publish_set();
    }

    fn scan_all(&mut self) {
        let dir = self.patches_dir();
        let mut ids: BTreeSet<String> = std::fs::read_dir(&dir)
            .map(|rd| {
                rd.flatten().filter(|e| e.path().is_dir()).filter_map(|e| e.file_name().to_str().map(str::to_string)).filter(|n| !n.starts_with('.')).collect()
            })
            .unwrap_or_default();
        ids.extend(self.entries.keys().cloned());
        for id in ids {
            self.reload(&id, true);
        }
    }

    fn set_enabled(&mut self, id: &str, on: bool) -> Result<(), String> {
        if !self.entries.contains_key(id) {
            return Err(format!("unknown patch `{id}`"));
        }
        let changed = if on { self.disabled.remove(id) } else { self.disabled.insert(id.to_string()) };
        if changed {
            self.persist_disabled();
        }
        let e = self.entries.get_mut(id).expect("checked");
        if let Some(w) = &e.worker {
            w.send(Ctrl::Enable(on));
        }
        self.publish_loader_status(id);
        self.publish_set();
        self.hub.log("info", TARGET, format!("patch `{id}` {}", if on { "enabled" } else { "disabled" }));
        Ok(())
    }

    fn config_changed(&mut self, cfg: &Config) {
        let (fps, canvas) = canvas_timing(cfg);
        if (fps, canvas) == (self.fps, self.canvas) {
            return;
        }
        self.fps = fps;
        self.canvas = canvas;
        for e in self.entries.values() {
            if let (Some(t), Some(m)) = (&e.timing, &e.manifest) {
                let (f, r) = self.script_timing(m);
                t.set(f, r);
            }
        }
    }

    /// A hosting runtime published `patch.<id>.error`.
    fn host_error(&mut self, id: &str, err: &str) {
        let Some(e) = self.entries.get_mut(id) else { return };
        if e.worker.is_some() || kind_of(e) == Some(Kind::Script) {
            return;
        }
        // our own manifest-error writes echo back through the bus
        if e.manifest_error.as_deref() == Some(err) {
            return;
        }
        if e.host_error != err {
            e.host_error = err.to_string();
            self.publish_loader_status(id);
        }
    }

    fn query(&self, only: Option<&str>) -> Value {
        let snap = self.hub.snapshot.load();
        let mut out = Vec::new();
        for (id, e) in &self.entries {
            if only.is_some_and(|o| o != id) {
                continue;
            }
            let enabled = !self.disabled.contains(id);
            let mut v = Value::map().with("id", id.as_str()).with("dir", e.dir.display().to_string()).with("enabled", enabled).with("generation", e.generation);
            let (st, err) = match (&e.worker, &e.manifest_error) {
                (Some(w), me) => {
                    let s = w.status.lock().clone();
                    let st = if s.state.is_empty() { state::LOADED } else { s.state };
                    let err = if s.error.is_empty() { me.clone().unwrap_or_default() } else { s.error.clone() };
                    let mut stats = Value::map()
                        .with("cpu_ms_avg", (s.cpu_ms_avg * 1000.0).round() / 1000.0)
                        .with("cpu_ms_peak", (s.cpu_ms_peak * 1000.0).round() / 1000.0)
                        .with("memory_kb", s.memory_kb)
                        .with("handlers", s.handlers)
                        .with("frames", s.frames)
                        .with("events", s.events)
                        .with("dropped_events", w.dropped.load(std::sync::atomic::Ordering::Relaxed))
                        .with("draw_ops", s.draw_ops)
                        .with("has_frame", s.has_frame);
                    if s.live {
                        stats = stats.with("live_generation", s.live_generation);
                    }
                    v = v.with("live", s.live).with("script", stats);
                    (st.to_string(), err)
                }
                (None, me) => {
                    let st = snap
                        .str(&format!("patch.{id}.state"))
                        .map(str::to_string)
                        .unwrap_or_else(|| if me.is_some() { state::ERROR.into() } else { state::LOADED.into() });
                    let err = me.clone().unwrap_or_else(|| snap.str(&format!("patch.{id}.error")).unwrap_or_default().to_string());
                    v = v.with("live", e.manifest.is_some());
                    (st, err)
                }
            };
            v = v.with("state", st).with("error", err.as_str());
            if let Some((file, line)) = split_location(&err) {
                v = v.with("error_file", file).with("error_line", line);
            }
            if let Some(m) = &e.manifest {
                let params: Vec<Value> = m
                    .params
                    .iter()
                    .map(|(n, p)| {
                        let addr = format!("patch.{id}.{n}");
                        let mut pv =
                            Value::map().with("name", n.as_str()).with("address", addr.as_str()).with("type", p.ty.as_str()).with("default", p.default_value());
                        if let Some([lo, hi]) = p.range {
                            pv = pv.with("range", vec![Value::Float(lo), Value::Float(hi)]);
                        }
                        if !p.options.is_empty() {
                            pv = pv.with("options", p.options.iter().map(|s| Value::Str(s.clone())).collect::<Vec<_>>());
                        }
                        if let Some(u) = &p.unit {
                            pv = pv.with("unit", u.as_str());
                        }
                        if let Some(d) = &p.description {
                            pv = pv.with("description", d.as_str());
                        }
                        if let Some(val) = snap.get(&addr) {
                            pv = pv.with("value", val.clone());
                        }
                        pv
                    })
                    .collect();
                v = v
                    .with("kind", m.kind.as_str())
                    .with("layer", m.layer.as_str())
                    .with("label", m.label.as_str())
                    .with("description", m.description.as_str())
                    .with("entry", m.entry.as_str())
                    .with("trigger", m.has_trigger)
                    .with("params", params)
                    .with("grants", m.grants.iter().map(|g| Value::Str(g.clone())).collect::<Vec<_>>())
                    .with("env", snap.f32(&format!("patch.{id}.env")).unwrap_or(0.0) as f64)
                    .with("active", snap.bool(&format!("patch.{id}.active")));
                if let Some([w, h]) = m.size {
                    v = v.with("size", vec![Value::Int(w as i64), Value::Int(h as i64)]);
                }
                let mut b = Value::map();
                if let Some(x) = m.budget.cpu_ms {
                    b = b.with("cpu_ms", x);
                }
                if let Some(x) = m.budget.gpu_ms {
                    b = b.with("gpu_ms", x);
                }
                v = v.with("budget", b);
            } else {
                v = v.with("label", id.as_str());
            }
            out.push(v);
        }
        Value::List(out)
    }

    fn health(&self) -> Value {
        let snap = self.hub.snapshot.load();
        let mut bad = Vec::new();
        for (id, e) in &self.entries {
            if self.disabled.contains(id) {
                continue;
            }
            let st = match &e.worker {
                Some(w) => w.status.lock().state.to_string(),
                None => snap.str(&format!("patch.{id}.state")).unwrap_or(if e.manifest_error.is_some() { state::ERROR } else { state::LOADED }).to_string(),
            };
            if st == state::ERROR || st == state::SUSPENDED {
                bad.push(format!("{id} ({st})"));
            }
        }
        if bad.is_empty() {
            Value::map().with("status", "pass").with("detail", format!("{} patches loaded", self.entries.len()))
        } else {
            Value::map().with("status", "warn").with("detail", format!("patches with problems: {}", bad.join(", ")))
        }
    }
}

fn log_manifest_error(err: &ManifestError) {
    tracing::warn!("{err}");
}

fn skip_name(name: &str) -> bool {
    name.starts_with('.') || name == "target" || name.ends_with('~') || name.ends_with(".swp") || name.ends_with(".swx") || name == "4913"
}

/// Hash of every relevant file's path, size, and mtime under a patch folder.
fn fingerprint(dir: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    fn walk(d: &Path, h: &mut std::collections::hash_map::DefaultHasher, depth: usize) {
        let Ok(rd) = std::fs::read_dir(d) else { return };
        let mut items: Vec<_> = rd.flatten().collect();
        items.sort_by_key(|e| e.file_name());
        for e in items {
            let name = e.file_name();
            if skip_name(&name.to_string_lossy()) {
                continue;
            }
            let Ok(md) = e.metadata() else { continue };
            name.hash(h);
            if md.is_dir() {
                if depth < 8 {
                    walk(&e.path(), h, depth + 1);
                }
            } else {
                md.len().hash(h);
                md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_nanos()).hash(h);
            }
        }
    }
    let mut h = std::collections::hash_map::DefaultHasher::new();
    walk(dir, &mut h, 0);
    h.finish()
}

/// Ids of patch folders touched by `paths` (relative to `patches/`).
fn touched_ids(patches: &Path, paths: &[PathBuf]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for p in paths {
        let Ok(rel) = p.strip_prefix(patches) else { continue };
        let mut comps = rel.components();
        let Some(first) = comps.next().and_then(|c| c.as_os_str().to_str()) else { continue };
        if first.starts_with('.') {
            continue;
        }
        let rest: Vec<String> = comps.map(|c| c.as_os_str().to_string_lossy().into_owned()).collect();
        let name = rest.last().cloned().unwrap_or_default();
        // editor droppings: hidden/backup/swap files and vim's write test
        if rest.iter().any(|s| skip_name(s)) || skip_name(&name) {
            continue;
        }
        out.insert(first.to_string());
    }
    out
}

fn arg_str<'a>(args: &'a Value, key: &str, pos: usize) -> Option<&'a str> {
    args.get_path(key).and_then(Value::as_str).or_else(|| args.get_path("args").and_then(Value::as_list).and_then(|l| l.get(pos)).and_then(Value::as_str))
}

/// Start the loader: initial scan, file watching, script workers, event routing, the
/// `patches`/`patch.templates` queries and `patch.*` actions. Returns the live patch set.
pub async fn start(ctx: EngineCtx) -> anyhow::Result<Patches> {
    let (set_tx, set_rx) = watch::channel(Arc::new(PatchSet::new()));
    let disabled: BTreeSet<String> = match ctx.db.kv_get(KV_NS, KV_DISABLED) {
        Ok(Some(Value::List(l))) => l.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        Ok(_) => BTreeSet::new(),
        Err(e) => {
            ctx.hub.log("error", TARGET, format!("could not read disabled patches: {e:#}"));
            BTreeSet::new()
        }
    };
    let (fps, canvas) = canvas_timing(&ctx.config.borrow());
    let loader = Arc::new(Mutex::new(Loader {
        hub: ctx.hub.clone(),
        db: ctx.db.clone(),
        root: ctx.project_root.clone(),
        share: ctx.share_dir.clone(),
        entries: BTreeMap::new(),
        disabled,
        set_tx,
        fps,
        canvas,
    }));
    let patches_dir = ctx.project_root.join("patches");
    if let Err(e) = std::fs::create_dir_all(&patches_dir) {
        ctx.hub.log("error", TARGET, format!("{}: {e}", patches_dir.display()));
    }

    // event routing starts before the first load so no trigger is missed
    spawn_bus(ctx.hub.clone(), loader.clone());
    loader.lock().scan_all();

    // --- file watching (debounced per folder) -------------------------------------------
    let (fs_tx, fs_rx) = mpsc::unbounded_channel::<Vec<PathBuf>>();
    let watcher = {
        use notify::{RecursiveMode, Watcher};
        let tx = fs_tx.clone();
        let mut w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
            if let Ok(ev) = res
                && !matches!(ev.kind, notify::EventKind::Access(_))
            {
                let _ = tx.send(ev.paths);
            }
        })?;
        w.watch(&patches_dir, RecursiveMode::Recursive)?;
        w
    };
    spawn_debounce(loader.clone(), patches_dir.clone(), fs_rx, watcher);

    // --- config changes: frame rate / canvas size ----------------------------------------
    {
        let loader = loader.clone();
        let mut cfg = ctx.config.clone();
        tokio::spawn(async move {
            while cfg.changed().await.is_ok() {
                let c = cfg.borrow_and_update().clone();
                loader.lock().config_changed(&c);
            }
        });
    }

    register_queries(&ctx, loader.clone());
    spawn_actions(&ctx, loader.clone());

    // --- preflight -------------------------------------------------------------------------
    {
        let loader = loader.clone();
        let hub = ctx.hub.clone();
        tokio::spawn(async move {
            let mut last = Value::Null;
            loop {
                let h = loader.lock().health();
                if h != last {
                    hub.publish("health.patches", h.clone());
                    last = h;
                }
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        });
    }

    Ok(Patches { rx: set_rx })
}

fn spawn_bus(hub: Arc<Hub>, loader: Arc<Mutex<Loader>>) {
    let mut rx = hub.subscribe();
    let log_hub = hub.clone();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(b) => match &*b {
                    Bus::Event(e) => {
                        let e = Arc::new(e.clone());
                        let l = loader.lock();
                        for entry in l.entries.values() {
                            if let Some(w) = &entry.worker {
                                w.offer(&e);
                            }
                        }
                    }
                    Bus::Changes(ch) => {
                        let mut hits: Vec<(String, String)> = Vec::new();
                        for (a, v) in ch {
                            if let Some(rest) = a.strip_prefix("patch.")
                                && let Some(id) = rest.strip_suffix(".error")
                                && !id.contains('.')
                            {
                                hits.push((id.to_string(), v.as_str().unwrap_or_default().to_string()));
                            }
                        }
                        if !hits.is_empty() {
                            let mut l = loader.lock();
                            for (id, err) in hits {
                                l.host_error(&id, &err);
                            }
                        }
                    }
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                    log_hub.log("warn", TARGET, format!("event routing lagged; {n} bus messages skipped"));
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });
}

fn spawn_debounce(loader: Arc<Mutex<Loader>>, patches_dir: PathBuf, mut rx: mpsc::UnboundedReceiver<Vec<PathBuf>>, watcher: notify::RecommendedWatcher) {
    tokio::spawn(async move {
        let _watcher = watcher;
        let mut pending: BTreeSet<String> = BTreeSet::new();
        loop {
            let msg = if pending.is_empty() {
                rx.recv().await
            } else {
                match tokio::time::timeout(DEBOUNCE, rx.recv()).await {
                    Ok(m) => m,
                    Err(_) => {
                        let ids = std::mem::take(&mut pending);
                        let mut l = loader.lock();
                        for id in ids {
                            l.reload(&id, false);
                        }
                        continue;
                    }
                }
            };
            let Some(paths) = msg else { break };
            // the patches folder itself was replaced: rescan everything
            if paths.iter().any(|p| p == &patches_dir) {
                pending.extend(loader.lock().entries.keys().cloned());
                if let Ok(rd) = std::fs::read_dir(&patches_dir) {
                    pending.extend(rd.flatten().filter_map(|e| e.file_name().to_str().map(str::to_string)).filter(|n| !n.starts_with('.')));
                }
            }
            pending.extend(touched_ids(&patches_dir, &paths));
        }
    });
}

fn register_queries(ctx: &EngineCtx, loader: Arc<Mutex<Loader>>) {
    let l = loader.clone();
    ctx.hub.register_query(
        "patches",
        Arc::new(move |_name, args| {
            let l = l.clone();
            Box::pin(async move {
                let only = args.get_path("id").and_then(Value::as_str).map(str::to_string);
                Ok(l.lock().query(only.as_deref()))
            })
        }),
    );
    let share = ctx.share_dir.clone();
    ctx.hub.register_query(
        "patch.templates",
        Arc::new(move |_name, _args| {
            let share = share.clone();
            Box::pin(async move {
                Ok(Value::List(
                    templates::list(&share)
                        .into_iter()
                        .map(|t| Value::map().with("kind", t.kind).with("name", t.name).with("description", t.description).with("layer", t.layer))
                        .collect(),
                ))
            })
        }),
    );
}

fn spawn_actions(ctx: &EngineCtx, loader: Arc<Mutex<Loader>>) {
    let mut rx = ctx.hub.route_actions("patch");
    let hub = ctx.hub.clone();
    tokio::spawn(async move {
        while let Some(c) = rx.recv().await {
            let Op::Action { name, args } = &c.op else { continue };
            if let Err(e) = action(&loader, name, args) {
                hub.log("error", TARGET, format!("{name}: {e}"));
            }
        }
    });
}

fn action(loader: &Arc<Mutex<Loader>>, name: &str, args: &Value) -> Result<(), String> {
    match name {
        "patch.reload" => {
            let mut l = loader.lock();
            match arg_str(args, "id", 0) {
                Some(id) => {
                    if !l.entries.contains_key(id) && !l.patches_dir().join(id).join("patch.toml").is_file() {
                        return Err(format!("unknown patch `{id}`"));
                    }
                    l.reload(id, true);
                }
                None => l.scan_all(),
            }
            Ok(())
        }
        "patch.enable" | "patch.disable" => {
            let id = arg_str(args, "id", 0).ok_or("missing patch id")?;
            loader.lock().set_enabled(id, name == "patch.enable")
        }
        "patch.new" => {
            let id = arg_str(args, "id", 0).ok_or("missing `id`")?.to_string();
            let kind = arg_str(args, "kind", 1).unwrap_or("script").to_string();
            let template = arg_str(args, "template", 2).unwrap_or("default").to_string();
            let open = args.get_path("open").is_some_and(Value::truthy);
            let editor = args.get_path("editor").and_then(Value::as_str).map(str::to_string);
            let (share, root, hub) = {
                let l = loader.lock();
                (l.share.clone(), l.root.clone(), l.hub.clone())
            };
            let m = templates::create(&share, &root, &id, &kind, &template)?;
            hub.log("info", TARGET, format!("created patch `{id}` ({kind}/{template}) in {}", m.dir.display()));
            loader.lock().reload(&id, false);
            if open {
                let used = templates::open_in_editor(&templates::edit_target(&m), editor.as_deref())?;
                hub.log("info", TARGET, format!("opened `{id}` with {used}"));
            }
            Ok(())
        }
        "patch.open" => {
            let id = arg_str(args, "id", 0).ok_or("missing patch id")?;
            let editor = args.get_path("editor").and_then(Value::as_str);
            let (m, hub) = {
                let l = loader.lock();
                let m = l.entries.get(id).and_then(|e| e.manifest.clone()).ok_or_else(|| format!("unknown patch `{id}`"))?;
                (m, l.hub.clone())
            };
            let used = templates::open_in_editor(&templates::edit_target(&m), editor)?;
            hub.log("info", TARGET, format!("opened `{id}` with {used}"));
            Ok(())
        }
        other => Err(format!("unknown action `{other}` (patch.new, patch.reload, patch.enable, patch.disable, patch.open)")),
    }
}
