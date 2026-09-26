//! Project versions (history + undo), on top of [`se_store::versions`]: a version is saved
//! automatically after each burst of project writes (from the UI or a text editor) and at
//! startup, and before anything is put back.
//!
//! * Query `project.versions` → newest first: `[{id, at_ms, label, auto, kind, changed, first,
//!   restored_from: {id, at_ms, label, kind}?}]`.
//! * Actions `project.version.save {label}`, `project.version.restore {id}`, `project.undo`,
//!   `project.redo`.
//! * State `project.versions.count`, `.latest` (id), `.undo` / `.redo` (`{kind, at_ms, changed}`
//!   of what they would change, or null), `.bytes` (disk used).
//! * Events `project.version.saved {id, kind, label, changed}`, `project.version.restored {id,
//!   kind, restored_from, changed}`; `health.versions` fails while versions can't be saved.

use crate::daemon::Ctx;
use crossbeam_channel::{Receiver, RecvTimeoutError};
use parking_lot::RwLock;
use se_proto::{Event, Op, Origin, Value};
use se_store::versions::{Kind, Preview, Version, Versions};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Quiet time after the last write before a version is saved.
const SETTLE: Duration = Duration::from_secs(5);
/// A steady stream of writes still gets a version this often.
const MAX_WAIT: Duration = Duration::from_secs(60);
/// After a failed save, try again this often.
const RETRY: Duration = Duration::from_secs(60);

enum Msg {
    Changed,
    Save(String),
    Restore(String),
    Undo,
    Redo,
}

fn now_ms() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

pub fn start(ctx: &Ctx) {
    let list = Arc::new(RwLock::new(Value::List(Vec::new())));
    {
        let list = list.clone();
        ctx.hub.register_query(
            "project.versions",
            Arc::new(move |_, _| {
                let v = list.read().clone();
                Box::pin(async move { Ok(v) })
            }),
        );
    }
    let dir = ctx.opts.data_dir.join("versions");
    let store = match Versions::open(&dir) {
        Ok(s) => s,
        Err(e) => {
            health(ctx, Some(format!("can't open the version store in {}: {e:#}", dir.display())));
            return;
        }
    };
    let (tx, rx) = crossbeam_channel::unbounded::<Msg>();
    let watcher = {
        let tx = tx.clone();
        match se_store::versions::watch(ctx.project.root(), move || {
            let _ = tx.send(Msg::Changed);
        }) {
            Ok(w) => Some(w),
            Err(e) => {
                health(ctx, Some(format!("can't watch the project for changes: {e:#}")));
                None
            }
        }
    };
    for prefix in ["project.version", "project.undo", "project.redo"] {
        let mut actions = ctx.hub.route_actions(prefix);
        let (tx, hub) = (tx.clone(), ctx.hub.clone());
        tokio::spawn(async move {
            while let Some(c) = actions.recv().await {
                let Op::Action { name, args } = &c.op else { continue };
                let msg = match name.as_str() {
                    "project.version.save" => Msg::Save(args.get_path("label").and_then(Value::as_str).unwrap_or("").trim().to_string()),
                    "project.version.restore" => {
                        let id = args.get_path("id").and_then(|v| v.as_str().map(String::from).or_else(|| v.as_i64().map(|n| n.to_string())));
                        match id {
                            Some(id) => Msg::Restore(id),
                            None => {
                                hub.log("error", "project", "project.version.restore needs `id`");
                                continue;
                            }
                        }
                    }
                    "project.undo" => Msg::Undo,
                    "project.redo" => Msg::Redo,
                    other => {
                        hub.log("warn", "project", format!("unknown action {other}"));
                        continue;
                    }
                };
                let _ = tx.send(msg);
            }
        });
    }
    let ctx = ctx.clone();
    if let Err(e) = std::thread::Builder::new().name("se-versions".into()).spawn(move || run(ctx, store, rx, list, watcher)) {
        tracing::error!("versions thread: {e}");
    }
}

/// Publish `health.versions`: a problem, or (after one) that versions are saved again.
fn health(ctx: &Ctx, problem: Option<String>) {
    match problem {
        Some(msg) => {
            ctx.hub.log("error", "project", format!("project versions: {msg}"));
            ctx.hub.publish("health.versions", Value::map().with("status", "fail").with("detail", msg));
        }
        None => ctx.hub.publish("health.versions", Value::map().with("status", "pass").with("detail", "versions are saved")),
    }
}

