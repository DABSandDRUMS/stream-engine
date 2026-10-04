//! se-obs against a fake OBS plugin (JSON lines over obs.sock) and a real hub + core.

use se_core::{Config, Core, SourceFile};
use se_hub::{Bus, EngineCtx, Hub, RunnerHooks, run_core};
use se_proto::{Command, Op, Origin, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::{broadcast, mpsc, watch};

fn project(obs_section: &str) -> Config {
    let table: toml::Table = format!("schema = 1\n{obs_section}").parse().unwrap();
    Config::build(&[SourceFile { kind: "project".into(), name: "project".into(), path: "project.toml".into(), table }])
}

struct Engine {
    hub: Arc<Hub>,
    cfg_tx: watch::Sender<Arc<Config>>,
    sock: std::path::PathBuf,
    session: mpsc::UnboundedReceiver<Command>,
    bus: broadcast::Receiver<Arc<Bus>>,
    _dir: tempfile::TempDir,
    core: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.hub.shutdown();
        if let Some(c) = self.core.take() {
            let _ = c.join();
        }
    }
}

fn obs_section(sock: &Path, extra: &str) -> String {
    format!("[obs]\nsocket = \"{}\"\nstale_ms = 300\nfallback_mode = \"always\"\nfallback_scene = \"BRB Tech\"\ncommand_timeout = 800\n{extra}", sock.display())
}

async fn engine() -> Engine {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("run/obs.sock");
    let cfg = Arc::new(project(&obs_section(&sock, "")));
    let clock = Arc::new(se_clock::Clock::new());
    let (hub, rx) = Hub::new(clock);
    let core = Core::new((*cfg).clone(), se_clock::now());
    let h2 = hub.clone();
    let core = std::thread::spawn(move || run_core(core, h2, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 1, on_runtime: None }));
    let session = hub.route_actions("session");
    let bus = hub.subscribe();
    let (cfg_tx, cfg_rx) = watch::channel(cfg);
    let ctx = EngineCtx {
        hub: hub.clone(),
        db: se_store::Db::memory().unwrap(),
        project_root: dir.path().to_path_buf(),
        data_dir: dir.path().join("data"),
        share_dir: dir.path().to_path_buf(),
        config: cfg_rx,
        http: "127.0.0.1:0".parse().unwrap(),
        dev: true,
    };
    se_obs::start(ctx).await.unwrap();
    Engine { hub, cfg_tx, sock, session, bus, _dir: dir, core: Some(core) }
}

async fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < Duration::from_secs(5), "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

impl Engine {
    fn get(&self, a: &str) -> Value {
        self.hub.snapshot.load().get(a).cloned().unwrap_or(Value::Null)
    }

