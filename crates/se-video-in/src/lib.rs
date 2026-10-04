//! Video sources (§4.2): V4L2 cameras/capture cards and media files, published into
//! `hub.video` slots named after the source (`sources/<name>.toml`).
//!
//! * Cameras: YUYV passthrough (`PixelFormat::Yuyv`) or MJPEG decoded to RGBA with
//!   libjpeg-turbo; master-clock timestamps; signal detection; fps/drops/CPU readbacks; camera
//!   controls as `source.<n>.ctrl.<control>` with the device's real ranges.
//! * Media files: FFmpeg (NVDEC `*_cuvid` when available) into NV12 or RGBA (alpha) slots.
//! * Only sources in use are captured: `render.sources.used` when the renderer publishes it,
//!   else every source referenced by a scene node, plus every source a recorder taps
//!   ([`se_hub::Hub::tap_video`], `video_in.<n>.taps`).
//! * Temporary previews ([`preview`]): Settings → Devices shows a live thumbnail of any camera,
//!   opened at a light mode while leased, or taken from the source already capturing it.
//!
//! Query `sources`, `video_in.preview`; actions `source.restart|seek|reopen|assign|save_controls`,
//! `video_in.preview`; preflight `health.sources`.

pub mod camera;
pub mod config;
pub mod controls;
pub mod frame;
pub mod media;
pub mod preview;
pub mod status;

use anyhow::{Result, anyhow};
use config::{COLOR_FIELDS, SourceDef};
use parking_lot::Mutex;
use se_hub::media::VideoWriter;
use se_hub::{Bus, EngineCtx};
use se_proto::{Meta, Value, ValueType};
use status::{OWNER, Status};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Sources that stop being used keep capturing this long (transitions, quick scene flips).
const UNUSED_GRACE: Duration = Duration::from_secs(5);

enum Tx {
    Camera(crossbeam_channel::Sender<camera::Cmd>),
    Media(crossbeam_channel::Sender<media::Cmd>),
}

struct Worker {
    tx: Tx,
    handle: std::thread::JoinHandle<VideoWriter>,
}

struct Src {
    def: Arc<SourceDef>,
    status: Arc<Status>,
    /// The slot writer while no worker owns it.
    writer: Option<VideoWriter>,
    worker: Option<Worker>,
    unused_since: Option<Instant>,
    /// Last published `video_in.<n>.taps`.
    taps: usize,
}

#[derive(Default)]
struct State {
    sources: BTreeMap<String, Src>,
    errors: HashMap<String, String>,
    health: Option<(String, String)>,
    /// Source names currently wanted by the renderer/scenes or recording taps.
    wanted: HashSet<String>,
    previews: preview::Previews,
}

type Shared = Arc<Mutex<State>>;

pub async fn start(ctx: EngineCtx) -> Result<()> {
    media::init();
    let shared: Shared = Arc::new(Mutex::new(State::default()));
    let bus = ctx.hub.subscribe();
    let actions = ctx.hub.route_actions("source");
    let preview_actions = ctx.hub.route_actions("video_in");
    ctx.hub.declare(
        "health.sources",
        Meta {
            ty: ValueType::Map,
            readonly: true,
            owner: Some(OWNER.into()),
            description: Some("cameras in use are present with live signal".into()),
            ..Default::default()
        },
    );
    register_query(&ctx, shared.clone());
    register_preview_query(&ctx, shared.clone());
    reload(&ctx, &shared);
    update_usage(&ctx, &shared).await;
    tokio::spawn(run(ctx, shared, bus, actions, preview_actions));
    Ok(())
}

async fn run(
    ctx: EngineCtx,
    shared: Shared,
    mut bus: tokio::sync::broadcast::Receiver<Arc<Bus>>,
    mut actions: tokio::sync::mpsc::UnboundedReceiver<se_proto::Command>,
    mut preview_actions: tokio::sync::mpsc::UnboundedReceiver<se_proto::Command>,
) {
    let mut config = ctx.config.clone();
    let mut tick = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            Ok(()) = config.changed() => {
                reload(&ctx, &shared);
                update_usage(&ctx, &shared).await;
            }
            msg = bus.recv() => match msg {
                Ok(m) => match &*m {
                    Bus::Event(e) if (e.ty == "devices.added" || e.ty == "devices.removed") && e.payload.get_path("kind").and_then(Value::as_str) == Some("camera") => {
                        let st = shared.lock();
                        for s in st.sources.values() {
                            if let Some(Worker { tx: Tx::Camera(tx), .. }) = &s.worker {
                                let _ = tx.send(camera::Cmd::Rescan);
                            }
                        }
                    }
                    Bus::Changes(ch) if ch.iter().any(|(a, _)| a == "render.sources.used") => update_usage(&ctx, &shared).await,
                    _ => {}
                },
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => update_usage(&ctx, &shared).await,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            () = ctx.hub.video.taps_changed() => update_usage(&ctx, &shared).await,
            Some(cmd) = actions.recv() => {
                if let se_proto::Op::Action { name, args } = &cmd.op {
                    match action(&ctx, &shared, name, args).await {
                        Ok(msg) => ctx.hub.log("info", OWNER, msg),
                        Err(e) => ctx.hub.log("error", OWNER, format!("{name}: {e}")),
                    }
                }
            }
            Some(cmd) = preview_actions.recv() => {
                if let se_proto::Op::Action { name, args } = &cmd.op {
                    match preview_action(&shared, name, args).await {
                        Ok(Some(msg)) => ctx.hub.log("info", OWNER, msg),
                        Ok(None) => {}
                        Err(e) => ctx.hub.log("error", OWNER, format!("{name}: {e}")),
                    }
                }
            }
            _ = tick.tick() => {
                update_usage(&ctx, &shared).await;
                update_previews(&shared).await;
                publish_health(&ctx, &shared);
            }
        }
    }
}

