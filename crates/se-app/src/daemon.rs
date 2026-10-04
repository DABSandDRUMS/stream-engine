//! `stream-engine daemon`: the headless engine process (§3.1). Runs the core thread, the API,
//! project hot reload, session logging, runtime persistence, and the systemd watchdog.

use anyhow::{Context, Result};
use parking_lot::Mutex;
use se_api::Auth;
use se_core::{Config, Core, Input, RuntimeState};
use se_hub::{Bus, Hub, HubInfo, RunnerHooks, run_core};
use se_proto::{Event, Op, Origin, Value};
use se_store::session::{LogRec, new_session_id};
use se_store::{Db, Project, SessionWriter, secrets};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

#[derive(Clone, Debug)]
pub struct DaemonOpts {
    pub project: PathBuf,
    pub dev: bool,
    pub socket: Option<PathBuf>,
    pub http: SocketAddr,
    pub osc: SocketAddr,
    pub restore: bool,
    pub web_root: PathBuf,
    pub data_dir: PathBuf,
}

/// Messages for the session-writer thread.
pub enum SessionMsg {
    Rec(LogRec),
    Signals(u64, Arc<Vec<String>>, Vec<f32>),
    Marker(Value),
    Meta(String, Value),
    /// Persist recorder metadata before file finalization permits session rotation.
    Persist {
        session: String,
        values: Value,
        done: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Flush,
    /// Close the current session and start `new_id` (with a Start record).
    Rotate {
        new_id: String,
        start: LogRec,
        done: tokio::sync::oneshot::Sender<Result<(), String>>,
    },
    Close,
}

pub fn run(opts: DaemonOpts) -> Result<()> {
    crate::logging::init(opts.dev);
    let project = Arc::new(Project::open(&opts.project)?);
    tracing::info!("project {}", project.root().display());
    for step in project.migrate().context("migrate project")? {
        tracing::warn!("migration: {step}");
    }
    let loaded = project.load();
    let mut config = Config::build(&loaded.files);
    config.errors.extend(loaded.errors.clone());
    for e in &config.errors {
        tracing::error!("{}: {}", e.file, e.msg);
    }

    std::fs::create_dir_all(&opts.data_dir)?;
    let db = Db::open(&opts.data_dir.join("runtime.db"))?;
    let prev = db.load_runtime()?;
    let restore: Option<RuntimeState> = if opts.restore { prev.as_ref().and_then(|(s, _)| serde_json::from_str(s).ok()) } else { None };
    let session_id = match db.open_session()? {
        Some((id, _)) if restore.is_some() => id,
        Some((id, _)) => {
            db.session_close(&id)?;
            new_session_id()
        }
        None => new_session_id(),
    };
    let sessions_root = project.root().join("sessions");
    db.session_open(&session_id, &sessions_root.join(&session_id).to_string_lossy())?;

    let clock = Arc::new(se_clock::Clock::new());
    let t0 = se_clock::now();
    let mut core = Core::new(config.clone(), t0);
    if let Some(rs) = &restore {
        core.restore(rs);
        tracing::info!("restored runtime state: mode={} scene={}", rs.mode, rs.program);
    }
    let period = core.period();
    let (hub, rx) = Hub::new(clock.clone());
    *hub.info.write() = HubInfo {
        session: session_id.clone(),
        version: env!("CARGO_PKG_VERSION").into(),
        project: project.root().display().to_string(),
        started_wall: (se_clock::wall_now_ns() / 1_000_000_000) as i64,
    };
    crate::logging::attach_hub(hub.clone());

    // session writer
    let (log_tx, log_rx) = crossbeam_channel::unbounded::<SessionMsg>();
    let start = LogRec::Start { t0, period, tick: 0, wall_ns: se_clock::wall_now_ns() as i64, restore: restore.clone() };
    // Register before the core starts: even its first events belong in the session journal.
    let journal_events = hub.subscribe_events();
    {
        let (root, id) = (sessions_root.clone(), session_id.clone());
        std::thread::Builder::new().name("se-session".into()).spawn(move || session_thread(root, id, start, log_rx))?;
    }

    // runtime-state writer: every change is persisted right away (§17.3)
    let (rt_tx, rt_rx) = crossbeam_channel::unbounded::<RuntimeState>();
    let session_shared = Arc::new(Mutex::new(session_id.clone()));
    {
        let (db, sess) = (db.clone(), session_shared.clone());
        std::thread::Builder::new().name("se-persist".into()).spawn(move || {
            while let Ok(mut rs) = rt_rx.recv() {
                while let Ok(newer) = rt_rx.try_recv() {
                    rs = newer;
                }
                match serde_json::to_string(&rs) {
                    Ok(js) => {
                        let sid = sess.lock().clone();
                        if let Err(e) = db.save_runtime(&js, Some(&sid)) {
                            tracing::error!("save runtime: {e:#}");
                        }
                    }
                    Err(e) => tracing::error!("serialize runtime: {e}"),
                }
            }
        })?;
    }

    // core thread
    {
        let hub = hub.clone();
        let tx = log_tx.clone();
        let hooks = RunnerHooks {
            on_applied: Box::new(move |v| {
                for (tick, mut input) in v {
                    // secrets never reach the session log (§19)
                    if let Input::Command { cmd } = &mut input
                        && cmd.op.is_secret()
                    {
                        cmd.op = cmd.op.redacted();
                    }
                    let _ = tx.send(SessionMsg::Rec(LogRec::In { tick, input }));
                }
            }),
            snapshot_every: 2,
            on_runtime: Some(Box::new(move |rs| {
                let _ = rt_tx.send(rs);
            })),
        };
        std::thread::Builder::new().name("se-core".into()).spawn(move || run_core(core, hub, rx, hooks))?;
    }

    let rt = tokio::runtime::Builder::new_multi_thread().enable_all().thread_name("se-io").build()?;
    let (config_tx, config_rx) = tokio::sync::watch::channel(Arc::new(config.clone()));
    let engine = se_hub::EngineCtx {
        hub: hub.clone(),
        db: db.clone(),
        project_root: project.root().to_path_buf(),
        data_dir: opts.data_dir.clone(),
        share_dir: crate::share_dir(),
        config: config_rx,
        http: opts.http,
        dev: opts.dev,
    };
    let ctx = Ctx {
        hub: hub.clone(),
        db: db.clone(),
        project: project.clone(),
        opts: opts.clone(),
        log_tx: log_tx.clone(),
        config: Arc::new(Mutex::new(config)),
        session: session_shared,
        t0,
        config_tx: Arc::new(config_tx),
        engine,
    };
    let res = rt.block_on(async_main(ctx, journal_events));
    hub.shutdown();
    let _ = log_tx.send(SessionMsg::Close);
    std::thread::sleep(Duration::from_millis(200));
    res
}

/// Shared engine context for subsystems.
#[derive(Clone)]
pub struct Ctx {
    pub hub: Arc<Hub>,
    pub db: Db,
    pub project: Arc<Project>,
    pub opts: DaemonOpts,
    pub log_tx: crossbeam_channel::Sender<SessionMsg>,
    pub config: Arc<Mutex<Config>>,
    pub session: Arc<Mutex<String>>,
    pub t0: u64,
    /// Publishes each applied configuration to subsystems.
    pub config_tx: Arc<tokio::sync::watch::Sender<Arc<Config>>>,
    pub engine: se_hub::EngineCtx,
}

fn session_thread(root: PathBuf, id: String, start: LogRec, rx: crossbeam_channel::Receiver<SessionMsg>) {
    let open = |id: &str, start: &LogRec| -> Option<SessionWriter> {
        match SessionWriter::open(&root, id) {
            Ok(mut w) => {
                let _ = w.write(start);
                let _ = w.flush();
                Some(w)
            }
            Err(e) => {
                tracing::error!("session {id}: {e:#}");
                None
            }
        }
    };
    let mut w = open(&id, &start);
    let mut current_id = id;
    let mut last_flush = std::time::Instant::now();
    loop {
        let msg = match rx.recv_timeout(Duration::from_millis(500)) {
            Ok(m) => Some(m),
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => None,
            Err(_) => break,
        };
        match msg {
            Some(SessionMsg::Rotate { new_id, start, done }) => {
                let closed = w.take().map_or(Ok(()), |old| old.close().map_err(|e| format!("{e:#}")));
                w = open(&new_id, &start);
                current_id = new_id;
                let result = closed.and_then(|_| w.as_ref().map(|_| ()).ok_or_else(|| "new session writer unavailable".to_string()));
                let _ = done.send(result);
            }
            Some(SessionMsg::Close) => break,
            Some(SessionMsg::Persist { session, values, done }) => {
                let result = if session != current_id {
                    Err(format!("cannot persist recording metadata for closed session {session}"))
                } else if let Some(writer) = w.as_mut() {
                    match values {
                        Value::Map(values) => {
                            values.iter().try_for_each(|(key, value)| writer.set_meta(key, value)).and_then(|_| writer.flush()).map_err(|e| format!("{e:#}"))
                        }
                        _ => Err("session.persist needs a values map".into()),
                    }
                } else {
                    Err("session writer unavailable".into())
                };
                let _ = done.send(result);
            }
            Some(m) => {
                if let Some(w) = w.as_mut() {
                    let r = match m {
                        // external commands/events are flushed at once so a crash loses none
                        SessionMsg::Rec(r @ LogRec::In { input: Input::Command { .. } | Input::Event { .. }, .. }) => w.write(&r).and_then(|_| w.flush()),
                        SessionMsg::Rec(r) => w.write(&r),
                        SessionMsg::Signals(ts, names, vals) => w.write_signals(ts, &names, &vals),
                        SessionMsg::Marker(m) => w.add_marker(&m),
                        SessionMsg::Meta(k, v) => w.set_meta(&k, &v),
                        SessionMsg::Flush => w.flush(),
                        SessionMsg::Rotate { .. } | SessionMsg::Persist { .. } | SessionMsg::Close => Ok(()),
                    };
                    if let Err(e) = r {
                        tracing::warn!("session write: {e:#}");
                    }
                }
            }
            None => {}
        }
        if last_flush.elapsed() > Duration::from_secs(1) {
            if let Some(w) = w.as_mut() {
                let _ = w.flush();
            }
            last_flush = std::time::Instant::now();
        }
    }
    if let Some(w) = w {
        let _ = w.close();
    }
}

async fn async_main(ctx: Ctx, journal_events: tokio::sync::mpsc::UnboundedReceiver<Event>) -> Result<()> {
    let hub = ctx.hub.clone();

    // --- API --------------------------------------------------------------------------
    let token = match secrets::get_or_create(secrets::names::API_TOKEN) {
        Ok(t) => Some(t),
        Err(e) => {
            tracing::error!("keyring unavailable ({e:#}); WebSocket/OSC API disabled");
            None
        }
    };
    let auth = Arc::new(Auth::new(token));
    let sock = ctx.opts.socket.clone().unwrap_or_else(se_proto::wire::default_socket_path);
    {
        let (h, a, s) = (hub.clone(), auth.clone(), sock.clone());
        tokio::spawn(async move {
            if let Err(e) = se_api::serve_unix(h, a, s).await {
                tracing::error!("unix socket: {e:#}");
                std::process::exit(2);
            }
        });
    }
    {
        let st = se_api::HttpState { hub: hub.clone(), auth: auth.clone() };
        let app = se_api::router(st, &ctx.opts.web_root, crate::http_extra::routes(&ctx, auth.clone()));
        let bind = ctx.opts.http;
        tokio::spawn(async move {
            if let Err(e) = se_api::serve_http(bind, app).await {
                tracing::error!("http: {e:#}");
            }
        });
    }
    {
        let (h, a, b) = (hub.clone(), auth.clone(), ctx.opts.osc);
        tokio::spawn(async move {
            if let Err(e) = se_api::osc::serve_osc(h, a, b).await {
                tracing::error!("osc: {e:#}");
            }
        });
    }

    // --- project hot reload + write-back ------------------------------------------------
    let (reload_tx, mut reload_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<String>>();
    let _watcher = ctx.project.watch(move |paths| {
        let _ = reload_tx.send(paths);
    })?;
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            while let Some(paths) = reload_rx.recv().await {
                reload(&ctx, &paths);
            }
        });
    }
    {
        let ctx = ctx.clone();
        let mut rx = hub.route_actions("project");
        tokio::spawn(async move {
            while let Some(c) = rx.recv().await {
                let Op::Action { name, args } = &c.op else { continue };
                match name.as_str() {
                    "project.persist_base" => {
                        let (Some(a), Some(v)) = (args.get_path("address").and_then(Value::as_str), args.get_path("value")) else { continue };
                        match ctx.project.write_base(a, v) {
                            Ok(file) => tracing::debug!("saved {a} → {file}"),
                            Err(e) => ctx.hub.log("error", "project", format!("could not save {a}: {e:#}")),
                        }
                    }
                    "project.reload" => reload(&ctx, &["(manual)".into()]),
                    "project.write" => match crate::project_io::write(&ctx, args) {
                        Ok(Some(rel)) => reload(&ctx, &[rel]),
                        Ok(None) => {}
                        Err(e) => ctx.hub.log("error", "project", format!("project.write: {e:#}")),
                    },
                    "project.import" | "project.asset.rename" | "project.asset.delete" => {
                        let changed = crate::assets::action(&ctx, name, args);
                        if !changed.is_empty() {
                            reload(&ctx, &changed);
                        }
                    }
                    other => ctx.hub.log("warn", "project", format!("unknown action {other}")),
                }
            }
        });
    }

    // --- session: events, signals, markers, rotation -------------------------------------
    crate::session_tasks::start(&ctx, journal_events);

    // --- runtime persistence + audit ------------------------------------------------------
    {
        let ctx = ctx.clone();
        tokio::spawn(async move { persist_loop(ctx).await });
    }

    // --- engine subsystems -----------------------------------------------------------------
    crate::subsystems::start(&ctx, auth.clone()).await;

    // --- queries ---------------------------------------------------------------------------
    crate::queries::register(&ctx);
    crate::queries::register_preflight(&ctx);
    crate::project_io::register_queries(&ctx);
    crate::assets::register_queries(&ctx);
    crate::presets_catalog::register(&ctx);
    crate::project_io::start_uptime(&ctx);
    crate::versions::start(&ctx);
    crate::api_admin::start(&ctx, auth.clone());

    // --- systemd readiness + watchdog -------------------------------------------------------
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Ready]);
    {
        let hub = hub.clone();
        let wd_period = sd_notify::watchdog_enabled();
        let wd = wd_period.is_some();
        std::thread::Builder::new().name("se-watchdog".into()).spawn(move || {
            let interval = wd_period.map(|d| d / 3).unwrap_or(Duration::from_secs(1));
            loop {
                std::thread::sleep(interval);
                let now = se_clock::now();
                let core_ok = now.saturating_sub(hub.core_heartbeat.load(Ordering::Relaxed)) < 2_000_000_000;
                let r = hub.render_heartbeat.load(Ordering::Relaxed);
                let render_ok = r == 0 || now.saturating_sub(r) < 2_000_000_000;
                if core_ok && render_ok {
                    if wd {
                        let _ = sd_notify::notify(&[sd_notify::NotifyState::Watchdog]);
                    }
                } else {
                    tracing::error!("watchdog: core_ok={core_ok} render_ok={render_ok}; withholding heartbeat");
                }
            }
        })?;
    }
    hub.ready();
    tracing::info!("engine ready (session {})", ctx.session.lock());
    hub.emit(Event::new("engine.started", Origin::System, Value::map().with("session", ctx.session.lock().clone())));

    // --- shutdown --------------------------------------------------------------------------
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    tokio::select! {
        _ = term.recv() => tracing::info!("SIGTERM"),
        _ = tokio::signal::ctrl_c() => tracing::info!("SIGINT"),
    }
    let _ = sd_notify::notify(&[sd_notify::NotifyState::Stopping]);
    // The app owns recording. Finalize encoders and persist their last metadata before
    // stopping subsystems or closing the session writer.
    if let Err(e) = ctx.hub.query("recording.finalize", Value::Null).await {
        tracing::warn!("recording finalization on shutdown: {e}");
    }
    save_runtime(&ctx).await;
    let _ = ctx.log_tx.send(SessionMsg::Flush);
    crate::subsystems::stop(&ctx).await;
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

