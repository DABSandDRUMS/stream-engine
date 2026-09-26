//! Recording and clips (§18): OBS captures every show in the chosen folder; the session journal
//! and an indexed show timeline keep video, songs, chat, scenes, lights and transcript aligned.
//! The hype detector makes markers; the post-show job ranks song and marker candidates, cuts
//! wide + tall video, and sends them to a human review queue. Talk clips may drop music;
//! performance clips keep it. The external `rank_command` can supply AI decisions.
//!
//! State: `clips.hype.enabled`, `clips.hype.threshold` (live), readback `clips.hype.active`,
//! `clips.hype.delay_ms`, `clips.hype.markers`, `clips.job.*`, `clips.pending`.
//! Signal: `hype.score`. Events: `clips.progress`, `clips.done`, `clips.failed`,
//! `clips.updated`.

pub mod captions;
pub mod config;
pub mod ffmpeg;
pub mod hype;
pub mod index;
pub mod job;
pub mod recording;
pub mod select;
pub mod session;
pub mod show;
pub mod store;
pub mod tracks;
pub mod transcribe;

use config::ClipsConfig;
use parking_lot::Mutex;
use se_hub::{Bus, EngineCtx, Hub};
use se_proto::{Command, Event, Meta, Op, Origin, Value};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, watch};

const TARGET: &str = "clips";

/// Work for the (single, serialized) clip worker.
#[derive(Clone, Debug, PartialEq)]
enum Work {
    Process(String),
    Make { session: String, t_in: f64, t_out: f64 },
    Retrim { id: i64, t_in: f64, t_out: f64 },
    Upload(i64),
}

struct Shared {
    ctx: EngineCtx,
    cfg: watch::Receiver<Arc<ClipsConfig>>,
    queue: Mutex<VecDeque<Work>>,
    queued: Mutex<HashSet<String>>,
    wake: mpsc::UnboundedSender<()>,
}

impl Shared {
    fn hub(&self) -> &Arc<Hub> {
        &self.ctx.hub
    }
    fn cfg(&self) -> Arc<ClipsConfig> {
        self.cfg.borrow().clone()
    }
    fn env(&self) -> job::JobEnv {
        job::JobEnv { db: self.ctx.db.clone(), project_root: self.ctx.project_root.clone(), data_dir: self.ctx.data_dir.clone(), cfg: (*self.cfg()).clone() }
    }
    fn push(&self, w: Work) {
        if let Work::Process(s) = &w {
            if !self.queued.lock().insert(s.clone()) {
                return;
            }
            if let Err(e) = store::job_queue(&self.ctx.db, s) {
                self.hub().log("error", TARGET, format!("queue {s}: {e:#}"));
            }
            job::update_keep(&self.env(), s);
        }
        let n = {
            let mut q = self.queue.lock();
            if !q.contains(&w) {
                q.push_back(w);
            }
            q.len()
        };
        self.hub().publish("clips.job.queue", Value::Int(n as i64));
        let _ = self.wake.send(());
    }
    fn publish_pending(&self) {
        if let Ok(n) = store::pending_count(&self.ctx.db) {
            self.hub().publish("clips.pending", Value::Int(n));
        }
    }
}

fn parse_cfg(ctx: &EngineCtx) -> Result<ClipsConfig, String> {
    ClipsConfig::from_section(ctx.project_section("clips").as_ref())
}