/// Parse `sources/*.toml`, keeping the last good version of broken files.
fn reload(ctx: &EngineCtx, shared: &Shared) {
    let files = ctx.kind("sources");
    let mut st = shared.lock();
    let mut seen = HashSet::new();
    for (name, table) in &files {
        seen.insert(name.clone());
        match config::parse(name, table, &ctx.project_root) {
            Ok(def) => {
                st.errors.remove(name);
                let def = Arc::new(def);
                match st.sources.get_mut(name) {
                    Some(s) if *s.def == *def => {}
                    Some(s) => {
                        let kind_changed = s.def.kind_str() != def.kind_str();
                        s.def = def.clone();
                        declare(ctx, &def);
                        s.taps = 0;
                        if let Some(w) = &s.worker {
                            if kind_changed {
                                // restarted with the right worker type by update_usage
                                stop_worker(s);
                            } else {
                                match &w.tx {
                                    Tx::Camera(tx) => drop(tx.send(camera::Cmd::Config(def.clone()))),
                                    Tx::Media(tx) => drop(tx.send(media::Cmd::Config(def.clone()))),
                                }
                            }
                        }
                    }
                    None => {
                        declare(ctx, &def);
                        let writer = ctx.hub.video.register(name);
                        st.sources.insert(
                            name.clone(),
                            Src { def, status: Arc::new(Status::default()), writer: Some(writer), worker: None, unused_since: None, taps: 0 },
                        );
                    }
                }
                if !st.sources[name].def.color.lut.is_empty() {
                    let lut = ctx.project_root.join(&st.sources[name].def.color.lut);
                    if !lut.exists() {
                        ctx.hub.log("warn", OWNER, format!("{name}: LUT {} not found", lut.display()));
                    }
                }
            }
            Err(e) => {
                if st.errors.get(name) != Some(&e) {
                    ctx.hub.log(
                        "error",
                        OWNER,
                        format!("sources/{name}.toml: {e}{}", if st.sources.contains_key(name) { " (keeping the last good version)" } else { "" }),
                    );
                    st.errors.insert(name.clone(), e);
                }
            }
        }
    }
    let gone: Vec<String> = st.sources.keys().filter(|n| !seen.contains(*n)).cloned().collect();
    for n in gone {
        if let Some(mut s) = st.sources.remove(&n) {
            stop_worker(&mut s);
        }
        st.errors.remove(&n);
        ctx.hub.submit(se_core::Input::Remove { prefix: format!("source.{n}") });
        ctx.hub.submit(se_core::Input::Remove { prefix: format!("video_in.{n}") });
    }
}

/// Declare the addresses of a source (readbacks + user-controllable color/media params).
fn declare(ctx: &EngineCtx, def: &SourceDef) {
    let hub = &ctx.hub;
    let n = &def.name;
    status::declare_readbacks(hub, n, def.kind_str());
    hub.publish(&format!("source.{n}.kind"), Value::from(def.kind_str()));
    hub.publish(&format!("source.{n}.label"), Value::from(def.label.as_str()));
    let device = match &def.kind {
        config::Kind::Camera(c) => c.device.clone(),
        config::Kind::File(f) => f.rel.clone(),
    };
    hub.publish(&format!("source.{n}.device"), Value::Str(device));
    hub.declare(
        &format!("video_in.{n}.taps"),
        Meta::int(0, [0.0, 1.0e6]).readonly().owner(OWNER).describe("recording taps on this source; it is captured while any exist"),
    );
    for (f, _, range, desc) in COLOR_FIELDS {
        hub.declare(&format!("source.{n}.color.{f}"), Meta::float(def.color.get(f), range).owner(OWNER).describe(desc));
    }
    hub.declare(&format!("source.{n}.lut"), Meta::string(&def.color.lut).owner(OWNER).describe("3D LUT (.cube, project-relative); empty = none"));
    hub.declare(&format!("source.{n}.lut_amount"), Meta::float(def.color.lut_amount, [0.0, 1.0]).owner(OWNER).describe("LUT mix"));
    hub.declare(&format!("source.{n}.decoder"), Meta::string("").readonly().owner(OWNER).describe("passthrough | turbojpeg | FFmpeg decoder"));
    if let Some(f) = def.file() {
        hub.declare(&format!("source.{n}.paused"), Meta::boolean(false).owner(OWNER).describe("hold the current frame"));
        hub.declare(&format!("source.{n}.rate"), Meta::float(f.rate, [0.05, 8.0]).owner(OWNER).describe("playback speed"));
        hub.declare(&format!("source.{n}.loop"), Meta::boolean(f.looping).owner(OWNER).describe("restart at the end"));
        hub.declare(&format!("source.{n}.position"), Meta::float(0.0, [0.0, 1.0e7]).readonly().owner(OWNER).unit("s"));
        hub.declare(&format!("source.{n}.duration"), Meta::float(0.0, [0.0, 1.0e7]).readonly().owner(OWNER).unit("s"));
        hub.declare(&format!("source.{n}.playing"), Meta::boolean(false).readonly().owner(OWNER));
        hub.declare(&format!("source.{n}.media"), Meta::string("").readonly().owner(OWNER).describe("timeline media id of the file (`file:<hash>`)"));
        hub.declare(
            &format!("source.{n}.isrc"),
            Meta::string("").readonly().owner(OWNER).describe("ISRC from the file's tags (timelines: `media = \"isrc:<code>\"`)"),
        );
    }
}

