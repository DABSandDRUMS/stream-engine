//! The render subsystem inside the engine: the dedicated render thread (own 60 fps pacing
//! clock, device-loss recovery), the loader thread, the frames.sock server, and the IO task
//! that turns configuration, patches, triggers, and stats into plans, messages, and state.

use crate::gpu::{Gpu, GpuOptions};
use crate::loader::{Loader, LoaderCmd, Report};
use crate::perf::{PASSES, Stats};
use crate::plan::{CANVAS_NAMES, LAYOUT_NAMES, PALETTE_SLOTS, Plan, SourceKind, atlas_tiles};
use crate::renderer::{AssetRequest, Inputs, Msg, Renderer};
use crossbeam_channel::{Receiver, Sender};
use se_frames::FramesServer;
use se_hub::{Bus, EngineCtx, Hub, Snapshot};
use se_patch::Manifest;
use se_proto::{Meta, Value, ValueType};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Control messages for the render thread.
pub enum Ctl {
    Msg(Msg),
    SimulateDeviceLoss,
    Shutdown,
}

/// Handle returned by [`start`]: stops the render thread (goodbye to clients) when dropped.
pub struct RenderHandle {
    ctl: Sender<Ctl>,
    thread: Option<std::thread::JoinHandle<()>>,
    pub stats: Arc<Stats>,
    pub frames_socket: Option<PathBuf>,
}

