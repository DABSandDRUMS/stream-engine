//! Audio subsystem (§8, §5 audio into OBS, §21): a PipeWire graph with our own nodes.
//!
//! * Capture: the Studio 24c stereo input (the 16R main mix) → `band` bus; more inputs (16R
//!   USB multichannel stems) are configurable. `hub.audio` slots (YouTube player, web
//!   patches, TTS, sound effects) feed `music`/`tts`/`sfx`/`game`.
//! * Buses `band`, `music`, `sfx`, `tts`, `game` (+ `drums`, `mic` stems) → `program`, each
//!   published as a PipeWire source node `se-<bus>` for OBS.
//! * Effect chains (`se-dsp`), ducking, per-bus gain/mute/limiter, A/V delay, sampler,
//!   `dsp` wasm patches, drum triggers, a low-latency monitor path.
//! * Live analysis (`se-analysis`) → `band.*`, `music.*`, `beat.*`, `mic.*` signals/events.
//! * Offline analysis for library songs: hub query `analysis.grid`.
//!
//! Threads: PipeWire main loop (`se-audio-pw`), the PipeWire data thread running the RT
//! graph, the control thread (`se-audio-ctl`), and the analysis thread (`se-audio-ana`).
//! Configuration format: `docs/audio.md`.

pub mod ana;
pub mod builder;
pub mod config;
pub mod control;
pub mod dsp;
pub mod graph;
pub mod pw;
pub mod rt;
pub mod sounds;
/// Taps API (`se_audio::taps::open`), in its own crate so consumers stay light.
pub use se_audio_taps as taps;
pub mod wasmfx;

use control::{Control, CtlMsg, Shared};
use parking_lot::Mutex;
use se_hub::EngineCtx;
use se_proto::Value;
use std::path::PathBuf;
use std::sync::Arc;

struct Running {
    tx: std::sync::mpsc::Sender<CtlMsg>,
    join: std::thread::JoinHandle<()>,
}

static RUNNING: Mutex<Option<Running>> = parking_lot::const_mutex(None);

/// Start the audio subsystem (returns once PipeWire and the control thread are up).
pub async fn start(ctx: EngineCtx) -> anyhow::Result<()> {
    let shared = Arc::new(Shared::default());
    let control = {
        let (ctx, shared) = (ctx.clone(), shared.clone());
        tokio::task::spawn_blocking(move || Control::start(ctx, shared)).await??
    };
    let (tx, rx) = std::sync::mpsc::channel();
    let join = std::thread::Builder::new().name("se-audio-ctl".into()).spawn(move || control.run(rx))?;

    // actions: audio.play, audio.duck, audio.panic, audio.fx.*, audio.tap, …
    let mut actions = ctx.hub.route_actions("audio");
    let atx = tx.clone();
    tokio::spawn(async move {
        while let Some(c) = actions.recv().await {
            if atx.send(CtlMsg::Action(Box::new(c))).is_err() {
                break;
            }
        }
    });
    // hot reload
    let mut cfg_rx = ctx.config.clone();
    let ctx_tx = tx.clone();
    tokio::spawn(async move {
        while cfg_rx.changed().await.is_ok() {
            let c = cfg_rx.borrow_and_update().clone();
            if ctx_tx.send(CtlMsg::Config(c)).is_err() {
                break;
            }
        }
    });
    register_queries(&ctx, shared);
    *RUNNING.lock() = Some(Running { tx, join });
    ctx.hub.log("info", "audio", "audio subsystem started");
    Ok(())
}

/// Stop the audio subsystem (PipeWire nodes are removed with the connection).
pub async fn stop() {
    let r = RUNNING.lock().take();
    if let Some(r) = r {
        let _ = r.tx.send(CtlMsg::Shutdown);
        let _ = tokio::task::spawn_blocking(move || r.join.join()).await;
    }
}

fn register_queries(ctx: &EngineCtx, shared: Arc<Shared>) {
    let s = shared.clone();
    ctx.hub.register_query(
        "audio.mix",
        Arc::new(move |_, _| {
            let v = s.mix.lock().clone();
            Box::pin(async move { Ok(v) })
        }),
    );
    let s = shared;
    ctx.hub.register_query(
        "audio.devices",
        Arc::new(move |_, _| {
            let v = serde_json::to_value(&*s.devices.lock()).ok().and_then(|j| serde_json::from_value::<Value>(j).ok()).unwrap_or_default();
            Box::pin(async move { Ok(v) })
        }),
    );
    let (db, root) = (ctx.db.clone(), ctx.project_root.clone());
    ctx.hub.register_query(
        "analysis.grid",
        Arc::new(move |_, args| {
            let (db, root) = (db.clone(), root.clone());
            Box::pin(async move { analysis_grid(db, root, args).await })
        }),
    );
}

const NS: &str = "audio.analysis";

/// `analysis.grid {path, force?}` → analyze (cached by content hash) ;
/// `analysis.grid {media}` → cached result or null.
async fn analysis_grid(db: se_store::Db, root: PathBuf, args: Value) -> Result<Value, String> {
    if let Some(media) = args.get_path("media").and_then(Value::as_str) {
        return db.kv_get(NS, media).map(|v| v.unwrap_or_default()).map_err(|e| e.to_string());
    }
    let Some(path) = args.get_path("path").and_then(Value::as_str) else {
        return Err("analysis.grid needs {path} or {media}".into());
    };
    let p = if std::path::Path::new(path).is_absolute() { PathBuf::from(path) } else { root.join(path) };
    let md = std::fs::metadata(&p).map_err(|e| format!("{}: {e}", p.display()))?;
    let stamp = format!("{}:{}", md.len(), md.modified().ok().and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_millis()).unwrap_or(0));
    let pkey = format!("path:{}", p.display());
    let force = args.get_path("force").is_some_and(Value::truthy);
    if !force
        && let Ok(Some(prev)) = db.kv_get(NS, &pkey)
        && prev.get_path("stamp").and_then(Value::as_str) == Some(stamp.as_str())
        && let Some(media) = prev.get_path("media").and_then(Value::as_str)
        && let Ok(Some(v)) = db.kv_get(NS, media)
    {
        return Ok(v);
    }
    let a = tokio::task::spawn_blocking(move || se_analysis::offline::analyze_file(&p)).await.map_err(|e| e.to_string())?.map_err(|e| format!("{e:#}"))?;
    let v: Value = serde_json::to_value(&a).ok().and_then(|j| serde_json::from_value(j).ok()).ok_or("serialize analysis")?;
    db.kv_set(NS, &a.media, &v).map_err(|e| e.to_string())?;
    db.kv_set(NS, &pkey, &Value::map().with("stamp", stamp).with("media", a.media.clone())).map_err(|e| e.to_string())?;
    Ok(v)
}