/// Names referenced by scene nodes (fallback until the renderer publishes its list).
fn scene_sources(cfg: &se_core::Config) -> HashSet<String> {
    let mut out = HashSet::new();
    for s in cfg.scenes.values() {
        for c in s.canvas.values() {
            for n in &c.nodes {
                out.insert(n.src.clone());
            }
        }
    }
    out
}

/// Sources to capture: the renderer's (or the scenes') plus every source a recorder taps.
fn wanted(ctx: &EngineCtx) -> HashSet<String> {
    let snap = ctx.hub.snapshot.load();
    let mut want = match snap.get("render.sources.used") {
        Some(Value::List(l)) => l.iter().filter_map(|v| v.as_str().map(str::to_string)).collect(),
        _ => scene_sources(&ctx.config.borrow()),
    };
    want.extend(ctx.hub.video.tapped());
    want
}

/// A source's `device` (identity, glob, or `/dev/…` path) among present cameras.
fn resolve_camera(spec: &str, cams: &[se_devices::DeviceInfo]) -> Option<String> {
    if spec.starts_with('/') {
        let target = std::fs::canonicalize(spec).ok()?;
        return cams.iter().find(|c| std::path::Path::new(&c.path) == target).map(|c| c.identity.clone());
    }
    cams.iter().find(|c| c.identity == spec).or_else(|| cams.iter().find(|c| se_devices::identity::glob(spec, &c.identity))).map(|c| c.identity.clone())
}

async fn scan_cameras() -> Vec<se_devices::DeviceInfo> {
    tokio::task::spawn_blocking(se_devices::scan_cameras).await.ok().and_then(Result::ok).unwrap_or_default()
}

async fn join_all(stopping: Vec<preview::Stopping>) {
    if !stopping.is_empty() {
        let _ = tokio::task::spawn_blocking(move || stopping.into_iter().for_each(preview::Stopping::join)).await;
    }
}

/// Camera identity → the source whose capture thread owns it (capturing, or trying to).
fn holders(st: &State, cams: &[se_devices::DeviceInfo]) -> HashMap<String, preview::Holder> {
    let mut out = HashMap::new();
    for (name, s) in &st.sources {
        let (Some(cam), Some(_)) = (s.def.camera(), &s.worker) else { continue };
        let known = s.status.info.lock().identity.clone();
        let identity = if known.is_empty() { resolve_camera(&cam.device, cams) } else { Some(known) };
        if let Some(id) = identity {
            let capturing = s.status.capturing.load(std::sync::atomic::Ordering::Relaxed);
            out.insert(id, preview::Holder { source: name.clone(), capturing, tap: s.status.preview.clone() });
        }
    }
    out
}

/// Cameras about to be captured by a source: stop our preview threads on them first.
async fn release_previews_for(shared: &Shared, want: &HashSet<String>) {
    let specs: Vec<String> = {
        let st = shared.lock();
        if !st.previews.has_workers() {
            return;
        }
        st.sources.iter().filter(|(n, s)| want.contains(*n) && s.worker.is_none()).filter_map(|(_, s)| s.def.camera().map(|c| c.device.clone())).collect()
    };
    if specs.is_empty() {
        return;
    }
    let cams = scan_cameras().await;
    let ids: Vec<String> = specs.iter().filter_map(|s| resolve_camera(s, &cams)).collect();
    let stopping = shared.lock().previews.yield_to_sources(&ids);
    join_all(stopping).await;
}

/// Expire leases, then open/close preview threads and switch source thumbnails on/off.
async fn update_previews(shared: &Shared) {
    let leased = {
        let mut st = shared.lock();
        st.previews.leases.expire(Instant::now());
        if st.previews.is_idle() {
            return;
        }
        !st.previews.leases.is_empty()
    };
    let cams = if leased { scan_cameras().await } else { Vec::new() };
    let stopping = {
        let mut st = shared.lock();
        let h = holders(&st, &cams);
        st.previews.reconcile(&h, &cams)
    };
    join_all(stopping).await;
}