impl RenderHandle {
    pub fn stop(&mut self) {
        let _ = self.ctl.send(Ctl::Shutdown);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for RenderHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

fn scan_manifests(hub: &Hub, root: &Path) -> Vec<Arc<Manifest>> {
    let mut out = Vec::new();
    for m in se_patch::scan(root) {
        match m {
            Ok(m) => out.push(Arc::new(m)),
            // the patch loader (se-patch) reports manifest errors on patch.<id>.error
            Err(e) => tracing::debug!(target: "render", "skipping patch: {e}"),
        }
    }
    let _ = hub;
    out
}

fn build_plan(ctx: &EngineCtx, manifests: &[Arc<Manifest>]) -> Arc<Plan> {
    let cfg = ctx.config.borrow().clone();
    let plan = Plan::build(&cfg, manifests, ctx.project_root.clone());
    for e in &plan.errors {
        ctx.hub.log("warn", "render", e.clone());
    }
    Arc::new(plan)
}

fn ro(m: Meta) -> Meta {
    m.readonly().owner("render")
}

fn list_meta(desc: &str) -> Meta {
    Meta { ty: ValueType::List, default: Value::List(Vec::new()), ..Meta::string("") }.readonly().owner("render").describe(desc)
}

fn declare(hub: &Hub, plan: &Plan) {
    for e in crate::effects::LIBRARY {
        for p in e.all_params() {
            let mut m = Meta::float(p.default as f64, [p.range[0] as f64, p.range[1] as f64]).owner("render").describe(p.description);
            if let Some(u) = p.unit {
                m = m.unit(u);
            }
            hub.declare(&format!("fx.{}.{}", e.name, p.name), m);
        }
        if matches!(e.exec, crate::effects::Exec::Lut) {
            hub.declare(&format!("fx.{}.file", e.name), Meta::string("").owner("render").describe("Project-relative .cube file"));
        }
        hub.submit(se_core::Input::DeclareTrigger { address: format!("fx.{}", e.name), spec: se_core::triggers::TriggerSpec::default() });
    }
    for (i, s) in PALETTE_SLOTS.iter().enumerate() {
        hub.declare(&format!("palette.{s}"), Meta::color(plan.settings.palette[i]).owner("render").describe("Stream palette (§16.2)"));
    }
    let ms = |d: &str| ro(Meta::float(0.0, [0.0, 1000.0]).unit("ms").describe(d));
    hub.declare("perf.gpu_ms", ms("GPU time per frame (all passes)"));
    hub.declare("perf.frame_ms", ms("Render thread CPU time per frame"));
    hub.declare("perf.frame_ms_max", ms("Worst render thread frame in the last interval"));
    for p in PASSES {
        hub.declare(&format!("perf.pass.{p}_ms"), ms("GPU time of this pass group"));
    }
    hub.declare("perf.dropped", ro(Meta::int(0, [0.0, 9.2e18]).describe("Frames dropped (missed deadline by a full period)")));
    hub.declare("perf.late", ro(Meta::int(0, [0.0, 9.2e18]).describe("Frames started late (> 2 ms)")));
    hub.declare("perf.fps", ro(Meta::float(0.0, [0.0, 240.0]).describe("Render frame rate")));
    hub.declare("perf.vram_mb", ro(Meta::float(0.0, [0.0, 65536.0]).unit("MB").describe("VRAM used by this process (VK_EXT_memory_budget)")));
    hub.declare("perf.vram_budget_mb", ro(Meta::float(0.0, [0.0, 65536.0]).unit("MB").describe("VRAM budget reported by the driver")));
    hub.declare("perf.render_mb", ro(Meta::float(0.0, [0.0, 65536.0]).unit("MB").describe("Renderer's own textures and buffers")));
    hub.declare("perf.fx_passes", ro(Meta::int(0, [0.0, 4096.0]).describe("Video effect passes in the last frame (a fused run counts once)")));
    hub.declare("perf.fx_fused", ro(Meta::int(0, [0.0, 4096.0]).describe("Video effects that ran inside fused passes in the last frame")));
    hub.declare("render.sources.used", list_meta("Sources the renderer currently shows (program, preview, multiview)"));
    hub.declare("render.atlas.layout", list_meta("Multiview atlas tiles: [{source, rect: [x, y, w, h]}] normalized"));
    hub.declare("render.gpu", ro(Meta::string("").describe("GPU adapter")));
    hub.declare("render.export", ro(Meta::string("").describe("Canvas export path: dmabuf | shm | none")));
    hub.declare("render.clients", ro(Meta::int(0, [0.0, 64.0]).describe("frames.sock clients")));
    hub.declare("render.recoveries", ro(Meta::int(0, [0.0, 9.2e18]).describe("GPU device-loss recoveries")));
    hub.declare("safety.video.limited", ro(Meta::boolean(false).describe("Video flash limiter engaged (§22)")));
    hub.declare("safety.video.limit", ro(Meta::float(0.0, [0.0, 1.0]).describe("Flash limiter strength")));
    hub.declare("safety.video.flash_rate", ro(Meta::float(0.0, [0.0, 60.0]).unit("Hz").describe("Highest flash rate on the output")));
    hub.declare("health.render", ro(Meta { ty: ValueType::Map, default: Value::map(), ..Meta::string("") }));
    publish_palette(hub, plan);
}

fn publish_palette(hub: &Hub, plan: &Plan) {
    let colors = if plan.settings.palette_mode_follow_theme { theme_palette().unwrap_or(plan.settings.palette) } else { plan.settings.palette };
    for (i, s) in PALETTE_SLOTS.iter().enumerate() {
        hub.publish(&format!("palette.{s}"), Value::from(colors[i]));
    }
}

/// `~/.local/state/omarchy/current/theme/colors.toml` (§16.2 follow_theme).
pub fn theme_colors_path() -> Option<PathBuf> {
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/omarchy/current/theme/colors.toml"))
}

pub fn parse_theme_palette(src: &str) -> Option<[[f32; 4]; 8]> {
    let t: toml::Table = src.parse().ok()?;
    let get = |k: &str| {
        t.get(k)
            .and_then(|v| v.as_str())
            .and_then(|s| se_proto::value::parse_hex_color(s.trim_start_matches('#')).or_else(|| se_proto::value::parse_hex_color(s)))
    };
    let mut out = crate::plan::DEFAULT_PALETTE;
    for (i, slot) in PALETTE_SLOTS.iter().enumerate() {
        if let Some(c) = get(slot) {
            out[i] = c;
        }
    }
    get("accent")?;
    Some(out)
}

fn theme_palette() -> Option<[[f32; 4]; 8]> {
    parse_theme_palette(&std::fs::read_to_string(theme_colors_path()?).ok()?)
}

struct HubReport(Arc<Hub>);

impl Report for HubReport {
    fn patch(&self, id: &str, result: Result<(), String>) {
        match &result {
            Ok(()) => self.0.log("info", "render", format!("patch.{id}: shader compiled")),
            Err(e) => self.0.log("error", "render", format!("patch.{id}: {e} (keeping the last good version)")),
        }
        self.0.publish(&format!("patch.{id}.error"), Value::Str(result.err().unwrap_or_default()));
    }
    fn transition(&self, name: &str, result: Result<(), String>) {
        let seg: String = name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
        let addr = format!("render.transition.{seg}.error");
        self.0.declare(&addr, ro(Meta::string("").describe("Transition shader compile error")));
        if let Err(e) = &result {
            self.0.log("error", "render", format!("transition `{name}`: {e} (keeping the last good version, else a crossfade)"));
        }
        self.0.publish(&addr, Value::Str(result.err().unwrap_or_default()));
    }
    fn style(&self, path: &str, result: Result<(), String>) {
        let seg: String = path.trim_end_matches(".wgsl").chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect();
        let addr = format!("render.style.{seg}.error");
        self.0.declare(&addr, ro(Meta::string("").describe("Enter/exit style shader compile error")));
        if let Err(e) = &result {
            self.0.log("error", "render", format!("style `{path}`: {e} (keeping the last good version, else a fade)"));
        }
        self.0.publish(&addr, Value::Str(result.err().unwrap_or_default()));
    }
}

/// The engine's render subsystem (one per process).
static ENGINE: parking_lot::Mutex<Option<RenderHandle>> = parking_lot::Mutex::new(None);

/// Start the render subsystem for the engine. Returns once its threads are spawned.
pub async fn start(ctx: EngineCtx) -> anyhow::Result<()> {
    let h = spawn(ctx).await?;
    if let Some(mut old) = ENGINE.lock().replace(h) {
        old.stop();
    }
    Ok(())
}

/// Stop the render thread: clients get `se_goodbye{shutdown}`, the socket is removed.
pub async fn stop() {
    let h = ENGINE.lock().take();
    if let Some(mut h) = h {
        let _ = tokio::task::spawn_blocking(move || h.stop()).await;
    }
}

/// Start the render subsystem and return its handle (tests and embedders own the lifetime).
pub async fn spawn(ctx: EngineCtx) -> anyhow::Result<RenderHandle> {
    let hub = ctx.hub.clone();
    let manifests = scan_manifests(&hub, &ctx.project_root);
    let plan = build_plan(&ctx, &manifests);
    declare(&hub, &plan);
    let stats = Arc::new(Stats::default());

    let socket = plan.settings.frames_socket.clone().unwrap_or_else(se_frames::default_socket_path);
    let frames = match FramesServer::start(&socket) {
        Ok(f) => {
            tracing::info!(target: "render", "frames.sock at {}", socket.display());
            Some(Arc::new(f))
        }
        Err(e) => {
            hub.log("error", "render", format!("frames.sock {}: {e} — canvases are not exported", socket.display()));
            None
        }
    };
    let (ctl_tx, ctl_rx) = crossbeam_channel::unbounded::<Ctl>();
    let (msg_tx, msg_rx) = crossbeam_channel::unbounded::<Msg>();
    let (loader_tx, loader_rx) = crossbeam_channel::unbounded::<LoaderCmd>();
    let (asset_tx, asset_rx) = crossbeam_channel::bounded::<AssetRequest>(256);
    let loader_thread = Loader::spawn(loader_rx, msg_tx.clone(), Box::new(HubReport(hub.clone())))?;
    // loader output and asset requests are forwarded through small bridges
    {
        let ctl = ctl_tx.clone();
        std::thread::Builder::new().name("se-render-bridge".into()).spawn(move || {
            while let Ok(m) = msg_rx.recv() {
                if ctl.send(Ctl::Msg(m)).is_err() {
                    break;
                }
            }
        })?;
        let lt = loader_tx.clone();
        std::thread::Builder::new().name("se-render-assets".into()).spawn(move || {
            while let Ok(a) = asset_rx.recv() {
                if lt.send(LoaderCmd::Asset(a)).is_err() {
                    break;
                }
            }
        })?;
    }
    loader_tx.send(LoaderCmd::Plan(plan.clone()))?;

    // `--dev`: Vulkan validation + debug labels (the validation layer must be installed)
    let opts = GpuOptions { adapter: plan.settings.adapter.clone(), debug: ctx.dev };
    let thread = {
        let (hub, stats, frames, loader_tx, plan) = (hub.clone(), stats.clone(), frames.clone(), loader_tx.clone(), plan.clone());
        std::thread::Builder::new().name("se-render".into()).spawn(move || {
            render_thread(hub, plan, opts, ctl_rx, loader_tx.clone(), asset_tx, frames, stats);
            // the loader holds a device reference: let it go before the process exits
            let _ = loader_tx.send(LoaderCmd::Shutdown);
            let _ = loader_thread.join();
        })?
    };

    let io = Io { ctx: ctx.clone(), ctl: ctl_tx.clone(), loader: loader_tx, stats: stats.clone(), frames: frames.clone(), plan: plan.clone(), manifests };
    tokio::spawn(io.run());
    register_query(&ctx, stats.clone(), frames.clone());
    Ok(RenderHandle { ctl: ctl_tx, thread: Some(thread), stats, frames_socket: frames.as_ref().map(|f| f.path().to_path_buf()) })
}

fn register_query(ctx: &EngineCtx, stats: Arc<Stats>, frames: Option<Arc<FramesServer>>) {
    let hub = ctx.hub.clone();
    ctx.hub.register_query(
        "render",
        Arc::new(move |_, _| {
            let (hub, stats, frames) = (hub.clone(), stats.clone(), frames.clone());
            Box::pin(async move {
                let v = stats.view();
                let snap = hub.snapshot.load();
                let mut m = Value::map()
                    .with("gpu", snap.get("render.gpu").cloned().unwrap_or_default())
                    .with("fps", v.fps as f64)
                    .with("frame_ms", v.frame_ms as f64)
                    .with("gpu_ms", v.gpu_ms as f64)
                    .with("dropped", v.dropped as i64)
                    .with("late", v.late as i64)
                    .with("vram_mb", v.vram_mb as f64)
                    .with("render_mb", v.own_mb as f64)
                    .with("recoveries", v.recoveries as i64)
                    .with("flash_limited", v.limited)
                    .with("alloc_violations", v.alloc_violations as i64)
                    .with("fx_passes", v.fx_passes as i64)
                    .with("fx_fused", v.fx_fused as i64);
                let passes: Vec<Value> = PASSES.iter().zip(v.pass_ms).map(|(n, ms)| Value::map().with("pass", *n).with("ms", ms as f64)).collect();
                m = m.with("passes", Value::List(passes));
                if let Some(f) = &frames {
                    let s = f.stats();
                    m = m.with(
                        "frames",
                        Value::map()
                            .with("socket", f.path().to_string_lossy().to_string())
                            .with("clients", s.clients as i64)
                            .with("dmabuf_clients", s.dmabuf_clients as i64)
                            .with("shm_clients", s.shm_clients as i64)
                            .with("sent", s.frames_sent as i64)
                            .with("dropped", s.frames_dropped as i64),
                    );
                }
                Ok(m)
            })
        }),
    );
}

#[allow(clippy::too_many_arguments)]
fn render_thread(
    hub: Arc<Hub>,
    mut plan: Arc<Plan>,
    opts: GpuOptions,
    ctl: Receiver<Ctl>,
    loader: Sender<LoaderCmd>,
    assets: Sender<AssetRequest>,
    frames: Option<Arc<FramesServer>>,
    stats: Arc<Stats>,
) {
    // Tight timer slack for the pacing sleep.
    unsafe { libc::prctl(libc::PR_SET_TIMERSLACK, 50_000u64, 0, 0, 0) };
    let janitor = spawn_janitor();
    let mut inputs = Inputs::default();
    let mut device_gen = 0u64;
    let shutdown = AtomicBool::new(false);
    'device: loop {
        device_gen += 1;
        let gpu = match Gpu::new(&opts) {
            Ok(g) => g,
            Err(e) => {
                stats.device_ok.store(false, Ordering::Relaxed);
                hub.log("error", "render", format!("GPU unavailable: {e:#} (retrying)"));
                if wait_or_shutdown(&ctl, Duration::from_secs(2), &mut plan) {
                    break 'device;
                }
                continue;
            }
        };
        hub.publish("render.gpu", Value::Str(format!("{} ({})", gpu.info.name, gpu.info.driver_info)));
        let mut r = match Renderer::new(gpu, device_gen, plan.clone(), assets.clone(), frames.clone(), stats.clone()) {
            Ok(r) => r,
            Err(e) => {
                stats.device_ok.store(false, Ordering::Relaxed);
                hub.log("error", "render", format!("renderer init failed: {e:#} (retrying)"));
                if wait_or_shutdown(&ctl, Duration::from_secs(2), &mut plan) {
                    break 'device;
                }
                continue;
            }
        };
        let _ = loader.send(LoaderCmd::Device { device: r.gpu.device.clone(), layouts: r.layouts(), generation: device_gen });
        let _ = loader.send(LoaderCmd::Plan(plan.clone()));
        inputs.refresh(&hub, &plan, true);
        let mut period = frame_period(&plan);
        let mut next = se_clock::now() + period;
        let mut fps_window = (se_clock::now(), 0u32);
        loop {
            sleep_until(next);
            let now = se_clock::now();
            if now > next + period {
                let missed = (now - next) / period;
                stats.dropped.fetch_add(missed, Ordering::Relaxed);
                next += missed * period;
            } else if now > next + 2_000_000 {
                stats.late.fetch_add(1, Ordering::Relaxed);
            }
            while let Ok(c) = ctl.try_recv() {
                match c {
                    Ctl::Shutdown => {
                        shutdown.store(true, Ordering::Relaxed);
                    }
                    Ctl::SimulateDeviceLoss => {
                        hub.log("warn", "render", "simulating GPU device loss");
                        r.gpu.simulate_loss();
                    }
                    Ctl::Msg(Msg::Plan(p)) => {
                        plan = p.clone();
                        r.apply(Msg::Plan(p));
                        inputs.refresh(&hub, &plan, true);
                        period = frame_period(&plan);
                    }
                    Ctl::Msg(m) => r.apply(m),
                }
            }
            if shutdown.load(Ordering::Relaxed) {
                break 'device;
            }
            if r.gpu.is_lost() {
                break;
            }
            inputs.refresh(&hub, &plan, false);
            let snap: Arc<Snapshot> = hub.snapshot.load_full();
            r.frame(&snap, next, &mut inputs);
            if janitor.try_send(snap).is_err() {
                // janitor busy: dropping here is fine (rare)
            }
            hub.render_heartbeat.store(se_clock::now(), Ordering::Relaxed);
            fps_window.1 += 1;
            let t = se_clock::now();
            if t - fps_window.0 >= 1_000_000_000 {
                stats.set_fps(fps_window.1 as f32 * 1e9 / (t - fps_window.0) as f32);
                fps_window = (t, 0);
            }
            if r.gpu.is_lost() {
                break;
            }
            next += period;
        }
        // device lost: tell clients, drop everything, recreate
        let why = r.gpu.lost_reason.lock().clone();
        hub.log("error", "render", format!("GPU device lost ({why}); recovering"));
        stats.device_ok.store(false, Ordering::Relaxed);
        stats.recoveries.fetch_add(1, Ordering::Relaxed);
        if let Some(f) = &frames {
            f.device_lost();
        }
        drop(r);
        std::thread::sleep(Duration::from_millis(100));
    }
    if let Some(f) = frames {
        // last reference → goodbye{shutdown} to all clients
        drop(f);
    }
    let _ = loader.send(LoaderCmd::Shutdown);
    tracing::info!(target: "render", "render thread stopped");
}

fn frame_period(plan: &Plan) -> u64 {
    1_000_000_000 / plan.canvases[crate::plan::WIDE].fps.max(1) as u64
}

fn sleep_until(t: u64) {
    let ts = libc::timespec { tv_sec: (t / 1_000_000_000) as libc::time_t, tv_nsec: (t % 1_000_000_000) as libc::c_long };
    loop {
        let r = unsafe { libc::clock_nanosleep(libc::CLOCK_MONOTONIC, libc::TIMER_ABSTIME, &ts, std::ptr::null_mut()) };
        if r != libc::EINTR {
            break;
        }
    }
}

/// Wait while the GPU is unavailable; returns true on shutdown. Keeps the latest plan.
fn wait_or_shutdown(ctl: &Receiver<Ctl>, d: Duration, plan: &mut Arc<Plan>) -> bool {
    let deadline = std::time::Instant::now() + d;
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        match ctl.recv_timeout(left) {
            Ok(Ctl::Shutdown) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return true,
            Ok(Ctl::Msg(Msg::Plan(p))) => *plan = p,
            Ok(_) => {}
            Err(crossbeam_channel::RecvTimeoutError::Timeout) => break,
        }
    }
    false
}