/// Start the clips subsystem.
pub async fn start(ctx: EngineCtx) -> anyhow::Result<()> {
    store::migrate(&ctx.db)?;
    let initial = parse_cfg(&ctx).unwrap_or_else(|e| {
        ctx.hub.log("error", TARGET, format!("{e}; using defaults"));
        ClipsConfig::default()
    });
    let (cfg_tx, cfg_rx) = watch::channel(Arc::new(initial));
    let (wake_tx, wake_rx) = mpsc::unbounded_channel();
    let shared = Arc::new(Shared { ctx: ctx.clone(), cfg: cfg_rx, queue: Mutex::new(VecDeque::new()), queued: Mutex::new(HashSet::new()), wake: wake_tx });
    declare(&shared);

    // config hot reload (a broken [clips] keeps the last good settings)
    {
        let ctx = ctx.clone();
        tokio::spawn(async move {
            let mut rx = ctx.config.clone();
            while rx.changed().await.is_ok() {
                match parse_cfg(&ctx) {
                    Ok(c) => {
                        if **cfg_tx.borrow() != c {
                            let _ = cfg_tx.send(Arc::new(c));
                        }
                    }
                    Err(e) => ctx.hub.log("error", TARGET, format!("{e}; keeping the previous [clips] settings")),
                }
            }
        });
    }
    tokio::spawn(hype_task(shared.clone()));
    tokio::spawn(worker(shared.clone(), wake_rx));
    tokio::spawn(actions(shared.clone()));
    tokio::spawn(events(shared.clone()));
    register_queries(&shared);
    recording::start(ctx.clone()).await?;
    index::start(ctx.clone()).await?;
    tokio::spawn(preflight(shared.clone()));
    // resume jobs interrupted by a restart
    for s in store::jobs_unfinished(&ctx.db).unwrap_or_default() {
        shared.push(Work::Process(s));
    }
    shared.publish_pending();
    Ok(())
}

/// Live-adjustable hype settings; their defaults follow `[clips.hype]` (explicit values win).
fn declare_hype(hub: &Hub, c: &ClipsConfig) {
    hub.declare("clips.hype.enabled", Meta::boolean(c.hype.enabled).describe("Hype detector writes markers").owner(TARGET));
    hub.declare(
        "clips.hype.threshold",
        Meta::float(c.hype.threshold, [0.1, 10.0]).describe("Hype score that opens a moment (lower = more markers)").owner(TARGET),
    );
}

fn declare(sh: &Shared) {
    let hub = sh.hub();
    let own = |m: Meta| m.owner(TARGET);
    declare_hype(hub, &sh.cfg());
    hub.declare("clips.hype.active", own(Meta::boolean(false).readonly().describe("A hype moment is in progress")));
    hub.declare("clips.hype.delay_ms", own(Meta::int(0, [0.0, 120_000.0]).readonly().unit("ms").describe("Stream delay applied to chat reactions")));
    hub.declare("clips.hype.markers", own(Meta::int(0, [0.0, 1e9]).readonly().describe("Hype markers this session")));
    hub.declare("clips.job.state", own(Meta::enumeration("idle", &["idle", "running", "failed"]).readonly()));
    hub.declare("clips.job.session", own(Meta::string("").readonly()));
    hub.declare("clips.job.stage", own(Meta::string("").readonly()));
    hub.declare("clips.job.progress", own(Meta::float(0.0, [0.0, 1.0]).readonly()));
    hub.declare("clips.job.queue", own(Meta::int(0, [0.0, 1e6]).readonly()));
    hub.declare("clips.pending", own(Meta::int(0, [0.0, 1e9]).readonly().describe("Clips waiting for review")));
}

// ---- hype detector task ----------------------------------------------------------------------