/// `video_in.preview {identity, on = true, lease = 6}`: take, renew or end a preview lease.
/// Renewals are silent; the first request and the release are logged.
async fn preview_action(shared: &Shared, name: &str, args: &Value) -> Result<Option<String>> {
    if name != "video_in.preview" {
        return Err(anyhow!("unknown action `{name}` (video_in.preview)"));
    }
    let identity = arg_str(args, "identity", 0).ok_or_else(|| anyhow!("needs a camera identity"))?;
    let on = arg(args, "on", 1).is_none_or(|v| v.truthy() && v.as_str() != Some("false"));
    let lease = match arg(args, "lease", 2) {
        None => preview::DEFAULT_LEASE,
        Some(v) => v
            .as_f64()
            .or_else(|| v.as_str().and_then(|s| se_proto::parse_duration_ms(s).map(|ms| ms as f64 / 1000.0)))
            .filter(|s| s.is_finite() && *s >= 0.0)
            .map(Duration::from_secs_f64)
            .ok_or_else(|| anyhow!("lease must be seconds or a duration like \"6s\""))?,
    };
    let msg = {
        let mut st = shared.lock();
        if on {
            let new = !st.previews.leases.contains(&identity);
            st.previews.leases.renew(&identity, lease, Instant::now());
            new.then(|| format!("preview of {identity} on"))
        } else {
            st.previews.leases.release(&identity).then(|| format!("preview of {identity} off"))
        }
    };
    update_previews(shared).await;
    Ok(msg)
}

/// Start sources that became used, stop ones unused for longer than the grace period.
async fn update_usage(ctx: &EngineCtx, shared: &Shared) {
    let want = wanted(ctx);
    release_previews_for(shared, &want).await;
    let mut to_join = Vec::new();
    {
        let mut st = shared.lock();
        st.wanted = want.clone();
        let names: Vec<String> = st.sources.keys().cloned().collect();
        for name in names {
            let s = st.sources.get_mut(&name).expect("present");
            let taps = ctx.hub.video.tap_count(&name);
            if taps != s.taps {
                s.taps = taps;
                ctx.hub.publish(&format!("video_in.{name}.taps"), Value::Int(taps as i64));
            }
            if want.contains(&name) {
                s.unused_since = None;
                if s.worker.is_none()
                    && let Some(writer) = s.writer.take()
                {
                    match start_worker(ctx, s, writer) {
                        Ok(w) => s.worker = Some(w),
                        Err((writer, e)) => {
                            s.writer = Some(writer);
                            ctx.hub.log("error", OWNER, format!("{name}: start: {e}"));
                        }
                    }
                }
            } else if s.worker.is_some() {
                let since = *s.unused_since.get_or_insert_with(Instant::now);
                if since.elapsed() >= UNUSED_GRACE {
                    ctx.hub.log("info", OWNER, format!("{name}: no longer used; stopping capture"));
                    if let Some(w) = s.worker.take() {
                        send_stop(&w);
                        to_join.push((name.clone(), w.handle));
                    }
                }
            }
        }
    }
    for (name, handle) in to_join {
        let writer = tokio::task::spawn_blocking(move || handle.join()).await;
        let mut st = shared.lock();
        match writer {
            Ok(Ok(w)) => {
                if let Some(s) = st.sources.get_mut(&name) {
                    s.writer = Some(w);
                }
            }
            _ => {
                ctx.hub.log("error", OWNER, format!("{name}: capture thread panicked; re-registering its slot"));
                if let Some(s) = st.sources.get_mut(&name) {
                    s.writer = Some(ctx.hub.video.register(&name));
                }
            }
        }
    }
}

fn start_worker(ctx: &EngineCtx, s: &Src, writer: VideoWriter) -> Result<Worker, (VideoWriter, String)> {
    // A thread that fails to spawn never runs, so its writer is dropped; re-register then.
    match &s.def.kind {
        config::Kind::Camera(_) => {
            let (tx, rx) = crossbeam_channel::unbounded();
            camera::spawn(ctx.hub.clone(), s.def.clone(), s.status.clone(), writer, rx)
                .map(|handle| Worker { tx: Tx::Camera(tx), handle })
                .map_err(|e| (ctx.hub.video.register(&s.def.name), e.to_string()))
        }
        config::Kind::File(_) => {
            let (tx, rx) = crossbeam_channel::unbounded();
            media::spawn(ctx.hub.clone(), s.def.clone(), s.status.clone(), writer, rx)
                .map(|handle| Worker { tx: Tx::Media(tx), handle })
                .map_err(|e| (ctx.hub.video.register(&s.def.name), e.to_string()))
        }
    }
}

fn send_stop(w: &Worker) {
    match &w.tx {
        Tx::Camera(tx) => drop(tx.send(camera::Cmd::Stop)),
        Tx::Media(tx) => drop(tx.send(media::Cmd::Stop)),
    }
}

