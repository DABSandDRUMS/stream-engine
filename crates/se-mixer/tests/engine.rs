//! The mixer subsystem against the real core + hub and a console simulator replaying the
//! recorded 16R traffic.

mod support;

use parking_lot::Mutex;
use se_core::{Config, Core, SourceFile};
use se_hub::{Bus, EngineCtx, Hub, RunnerHooks, run_core};
use se_proto::{Command, Event, Op, Origin, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};
use support::FakeConsole;

struct Engine {
    hub: Arc<Hub>,
    events: Arc<Mutex<Vec<Event>>>,
    dir: tempfile::TempDir,
    _cfg_tx: tokio::sync::watch::Sender<Arc<Config>>,
    core: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.hub.shutdown();
        if let Some(t) = self.core.take() {
            let _ = t.join();
        }
    }
}

fn file(kind: &str, name: &str, toml: &str) -> SourceFile {
    SourceFile {
        kind: kind.into(),
        name: name.into(),
        path: format!("{kind}/{name}.toml"),
        table: toml.parse().unwrap_or_else(|e| panic!("{kind}/{name}: {e}")),
    }
}

const WRITABLE: &str = "writable = [\"ch.14.*\", \"ch.15.*\", \"ch.16.*\", \"aux.*.ch.16.send\"]";

async fn engine(console: &FakeConsole, mixer_extra: &str, extra: Vec<SourceFile>) -> Engine {
    engine_full(console, "", mixer_extra, extra).await
}

/// `top` goes before `[mixer]` in project.toml (e.g. a `[safety]` table).
async fn engine_full(console: &FakeConsole, top: &str, mixer_extra: &str, extra: Vec<SourceFile>) -> Engine {
    let project = format!("schema = 1\ntick_hz = 200\n{top}\n[mixer]\nhost = \"{}\"\n{mixer_extra}\n", console.addr);
    let mut files = vec![file("project", "project", &project)];
    files.extend(extra);
    let cfg = Config::build(&files);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let clock = Arc::new(se_clock::Clock::new());
    let (hub, rx) = Hub::new(clock);
    let core = Core::new(cfg.clone(), se_clock::now());
    let h2 = hub.clone();
    let core = std::thread::spawn(move || run_core(core, h2, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 1, on_runtime: None }));
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("project.toml"), &project).unwrap();
    std::fs::create_dir(dir.path().join("mixes")).unwrap();
    let db = se_store::Db::open(&dir.path().join("engine.db")).unwrap();
    let (cfg_tx, cfg_rx) = tokio::sync::watch::channel(Arc::new(cfg));
    let events = Arc::new(Mutex::new(Vec::new()));
    let mut bus = hub.subscribe();
    let ev2 = events.clone();
    tokio::spawn(async move {
        while let Ok(m) = bus.recv().await {
            if let Bus::Event(e) = &*m {
                ev2.lock().push(e.clone());
            }
        }
    });
    let ctx = EngineCtx {
        hub: hub.clone(),
        db,
        project_root: dir.path().to_path_buf(),
        data_dir: dir.path().to_path_buf(),
        share_dir: dir.path().to_path_buf(),
        config: cfg_rx,
        http: "127.0.0.1:0".parse().unwrap(),
        dev: true,
    };
    se_mixer::start(ctx).await.unwrap();
    let e = Engine { hub, events, dir, _cfg_tx: cfg_tx, core: Some(core) };
    e.wait("connected", || e.state("mixer.16r.connected") == Some(Value::Bool(true))).await;
    e
}

