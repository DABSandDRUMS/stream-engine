//! The engine subsystem on a real hub + core thread: actions, events, audio in the `tts` slot,
//! skip fade, deletion sync, the user toggle, queue limits, model fetch, and config hot reload
//! with the real model (skipped with a message when it isn't installed), plus the failing
//! health / rejected requests without a model.

use se_core::{Config, Core, SourceFile};
use se_hub::{Bus, EngineCtx, Hub, RunnerHooks};
use se_proto::{Command, Event, Op, Origin, Value};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;

fn model_dir() -> Option<PathBuf> {
    let dir = std::env::var_os("SE_TTS_MODEL_DIR").map(PathBuf::from).unwrap_or_else(|| {
        let data = std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/share"));
        data.join("stream-engine/models/kokoro")
    });
    if dir.join(se_tts::config::DEFAULT_MODEL).is_file() && dir.join("voices/af_heart.bin").is_file() {
        return Some(dir);
    }
    let msg = format!("Kokoro model not found in {} — run scripts/fetch-tts-model.sh", dir.display());
    assert!(std::env::var_os("SE_TTS_REQUIRE_MODEL").is_none(), "{msg}");
    eprintln!("SKIPPED: {msg}");
    None
}

fn config(tts: &str) -> Arc<Config> {
    let table: toml::Table = format!("schema = 1\n{tts}").parse().unwrap();
    Arc::new(Config::build(&[SourceFile { kind: "project".into(), name: "project".into(), path: "project.toml".into(), table }]))
}

struct Rig {
    hub: Arc<Hub>,
    bus: broadcast::Receiver<Arc<Bus>>,
    cfg: tokio::sync::watch::Sender<Arc<Config>>,
    audio: Arc<parking_lot::Mutex<Vec<f32>>>,
    stop: Arc<AtomicBool>,
    _dir: tempfile::TempDir,
}

impl Rig {
    fn say(&self, args: Value) {
        self.hub.command(Command::new(Origin::Cli, Op::Action { name: "tts.say".into(), args }));
    }
    fn action(&self, name: &str, args: Value) {
        self.hub.command(Command::new(Origin::Cli, Op::Action { name: name.into(), args }));
    }
    /// Next event of `ty` with payload `id` (other bus traffic is skipped).
    async fn event(&mut self, ty: &str, id: &str, within: Duration) -> Value {
        let deadline = Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let msg = tokio::time::timeout(left, self.bus.recv()).await.unwrap_or_else(|_| panic!("no {ty} {id} within {within:?}")).unwrap();
            if let Bus::Event(e) = &*msg
                && e.ty == ty
                && e.payload.get_path("id").and_then(Value::as_str) == Some(id)
            {
                return e.payload.clone();
            }
        }
    }
    async fn query(&self) -> Value {
        self.hub.query("tts", Value::Null).await.unwrap()
    }
}