/// Stop synchronously (config removal / kind change); the thread exits within ~100 ms.
fn stop_worker(s: &mut Src) {
    if let Some(w) = s.worker.take() {
        send_stop(&w);
        if let Ok(writer) = w.handle.join() {
            s.writer = Some(writer);
        }
    }
}

fn publish_health(ctx: &EngineCtx, shared: &Shared) {
    let mut st = shared.lock();
    let mut fails = Vec::new();
    let mut warns = Vec::new();
    let mut live = 0;
    for (name, s) in &st.sources {
        if !st.wanted.contains(name) {
            continue;
        }
        let err = s.status.info.lock().error.clone();
        if s.status.missing.load(std::sync::atomic::Ordering::Relaxed) || (s.def.file().is_some() && !err.is_empty()) {
            fails.push(format!("{name}: {}", if err.is_empty() { "device missing".to_string() } else { err }));
        } else if !s.status.capturing.load(std::sync::atomic::Ordering::Relaxed) {
            warns.push(format!("{name}: not capturing{}", if err.is_empty() { String::new() } else { format!(" ({err})") }));
        } else if !s.status.signal.load(std::sync::atomic::Ordering::Relaxed) {
            warns.push(format!("{name}: no signal"));
        } else {
            live += 1;
        }
    }
    for (name, e) in &st.errors {
        warns.push(format!("sources/{name}.toml: {e}"));
    }
    let (status, detail) = if !fails.is_empty() {
        ("fail", fails.into_iter().chain(warns).collect::<Vec<_>>().join("; "))
    } else if !warns.is_empty() {
        ("warn", format!("{live} live; {}", warns.join("; ")))
    } else if live == 0 {
        ("pass", "no sources in use".to_string())
    } else {
        ("pass", format!("{live} source(s) live"))
    };
    let h = (status.to_string(), detail);
    if st.health.as_ref() != Some(&h) {
        ctx.hub.publish("health.sources", Value::map().with("status", h.0.as_str()).with("detail", h.1.as_str()));
        st.health = Some(h);
    }
}

fn control_value(c: &se_devices::v4l2::Control) -> Value {
    let mut v = Value::map()
        .with("name", c.name.as_str())
        .with("label", c.label.as_str())
        .with(
            "type",
            match c.kind {
                se_devices::v4l2::ControlKind::Boolean => "bool",
                se_devices::v4l2::ControlKind::Menu | se_devices::v4l2::ControlKind::IntegerMenu => "enum",
                _ => "int",
            },
        )
        .with("min", c.min)
        .with("max", c.max)
        .with("step", c.step)
        .with("default", controls::from_device(c, c.default))
        .with("value", c.value.map(|v| controls::from_device(c, v)).unwrap_or(Value::Null))
        .with("inactive", c.inactive())
        .with("read_only", c.read_only());
    if !c.menu.is_empty() {
        v = v.with(
            "menu",
            Value::List(c.menu.iter().map(|m| Value::map().with("name", m.name.as_str()).with("label", m.label.as_str()).with("index", m.index)).collect()),
        );
    }
    v
}

fn register_query(ctx: &EngineCtx, shared: Shared) {
    ctx.hub.register_query(
        "sources",
        Arc::new(move |_n, _a| {
            let shared = shared.clone();
            Box::pin(async move {
                let st = shared.lock();
                let list = st
                    .sources
                    .iter()
                    .map(|(name, s)| {
                        let i = s.status.info.lock().clone();
                        let a = std::sync::atomic::Ordering::Relaxed;
                        let mut v = Value::map()
                            .with("name", name.as_str())
                            .with("label", s.def.label.as_str())
                            .with("kind", s.def.kind_str())
                            .with("used", st.wanted.contains(name))
                            .with("capturing", s.status.capturing.load(a))
                            .with("signal", s.status.signal.load(a))
                            .with("missing", s.status.missing.load(a))
                            .with("measured_fps", s.status.fps())
                            .with("dropped", s.status.dropped.load(a) as i64)
                            .with("frames", s.status.frames.load(a) as i64)
                            .with("cpu", s.status.cpu())
                            .with("path", i.path.as_str())
                            .with("identity", i.identity.as_str())
                            .with("slot_format", i.format.as_str())
                            .with("width", i.width as i64)
                            .with("height", i.height as i64)
                            .with("matrix", i.matrix.as_str())
                            .with("range", i.range.as_str())
                            .with("decoder", i.decoder.as_str())
                            .with("error", i.error.as_str())
                            .with("controls", Value::List(i.controls.iter().filter(|c| controls::exposed(c)).map(control_value).collect()));
                        match &s.def.kind {
                            config::Kind::Camera(c) => {
                                v = v
                                    .with("device", c.device.as_str())
                                    .with("format", se_devices::v4l2::format_name(c.fourcc))
                                    .with("size", Value::List(vec![Value::Int(c.width as i64), Value::Int(c.height as i64)]))
                                    .with("fps", c.fps)
                                    .with("nominal_fps", i.nominal_fps);
                            }
                            config::Kind::File(f) => {
                                v = v
                                    .with("device", f.rel.as_str())
                                    .with("file", f.rel.as_str())
                                    .with("fps", i.nominal_fps)
                                    .with("loop", f.looping)
                                    .with("rate", f.rate);
                            }
                        }
                        if let Some(e) = st.errors.get(name) {
                            v = v.with("config_error", e.as_str());
                        }
                        v
                    })
                    .collect();
                Ok(Value::List(list))
            })
        }),
    );
}

