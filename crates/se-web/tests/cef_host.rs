//! End-to-end tests against the real CEF host (`stream-engine-web`): pages served over HTTP,
//! rendered off-screen, delivered into `hub.video` / `hub.audio`, with renderer and host crashes.
//!
//! They need the CEF runtime, so they are `#[ignore]`d in normal runs:
//!
//! ```sh
//! scripts/install-cef.sh                       # once: downloads CEF, installs the host
//! cargo test -p se-web -- --ignored            # or STREAM_ENGINE_WEB_HOST=<path> cargo test …
//! ```
//!
//! Without an installed host they fail with the install instruction instead of passing.

use parking_lot::Mutex;
use se_api::Auth;
use se_api::auth::Scope;
use se_core::{Config, Core, SourceFile};
use se_hub::media::{PixelFormat, VideoReader};
use se_hub::{EngineCtx, Hub, RunnerHooks, run_core};
use se_patch::{Manifest, PatchInfo, PatchSet};
use se_proto::Value;
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

const WAIT: Duration = Duration::from_secs(45);

// ----- tiny HTTP server (stands in for the engine's `/patches/<id>/*` route) --------------

#[derive(Clone, Default)]
struct Http {
    files: Arc<Mutex<HashMap<String, String>>>,
    requests: Arc<Mutex<Vec<String>>>,
}

impl Http {
    fn start() -> (Http, SocketAddr) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let http = Http::default();
        let h = http.clone();
        std::thread::spawn(move || {
            for conn in l.incoming() {
                let Ok(mut s) = conn else { continue };
                let h = h.clone();
                std::thread::spawn(move || {
                    let mut r = BufReader::new(s.try_clone().unwrap());
                    let mut line = String::new();
                    if r.read_line(&mut line).is_err() {
                        return;
                    }
                    loop {
                        let mut hdr = String::new();
                        if r.read_line(&mut hdr).map_or(true, |n| n == 0) || hdr == "\r\n" {
                            break;
                        }
                    }
                    let target = line.split_whitespace().nth(1).unwrap_or("/").to_string();
                    h.requests.lock().push(target.clone());
                    let path = target.split('?').next().unwrap_or("/");
                    let body = h.files.lock().get(path).cloned();
                    let resp = match body {
                        Some(b) => format!("HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{b}", b.len()),
                        None => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
                    };
                    let _ = s.write_all(resp.as_bytes());
                });
            }
        });
        (http, addr)
    }

    fn put(&self, path: &str, body: &str) {
        self.files.lock().insert(path.into(), body.into());
    }
}

// ----- engine harness -----------------------------------------------------------------------

struct Engine {
    hub: Arc<Hub>,
    auth: Arc<Auth>,
    http: Http,
    config_tx: watch::Sender<Arc<Config>>,
    patches_tx: watch::Sender<Arc<PatchSet>>,
    _dirs: Vec<tempfile::TempDir>,
    patch_root: PathBuf,
}

const PROJECT: &str = "schema = 1\n[canvas.wide]\nwidth = 1920\nheight = 1080\n";

fn config(scene: &str) -> Config {
    let files = vec![
        SourceFile { kind: "project".into(), name: "project".into(), path: "project.toml".into(), table: PROJECT.parse().unwrap() },
        SourceFile { kind: "scenes".into(), name: "main".into(), path: "scenes/main.toml".into(), table: scene.parse().unwrap() },
    ];
    let c = Config::build(&files);
    assert!(c.errors.is_empty(), "{:?}", c.errors);
    c
}

fn require_host() {
    if se_web::find_host(Path::new("/nonexistent")).is_none() {
        panic!("{} (or set STREAM_ENGINE_WEB_HOST to a built stream-engine-web with libcef.so beside it)", se_web::INSTALL_HINT);
    }
}