pub fn reload(ctx: &Ctx, paths: &[String]) {
    // Watcher reloads must not observe a half-written multi-file input edit.
    let _write_guard = crate::project_io::CONFIG_WRITE.lock();
    let loaded = ctx.project.load();
    let mut next = Config::build(&loaded.files);
    let prev = ctx.config.lock().clone();
    let mut failed = loaded.failed.clone();
    failed.extend(next.errors.iter().map(|e| e.file.clone()));
    next = next.merge_last_good(&prev, &failed);
    next.errors.extend(loaded.errors.clone());
    let n = next.errors.len();
    for e in &next.errors {
        ctx.hub.log("error", "project", format!("{}: {}", e.file, e.msg));
    }
    *ctx.config.lock() = next.clone();
    ctx.hub.submit(Input::Config { config: Box::new(next.clone()) });
    let _ = ctx.config_tx.send(Arc::new(next));
    ctx.hub.publish("project.errors", Value::Int(n as i64));
    ctx.hub.emit(Event::new("project.reloaded", Origin::System, Value::map().with("files", paths.to_vec()).with("errors", n)));
    tracing::info!("project reloaded ({} changed, {n} errors)", paths.len());
}

async fn save_runtime(ctx: &Ctx) {
    if let Some(rs) = ctx.hub.runtime().await {
        match serde_json::to_string(&rs) {
            Ok(js) => {
                let s = ctx.session.lock().clone();
                if let Err(e) = ctx.db.save_runtime(&js, Some(&s)) {
                    tracing::error!("save runtime: {e:#}");
                }
            }
            Err(e) => tracing::error!("serialize runtime: {e}"),
        }
    }
}