/// `video_in.preview {have: [{identity, seq}]}` → `{previews: [{identity, state, source, error,
/// seq, width, height, jpeg}]}` for every leased camera. `jpeg` (base64) is left out when the
/// client already has picture `seq`.
fn register_preview_query(ctx: &EngineCtx, shared: Shared) {
    use base64::Engine;
    ctx.hub.register_query(
        "video_in.preview",
        Arc::new(move |_n, args| {
            let shared = shared.clone();
            let have: HashMap<String, u64> = args
                .get_path("have")
                .and_then(Value::as_list)
                .unwrap_or(&[])
                .iter()
                .filter_map(|v| Some((v.get_path("identity")?.as_str()?.to_string(), u64::try_from(v.get_path("seq")?.as_i64()?).ok()?)))
                .collect();
            Box::pin(async move {
                let reports = {
                    let st = shared.lock();
                    st.previews.report(&holders(&st, &[]), &have, se_clock::now())
                };
                let b64 = base64::engine::general_purpose::STANDARD;
                let list = reports
                    .into_iter()
                    .map(|r| {
                        let mut v = Value::map()
                            .with("identity", r.identity)
                            .with("state", r.state)
                            .with("source", r.source.map(Value::Str).unwrap_or(Value::Null))
                            .with("error", r.error);
                        if let Some(p) = r.picture {
                            v = v.with("seq", p.seq as i64).with("width", p.width as i64).with("height", p.height as i64);
                            if !p.jpeg.is_empty() {
                                v = v.with("jpeg", b64.encode(&p.jpeg));
                            }
                        }
                        v
                    })
                    .collect();
                Ok(Value::map().with("previews", Value::List(list)))
            })
        }),
    );
}

fn arg(args: &Value, key: &str, pos: usize) -> Option<Value> {
    args.get_path(key).or_else(|| args.get_path("args").and_then(Value::as_list).and_then(|l| l.get(pos))).cloned()
}

fn arg_str(args: &Value, key: &str, pos: usize) -> Option<String> {
    arg(args, key, pos).map(|v| match v {
        Value::Str(s) => s,
        other => other.to_string(),
    })
}