impl Engine {
    async fn start(scene: &str, patches: &[(&str, &str, &str)]) -> Engine {
        require_host();
        let (http, addr) = Http::start();
        let project = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let share = tempfile::tempdir().unwrap();
        let cfg = config(scene);
        let (hub, rx) = Hub::new(Arc::new(se_clock::Clock::new()));
        let core = Core::new(cfg.clone(), se_clock::now());
        {
            let hub = hub.clone();
            std::thread::spawn(move || run_core(core, hub, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 2, on_runtime: None }));
        }
        let (config_tx, config_rx) = watch::channel(Arc::new(cfg));
        let patch_root = project.path().join("patches");
        let mut set = PatchSet::new();
        for (id, manifest, page) in patches {
            let m = Manifest::parse(&patch_root.join(id), manifest).unwrap();
            http.put(&format!("/patches/{id}/{}", m.entry), page);
            set.insert(id.to_string(), PatchInfo { manifest: Arc::new(m), enabled: true, generation: 1 });
        }
        let (patches_tx, patches_rx) = watch::channel(Arc::new(set));
        let ctx = EngineCtx {
            hub: hub.clone(),
            db: se_store::Db::memory().unwrap(),
            project_root: project.path().to_path_buf(),
            data_dir: data.path().to_path_buf(),
            share_dir: share.path().to_path_buf(),
            config: config_rx,
            http: addr,
            dev: true,
        };
        let auth = Arc::new(Auth::new(None));
        se_web::start_with(ctx, auth.clone(), patches_rx).await.unwrap();
        Engine { hub, auth, http, config_tx, patches_tx, _dirs: vec![project, data, share], patch_root }
    }

    fn get(&self, addr: &str) -> Option<Value> {
        self.hub.snapshot.load().get(addr).cloned()
    }

    async fn until<T>(&self, what: &str, mut f: impl FnMut(&Engine) -> Option<T>) -> T {
        let t0 = Instant::now();
        loop {
            if let Some(v) = f(self) {
                return v;
            }
            if t0.elapsed() > WAIT {
                let q = self.hub.query("web", Value::Null).await.unwrap_or_default();
                panic!("timed out waiting for {what}; web query: {}", serde_json::to_string_pretty(&q).unwrap());
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    async fn reader(&self, slot: &str) -> VideoReader {
        self.until(&format!("video slot {slot}"), |e| e.hub.video.take_reader(slot)).await
    }

    async fn web(&self) -> Value {
        self.hub.query("web", Value::Null).await.unwrap()
    }

    async fn host_pid(&self) -> u32 {
        let t0 = Instant::now();
        loop {
            let q = self.web().await;
            if q.get_path("host.state").and_then(Value::as_str) == Some("ready")
                && let Some(pid) = q.get_path("host.pid").and_then(Value::as_i64)
            {
                return pid as u32;
            }
            assert!(t0.elapsed() < WAIT, "host never became ready: {q:?}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

/// A frame as the render thread would see it.
#[derive(Debug)]
struct Frame {
    width: u32,
    height: u32,
    stride: u32,
    format: PixelFormat,
    seq: u64,
    data: Vec<u8>,
}

impl Frame {
    fn px(&self, x: u32, y: u32) -> [u8; 4] {
        let i = (y * self.stride + x * 4) as usize;
        self.data[i..i + 4].try_into().unwrap()
    }
    fn center(&self) -> [u8; 4] {
        self.px(self.width / 2, self.height / 2)
    }
}

async fn next_frame(r: &mut VideoReader, what: &str, ok: impl Fn(&Frame) -> bool) -> Frame {
    let t0 = Instant::now();
    let mut last = None;
    loop {
        if let Some(f) = r.fresh() {
            let f = Frame { width: f.width, height: f.height, stride: f.stride, format: f.format, seq: f.seq, data: f.data.clone() };
            if ok(&f) {
                return f;
            }
            last = Some((f.width, f.height, f.format, f.center()));
        }
        assert!(t0.elapsed() < WAIT, "timed out waiting for {what}; last frame (w, h, format, center BGRA) = {last:?}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

const GREEN: [u8; 4] = [0, 255, 0, 255];

/// Solid green with a small corner square toggling every animation frame (continuous paints).
const ANIMATED: &str = r##"<!doctype html><html><body style="margin:0;background:#00ff00">
<div id="b" style="position:absolute;left:0;top:0;width:8px;height:8px"></div>
<script>let t=0;(function f(){t++;document.getElementById("b").style.background=t%2?"#f00":"#00f";requestAnimationFrame(f)})();</script>
</body></html>"##;

/// Transparent page, left half 50% red.
const ALPHA: &str = r##"<!doctype html><html><body style="margin:0;background:transparent">
<div style="position:absolute;left:0;top:0;width:50%;height:100%;background:rgba(255,0,0,0.5)"></div></body></html>"##;

fn scene(w: f32, h: f32) -> String {
    format!("[canvas.wide]\nnodes = [{{ src = \"patch.solid\", rect = [0.0, 0.0, {w}, {h}] }}]")
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the CEF runtime: scripts/install-cef.sh, then cargo test -p se-web -- --ignored"]
async fn renders_patch_pages_into_video_slots_and_resizes_live() {
    let e = Engine::start(&scene(0.25, 0.25), &[("solid", "kind = \"web\"", ANIMATED), ("alpha", "kind = \"web\"\nsize = [64, 32]\nfps = 10", ALPHA)]).await;
    let mut solid = e.reader("patch.solid").await;
    let mut alpha = e.reader("patch.alpha").await;

    // first frame: node size 0.25 × 1920x1080, BGRA, the page's green
    let t0 = Instant::now();
    let f = next_frame(&mut solid, "a green 480x270 frame", |f| f.width == 480 && f.height == 270 && f.center() == GREEN).await;
    println!("first frame after {:?} (host start + page load)", t0.elapsed());
    assert_eq!((f.format, f.stride), (PixelFormat::Bgra8, 480 * 4));
    assert_eq!(f.data.len(), 480 * 270 * 4);

    // transparent background + premultiplied alpha
    let a = next_frame(&mut alpha, "the alpha page", |f| f.width == 64 && f.height == 32 && f.px(48, 16)[3] == 0 && f.px(16, 16)[3] > 0).await;
    let left = a.px(16, 16);
    println!("50% red over transparent = BGRA {left:?}");
    assert_eq!(a.px(48, 16), [0, 0, 0, 0], "no page background → fully transparent");
    assert!(
        (left[3] as i32 - 128).abs() <= 2 && (left[2] as i32 - left[3] as i32).abs() <= 2 && left[0] == 0 && left[1] == 0,
        "premultiplied BGRA, got {left:?}"
    );

    // status, tokens, health
    e.until("patch.solid running", |e| (e.get("web.patch.solid.status") == Some(Value::Str("running".into()))).then_some(())).await;
    assert_eq!(e.get("patch.solid.error"), Some(Value::Str(String::new())));
    let health = e.until("health.cef", |e| e.get("health.cef")).await;
    assert_eq!(health.get_path("status").and_then(Value::as_str), Some("pass"), "{health:?}");
    let target = e.http.requests.lock().iter().find(|r| r.starts_with("/patches/solid/index.html?token=")).cloned().expect("page requested with a token");
    let token = target.rsplit("token=").next().unwrap().to_string();
    assert_eq!(token.len(), 64);
    assert_eq!(e.auth.check(&token), Some(Scope::Patch("solid".into(), Vec::new())), "token scoped to the patch");

    // paint rate and latency
    let (n0, t0) = (f.seq, Instant::now());
    tokio::time::sleep(Duration::from_secs(2)).await;
    let f2 = next_frame(&mut solid, "more frames", |_| true).await;
    let fps = (f2.seq - n0) as f64 / t0.elapsed().as_secs_f64();
    let q = e.web().await;
    println!("measured {fps:.1} fps; web query: {}", serde_json::to_string(&q).unwrap());
    // 60 fps on an idle machine; the bound leaves room for a heavily loaded CI box
    assert!(fps > 25.0, "animated page should paint continuously (target 60 fps), got {fps:.1}");
    let src = q
        .get_path("sources")
        .and_then(Value::as_list)
        .unwrap()
        .iter()
        .find(|s| s.get_path("slot").and_then(Value::as_str) == Some("patch.solid"))
        .unwrap()
        .clone();
    let url = src.get_path("url").and_then(Value::as_str).unwrap();
    assert!(url.ends_with("/patches/solid/index.html?token=…") && !url.contains(&token), "token redacted: {url}");
    let published = e.until("web.patch.solid.fps", |e| e.get("web.patch.solid.fps").and_then(|v| v.as_f64()).filter(|v| *v > 20.0)).await;
    println!("web.patch.solid.fps = {published}, latency {:?} ms", src.get_path("latency_ms"));

    // live resize from a scene change
    e.config_tx.send(Arc::new(config(&scene(0.5, 0.5)))).unwrap();
    let r = next_frame(&mut solid, "a 960x540 frame after the resize", |f| f.width == 960 && f.height == 540 && f.center() == GREEN).await;
    assert_eq!(r.stride, 960 * 4);

    // disabling the patch stops output (transparent frame) and revokes its token
    let mut set = (**e.patches_tx.borrow()).clone();
    set.get_mut("solid").unwrap().enabled = false;
    e.patches_tx.send(Arc::new(set)).unwrap();
    next_frame(&mut solid, "a transparent frame after disabling", |f| f.data.iter().all(|&b| b == 0)).await;
    e.until("token revoked", |e| e.auth.check(&token).is_none().then_some(())).await;
    e.until("web.patch.solid removed", |e| e.get("web.patch.solid.status").is_none().then_some(())).await;
    let _ = &e.patch_root;
}

/// Pids of all descendants of `root` with their command lines.
fn descendants(root: u32) -> Vec<(u32, String)> {
    let mut parent = HashMap::new();
    let mut cmd = HashMap::new();
    for d in std::fs::read_dir("/proc").unwrap().flatten() {
        let Ok(pid) = d.file_name().to_string_lossy().parse::<u32>() else { continue };
        let Ok(stat) = std::fs::read_to_string(d.path().join("stat")) else { continue };
        let Some(rest) = stat.rfind(')').map(|i| &stat[i + 2..]) else { continue };
        let ppid: u32 = rest.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
        parent.insert(pid, ppid);
        cmd.insert(pid, std::fs::read(d.path().join("cmdline")).map(|b| String::from_utf8_lossy(&b).replace('\0', " ")).unwrap_or_default());
    }
    let mut out = Vec::new();
    let mut seen = HashSet::from([root]);
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        for (&c, &pp) in &parent {
            if pp == p && seen.insert(c) {
                out.push((c, cmd.get(&c).cloned().unwrap_or_default()));
                frontier.push(c);
            }
        }
    }
    out
}

/// Renderer processes of a host with a page loaded. Chromium forks renderers from its zygote,
/// so their command line still says `--type=zygote`; an active renderer runs a thread named
/// `Compositor` (the GPU process has `VizCompositorTh`).
fn renderers(host: u32) -> Vec<u32> {
    descendants(host)
        .into_iter()
        .filter(|(pid, c)| {
            c.contains("--type=zygote")
                && std::fs::read_dir(format!("/proc/{pid}/task"))
                    .map(|d| d.flatten().any(|t| std::fs::read_to_string(t.path().join("comm")).is_ok_and(|n| n.trim() == "Compositor")))
                    .unwrap_or(false)
        })
        .map(|(p, _)| p)
        .collect()
}

fn kill9(pid: u32) {
    // SAFETY: sending a signal to a pid we just looked up.
    assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0, "kill -9 {pid}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the CEF runtime: scripts/install-cef.sh, then cargo test -p se-web -- --ignored"]
async fn recovers_from_renderer_and_host_crashes() {
    let e = Engine::start(&scene(0.25, 0.25), &[("solid", "kind = \"web\"", ANIMATED)]).await;
    let mut r = e.reader("patch.solid").await;
    next_frame(&mut r, "first green frame", |f| f.center() == GREEN).await;
    let host = e.host_pid().await;
    let before = e.until("a renderer process", |_| Some(renderers(host)).filter(|v| !v.is_empty())).await;
    let core_tick = e.hub.snapshot.load().tick;

    // renderer crash → page reloads in a new renderer, frames resume
    for p in &before {
        kill9(*p);
    }
    let t0 = Instant::now();
    e.until("the crash to be counted", |e| e.get("web.patch.solid.crashes").and_then(|v| v.as_i64()).filter(|n| *n >= 1)).await;
    let err = e.get("patch.solid.error").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
    assert!(err.starts_with("renderer crashed (") && err.ends_with("— restarted"), "patch.solid.error = {err:?}");
    while r.fresh().is_some() {}
    let f = next_frame(&mut r, "frames after the renderer crash", |f| f.center() == GREEN && f.width == 480).await;
    println!("renderer killed → frames again after {:?} (seq {})", t0.elapsed(), f.seq);
    let after = e.until("a new renderer process", |_| Some(renderers(host)).filter(|v| !v.is_empty() && v.iter().all(|p| !before.contains(p)))).await;
    println!("renderer pids before {before:?}, after {after:?}");
    assert_eq!(e.host_pid().await, host, "the host itself survived");

    // host crash → the supervisor restarts it, frames resume, the engine never noticed
    kill9(host);
    let t0 = Instant::now();
    let (mut t_idle, mut t_start) = (None, None);
    let host2 = loop {
        let q = e.web().await;
        if q.get_path("host.state").and_then(Value::as_str) == Some("ready")
            && let Some(p) = q.get_path("host.pid").and_then(Value::as_i64).filter(|p| *p as u32 != host)
        {
            break p;
        }
        if q.get_path("host.state").and_then(Value::as_str) == Some("idle") && t_idle.is_none() {
            t_idle = Some(t0.elapsed());
        }
        if q.get_path("host.state").and_then(Value::as_str) == Some("starting") && t_start.is_none() {
            t_start = Some(t0.elapsed());
        }
        assert!(t0.elapsed() < WAIT, "no new host: {q:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    println!("host restart timeline: exit noticed ≤{t_idle:?}, respawned ≤{t_start:?}, ready {:?}", t0.elapsed());
    while r.fresh().is_some() {}
    let f = next_frame(&mut r, "frames after the host restart", |f| f.center() == GREEN && f.width == 480).await;
    println!("host {host} killed → new host {host2}, frames again after {:?} (seq {})", t0.elapsed(), f.seq);
    let restarts = e.web().await.get_path("host.restarts").and_then(Value::as_i64);
    assert_eq!(restarts, Some(1));
    assert!(e.hub.snapshot.load().tick > core_tick, "the core kept ticking");
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(descendants(host).is_empty(), "no orphaned CEF children of the killed host");
}

const TONE: &str = r##"<!doctype html><html><body><script>
const ctx = new AudioContext();
const osc = ctx.createOscillator(); osc.frequency.value = 440;
const gain = ctx.createGain(); gain.gain.value = 0.5;
osc.connect(gain).connect(ctx.destination); osc.start();
</script></body></html>"##;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "needs the CEF runtime: scripts/install-cef.sh, then cargo test -p se-web -- --ignored"]
async fn delivers_webaudio_into_the_audio_slot() {
    let e = Engine::start("[canvas.wide]\nnodes = []", &[("tone", "kind = \"web\"\nsize = [64, 64]", TONE)]).await;
    let mut stream = e.until("audio slot patch.tone", |e| e.hub.audio.take_new().into_iter().find(|(n, _)| n == "patch.tone").map(|(_, s)| s)).await;
    assert_eq!((stream.channels, stream.rate), (2, 48_000));
    let t0 = Instant::now();
    let mut samples: Vec<f32> = Vec::new();
    // wait for the oscillator to start, then measure 2 s
    let mut started = None;
    while t0.elapsed() < WAIT {
        while let Ok(s) = stream.consumer.pop() {
            if started.is_none() && s.abs() > 0.01 {
                started = Some(Instant::now());
            }
            if started.is_some() {
                samples.push(s);
            }
        }
        if started.is_some_and(|t| t.elapsed() > Duration::from_secs(2)) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let secs = started.expect("no audible samples arrived").elapsed().as_secs_f64();
    let rms = (samples.iter().map(|x| (x * x) as f64).sum::<f64>() / samples.len() as f64).sqrt();
    let peak = samples.iter().fold(0f32, |m, x| m.max(x.abs()));
    let rate = samples.len() as f64 / 2.0 / secs;
    println!("{} samples in {secs:.2} s ≈ {rate:.0} frames/s, rms {rms:.3}, peak {peak:.3}", samples.len());
    assert!(peak > 0.3 && rms > 0.2, "0.5-gain sine expected (rms ≈ 0.35): rms {rms}, peak {peak}");
    let lr = samples.as_chunks::<2>().0.iter().map(|c| (c[0] - c[1]).abs()).fold(0f32, f32::max);
    assert!(lr < 1e-3, "mono oscillator → identical L/R, max diff {lr}");
    assert!((rate - 48_000.0).abs() < 48_000.0 * 0.25, "≈48 kHz stereo expected, got {rate:.0} frames/s");
}