fn run(ctx: Ctx, mut store: Versions, rx: Receiver<Msg>, list: Arc<RwLock<Value>>, _watcher: Option<notify::RecommendedWatcher>) {
    let mut failing = false;
    // First pass right away: catches edits made while the engine was off.
    let mut due = Some(Instant::now());
    let mut first_change: Option<Instant> = None;
    publish(&ctx, &store, &list);
    loop {
        let wait = due.map_or(Duration::from_secs(3600), |d| d.saturating_duration_since(Instant::now()));
        let msg = match rx.recv_timeout(wait) {
            Ok(Msg::Changed) => {
                let now = Instant::now();
                let first = *first_change.get_or_insert(now);
                due = Some((now + SETTLE).min(first + MAX_WAIT));
                continue;
            }
            Ok(msg) => msg,
            Err(RecvTimeoutError::Timeout) if due.is_some() => Msg::Changed,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        // every message below saves the current state first, so pending edits are captured
        due = None;
        first_change = None;
        let problem = handle(&ctx, &mut store, msg).err();
        if problem.is_some() {
            due = Some(Instant::now() + RETRY);
        }
        if problem.is_some() || failing {
            failing = problem.is_some();
            health(&ctx, problem);
        }
        match store.prune(now_ms()) {
            Ok(0) => {}
            Ok(n) => tracing::info!("versions: {n} old versions removed"),
            Err(e) => tracing::warn!("versions: cleanup: {e:#}"),
        }
        publish(&ctx, &store, &list);
    }
}

/// Run one request. `Err` = versions can't be saved (a health problem); requests that just
/// can't be done ("nothing to undo") are logged and are `Ok`.
fn handle(ctx: &Ctx, store: &mut Versions, msg: Msg) -> Result<(), String> {
    let project = &ctx.project;
    let saved = |v: &Version| {
        ctx.hub.emit(Event::new(
            "project.version.saved",
            Origin::System,
            Value::map().with("id", v.id.clone()).with("kind", v.kind.as_str()).with("label", v.label.clone()).with("changed", v.changed.clone()),
        ));
    };
    let (kind, res) = match msg {
        Msg::Changed => {
            return match store.snapshot(project, Kind::Edit, "", now_ms()) {
                Ok(Some(v)) => {
                    tracing::info!("versions: saved {} ({} changed)", v.id, v.changed.len());
                    saved(v);
                    Ok(())
                }
                Ok(None) => Ok(()),
                Err(e) => Err(format!("could not save a version of the project: {e:#}")),
            };
        }
        Msg::Save(label) => {
            if label.is_empty() {
                ctx.hub.log("error", "project", "project.version.save needs a `label` (the version's name)");
                return Ok(());
            }
            return match store.snapshot(project, Kind::Named, &label, now_ms()) {
                Ok(Some(v)) => {
                    saved(v);
                    Ok(())
                }
                Ok(None) => Ok(()),
                Err(e) => Err(format!("could not save the version \"{label}\": {e:#}")),
            };
        }
        Msg::Restore(id) => (Kind::Restore, store.restore(project, &id, now_ms())),
        Msg::Undo => (Kind::Undo, store.undo(project, now_ms())),
        Msg::Redo => (Kind::Redo, store.redo(project, now_ms())),
    };
    match res {
        Ok(changed) => {
            let v = store.list().last().filter(|v| v.kind == kind);
            let (id, from) = v.map(|v| (v.id.clone(), v.restored_from.clone().unwrap_or_default())).unwrap_or_default();
            tracing::info!("versions: {} → {from} ({} files)", kind.as_str(), changed.len());
            if !changed.is_empty() {
                crate::daemon::reload(ctx, &changed);
            }
            ctx.hub.emit(Event::new(
                "project.version.restored",
                Origin::System,
                Value::map().with("id", id).with("kind", kind.as_str()).with("restored_from", from).with("changed", changed),
            ));
            Ok(())
        }
        Err(e) => {
            ctx.hub.log("warn", "project", format!("{e:#}"));
            Ok(())
        }
    }
}

fn preview(p: Option<Preview>) -> Value {
    p.map_or(Value::Null, |p| Value::map().with("kind", p.kind.as_str()).with("at_ms", p.at_ms).with("changed", p.changed))
}

/// Refresh the query result and the `project.versions.*` state.
fn publish(ctx: &Ctx, store: &Versions, list: &RwLock<Value>) {
    let all = store.list();
    let rows: Vec<Value> = all
        .iter()
        .rev()
        .map(|v| {
            let from = v.restored_from.as_deref().and_then(|id| store.get(id)).map_or(Value::Null, |s| {
                Value::map().with("id", s.id.clone()).with("at_ms", s.at_ms).with("label", s.label.clone()).with("kind", s.kind.as_str())
            });
            Value::map()
                .with("id", v.id.clone())
                .with("at_ms", v.at_ms)
                .with("label", v.label.clone())
                .with("auto", v.auto())
                .with("kind", v.kind.as_str())
                .with("changed", v.changed.clone())
                .with("first", v.first)
                .with("restored_from", from)
        })
        .collect();
    *list.write() = Value::List(rows);
    ctx.hub.publish("project.versions.count", Value::Int(all.len() as i64));
    ctx.hub.publish("project.versions.latest", all.last().map_or(Value::Null, |v| Value::Str(v.id.clone())));
    ctx.hub.publish("project.versions.undo", preview(store.undo_preview()));
    ctx.hub.publish("project.versions.redo", preview(store.redo_preview()));
    ctx.hub.publish("project.versions.bytes", Value::Int(store.disk_bytes() as i64));
}