async fn action(ctx: &EngineCtx, shared: &Shared, name: &str, args: &Value) -> Result<String> {
    let source = arg_str(args, "source", 0).ok_or_else(|| anyhow!("needs a source"))?;
    match name {
        "source.restart" | "source.reopen" | "source.seek" => {
            let st = shared.lock();
            let s = st.sources.get(&source).ok_or_else(|| anyhow!("unknown source `{source}`"))?;
            let w = s.worker.as_ref().ok_or_else(|| anyhow!("{source} is not in use (not captured)"))?;
            match (&w.tx, name) {
                (Tx::Camera(tx), "source.restart" | "source.reopen") => drop(tx.send(camera::Cmd::Reopen)),
                (Tx::Media(tx), "source.restart" | "source.reopen") => drop(tx.send(media::Cmd::Restart)),
                (Tx::Media(tx), "source.seek") => {
                    let secs = arg(args, "seconds", 1)
                        .and_then(|v| v.as_f64().or_else(|| v.as_str().and_then(|s| se_proto::parse_duration_ms(s).map(|ms| ms as f64 / 1000.0))))
                        .ok_or_else(|| anyhow!("needs seconds"))?;
                    drop(tx.send(media::Cmd::Seek(secs)));
                }
                (Tx::Camera(_), _) => return Err(anyhow!("{source} is a camera; seek applies to media files")),
                _ => unreachable!(),
            }
            Ok(format!("{name} {source}"))
        }
        "source.assign" => {
            let identity = arg_str(args, "identity", 1).ok_or_else(|| anyhow!("needs identity"))?;
            let ident2 = identity.clone();
            let dev =
                tokio::task::spawn_blocking(move || se_devices::find_camera(&ident2)).await?.ok_or_else(|| anyhow!("no camera with identity `{identity}`"))?;
            let rel = format!("sources/{source}.toml");
            let exists = ctx.project_root.join(&rel).exists();
            let project = se_store::Project::open(&ctx.project_root)?;
            if exists {
                project.edit(&rel, |doc| {
                    if doc.contains_key("file") {
                        return Err(anyhow!("{rel} is a media-file source"));
                    }
                    doc["device"] = toml_edit::value(dev.identity.as_str());
                    Ok(())
                })?;
            } else {
                let path = dev.path.clone();
                let modes = tokio::task::spawn_blocking(move || se_devices::v4l2::Device::open(&path, true).and_then(|d| d.modes())).await??;
                let (fmt, w, h, fps) = best_mode(&modes).ok_or_else(|| anyhow!("{} reports no capture modes", dev.path))?;
                let content = format!(
                    "# {} ({})\nlabel = \"{}\"\ndevice = \"{}\"\nformat = \"{}\"\nsize = [{w}, {h}]\nfps = {}\n",
                    dev.name,
                    dev.path,
                    source,
                    dev.identity,
                    se_devices::v4l2::format_name(fmt),
                    if fps.fract() == 0.0 { format!("{fps:.0}") } else { format!("{fps}") }
                );
                project.write_file(&rel, &content)?;
            }
            Ok(format!("{source} ← {} ({})", dev.identity, dev.path))
        }
        "source.save_controls" => {
            let (controls, rel) = {
                let st = shared.lock();
                let s = st.sources.get(&source).ok_or_else(|| anyhow!("unknown source `{source}`"))?;
                if s.def.camera().is_none() {
                    return Err(anyhow!("{source} is not a camera"));
                }
                (s.status.info.lock().controls.clone(), format!("sources/{source}.toml"))
            };
            if controls.is_empty() {
                return Err(anyhow!("{source}: no controls known (open the camera first)"));
            }
            let snap = ctx.hub.snapshot.load();
            let project = se_store::Project::open(&ctx.project_root)?;
            let mut n = 0;
            project.edit(&rel, |doc| {
                let t = doc
                    .entry("controls")
                    .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
                    .as_table_mut()
                    .ok_or_else(|| anyhow!("[controls] is not a table"))?;
                for c in controls.iter().filter(|c| controls::exposed(c) && !c.read_only()) {
                    let Some(v) = snap.get(&format!("source.{source}.ctrl.{}", c.name)) else { continue };
                    let item = match v {
                        Value::Bool(b) => toml_edit::value(*b),
                        Value::Int(i) => toml_edit::value(*i),
                        Value::Float(f) => toml_edit::value(f.round() as i64),
                        Value::Str(s) => toml_edit::value(s.as_str()),
                        _ => continue,
                    };
                    t.insert(&c.name, item);
                    n += 1;
                }
                Ok(())
            })?;
            Ok(format!("{source}: saved {n} control value(s) to {rel}"))
        }
        other => Err(anyhow!("unknown action `{other}` (source.restart|reopen|seek|assign|save_controls)")),
    }
}

/// Preferred mode for a new source: the largest size ≤ 1080p at its highest rate, preferring
/// YUYV when it runs at ≥ 30 fps (no decode), else MJPEG.
pub fn best_mode(modes: &[se_devices::v4l2::Mode]) -> Option<(u32, u32, u32, f64)> {
    let score = |m: &se_devices::v4l2::Mode| {
        let fps = m.fps.first().copied().unwrap_or(0.0);
        let fits = m.width <= 1920 && m.height <= 1080;
        let yuyv_ok = m.fourcc == se_devices::v4l2::PIX_YUYV && fps >= 29.0;
        let usable = m.fourcc == se_devices::v4l2::PIX_YUYV || m.fourcc == se_devices::v4l2::PIX_MJPEG;
        (usable, fits, (m.width * m.height).min(1920 * 1080), fps.min(60.0) as u32, yuyv_ok)
    };
    modes.iter().filter(|m| !m.fps.is_empty()).max_by_key(|m| score(m)).map(|m| (m.fourcc, m.width, m.height, m.fps[0]))
}

#[cfg(test)]
#[global_allocator]
static ALLOC: se_alloc::Counting = se_alloc::Counting;

#[cfg(test)]
mod tests {
    use super::*;
    use se_devices::v4l2::{Mode, PIX_MJPEG, PIX_YUYV};

    #[test]
    fn best_mode_prefers_uncompressed_at_full_rate() {
        let msi = [Mode { fourcc: PIX_MJPEG, width: 1280, height: 720, fps: vec![60.0] }, Mode { fourcc: PIX_YUYV, width: 1280, height: 720, fps: vec![60.0] }];
        assert_eq!(best_mode(&msi), Some((PIX_YUYV, 1280, 720, 60.0)));
        let sonix = [
            Mode { fourcc: PIX_MJPEG, width: 1920, height: 1080, fps: vec![30.0, 25.0] },
            Mode { fourcc: PIX_YUYV, width: 1920, height: 1080, fps: vec![5.0] },
            Mode { fourcc: PIX_YUYV, width: 640, height: 480, fps: vec![25.0] },
        ];
        assert_eq!(best_mode(&sonix), Some((PIX_MJPEG, 1920, 1080, 30.0)));
        let hws = [Mode { fourcc: PIX_YUYV, width: 1920, height: 1080, fps: vec![60.0] }];
        assert_eq!(best_mode(&hws), Some((PIX_YUYV, 1920, 1080, 60.0)));
        let big =
            [Mode { fourcc: PIX_MJPEG, width: 3840, height: 2160, fps: vec![30.0] }, Mode { fourcc: PIX_MJPEG, width: 1920, height: 1080, fps: vec![30.0] }];
        assert_eq!(best_mode(&big), Some((PIX_MJPEG, 1920, 1080, 30.0)));
    }