async fn hype_task(sh: Arc<Shared>) {
    let hub = sh.hub().clone();
    let mut cfg_rx = sh.cfg.clone();
    let mut cfg = sh.cfg();
    let mut det = hype::HypeDetector::new(cfg.hype.clone());
    let mut bus = hub.subscribe();
    let mut tick = tokio::time::interval(Duration::from_millis(1000 / hype::SAMPLE_HZ as u64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut last_eval = 0u64;
    let mut markers = 0i64;
    let mut active = false;
    let mut delay_pub = u64::MAX;
    loop {
        tokio::select! {
            b = bus.recv() => match b {
                Ok(b) => {
                    if let Bus::Event(e) = &*b {
                        if e.ty == "session.closed" {
                            markers = 0;
                            hub.publish("clips.hype.markers", Value::Int(0));
                        } else if let Some(input) = hype::classify(e) {
                            det.input(if e.ts == 0 { se_clock::now() } else { e.ts }, &input, delay_ns(&hub, &cfg));
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => tracing::warn!("hype detector lagged {n} bus messages"),
                Err(_) => break,
            },
            r = cfg_rx.changed() => {
                if r.is_err() { break; }
                cfg = cfg_rx.borrow_and_update().clone();
                det.reconfigure(cfg.hype.clone());
                declare_hype(&hub, &cfg);
            }
            _ = tick.tick() => {
                let now = se_clock::now();
                let delay = delay_ns(&hub, &cfg);
                let snap = hub.snapshot.load();
                let enabled = snap.get("clips.hype.enabled").map(Value::truthy).unwrap_or(cfg.hype.enabled);
                if let Some(t) = snap.f32("clips.hype.threshold") {
                    det.set_threshold(t as f64);
                }
                let music = cfg.hype.music_signals.iter().filter_map(|s| snap.signal(s)).reduce(f32::max);
                det.sample(now, snap.signal(&cfg.hype.mic_signal), music, snap.signal(&cfg.hype.chat_rate_signal), delay);
                let offline = snap.str("show.mode") == Some("offline");
                drop(snap);
                for m in det.advance(now, delay) {
                    if !enabled || offline {
                        continue;
                    }
                    markers += 1;
                    hub.publish("clips.hype.markers", Value::Int(markers));
                    hub.command(Command::new(Origin::Patch, Op::Action { name: "session.marker".into(), args: m.to_value() }));
                    hub.command(Command::new(Origin::Patch, Op::Action { name: "twitch.marker".into(), args: Value::map().with("description", m.description()) }));
                    tracing::info!("hype marker {:.2} ({})", m.score, m.reasons.join(", "));
                }
                if det.last_eval != last_eval {
                    last_eval = det.last_eval;
                    hub.signal("hype.score", if enabled { det.score as f32 } else { 0.0 });
                }
                if det.active() != active {
                    active = det.active();
                    hub.publish("clips.hype.active", Value::Bool(active && enabled));
                }
                if delay != delay_pub {
                    delay_pub = delay;
                    hub.publish("clips.hype.delay_ms", Value::Int((delay / 1_000_000) as i64));
                }
            }
        }
    }
}

/// Measured Twitch delay (§3.2), or the configured fallback until it's measured.
fn delay_ns(hub: &Hub, cfg: &ClipsConfig) -> u64 {
    let measured = hub.clock.mappings().twitch_delay_ms as u64;
    (if measured > 0 { measured } else { cfg.hype.fallback_delay.0 }) * 1_000_000
}

// ---- events: session close → job; manual markers → Twitch markers -----------------------------

async fn events(sh: Arc<Shared>) {
    let mut bus = sh.hub().subscribe();
    loop {
        let b = match bus.recv().await {
            Ok(b) => b,
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(_) => break,
        };
        let Bus::Event(e) = &*b else { continue };
        match e.ty.as_str() {
            "session.closed" => {
                if let Some(s) = e.payload.get_path("session").and_then(Value::as_str)
                    && sh.cfg().auto_process
                {
                    tracing::info!("session {s} closed: clip job queued");
                    sh.push(Work::Process(s.to_string()));
                }
            }
            "session.marker" if e.payload.get_path("args.kind").and_then(Value::as_str) != Some("hype") => {
                let label = e.payload.get_path("label").map(|v| v.to_string()).unwrap_or_else(|| "clip".into());
                let desc: String = format!("clip: {label}").chars().take(140).collect();
                sh.hub().command(Command::new(Origin::Patch, Op::Action { name: "twitch.marker".into(), args: Value::map().with("description", desc) }));
            }
            _ => {}
        }
    }
}

// ---- worker ----------------------------------------------------------------------------------

async fn worker(sh: Arc<Shared>, mut wake: mpsc::UnboundedReceiver<()>) {
    loop {
        let next = sh.queue.lock().pop_front();
        let Some(w) = next else {
            sh.hub().publish("clips.job.queue", Value::Int(0));
            if wake.recv().await.is_none() {
                break;
            }
            continue;
        };
        sh.hub().publish("clips.job.queue", Value::Int(sh.queue.lock().len() as i64));
        run_work(&sh, w).await;
        sh.publish_pending();
    }
}

fn progress_fn(hub: Arc<Hub>) -> impl Fn(job::Progress) + Send + 'static {
    move |p: job::Progress| {
        let f = p.fraction();
        hub.publish("clips.job.stage", Value::Str(p.stage.clone()));
        hub.publish("clips.job.progress", Value::Float(f));
        hub.emit(Event::new(
            "clips.progress",
            Origin::System,
            Value::map()
                .with("session", p.session)
                .with("stage", p.stage)
                .with("done", p.done as i64)
                .with("total", p.total as i64)
                .with("progress", (f * 1000.0).round() / 1000.0)
                .with("detail", p.detail),
        ));
    }
}

async fn run_work(sh: &Arc<Shared>, w: Work) {
    let hub = sh.hub().clone();
    let env = sh.env();
    let nice = env.cfg.nice;
    match w {
        Work::Process(session) => {
            hub.publish("clips.job.state", Value::Str("running".into()));
            hub.publish("clips.job.session", Value::Str(session.clone()));
            // the session usually closes right after "stop OBS": let the recording finish first
            let waited = std::time::Instant::now();
            if hub.snapshot.load().bool("obs.record.active") {
                hub.publish("clips.job.stage", Value::Str("waiting for OBS to stop recording".into()));
            }
            while hub.snapshot.load().bool("obs.record.active") && waited.elapsed() < Duration::from_secs(600) {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
            let _ = store::job_update(&env.db, &session, "running", "load", None, 0, &Value::Null);
            let prog = progress_fn(hub.clone());
            let s = session.clone();
            let index_hub = hub.clone();
            let index_cfg = show::RecordingConfig::from_section(sh.ctx.project_section("recording").as_ref());
            let res = tokio::task::spawn_blocking(move || {
                transcribe::niced(nice, move || {
                    if let Ok(cfg) = index_cfg
                        && cfg.index.enabled
                        && let Err(e) = index::ensure(&job::session_dir(&env, &s), &s, &cfg)
                    {
                        index_hub.log("warn", TARGET, format!("show timeline for {s}: {e}; cutting from markers and events"));
                    }
                    job::process(&env, &s, &prog)
                })
                .and_then(|r| r)
            })
            .await
            .unwrap_or_else(|e| Err(format!("clip job panicked: {e}")));
            sh.queued.lock().remove(&session);
            let env = sh.env();
            match res {
                Ok(report) => {
                    let timings = report.timings_value();
                    let _ = store::job_update(&env.db, &session, "done", "done", None, report.clips as i64, &timings);
                    hub.publish("clips.job.state", Value::Str("idle".into()));
                    hub.publish("clips.job.stage", Value::Str("done".into()));
                    hub.publish("clips.job.progress", Value::Float(1.0));
                    tracing::info!("clips for {session}: {} ok, {} failed, timings {timings}", report.clips, report.failed);
                    for s in &report.skipped {
                        hub.log("warn", TARGET, format!("{session}: {s}"));
                    }
                    hub.emit(Event::new(
                        "clips.done",
                        Origin::System,
                        Value::map()
                            .with("session", session.clone())
                            .with("clips", report.clips as i64)
                            .with("failed", report.failed as i64)
                            .with("skipped", report.skipped.clone())
                            .with("timings", timings),
                    ));
                }
                Err(e) => {
                    let _ = store::job_update(&env.db, &session, "failed", "failed", Some(&e), 0, &Value::Null);
                    hub.publish("clips.job.state", Value::Str("failed".into()));
                    hub.log("error", TARGET, format!("clip job {session}: {e}"));
                    hub.emit(Event::new("clips.failed", Origin::System, Value::map().with("session", session.clone()).with("error", e)));
                }
            }
            job::update_keep(&env, &session);
        }
        Work::Make { session, t_in, t_out } => {
            hub.publish("clips.job.state", Value::Str("running".into()));
            hub.publish("clips.job.session", Value::Str(session.clone()));
            hub.publish("clips.job.stage", Value::Str("cutting your selection".into()));
            let prog = progress_fn(hub.clone());
            let s = session.clone();
            let res = tokio::task::spawn_blocking(move || transcribe::niced(nice, move || job::make(&env, &s, t_in, t_out, &prog)).and_then(|r| r))
                .await
                .unwrap_or_else(|e| Err(format!("manual clip panicked: {e}")));
            hub.publish("clips.job.state", Value::Str("idle".into()));
            match res {
                Ok(row) => {
                    job::update_keep(&sh.env(), &row.session);
                    sh.publish_pending();
                    hub.emit(Event::new("clips.updated", Origin::System, Value::map().with("id", row.id).with("change", "made").with("clip", row.to_value())));
                }
                Err(e) => hub.log("error", TARGET, format!("manual clip for {session}: {e}")),
            }
        }
        Work::Retrim { id, t_in, t_out } => {
            hub.publish("clips.job.state", Value::Str("running".into()));
            let prog = progress_fn(hub.clone());
            let res = tokio::task::spawn_blocking(move || transcribe::niced(nice, move || job::retrim(&env, id, t_in, t_out, &prog)).and_then(|r| r))
                .await
                .unwrap_or_else(|e| Err(format!("retrim panicked: {e}")));
            hub.publish("clips.job.state", Value::Str("idle".into()));
            match res {
                Ok(row) => {
                    job::update_keep(&sh.env(), &row.session);
                    hub.emit(Event::new("clips.updated", Origin::System, Value::map().with("id", id).with("change", "retrim").with("clip", row.to_value())));
                }
                Err(e) => hub.log("error", TARGET, format!("retrim clip {id}: {e}")),
            }
        }
        Work::Upload(id) => {
            let res = tokio::task::spawn_blocking(move || job::upload(&env, id)).await.unwrap_or_else(|e| Err(format!("upload panicked: {e}")));
            match res {
                Ok(url) => {
                    if let Ok(Some(row)) = store::get(&sh.ctx.db, id) {
                        job::update_keep(&sh.env(), &row.session);
                    }
                    hub.emit(Event::new(
                        "clips.updated",
                        Origin::System,
                        Value::map().with("id", id).with("change", "uploaded").with("url", url.map(Value::Str).unwrap_or_default()),
                    ));
                }
                Err(e) => hub.log("error", TARGET, format!("upload clip {id}: {e}")),
            }
        }
    }
}

// ---- actions ---------------------------------------------------------------------------------

fn arg<'a>(args: &'a Value, key: &str, pos: usize) -> Option<&'a Value> {
    args.get_path(key).or_else(|| args.get_path("args").and_then(Value::as_list).and_then(|l| l.get(pos)))
}

/// Seconds from a number or a duration string (`83.5`, `"1:23.5"`, `"83s"`).
fn seconds(v: &Value) -> Option<f64> {
    match v {
        Value::Str(s) => se_proto::parse_duration_ms(s).map(|ms| ms as f64 / 1000.0),
        v => v.as_f64(),
    }
}

async fn actions(sh: Arc<Shared>) {
    let mut rx = sh.hub().route_actions("clips");
    while let Some(c) = rx.recv().await {
        let Op::Action { name, args } = &c.op else { continue };
        if let Err(e) = action(&sh, name, args) {
            sh.hub().log("error", TARGET, format!("{name}: {e}"));
        }
    }
}

fn action(sh: &Arc<Shared>, name: &str, args: &Value) -> Result<(), String> {
    let db = &sh.ctx.db;
    let id = || arg(args, "id", 0).and_then(Value::as_i64).ok_or_else(|| "needs a clip id (id=N)".to_string());
    match name {
        "clips.process" => {
            let session = match arg(args, "session", 0) {
                Some(v) => v.to_string(),
                None => latest_closed_session(db).ok_or("no closed session to process")?,
            };
            sh.push(Work::Process(session));
        }
        "clips.make" => {
            let session = arg(args, "session", 0).and_then(Value::as_str).ok_or("needs a session")?;
            let t_in = arg(args, "in", 1).and_then(seconds).ok_or("needs a start time")?;
            let t_out = arg(args, "out", 2).and_then(seconds).ok_or("needs an end time")?;
            if !t_in.is_finite() || !t_out.is_finite() || t_in < 0.0 || t_out <= t_in {
                return Err("select a range with an end after its start".into());
            }
            sh.push(Work::Make { session: session.to_string(), t_in, t_out });
        }
        "clips.approve" | "clips.reject" => {
            let id = id()?;
            let status = if name == "clips.approve" { "approved" } else { "rejected" };
            if !store::set_status(db, id, status, None).map_err(|e| format!("{e:#}"))? {
                return Err(format!("no clip {id}"));
            }
            if let Ok(Some(row)) = store::get(db, id) {
                job::update_keep(&sh.env(), &row.session);
            }
            sh.hub().emit(Event::new("clips.updated", Origin::System, Value::map().with("id", id).with("change", status)));
            sh.publish_pending();
            if status == "approved" && sh.cfg().upload_on_approve && !sh.cfg().upload_command.is_empty() {
                sh.push(Work::Upload(id));
            }
        }
        "clips.retrim" => {
            let id = id()?;
            let row = store::get(db, id).map_err(|e| format!("{e:#}"))?.ok_or_else(|| format!("no clip {id}"))?;
            let t_in = arg(args, "in", 1).and_then(seconds).unwrap_or(row.in_s);
            let t_out = arg(args, "out", 2).and_then(seconds).unwrap_or(row.out_s);
            if t_out.is_nan() || t_in.is_nan() || t_out <= t_in {
                return Err(format!("out ({t_out:.2}) must be after in ({t_in:.2})"));
            }
            sh.push(Work::Retrim { id, t_in, t_out });
        }
        "clips.upload" => {
            if sh.cfg().upload_command.is_empty() {
                return Err("no [clips] upload_command configured".into());
            }
            sh.push(Work::Upload(id()?));
        }
        other => return Err(format!("unknown action `{other}` (clips.process|make|approve|reject|retrim|upload)")),
    }
    Ok(())
}

fn latest_closed_session(db: &se_store::Db) -> Option<String> {
    db.with(|c| {
        use rusqlite::OptionalExtension;
        c.query_row("SELECT id FROM sessions WHERE ended_at IS NOT NULL ORDER BY ended_at DESC LIMIT 1", [], |r| r.get(0)).optional()
    })
    .ok()
    .flatten()
}

// ---- queries ---------------------------------------------------------------------------------

fn register_queries(sh: &Arc<Shared>) {
    let s = sh.clone();
    sh.hub().register_query(
        "clips",
        Arc::new(move |name, args| {
            let s = s.clone();
            Box::pin(async move { tokio::task::spawn_blocking(move || query(&s, &name, &args)).await.map_err(|e| e.to_string())? })
        }),
    );
}

/// `clips {session?, status?, limit?}` → ranked review queue;
/// `clips.session {session}` → markers timeline + clips + job + recordings + clip length limits;
/// `clips.jobs` → recent jobs; `clips.feedback {session}` → review decisions.
fn query(sh: &Shared, name: &str, args: &Value) -> Result<Value, String> {
    let db = &sh.ctx.db;
    let s = |k: &str| args.get_path(k).and_then(Value::as_str).map(String::from);
    let e = |e: anyhow::Error| format!("{e:#}");
    match name {
        "clips" => {
            let limit = args.get_path("limit").and_then(Value::as_i64).unwrap_or(200).clamp(1, 5000) as usize;
            let rows = store::list(db, s("session").as_deref(), s("status").as_deref(), limit).map_err(e)?;
            Ok(Value::List(rows.iter().map(store::ClipRow::to_value).collect()))
        }
        "clips.session" => {
            let session = s("session").ok_or("needs session")?;
            let env = sh.env();
            let dir = job::session_dir(&env, &session);
            let files = session::load(&dir, &env.cfg)?;
            let rows = store::list(db, Some(&session), None, 1000).map_err(e)?;
            let recordings: Vec<Value> = files
                .meta
                .recordings
                .iter()
                .map(|r| {
                    Value::map()
                        .with("canvas", r.canvas.clone())
                        .with("path", r.path.to_string_lossy().into_owned())
                        .with("exists", r.path.exists())
                        .with("start_ns", r.start_ns.map(|x| Value::Int(x as i64)).unwrap_or_default())
                        .with("end_ns", r.end_ns.map(|x| Value::Int(x as i64)).unwrap_or_default())
                        .with("tracks", r.tracks.len() as i64)
                })
                .collect();
            Ok(Value::map()
                .with("session", session.clone())
                .with("dir", dir.to_string_lossy().into_owned())
                .with("markers", files.markers.iter().map(session::Marker::to_value).collect::<Vec<_>>())
                .with("clips", rows.iter().map(store::ClipRow::to_value).collect::<Vec<_>>())
                .with("min_len", env.cfg.min_len.0 as f64 / 1000.0)
                .with("max_len", env.cfg.max_len.0 as f64 / 1000.0)
                .with("job", store::job(db, &session).map_err(e)?.unwrap_or_default())
                .with("recordings", recordings))
        }
        "clips.jobs" => Ok(Value::List(store::jobs(db, 50).map_err(e)?)),
        "clips.feedback" => {
            let session = s("session").ok_or("needs session")?;
            Ok(Value::List(store::feedback(db, &session).map_err(e)?))
        }
        other => Err(format!("unknown query `{other}` (clips | clips.session | clips.jobs | clips.feedback)")),
    }
}

// ---- preflight -------------------------------------------------------------------------------

async fn preflight(sh: Arc<Shared>) {
    let mut rx = sh.cfg.clone();
    loop {
        let cfg = sh.cfg();
        let data = sh.ctx.data_dir.clone();
        let (status, detail) = tokio::task::spawn_blocking(move || health(&cfg, &data)).await.unwrap_or(("fail", "health check panicked".into()));
        sh.hub().publish("health.clips", Value::map().with("status", status).with("detail", detail));
        if rx.changed().await.is_err() {
            break;
        }
    }
}

fn health(cfg: &ClipsConfig, data_dir: &std::path::Path) -> (&'static str, String) {
    if std::process::Command::new("ffprobe").arg("-version").output().is_err() {
        return ("fail", "ffmpeg/ffprobe not installed: clips can't be cut".into());
    }
    let mut warn = Vec::new();
    let mut ok = Vec::new();
    match cfg.encoder {
        config::Encoder::X264 => ok.push("x264".to_string()),
        _ if ffmpeg::has_encoder("h264_nvenc") => ok.push("NVENC".into()),
        config::Encoder::Nvenc => return ("fail", "ffmpeg has no h264_nvenc ([clips] encoder = \"nvenc\")".into()),
        config::Encoder::Auto => warn.push("ffmpeg has no h264_nvenc: clips encode with x264".to_string()),
    }
    if !ffmpeg::has_encoder("libx264") && cfg.encoder != config::Encoder::Nvenc {
        warn.push("ffmpeg has no libx264 fallback".into());
    }
    match transcribe::model_status(data_dir, &cfg.whisper.model) {
        Ok(_) => ok.push(format!("Whisper {}", cfg.whisper.model)),
        Err(e) => warn.push(e),
    }
    if warn.is_empty() { ("pass", ok.join(", ")) } else { ("warn", warn.join("; ")) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_seconds_accept_numbers_and_durations() {
        assert_eq!(seconds(&Value::Float(83.5)), Some(83.5));
        assert_eq!(seconds(&Value::Str("1:23.500".into())), Some(83.5));
        assert_eq!(seconds(&Value::Str("90s".into())), Some(90.0));
        let args = Value::map().with("args", vec![Value::Int(7), Value::Float(1.5)]);
        assert_eq!(arg(&args, "id", 0).and_then(Value::as_i64), Some(7));
        assert_eq!(arg(&args, "in", 1).and_then(seconds), Some(1.5));
    }
}