    async fn state(&self, a: &str, want: impl Into<Value>) {
        let want = want.into();
        let t0 = Instant::now();
        loop {
            let v = self.get(a);
            if v == want {
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "{a} = {v:?}, want {want:?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn health(&self) -> (String, String) {
        let v = self.get("health.obs");
        (v.get_path("status").and_then(Value::as_str).unwrap_or("").to_string(), v.get_path("detail").and_then(Value::as_str).unwrap_or("").to_string())
    }

    async fn health_is(&self, status: &str, detail_contains: &str) {
        let t0 = Instant::now();
        loop {
            let (s, d) = self.health();
            if s == status && d.contains(detail_contains) {
                return;
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "health.obs = {s}: {d}; want {status} containing {detail_contains:?}");
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// Next `session.meta` action for `key`.
    async fn meta(&mut self, key: &str) -> Value {
        loop {
            let c = tokio::time::timeout(Duration::from_secs(5), self.session.recv()).await.expect("session.meta").unwrap();
            if let Op::Action { name, args } = &c.op
                && name == "session.meta"
                && args.get_path("key").and_then(Value::as_str) == Some(key)
            {
                return args.get_path("value").cloned().unwrap();
            }
        }
    }

    async fn event(&mut self, ty: &str) -> se_proto::Event {
        let t0 = Instant::now();
        loop {
            let left = Duration::from_secs(5).saturating_sub(t0.elapsed());
            match tokio::time::timeout(left, self.bus.recv()).await.expect("event") {
                Ok(b) => {
                    if let Bus::Event(e) = &*b
                        && e.ty == ty
                    {
                        return e.clone();
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => panic!("bus: {e}"),
            }
        }
    }

    fn action(&self, name: &str) {
        self.hub.command(Command::new(Origin::Cli, Op::Action { name: name.into(), args: Value::Null }));
    }

    async fn log_containing(&mut self, needle: &str) -> String {
        let t0 = Instant::now();
        loop {
            let left = Duration::from_secs(5).saturating_sub(t0.elapsed());
            match tokio::time::timeout(left, self.bus.recv()).await.unwrap_or_else(|_| panic!("no log containing {needle:?}")) {
                Ok(b) => {
                    if let Bus::Log { msg, .. } = &*b
                        && msg.contains(needle)
                    {
                        return msg.clone();
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(e) => panic!("bus: {e}"),
            }
        }
    }
}

struct Plugin {
    rd: BufReader<OwnedReadHalf>,
    wr: OwnedWriteHalf,
}

impl Plugin {
    async fn connect(path: &Path) -> Plugin {
        let t0 = Instant::now();
        loop {
            if let Ok(s) = UnixStream::connect(path).await {
                let (rd, wr) = s.into_split();
                return Plugin { rd: BufReader::new(rd), wr };
            }
            assert!(t0.elapsed() < Duration::from_secs(5), "obs.sock never appeared");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn send(&mut self, v: serde_json::Value) {
        let mut s = v.to_string();
        s.push('\n');
        self.wr.write_all(s.as_bytes()).await.unwrap();
    }

    async fn recv(&mut self) -> serde_json::Value {
        let mut line = String::new();
        let n = tokio::time::timeout(Duration::from_secs(5), self.rd.read_line(&mut line)).await.expect("line from engine").unwrap();
        assert!(n > 0, "engine closed the connection");
        serde_json::from_str(&line).unwrap()
    }

    async fn hello(&mut self) -> serde_json::Value {
        self.send(serde_json::json!({"t":"hello","obs":"32.2.2","plugin":"0.1.0","canvases":["wide","tall"],"pid":42})).await;
        self.recv().await
    }
}

fn status(extra: serde_json::Value) -> serde_json::Value {
    let now = se_clock::now();
    let mut s = serde_json::json!({
        "t": "status", "streaming": false, "kbps": 0.0, "dropped": 0, "total": 0,
        "lag_ms": 0.0, "fps": 60.0, "obs_ns": now + 5_000, "mono_ns": now,
        "stale": {"wide": false, "tall": false}, "sources": {"wide": 1, "tall": 1},
        "scene": "Scene", "outputs": []
    });
    for (k, v) in extra.as_object().unwrap() {
        s[k] = v.clone();
    }
    s
}

#[tokio::test(flavor = "multi_thread")]
async fn hello_status_health_and_disconnect() {
    let e = engine().await;
    e.state("obs.link", false).await;
    e.health_is("fail", "not").await;

    let mut p = Plugin::connect(&e.sock).await;
    let cfg = p.hello().await;
    assert_eq!(cfg["t"], "config");
    assert_eq!(cfg["stale_ms"], 300);
    assert_eq!(cfg["fallback_mode"], "always");
    assert_eq!(cfg["fallback_scene"], "BRB Tech");
    e.state("obs.link", true).await;
    e.state("obs.version", "32.2.2").await;
    e.state("obs.plugin.installed", true).await;

    // only wide is placed in OBS; tall has no source yet
    p.send(status(serde_json::json!({"streaming": true, "kbps": 6123.45, "dropped": 3, "total": 3600, "lag_ms": 16.7,
        "stale": {"wide": false, "tall": true}, "sources": {"wide": 1, "tall": 0}, "fallback": false,
        "outputs": [{"name":"simple_stream","id":"rtmp_output","kind":"stream","active":true,"kbps":6123.45,"dropped":3,"total":3600,"canvas":"wide"},
                    {"name":"Aitum Vertical Stream","id":"rtmp_output","kind":"stream","active":true,"kbps":4000.0,"dropped":0,"total":3500,"canvas":"tall"}]})))
    .await;
    e.state("obs.stream.active", true).await;
    e.state("obs.stream.kbps", 6123.5).await;
    e.state("obs.stream.dropped", 3i64).await;
    e.state("obs.stream.lag_ms", 16.7).await;
    e.state("obs.stale.wide", false).await;
    e.state("obs.stale.tall", true).await;
    e.state("obs.fps", 60.0).await;
    e.state("obs.output.aitum_vertical_stream.active", true).await;
    e.state("obs.output.aitum_vertical_stream.canvas", "tall").await;
    e.state("obs.output.aitum_vertical_stream.label", "Aitum Vertical Stream").await;
    e.health_is("warn", "no `stream-engine: tall` source").await;

    // tall placed but stale → fail; both fresh → pass
    p.send(status(serde_json::json!({"stale": {"wide": false, "tall": true}, "sources": {"wide": 1, "tall": 1}}))).await;
    e.health_is("fail", "tall canvas: no frames").await;
    p.send(status(serde_json::json!({}))).await;
    e.health_is("pass", "receiving wide + tall").await;
    e.state("obs.stale.tall", false).await;

    // OBS goes away: nothing is confirmed live any more
    drop(p);
    e.state("obs.link", false).await;
    e.state("obs.stream.active", false).await;
    e.state("obs.stale.wide", true).await;
    e.state("obs.output.aitum_vertical_stream.active", false).await;
    e.health_is("fail", "not connected").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn actions_reach_obs_and_replies_are_reported() {
    let mut e = engine().await;
    // no plugin: the action fails loudly
    e.action("obs.stream.start");
    let m = e.log_containing("obs.stream.start failed").await;
    assert!(m.contains("not connected"), "{m}");

    let mut p = Plugin::connect(&e.sock).await;
    p.hello().await;
    e.state("obs.link", true).await;

    for (action, op) in [("obs.stream.start", "stream.start"), ("obs.stream.stop", "stream.stop"), ("obs.fallback.on", "fallback.on"), ("obs.setup", "setup")] {
        e.action(action);
        let cmd = p.recv().await;
        assert_eq!(cmd["t"], "cmd");
        assert_eq!(cmd["op"], op);
        let id = cmd["id"].as_u64().unwrap();
        if op == "fallback.on" {
            p.send(serde_json::json!({"t":"reply","id":id,"ok":false,"error":"no canvas program shows a stream-engine source"})).await;
            let m = e.log_containing("obs.fallback.on failed").await;
            assert!(m.contains("no canvas program"), "{m}");
        } else {
            p.send(serde_json::json!({"t":"reply","id":id,"ok":true,"error":null,"result":"starting"})).await;
            e.log_containing(&format!("{action}: \"starting\"")).await;
        }
    }

    // unanswered command times out (command_timeout = 800 ms)
    e.action("obs.stream.start");
    let cmd = p.recv().await;
    assert_eq!(cmd["op"], "stream.start");
    let m = e.log_containing("obs.stream.start failed").await;
    assert!(m.contains("did not answer"), "{m}");

    // disconnect while a command is pending
    e.action("obs.stream.stop");
    let _ = p.recv().await;
    drop(p);
    let m = e.log_containing("obs.stream.stop failed").await;
    assert!(m.contains("disconnected"), "{m}");
}

#[tokio::test(flavor = "multi_thread")]
async fn stream_events_session_meta_and_clock() {
    let mut e = engine().await;
    let mut p = Plugin::connect(&e.sock).await;
    p.hello().await;
    e.state("obs.link", true).await;

    let now = se_clock::now();
    let off: u64 = 7_000; // OBS clock runs 7 µs ahead of CLOCK_MONOTONIC in this fake
    p.send(status(serde_json::json!({"stream_start_ns": now - 500_000_000 + off,
        "streaming": true, "obs_ns": now + off, "mono_ns": now})))
        .await;
    let clock = e.meta("clock").await;
    assert!(clock.get_path("obs_stream.valid").unwrap().truthy());
    let maps = e.hub.clock.mappings();
    assert_eq!(maps.obs_stream.to_other(now), Some(500_000_000));
    assert_eq!(maps.obs_stream.to_master(0), Some(now - 500_000_000));

    // events → engine events with master-clock timestamps
    p.send(serde_json::json!({"t":"event","name":"stream_stopped","obs_ns": now + off + 1_000, "mono_ns": now + 1_000})).await;
    let ev = e.event("obs.stream_stopped").await;
    assert_eq!(ev.ts, now + 1_000);
    e.state("obs.stream.active", false).await;

    p.send(serde_json::json!({"t":"event","name":"scene_fallback","canvas":"Main","scene":"BRB Tech","from":"Scene","canvases":["wide"],"reason":"stale","obs_ns": now + off, "mono_ns": now})).await;
    let ev = e.event("obs.fallback").await;
    assert_eq!(ev.payload.get_path("active"), Some(&Value::Bool(true)));
    assert_eq!(ev.payload.get_path("from").unwrap().as_str(), Some("Scene"));
    e.state("obs.fallback.active", true).await;
    p.send(serde_json::json!({"t":"event","name":"stream_started","obs_ns": now + off, "mono_ns": now})).await;
    e.event("obs.stream_started").await;

    // A new session carries only the stream clock; OBS never owns recording metadata.
    e.hub.emit(se_proto::Event::new("session.closed", Origin::System, Value::map()));
    let c = tokio::time::timeout(Duration::from_secs(5), e.session.recv()).await.unwrap().unwrap();
    let Op::Action { name, args } = c.op else { panic!("not a session action") };
    assert_eq!(name, "session.meta");
    assert_eq!(args.get_path("key").and_then(Value::as_str), Some("clock"));
    assert!(args.get_path("value.obs_stream.valid").unwrap().truthy());
}

#[tokio::test(flavor = "multi_thread")]
async fn second_plugin_rejected_config_hot_reload_and_bad_lines() {
    let e = engine().await;
    let mut p = Plugin::connect(&e.sock).await;
    p.hello().await;
    e.state("obs.link", true).await;

    // a second OBS while the first is alive is refused
    let mut q = Plugin::connect(&e.sock).await;
    let err = q.recv().await;
    assert_eq!(err["t"], "error");
    assert!(err["error"].as_str().unwrap().contains("already connected"));

    // junk and unknown message types do not drop the link
    p.wr.write_all(b"not json\n{\"t\":\"from_the_future\"}\n{\"no_t\":1}\n").await.unwrap();
    p.send(status(serde_json::json!({"fps": 59.9}))).await;
    e.state("obs.fps", 59.9).await;
    e.state("obs.link", true).await;

    // hot reload pushes the new settings; a broken [obs] keeps the last good ones
    let sock = e.sock.clone();
    e.cfg_tx.send(Arc::new(project(&obs_section(&sock, "fallback_text = \"brb\"").replace("stale_ms = 300", "stale_ms = 900")))).unwrap();
    let c = p.recv().await;
    assert_eq!(c["t"], "config");
    assert_eq!(c["stale_ms"], 900);
    assert_eq!(c["fallback_text"], "brb");
    e.cfg_tx.send(Arc::new(project(&obs_section(&sock, "").replace("fallback_mode = \"always\"", "fallback_mode = \"sometimes\"")))).unwrap();
    e.cfg_tx.send(Arc::new(project(&obs_section(&sock, "")))).unwrap();
    let c = p.recv().await;
    assert_eq!(c["stale_ms"], 300, "the valid config after the broken one is applied");
}

#[tokio::test(flavor = "multi_thread")]
async fn rehearsal_never_starts_the_stream() {
    let mut e = engine().await;
    let mut p = Plugin::connect(&e.sock).await;
    p.hello().await;
    e.state("obs.link", true).await;
    e.hub.command(Command::new(Origin::Cli, Op::ModeSet { mode: "rehearsal".into() }));
    e.state("show.mode", "rehearsal").await;

    e.action("obs.stream.start");
    let ev = e.event("obs.dry_run").await;
    assert_eq!(ev.payload.get_path("action").and_then(Value::as_str), Some("obs.stream.start"));
    // Fallback remains available off-air and is the first command OBS receives.
    e.action("obs.fallback.on");
    assert_eq!(p.recv().await["op"], "fallback.on");

    // "Go live" from rehearsal: the mode change sent just before stream.start counts
    e.hub.command(Command::new(Origin::Cli, Op::ModeSet { mode: "preshow".into() }));
    e.action("obs.stream.start");
    assert_eq!(p.recv().await["op"], "stream.start");
}

#[tokio::test(flavor = "multi_thread")]
async fn stale_socket_file_is_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let sock = dir.path().join("obs.sock");
    // leftover from a crashed engine: a socket file nobody listens on
    drop(std::os::unix::net::UnixListener::bind(&sock).unwrap());
    assert!(sock.exists());
    let e = engine().await;
    let s2 = sock.clone();
    let section = obs_section(&s2, "");
    e.cfg_tx.send(Arc::new(project(&section))).unwrap();
    let mut p = Plugin::connect(&sock).await;
    p.hello().await;
    e.state("obs.link", true).await;
    wait_for("old socket removed", || !e.sock.exists()).await;
}