    #[test]
    fn allocator_is_counting() {
        assert!(se_alloc::installed());
    }

    #[test]
    fn camera_frame_writes_do_not_allocate_with_or_without_taps() {
        use se_hub::media::{PixelFormat, VideoSlots};
        let slots = VideoSlots::default();
        let mut w = slots.register("cam");
        let frame = vec![7u8; 1280 * 2 * 720];
        let write = |w: &mut se_hub::media::VideoWriter, ts| w.write(1280, 720, 2560, PixelFormat::Yuyv, ts, &frame);
        // the triple buffer sizes its three buffers on first use
        for ts in 0..3 {
            write(&mut w, ts);
        }
        let s = se_alloc::Scope::begin();
        for ts in 0..100 {
            write(&mut w, ts);
        }
        assert_eq!(s.allocs(), 0, "untapped path");
        drop(s);
        let tap = slots.tap("cam", 2);
        for ts in 0..3 {
            write(&mut w, ts);
            drop(tap.recv_timeout(Duration::ZERO));
        }
        let s = se_alloc::Scope::begin();
        for ts in 0..100 {
            write(&mut w, ts);
            drop(tap.recv_timeout(Duration::ZERO).unwrap());
        }
        assert_eq!(s.allocs(), 0, "tapped steady state reuses its copies");
    }

    /// A file source stands in for a camera (same demand path; no devices are opened).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_tap_adds_capture_demand_until_the_grace_after_its_last_drop() {
        let dir = tempfile::tempdir().unwrap();
        let made = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-f", "lavfi", "-i", "testsrc=size=64x48:rate=30", "-t", "2", "-pix_fmt", "yuv420p"])
            .arg(dir.path().join("loop.mp4"))
            .status();
        if !made.is_ok_and(|s| s.success()) {
            eprintln!("ffmpeg CLI not available: skipped");
            return;
        }
        let (hub, core_rx) = se_hub::Hub::new(Arc::new(se_clock::Clock::new()));
        let (_tx, cfg_rx) = tokio::sync::watch::channel(Arc::new(se_core::Config::default()));
        let ctx = EngineCtx {
            hub: hub.clone(),
            db: se_store::Db::memory().unwrap(),
            project_root: dir.path().to_path_buf(),
            data_dir: dir.path().to_path_buf(),
            share_dir: dir.path().to_path_buf(),
            config: cfg_rx,
            http: "127.0.0.1:0".parse().unwrap(),
            dev: true,
        };
        let def = Arc::new(config::parse("loop", &toml::from_str("file = \"loop.mp4\"\nhwaccel = \"none\"").unwrap(), dir.path()).unwrap());
        let shared: Shared = Arc::default();
        let src = Src { def, status: Arc::new(Status::default()), writer: Some(hub.video.register("loop")), worker: None, unused_since: None, taps: 0 };
        shared.lock().sources.insert("loop".into(), src);
        let capturing = |sh: &Shared| sh.lock().sources["loop"].worker.is_some();
        let taps_published = || {
            let mut last = None;
            while let Ok(m) = core_rx.try_recv() {
                if let se_hub::CoreMsg::Input(se_core::Input::Publish { address, value }) = m
                    && address == "video_in.loop.taps"
                {
                    last = value.as_i64();
                }
            }
            last
        };

        update_usage(&ctx, &shared).await;
        assert!(!capturing(&shared), "unused and untapped");

        let tap = hub.tap_video("loop", 4);
        assert!(wanted(&ctx).contains("loop"));
        update_usage(&ctx, &shared).await;
        assert!(capturing(&shared), "a tap alone makes it captured");
        assert_eq!(taps_published(), Some(1));
        let f = tokio::task::spawn_blocking(move || (tap.recv_timeout(Duration::from_secs(5)), tap)).await.unwrap();
        let (frame, tap) = (f.0.expect("tapped frame"), f.1);
        assert_eq!((frame.width, frame.height, frame.format), (64, 48, se_hub::media::PixelFormat::Nv12));
        assert_eq!(frame.data.len(), (frame.stride * 48 * 3 / 2) as usize);
        assert!(frame.seq > 0 && frame.ts > 0);

        drop(tap);
        assert!(!wanted(&ctx).contains("loop"));
        update_usage(&ctx, &shared).await;
        assert!(capturing(&shared), "kept through the unused grace");
        assert_eq!(taps_published(), Some(0));
        shared.lock().sources.get_mut("loop").unwrap().unused_since = Some(Instant::now() - UNUSED_GRACE);
        update_usage(&ctx, &shared).await;
        assert!(!capturing(&shared), "released after the grace");
        assert!(shared.lock().sources["loop"].writer.is_some(), "slot writer handed back");
    }
}