impl Engine {
    fn state(&self, a: &str) -> Option<Value> {
        self.hub.snapshot.load().get(a).cloned()
    }
    fn f(&self, a: &str) -> f64 {
        self.state(a).and_then(|v| v.as_f64()).unwrap_or(f64::NAN)
    }
    fn signal(&self, n: &str) -> Option<f32> {
        self.hub.snapshot.load().signal(n)
    }
    async fn wait(&self, what: &str, cond: impl Fn() -> bool) {
        let t0 = Instant::now();
        while !cond() {
            assert!(t0.elapsed() < Duration::from_secs(8), "timed out waiting for {what}");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    async fn exec(&self, origin: Origin, op: Op) -> Result<(), String> {
        self.hub.exec(Command::new(origin, op)).await
    }
    fn events_of(&self, ty: &str) -> Vec<Event> {
        self.events.lock().iter().filter(|e| e.ty == ty).cloned().collect()
    }
}

fn near(a: Option<f64>, b: f64) -> bool {
    a.is_some_and(|a| (a - b).abs() < 1e-6)
}

fn set(a: &str, v: impl Into<Value>) -> Op {
    Op::Set { address: a.into(), value: v.into() }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn connects_declares_measured_addresses_and_streams_meters() {
    let console = FakeConsole::start().await;
    let e = engine(&console, &format!("{WRITABLE}\n[mixer.talk]\nchannel = \"floor tom\"\nthreshold_db = -70\nattack = \"0ms\""), vec![]).await;
    assert_eq!(e.state("mixer.16r.model"), Some(Value::Str("StudioLive 16R".into())));
    assert_eq!(e.state("mixer.16r.firmware"), Some(Value::Str("3.2.0.108461".into())));
    assert_eq!(e.state("mixer.16r.channels"), Some(Value::Int(16)));
    assert_eq!(e.state("mixer.16r.auxes"), Some(Value::Int(6)));
    assert_eq!(e.state("mixer.16r.ch.1.name"), Some(Value::Str("Kick".into())));
    assert!((e.f("mixer.16r.ch.1.fader") - 0.7529).abs() < 1e-3);
    assert!(e.f("mixer.16r.ch.1.db") > 0.5 && e.f("mixer.16r.ch.1.db") < 2.0);
    assert!((e.f("mixer.16r.aux.1.ch.10.send") - 0.74).abs() < 0.01);
    assert_eq!(e.state("mixer.16r.ch.11.link"), Some(Value::Bool(true)));
    // only what the console has is declared
    assert!(e.state("mixer.16r.ch.17.fader").is_none());
    assert!(e.state("mixer.16r.aux.7.fader").is_none());
    assert!(e.state("mixer.16r.aux.1.ch.1.send").is_some());
    // metadata marks non-writable controls readonly
    let meta = e.hub.get("mixer.16r.ch.1.fader", true).await;
    assert!(meta[0].meta.as_ref().unwrap().readonly);
    let meta = e.hub.get("mixer.16r.ch.16.fader", true).await;
    assert!(!meta[0].meta.as_ref().unwrap().readonly);
    // meters → signals; "Floor Tom" (ch 5) is matched by its console label
    e.wait("meters", || e.signal("mixer.16r.meter.ch.6").is_some_and(|v| v > 0.0)).await;
    assert_eq!(e.signal("mixer.16r.meter.ch.16"), Some(0.0));
    let st = e.hub.query("mixer.status", Value::Null).await.unwrap();
    assert_eq!(st.get_path("talk_channel"), Some(&Value::Int(5)));
    assert_eq!(st.get_path("meters"), Some(&Value::Bool(true)));
    e.wait("health", || {
        e.state("health.mixer").and_then(|h| h.get_path("detail").and_then(|d| d.as_str().map(String::from))).is_some_and(|d| d.contains("StudioLive 16R"))
    })
    .await;
    let cov = e.hub.query("mixer.coverage", Value::Null).await.unwrap();
    assert!(
        cov.as_list()
            .unwrap()
            .iter()
            .any(|c| c.get_path("path") == Some(&Value::Str("line/ch16/volume".into())) && c.get_path("writable") == Some(&Value::Bool(true)))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mic_talking_follows_the_vocal_channel_meter() {
    let console = FakeConsole::start().await;
    // ch 6's input carries room noise around −59 dBFS in the capture
    let e = engine(&console, "[mixer.talk]\nchannel = 6\nthreshold_db = -70\nattack = \"0ms\"\nhold = \"50ms\"", vec![]).await;
    e.wait("mic.talking on", || e.signal("mic.talking") == Some(1.0)).await;
    let quiet = FakeConsole::start().await;
    let q = engine(&quiet, "[mixer.talk]\nchannel = 16", vec![]).await;
    q.wait("meters", || q.signal("mixer.16r.meter.ch.6").is_some_and(|v| v > 0.0)).await;
    assert_eq!(q.signal("mic.talking"), Some(0.0), "an unplugged channel never talks");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn engine_set_reaches_console_and_its_echo_is_not_a_change() {
    let console = FakeConsole::start().await;
    let e = engine(&console, WRITABLE, vec![]).await;
    e.exec(Origin::Cli, set("mixer.16r.ch.16.fader", 0.5)).await.unwrap();
    e.exec(Origin::Cli, set("mixer.16r.ch.16.mute", true)).await.unwrap();
    e.wait("console fader", || near(console.num("line/ch16/volume"), 0.5)).await;
    e.wait("console mute", || near(console.num("line/ch16/mute"), 1.0)).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(console.sets_of("line/ch16/volume"), vec![0.5], "sent exactly once");
    assert_eq!(console.sets_of("line/ch16/mute"), vec![1.0]);
    assert!(e.events_of("mixer.changed").is_empty(), "our own echoes are not console changes");
    assert!((e.f("mixer.16r.ch.16.db") + 10.1).abs() < 0.5, "half travel ≈ −10 dB on the console law");
    // readonly controls are refused by the core and never reach the console
    assert!(e.exec(Origin::Cli, set("mixer.16r.ch.1.fader", 0.1)).await.is_err());
    assert!(e.exec(Origin::Cli, set("mixer.16r.main.fader", 1.0)).await.is_err());
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(console.sets_of("line/ch1/volume").is_empty() && console.sets_of("main/ch1/volume").is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn console_change_updates_state_takes_over_and_is_never_sent_back() {
    let console = FakeConsole::start().await;
    let e = engine(&console, WRITABLE, vec![]).await;
    e.exec(Origin::Cli, set("mixer.16r.ch.16.fader", 0.5)).await.unwrap();
    e.wait("sent", || near(console.num("line/ch16/volume"), 0.5)).await;
    // UC Surface drags the fader while our manual override holds 0.5
    for v in [0.55, 0.6, 0.62] {
        console.external("line/ch16/volume", v);
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    console.external("line/ch16/mute", 1.0);
    e.wait("state follows the console", || (e.f("mixer.16r.ch.16.fader") - 0.62).abs() < 1e-4).await;
    e.wait("mute follows", || e.state("mixer.16r.ch.16.mute") == Some(Value::Bool(true))).await;
    // the override was released (console operator took over): provenance has none left
    let p = e.hub.explain("mixer.16r.ch.16.fader").await.unwrap();
    assert!(!p.layers.iter().any(|l| l.kind == "override" && l.active), "{:?}", p.layers);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(console.sets_of("line/ch16/volume"), vec![0.5], "nothing pushed back");
    assert!((console.num("line/ch16/volume").unwrap() - 0.62).abs() < 1e-6);
    let changed = e.events_of("mixer.changed");
    assert!(changed.iter().any(|ev| ev.payload.get_path("address") == Some(&Value::Str("mixer.16r.ch.16.fader".into()))), "{changed:?}");
    assert!(changed.iter().all(|ev| ev.origin == Origin::Mixer));
    // a console change of a control we don't own is still readback
    console.external("line/ch1/volume", 0.3);
    e.wait("readonly readback", || (e.f("mixer.16r.ch.1.fader") - 0.3).abs() < 1e-4).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn sends_are_rate_limited_latest_value_wins() {
    let console = FakeConsole::start().await;
    let e = engine(&console, &format!("{WRITABLE}\nrate_hz = 20"), vec![]).await;
    let t0 = Instant::now();
    for i in 1..=40 {
        e.hub.command(Command::new(Origin::Midi, set("mixer.16r.ch.15.fader", i as f64 / 50.0)));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let span = t0.elapsed();
    e.wait("final value", || console.num("line/ch15/volume").is_some_and(|v| (v - 0.8).abs() < 1e-6)).await;
    let sends = console.sets_of("line/ch15/volume");
    // ≤ 20 Hz per control over the burst (+1 for the leading edge, +1 for the trailing value)
    let allowed = (span.as_secs_f64() * 20.0).ceil() as usize + 2;
    assert!(sends.len() <= allowed && sends.len() >= 2, "{} sends in {span:?}: {sends:?}", sends.len());
    assert_eq!(*sends.last().unwrap(), 0.8);
    assert!(sends.windows(2).all(|w| w[0] < w[1]), "values only move forward: {sends:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preset_crossfades_a_snapshot_and_mutes_after_the_fade() {
    let console = FakeConsole::start().await;
    let snap = file(
        "mixes",
        "fadeout",
        "label = \"Fade out 16\"\nfade = \"5s\"\n[values]\n\"ch.16.fader\" = 0.6\n\"ch.14.mute\" = true\n\"ch.15.mute\" = false\n\"ch.1.fader\" = 0.0\n",
    );
    let preset = file("presets", "brb", "mix = { snapshot = \"fadeout\", fade = \"600ms\" }");
    let e = engine(&console, WRITABLE, vec![snap, preset]).await;
    console.external("line/ch15/mute", 1.0);
    e.wait("ch15 muted on the console", || e.state("mixer.16r.ch.15.mute") == Some(Value::Bool(true))).await;
    let t0 = Instant::now();
    e.exec(Origin::Deck, Op::PresetFire { name: "brb".into(), payload: Value::Null }).await.unwrap();
    // unmute happens at the start, mute at the end of the fade
    e.wait("unmute first", || near(console.num("line/ch15/mute"), 0.0)).await;
    assert_eq!(console.num("line/ch14/mute"), Some(0.0), "mute waits for the fade");
    e.wait("fade done", || console.num("line/ch16/volume").is_some_and(|v| (v - 0.6).abs() < 1e-6)).await;
    e.wait("mute at the end", || near(console.num("line/ch14/mute"), 1.0)).await;
    let took = t0.elapsed();
    assert!(took >= Duration::from_millis(550), "{took:?}");
    let steps = console.sets_of("line/ch16/volume");
    assert!(steps.len() >= 8, "a crossfade, not a jump: {steps:?}");
    assert!(steps.windows(2).all(|w| w[0] <= w[1]));
    assert!(console.sets_of("line/ch1/volume").is_empty(), "readonly addresses in a snapshot are skipped");
    let rec = e.events_of("mixer.snapshot.recalled");
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].payload.get_path("fade"), Some(&Value::Int(600)));
    assert_eq!(rec[0].payload.get_path("skipped"), Some(&Value::Int(1)));
    // the snapshot's own default fade applies when the recall gives none
    e.exec(Origin::Cli, Op::Action { name: "mixer.snapshot.recall".into(), args: Value::map().with("snapshot", "fadeout") }).await.unwrap();
    e.wait("second recall", || e.events_of("mixer.snapshot.recalled").len() == 2).await;
    assert_eq!(e.events_of("mixer.snapshot.recalled")[1].payload.get_path("fade"), Some(&Value::Int(5000)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn chat_can_never_touch_the_mixer() {
    let console = FakeConsole::start().await;
    let snap = file("mixes", "loud", "[values]\n\"ch.16.fader\" = 1.0\n");
    let preset = file("presets", "hype", "chat = true\nmix = { snapshot = \"loud\" }\nset = { \"mixer.16r.ch.15.fader\" = 0.9 }");
    let e = engine_full(&console, "start_mode = \"live\"", WRITABLE, vec![snap, preset]).await;
    let chat = |op: Op| {
        Command::new(Origin::Chat, op).with_actor(Some(se_proto::Actor { platform: "twitch".into(), id: "1".into(), name: "viewer".into(), roles: vec![] }))
    };
    assert!(e.hub.exec(chat(set("mixer.16r.ch.16.fader", 1.0))).await.is_err());
    assert!(e.hub.exec(chat(Op::Animate { address: "mixer.16r.ch.16.fader".into(), to: Value::Float(1.0), ms: 100, ease: Default::default() })).await.is_err());
    assert!(e.hub.exec(chat(Op::Action { name: "mixer.snapshot.recall".into(), args: Value::map().with("snapshot", "loud") })).await.is_err());
    assert!(e.hub.exec(chat(Op::Action { name: "mixer.panic".into(), args: Value::Null })).await.is_err());
    // a chat-fired preset runs, but without its mixer parts
    e.hub.exec(chat(Op::PresetFire { name: "hype".into(), payload: Value::Null })).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(console.received.lock().is_empty(), "{:?}", console.received.lock());
    assert!(e.events_of("mixer.snapshot.recalled").is_empty());
    assert_eq!(e.f("mixer.16r.ch.16.fader"), 0.0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn safety_caps_pull_outputs_down_and_clamp_engine_values() {
    let console = FakeConsole::start().await;
    // the console's main sits at 0.725; cap it at 0.6
    let e = engine_full(&console, "[safety]\ncaps = { \"mixer.16r.main.fader\" = [0.0, 0.6] }", "writable = [\"main.*\", \"ch.16.*\"]", vec![]).await;
    e.wait("main pulled to its cap", || console.num("main/ch1/volume").is_some_and(|v| (v - 0.6).abs() < 1e-6)).await;
    e.exec(Origin::Cli, set("mixer.16r.main.fader", 0.95)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(200)).await;
    let sent = console.sets_of("main/ch1/volume");
    assert!(sent.iter().all(|v| *v <= 0.6 + 1e-6), "{sent:?}");
    assert!((e.f("mixer.16r.main.fader") - 0.6).abs() < 1e-6);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reconnects_after_the_link_drops_without_moving_anything() {
    let console = FakeConsole::start().await;
    let e = engine(&console, WRITABLE, vec![]).await;
    e.exec(Origin::Cli, set("mixer.16r.ch.16.fader", 0.4)).await.unwrap();
    e.wait("sent", || near(console.num("line/ch16/volume"), 0.4)).await;
    console.drop_clients();
    e.wait("disconnected", || e.state("mixer.16r.connected") == Some(Value::Bool(false))).await;
    // while offline someone moves the fader on the console
    console.external("line/ch16/volume", 0.2);
    e.wait("reconnected", || e.state("mixer.16r.connected") == Some(Value::Bool(true))).await;
    assert_eq!(*console.subscriptions.lock(), 2);
    // the console is authoritative after a reconnect: state follows it, nothing is pushed
    e.wait("state follows the console", || (e.f("mixer.16r.ch.16.fader") - 0.2).abs() < 1e-4).await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(console.sets_of("line/ch16/volume"), vec![0.4]);
    assert_eq!(e.events_of("mixer.disconnected").len(), 1);
    assert_eq!(e.events_of("mixer.connected").len(), 2);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn snapshot_store_writes_the_current_mix() {
    let console = FakeConsole::start().await;
    let e = engine(&console, WRITABLE, vec![]).await;
    e.exec(Origin::Cli, Op::Action { name: "mixer.snapshot.store".into(), args: Value::map().with("snapshot", "safe").with("label", "Safe") }).await.unwrap();
    let path = e.dir.path().join("mixes/safe.toml");
    e.wait("file written", || path.exists()).await;
    e.wait("stored event", || !e.events_of("mixer.snapshot.stored").is_empty()).await;
    let text = std::fs::read_to_string(&path).unwrap();
    let snap = se_mixer::snapshots::parse("safe", &text.parse().unwrap(), "mixer.16r").unwrap();
    assert_eq!(snap.label.as_deref(), Some("Safe"));
    assert_eq!(snap.values.get("mixer.16r.ch.1.fader").and_then(Value::as_f64).map(|v| (v * 1e4).round()), Some(7528.0));
    assert!(!snap.values.contains_key("mixer.16r.main.fader"), "outputs are not stored by default");
    assert!(snap.values.contains_key("mixer.16r.aux.1.ch.10.send"));
}