async fn persist_loop(ctx: Ctx) {
    let mut bus = ctx.hub.subscribe();
    let mut tick = tokio::time::interval(Duration::from_millis(250));
    let mut pending: std::collections::HashMap<u64, (String, Option<String>, String)> = Default::default();
    let mut since_full = std::time::Instant::now();
    loop {
        tokio::select! {
            b = bus.recv() => match b {
                Ok(b) => match &*b {
                    Bus::Ack { id, ok, error } => {
                        if let Some((origin, actor, op)) = pending.remove(id) {
                            let _ = ctx.db.audit(&origin, actor.as_deref(), &op, *ok, error.as_deref());
                        }
                    }
                    Bus::Trace(recs) => {
                        // external commands appear as root `command` records: remember for audit
                        for r in recs.iter().filter(|r| r.kind == "command" && r.parent.is_none()) {
                            let origin = r.label.rsplit_once('(').map(|(_, o)| o.trim_end_matches(')').to_string()).unwrap_or_default();
                            // the UI's focus reports are bookkeeping, not something the operator did
                            if origin != "system" && !r.label.starts_with("ui.focus ") {
                                pending.insert(r.id, (origin, None, r.label.clone()));
                            }
                        }
                        if pending.len() > 5000 {
                            pending.clear();
                        }
                    }
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                Err(_) => break,
            },
            _ = tick.tick() => {
                if since_full.elapsed() > Duration::from_secs(5) {
                    since_full = std::time::Instant::now();
                    save_runtime(&ctx).await;
                }
            }
        }
    }
}

#[cfg(test)]
mod session_tests {
    use super::*;

    #[tokio::test]
    async fn recording_metadata_is_persisted_before_close_and_cannot_leak_into_next_show() {
        let root = tempfile::tempdir().unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let dir = root.path().to_owned();
        let start = || LogRec::Start { t0: 1, period: 4_166_666, tick: 0, wall_ns: 0, restore: None };
        let thread = std::thread::spawn(move || session_thread(dir, "first".into(), start(), rx));
        let (done, ack) = tokio::sync::oneshot::channel();
        let recordings = Value::List(vec![Value::map().with("path", "wide.mkv").with("start_ns", 1000i64).with("end_ns", 9000i64)]);
        tx.send(SessionMsg::Persist { session: "first".into(), values: Value::map().with("recordings", recordings), done }).unwrap();
        ack.await.unwrap().unwrap();
        let meta: toml::Value = toml::from_str(&std::fs::read_to_string(root.path().join("first/meta.toml")).unwrap()).unwrap();
        assert_eq!(meta["recordings"][0]["end_ns"].as_integer(), Some(9000));
        let (done, ack) = tokio::sync::oneshot::channel();
        tx.send(SessionMsg::Rotate { new_id: "second".into(), start: start(), done }).unwrap();
        ack.await.unwrap().unwrap();
        let (done, ack) = tokio::sync::oneshot::channel();
        tx.send(SessionMsg::Persist { session: "first".into(), values: Value::map().with("recordings", Value::List(vec![])), done }).unwrap();
        assert!(ack.await.unwrap().is_err(), "late metadata must not overwrite the next show");
        assert!(!root.path().join("second/meta.toml").exists());
        tx.send(SessionMsg::Close).unwrap();
        thread.join().unwrap();
    }
}