async fn rig(dir: &std::path::Path, extra: &str, wait_ready: bool) -> Rig {
    let clock = Arc::new(se_clock::Clock::new());
    let (hub, rx) = Hub::new(clock);
    let cfg = config(&format!("[tts]\nmodel_dir = \"{}\"\n{extra}", dir.display()));
    let core = Core::new((*cfg).clone(), se_clock::now());
    let h = hub.clone();
    std::thread::spawn(move || se_hub::run_core(core, h, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 1, on_runtime: None }));
    let tmp = tempfile::tempdir().unwrap();
    let (tx, crx) = tokio::sync::watch::channel(cfg);
    let ctx = EngineCtx {
        hub: hub.clone(),
        db: se_store::Db::open(&tmp.path().join("runtime.db")).unwrap(),
        project_root: tmp.path().to_path_buf(),
        data_dir: tmp.path().to_path_buf(),
        share_dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../.."),
        config: crx,
        http: "127.0.0.1:0".parse().unwrap(),
        dev: true,
    };
    let bus = hub.subscribe();
    se_tts::start(ctx).await.unwrap();
    // the audio graph: drain the `tts` slot in real time (10 ms blocks) and keep what it hears
    let (name, mut stream) = hub.audio.take_new().into_iter().find(|(n, _)| n == "tts").expect("tts slot registered");
    assert_eq!((name.as_str(), stream.channels, stream.rate), ("tts", 1, 48_000));
    let audio = Arc::new(parking_lot::Mutex::new(Vec::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let (a, s) = (audio.clone(), stop.clone());
    std::thread::spawn(move || {
        let t0 = Instant::now();
        let mut blocks = 0u64;
        while !s.load(Ordering::Relaxed) {
            blocks += 1;
            let mut out = a.lock();
            for _ in 0..480 {
                out.push(stream.consumer.pop().unwrap_or(0.0));
            }
            drop(out);
            let next = t0 + Duration::from_millis(10 * blocks);
            std::thread::sleep(next.saturating_duration_since(Instant::now()));
        }
    });
    let r = Rig { hub, bus, cfg: tx, audio, stop, _dir: tmp };
    let t = Instant::now();
    while wait_ready && !r.query().await.get_path("ready").is_some_and(Value::truthy) {
        assert!(t.elapsed() < Duration::from_secs(60), "model never became ready: {:?}", r.query().await);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    r
}

fn rms(x: &[f32]) -> f64 {
    (x.iter().map(|v| (*v as f64).powi(2)).sum::<f64>() / x.len().max(1) as f64).sqrt()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_subsystem_end_to_end() {
    let Some(dir) = model_dir() else { return };
    let mut r =
        rig(&dir, "max_queue = 3\nvolume = 1.0\n[[tts.voices]]\nwhen = \"kind == 'twitch.cheer' && amount >= 1000\"\nvoice = \"am_michael\"", true).await;

    // --- say → started → audible audio → finished
    r.audio.lock().clear();
    r.say(
        Value::map()
            .with("text", "Thanks for the five hundred bits, let's go!")
            .with("id", "a1")
            .with("user", "drumfan")
            .with("kind", "twitch.cheer")
            .with("amount", 500),
    );
    let started = r.event("tts.started", "a1", Duration::from_secs(30)).await;
    let dur_ms = started.get_path("duration_ms").and_then(Value::as_i64).unwrap();
    assert!((1500..6000).contains(&dur_ms), "{started:?}");
    assert_eq!(started.get_path("voice").and_then(Value::as_str), Some("af_heart"), "amount < 1000 → default voice");
    assert_eq!(r.hub.snapshot.load().get("tts.current.user").and_then(Value::as_str), Some("drumfan"));
    let t0 = Instant::now();
    let fin = r.event("tts.finished", "a1", Duration::from_secs(15)).await;
    let heard = t0.elapsed().as_millis() as i64;
    assert_eq!(fin.get_path("skipped"), Some(&Value::Bool(false)));
    assert!((heard - dur_ms).abs() < 400, "finished {heard} ms after start for {dur_ms} ms of audio");
    let played = r.audio.lock().clone();
    let loud = played.iter().filter(|v| v.abs() > 0.01).count() as f64 / 48_000.0;
    println!("a1: {dur_ms} ms synthesized, {heard} ms until finished, {loud:.2} s audible in the slot, RMS {:.4}", rms(&played));
    assert!(loud > dur_ms as f64 / 1000.0 * 0.4, "audio reached the slot: {loud:.2} s");

    // --- rule voice + skip with a click-free fade
    r.audio.lock().clear();
    r.say(
        Value::map()
            .with("text", "This is a much longer message that keeps going for a while so that it can be skipped halfway through by a moderator.")
            .with("id", "b1")
            .with("user", "bigcheer")
            .with("kind", "twitch.cheer")
            .with("amount", 5000),
    );
    let started = r.event("tts.started", "b1", Duration::from_secs(30)).await;
    assert_eq!(started.get_path("voice").and_then(Value::as_str), Some("am_michael"), "rule matched");
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let t_skip = Instant::now();
    r.action("tts.skip", Value::Null);
    let fin = r.event("tts.finished", "b1", Duration::from_secs(5)).await;
    let latency = t_skip.elapsed();
    assert_eq!(fin.get_path("skipped"), Some(&Value::Bool(true)));
    assert!(latency < Duration::from_millis(400), "skip latency {latency:?}");
    tokio::time::sleep(Duration::from_millis(300)).await;
    let played = r.audio.lock().clone();
    let last = played.iter().rposition(|v| v.abs() > 1e-4).expect("audio played");
    let tail = &played[last.saturating_sub(96)..=last];
    let peak = played.iter().fold(0f32, |m, v| m.max(v.abs()));
    let tail_peak = tail.iter().fold(0f32, |m, v| m.max(v.abs()));
    println!("b1: skip latency {latency:?}, speech peak {peak:.3}, last 2 ms peak {tail_peak:.4}");
    assert!(tail_peak < peak * 0.1, "faded out, no click: tail {tail_peak} vs peak {peak}");

    // --- deletion sync: a ban drops that user's queued items (by id and by name)
    r.say(Value::map().with("text", "First message keeps the player busy for a moment.").with("id", "c0").with("user", "friend"));
    r.say(Value::map().with("text", "spam one").with("id", "c1").with("user", "spammer").with("user_id", "42"));
    r.say(Value::map().with("text", "spam two").with("id", "c2").with("user", "SPAMMER"));
    r.event("tts.started", "c0", Duration::from_secs(30)).await;
    r.say(Value::map().with("text", "a friendly message").with("id", "c3").with("user", "friend2"));
    r.hub.emit(Event::new("twitch.user.purge", Origin::Twitch, Value::map().with("user_id", "42").with("user", "spammer")));
    for id in ["c1", "c2"] {
        let f = r.event("tts.finished", id, Duration::from_secs(5)).await;
        assert_eq!(f.get_path("skipped"), Some(&Value::Bool(true)), "{id}");
        assert_eq!(f.get_path("error").and_then(Value::as_str), Some("user banned or timed out"));
    }
    let queue = r.query().await.get_path("queue").cloned().unwrap();
    let ids: Vec<&str> = queue.as_list().unwrap().iter().filter_map(|i| i.get_path("id").and_then(Value::as_str)).collect();
    assert_eq!(ids, ["c3"]);
    // drop one queued item by id, then chat clear leaves the current one playing
    r.action("tts.skip", Value::map().with("id", "c3"));
    r.event("tts.finished", "c3", Duration::from_secs(5)).await;
    assert_eq!(r.query().await.get_path("current.id").and_then(Value::as_str), Some("c0"));

    // --- queue limit (max_queue = 3): extra requests are rejected with a reason
    for i in 0..6 {
        r.say(Value::map().with("text", format!("queued message number {i}")).with("id", format!("q{i}")));
    }
    let f = r.event("tts.finished", "q5", Duration::from_secs(5)).await;
    assert!(f.get_path("error").and_then(Value::as_str).is_some_and(|e| e.starts_with("queue full")), "{f:?}");

    // --- the user toggle stops and clears everything, and rejects new requests
    r.hub.command(Command::new(Origin::Ui, Op::Set { address: "tts.enabled".into(), value: Value::Bool(false) }));
    r.event("tts.finished", "q0", Duration::from_secs(5)).await;
    let t = Instant::now();
    loop {
        let q = r.query().await;
        if q.get_path("current").is_some_and(Value::is_null) && q.get_path("queue").and_then(Value::as_list).is_some_and(<[Value]>::is_empty) {
            assert_eq!(q.get_path("enabled"), Some(&Value::Bool(false)));
            break;
        }
        assert!(t.elapsed() < Duration::from_secs(3), "not cleared: {q:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    r.say(Value::map().with("text", "hello").with("id", "d1"));
    let f = r.event("tts.finished", "d1", Duration::from_secs(5)).await;
    assert!(f.get_path("error").and_then(Value::as_str).is_some_and(|e| e.contains("tts.enabled")), "{f:?}");
    r.hub.command(Command::new(Origin::Ui, Op::Set { address: "tts.enabled".into(), value: Value::Bool(true) }));

    // --- model fetch runs the pinned script (here: everything verifies, nothing downloads)
    r.action("tts.model.fetch", Value::Null);
    let deadline = Instant::now() + Duration::from_secs(120);
    loop {
        let msg = tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), r.bus.recv()).await.expect("fetch finished").unwrap();
        if let Bus::Log { target, msg, level, .. } = &*msg
            && target == "tts"
            && (msg.starts_with("Kokoro model files verified") || msg.starts_with("tts.model.fetch failed"))
        {
            assert_eq!(level, "info", "{msg}");
            break;
        }
    }
    let status = r.query().await.get_path("model_status").and_then(Value::as_str).unwrap_or_default().to_string();
    assert!(status.starts_with("loaded model_quantized.onnx"), "{status}");

    // --- hot reload: a broken [tts] is reported and the last good settings stay
    let good_voice = r.query().await.get_path("voice").cloned();
    r.cfg.send(config(&format!("[tts]\nmodel_dir = \"{}\"\nvoice = \"bf_emma\"\nspeed = 9", dir.display()))).unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let msg = tokio::time::timeout(deadline.saturating_duration_since(Instant::now()), r.bus.recv()).await.expect("error logged").unwrap();
        if let Bus::Log { level, target, msg, .. } = &*msg
            && level == "error"
            && target == "tts"
        {
            assert!(msg.contains("speed"), "{msg}");
            break;
        }
    }
    assert_eq!(r.query().await.get_path("voice").cloned(), good_voice);
    r.cfg.send(config(&format!("[tts]\nmodel_dir = \"{}\"\nvoice = \"bf_emma\"", dir.display()))).unwrap();
    let t = Instant::now();
    while r.query().await.get_path("voice").and_then(Value::as_str) != Some("bf_emma") {
        assert!(t.elapsed() < Duration::from_secs(5), "config applied");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    r.stop.store(true, Ordering::Relaxed);
    r.hub.shutdown();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn missing_model_fails_health_and_rejects_requests() {
    let empty = tempfile::tempdir().unwrap();
    let mut r = rig(empty.path(), "", false).await;
    let t = Instant::now();
    let health = loop {
        let h = r.hub.snapshot.load().get("health.tts").cloned();
        if let Some(h) = h.clone().filter(|h| h.get_path("status").and_then(Value::as_str) == Some("fail")) {
            break h;
        }
        assert!(t.elapsed() < Duration::from_secs(10), "no failing health: {h:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    };
    let detail = health.get_path("detail").and_then(Value::as_str).unwrap();
    assert!(detail.contains("missing") && detail.contains("tts.model.fetch"), "{detail}");
    let q = r.query().await;
    assert_eq!(q.get_path("ready"), Some(&Value::Bool(false)));
    assert!(q.get_path("model_status").and_then(Value::as_str).is_some_and(|s| s.starts_with("error:")), "{q:?}");
    r.say(Value::map().with("text", "hello").with("id", "x1"));
    let f = r.event("tts.finished", "x1", Duration::from_secs(5)).await;
    assert_eq!(f.get_path("skipped"), Some(&Value::Bool(true)));
    assert!(f.get_path("error").and_then(Value::as_str).is_some_and(|e| e.starts_with("not ready")), "{f:?}");
    r.stop.store(true, Ordering::Relaxed);
    r.hub.shutdown();
}