/// Drops old snapshots off the render thread (freeing a large snapshot takes time).
fn spawn_janitor() -> Sender<Arc<Snapshot>> {
    let (tx, rx) = crossbeam_channel::bounded::<Arc<Snapshot>>(8);
    let _ = std::thread::Builder::new().name("se-render-janitor".into()).spawn(move || while rx.recv().is_ok() {});
    tx
}

struct Io {
    ctx: EngineCtx,
    ctl: Sender<Ctl>,
    loader: Sender<LoaderCmd>,
    stats: Arc<Stats>,
    frames: Option<Arc<FramesServer>>,
    plan: Arc<Plan>,
    manifests: Vec<Arc<Manifest>>,
}

impl Io {
    fn replan(&mut self, rescan: bool) {
        if rescan {
            self.manifests = scan_manifests(&self.ctx.hub, &self.ctx.project_root);
        }
        let plan = build_plan(&self.ctx, &self.manifests);
        publish_palette(&self.ctx.hub, &plan);
        publish_atlas(&self.ctx.hub, &plan);
        let _ = self.loader.send(LoaderCmd::Plan(plan.clone()));
        let _ = self.ctl.send(Ctl::Msg(Msg::Plan(plan.clone())));
        self.plan = plan;
    }

    async fn run(mut self) {
        let hub = self.ctx.hub.clone();
        let mut config = self.ctx.config.clone();
        let mut bus = hub.subscribe();
        let mut actions = hub.route_actions("render");
        let (watch_tx, mut watch_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<PathBuf>>();
        let root = self.ctx.project_root.clone();
        let _watcher = start_watcher(&root, watch_tx);
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        let mut last_used = u64::MAX;
        let mut last_dropped = 0u64;
        publish_atlas(&hub, &self.plan);
        let mut pending_replan: Option<(tokio::time::Instant, bool)> = None;
        loop {
            let debounce = async {
                match pending_replan {
                    Some((t, _)) => tokio::time::sleep_until(t).await,
                    None => std::future::pending().await,
                }
            };
            tokio::select! {
                r = config.changed() => {
                    if r.is_err() { break; }
                    self.replan(false);
                }
                Some(paths) = watch_rx.recv() => {
                    let root = &self.ctx.project_root;
                    let manifests_changed = paths.iter().any(|p| p.strip_prefix(root).is_ok_and(|r| r.starts_with("patches")) && p.file_name().is_some_and(|n| n == "patch.toml") || p.is_dir());
                    let shaders_changed = paths.iter().any(|p| p.extension().is_some_and(|e| e == "wgsl"));
                    for p in &paths {
                        if let Ok(rel) = p.strip_prefix(root)
                            && rel.starts_with("assets")
                        {
                            let _ = self.ctl.send(Ctl::Msg(Msg::AssetChanged(rel.to_string_lossy().to_string())));
                        }
                    }
                    if manifests_changed || shaders_changed {
                        let rescan = manifests_changed || pending_replan.is_some_and(|p| p.1);
                        pending_replan = Some((tokio::time::Instant::now() + Duration::from_millis(150), rescan));
                    }
                }
                _ = debounce => {
                    let rescan = pending_replan.take().is_some_and(|p| p.1);
                    if rescan { self.replan(true); } else { let _ = self.loader.send(LoaderCmd::Recompile); }
                }
                ev = bus.recv() => match ev {
                    Ok(b) => {
                        if let Bus::Event(e) = &*b
                            && let Some(base) = e.ty.strip_suffix(".trigger")
                        {
                            if let Some(fx) = base.strip_prefix("fx.") {
                                let level = e.payload.get_path("amount").or_else(|| e.payload.get_path("level")).and_then(Value::as_f32).map(|v| v.clamp(0.0, 1.0));
                                let _ = self.ctl.send(Ctl::Msg(Msg::TriggerLevel { effect: fx.to_string(), level }));
                            } else if let Some(id) = base.strip_prefix("patch.") {
                                let payload = se_core::triggers::TriggerPayload::from_event(&e.payload, e.actor.as_ref()).floats();
                                let _ = self.ctl.send(Ctl::Msg(Msg::PatchTrigger { patch: id.to_string(), payload }));
                            }
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(_) => break,
                },
                Some(cmd) = actions.recv() => {
                    if let se_proto::Op::Action { name, .. } = &cmd.op {
                        match name.as_str() {
                            "render.simulate_device_loss" => { let _ = self.ctl.send(Ctl::SimulateDeviceLoss); }
                            "render.reload" => self.replan(true),
                            other => hub.log("warn", "render", format!("unknown action `{other}`")),
                        }
                    }
                }
                _ = tick.tick() => {
                    let g = self.stats.used_generation.load(Ordering::Acquire);
                    if g != last_used {
                        last_used = g;
                        let bits = self.stats.used();
                        let names: Vec<Value> = self.plan.sources.iter().enumerate().filter(|(i, _)| *i < 256 && bits[i / 64] & (1 << (i % 64)) != 0).filter(|(_, s)| !matches!(s.kind, SourceKind::Solid(_))).map(|(_, s)| Value::Str(s.name.clone())).collect();
                        hub.publish("render.sources.used", Value::List(names));
                    }
                    last_dropped = publish_stats(&hub, &self.stats, self.frames.as_deref(), &self.plan, last_dropped);
                }
            }
        }
    }
}

fn publish_atlas(hub: &Hub, plan: &Plan) {
    let srcs = plan.atlas_sources();
    let size = [plan.canvases[crate::plan::ATLAS].width, plan.canvases[crate::plan::ATLAS].height];
    let tiles = atlas_tiles(srcs.len(), size);
    let list = srcs.iter().zip(tiles).map(|(s, t)| Value::map().with("source", plan.sources[*s as usize].name.clone()).with("rect", Value::from(t))).collect();
    hub.publish("render.atlas.layout", Value::List(list));
}

fn publish_stats(hub: &Hub, stats: &Stats, frames: Option<&FramesServer>, plan: &Plan, last_dropped: u64) -> u64 {
    let v = stats.view();
    let r2 = |x: f32| ((x * 100.0).round() / 100.0) as f64;
    hub.publish("perf.gpu_ms", Value::Float(r2(v.gpu_ms)));
    hub.publish("perf.frame_ms", Value::Float(r2(v.frame_ms)));
    hub.publish("perf.frame_ms_max", Value::Float(r2(v.frame_ms_max)));
    for (p, ms) in PASSES.iter().zip(v.pass_ms) {
        hub.publish(&format!("perf.pass.{p}_ms"), Value::Float(r2(ms)));
    }
    hub.publish("perf.dropped", Value::Int(v.dropped as i64));
    hub.publish("perf.late", Value::Int(v.late as i64));
    hub.publish("perf.fps", Value::Float(r2(v.fps)));
    hub.publish("perf.vram_mb", Value::Float(v.vram_mb.round() as f64));
    hub.publish("perf.vram_budget_mb", Value::Float(v.vram_budget_mb.round() as f64));
    hub.publish("perf.render_mb", Value::Float(v.own_mb.round() as f64));
    hub.publish("perf.fx_passes", Value::Int(v.fx_passes as i64));
    hub.publish("perf.fx_fused", Value::Int(v.fx_fused as i64));
    hub.publish("safety.video.limited", Value::Bool(v.limited));
    hub.publish("safety.video.limit", Value::Float(r2(v.limit)));
    hub.publish("safety.video.flash_rate", Value::Float(r2(v.flash_rate)));
    hub.publish("render.recoveries", Value::Int(v.recoveries as i64));
    let fs = frames.map(|f| f.stats());
    hub.publish("render.clients", Value::Int(fs.map_or(0, |s| s.clients as i64)));
    let export = match (frames, v.exporting, v.export_error) {
        (None, _, _) => "none",
        (Some(_), true, false) => "dmabuf",
        (Some(_), _, true) => "shm (dmabuf failed)",
        (Some(_), false, false) => "idle",
    };
    hub.publish("render.export", Value::Str(export.into()));
    let dropping = v.dropped > last_dropped;
    let (status, detail) = if !v.device_ok {
        ("fail", "GPU device unavailable or recovering".to_string())
    } else {
        let mut problems = Vec::new();
        if v.fps > 0.0 && v.fps < 57.0 {
            problems.push(format!("{:.1} fps", v.fps));
        }
        if dropping {
            problems.push(format!("{} dropped frames", v.dropped - last_dropped));
        }
        if v.gpu_ms > 8.0 {
            problems.push(format!("GPU {:.1} ms > 8 ms budget", v.gpu_ms));
        }
        if v.export_error {
            problems.push("dmabuf export failed (shm fallback only)".into());
        }
        if frames.is_none() {
            problems.push("frames.sock not available".into());
        }
        if !plan.errors.is_empty() {
            problems.push(format!("{} config problems (see log)", plan.errors.len()));
        }
        let summary =
            format!("{:.1} fps, GPU {:.2} ms, CPU {:.2} ms, VRAM {:.0} MB, {} clients", v.fps, v.gpu_ms, v.frame_ms, v.vram_mb, fs.map_or(0, |s| s.clients));
        if problems.is_empty() { ("pass", summary) } else { ("warn", format!("{}; {summary}", problems.join(", "))) }
    };
    hub.publish("health.render", Value::map().with("status", status).with("detail", detail));
    let _ = (CANVAS_NAMES, LAYOUT_NAMES);
    v.dropped
}

/// Watch patches/, transitions/, assets/ (and the Omarchy theme for follow_theme palettes).
fn start_watcher(root: &Path, tx: tokio::sync::mpsc::UnboundedSender<Vec<PathBuf>>) -> Option<notify::RecommendedWatcher> {
    use notify::Watcher;
    let mut w = notify::recommended_watcher(move |ev: notify::Result<notify::Event>| {
        if let Ok(ev) = ev
            && !matches!(ev.kind, notify::EventKind::Access(_))
        {
            let _ = tx.send(ev.paths);
        }
    })
    .map_err(|e| tracing::warn!(target: "render", "file watcher unavailable: {e}"))
    .ok()?;
    for d in ["patches", "transitions", "assets"] {
        let p = root.join(d);
        if p.exists()
            && let Err(e) = w.watch(&p, notify::RecursiveMode::Recursive)
        {
            tracing::warn!(target: "render", "watching {}: {e}", p.display());
        }
    }
    Some(w)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn theme_palette_parsing() {
        let p = parse_theme_palette("accent = \"#7e9cd8\"\nbackground = \"#1f1f28\"\nred = \"#e82424\"\nmode = \"dark\"").unwrap();
        assert!((p[0][0] - 0x7e as f32 / 255.0).abs() < 1e-3);
        assert!((p[3][0] - 0xe8 as f32 / 255.0).abs() < 1e-3);
        assert_eq!(p[4], crate::plan::DEFAULT_PALETTE[4], "missing keys keep defaults");
        assert!(parse_theme_palette("foreground = \"#ffffff\"").is_none(), "accent required");
    }
}
