//! End-to-end: real core + hub + lights control task + output thread, with sACN captured on
//! localhost. Covers the M7 acceptance flows: a cue list with fades and follows run from an
//! action, a beat-synced chase following a synthetic `beat.phase`, a palette edit updating a
//! running cue, HTP/LTP across playbacks and the programmer, chat caps, panic, and the flash
//! limiter on the wire.

use se_core::{Config, Core, Input, SourceFile};
use se_hub::{EngineCtx, Hub, RunnerHooks, run_core};
use se_proto::{Command, Op, Origin, Value};
use std::net::UdpSocket;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

struct H {
    hub: Arc<Hub>,
    tx: watch::Sender<Arc<Config>>,
    files: Vec<SourceFile>,
    core: Option<std::thread::JoinHandle<()>>,
    lights: se_dmx::Lights,
    _dir: tempfile::TempDir,
}

impl Drop for H {
    fn drop(&mut self) {
        self.lights.stop();
        self.hub.shutdown();
        if let Some(t) = self.core.take() {
            let _ = t.join();
        }
    }
}

fn file(kind: &str, name: &str, src: &str) -> SourceFile {
    let path = if kind == "project" { "project.toml".to_string() } else { format!("{kind}/{name}.toml") };
    SourceFile { kind: kind.into(), name: name.into(), path, table: toml::from_str(src).unwrap_or_else(|e| panic!("{kind}/{name}: {e}")) }
}

const PROJECT: &str = "schema = 1\n[safety]\nchat_caps = { \"lights.*.intensity\" = [0.0, 0.5] }\n";

fn rig(extra: &str) -> String {
    format!(
        "[fixtures.p1]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 1\nposition = [0.2, 0.2]\nlayout_verified = true\n[fixtures.p2]\nprofile = \"generic_rgb\"\nmode = \"3ch\"\naddress = 4\nposition = [0.8, 0.2]\nlayout_verified = true\n[groups]\nfront = [\"p1\"]\n{extra}"
    )
}

async fn harness(files: Vec<SourceFile>) -> H {
    harness_with_db(files, se_store::Db::memory().unwrap()).await
}

async fn harness_with_db(files: Vec<SourceFile>, db: se_store::Db) -> H {
    let cfg = Config::build(&files);
    assert!(cfg.errors.is_empty(), "{:?}", cfg.errors);
    let clock = Arc::new(se_clock::Clock::new());
    let (hub, rx) = Hub::new(clock);
    let core = Core::new(cfg.clone(), se_clock::now());
    let h2 = hub.clone();
    let t = std::thread::spawn(move || run_core(core, h2, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 2, on_runtime: None }));
    let (tx, crx) = watch::channel(Arc::new(cfg));
    let dir = tempfile::tempdir().unwrap();
    let ctx = EngineCtx {
        hub: hub.clone(),
        db,
        project_root: dir.path().to_path_buf(),
        data_dir: dir.path().to_path_buf(),
        share_dir: dir.path().to_path_buf(),
        config: crx,
        http: "127.0.0.1:0".parse().unwrap(),
        dev: true,
    };
    let lights = se_dmx::run(ctx).await.unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    H { hub, tx, files, core: Some(t), lights, _dir: dir }
}

impl H {
    fn act(&self, origin: Origin, text: &str) {
        self.hub.command(Command::new(origin, Op::parse(text).unwrap()));
    }
    fn f(&self, a: &str) -> f64 {
        self.hub.snapshot.load().get(a).and_then(Value::as_f64).unwrap_or(f64::NAN)
    }
    fn color(&self, a: &str) -> [f32; 4] {
        self.hub.snapshot.load().get(a).and_then(Value::as_color).unwrap_or([f32::NAN; 4])
    }
    fn s(&self, a: &str) -> String {
        self.hub.snapshot.load().str(a).unwrap_or("").to_string()
    }
    async fn dmx(&self) -> Vec<i64> {
        let v = self.hub.query("lights.output", Value::Null).await.unwrap();
        v.get_path("universes.1").and_then(Value::as_list).unwrap().iter().map(|x| x.as_i64().unwrap()).collect()
    }
    async fn wait_dmx(&self, expected: &[i64]) {
        let deadline = Instant::now() + Duration::from_secs(1);
        loop {
            let bytes = self.dmx().await;
            if bytes.starts_with(expected) {
                return;
            }
            assert!(Instant::now() < deadline, "DMX expected {expected:?}, got {:?}", &bytes[..expected.len()]);
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }
    fn reload(&mut self, kind: &str, name: &str, src: &str) {
        self.files.retain(|f| !(f.kind == kind && f.name == name));
        self.files.push(file(kind, name, src));
        let cfg = Config::build(&self.files);
        self.hub.submit(Input::Config { config: Box::new(cfg.clone()) });
        self.tx.send(Arc::new(cfg)).unwrap();
    }
    fn remove(&mut self, kind: &str, name: &str) {
        self.files.retain(|f| !(f.kind == kind && f.name == name));
        let cfg = Config::build(&self.files);
        self.hub.submit(Input::Config { config: Box::new(cfg.clone()) });
        self.tx.send(Arc::new(cfg)).unwrap();
    }
    /// Advanced manual `lights.cue` / `lights.release` look playback, separate from authored layers.
    fn look(&self, action: &str, look: &str, priority: Option<u16>) {
        let mut args = Value::map().with("cue", "").with("cuelist", "").with("look", look);
        if let Some(p) = priority {
            args = args.with("priority", p as i64);
        }
        self.hub.command(Command::new(Origin::Rule, Op::Action { name: action.into(), args }));
    }
    async fn layer(&self, addr: &str, source: &str) -> bool {
        self.hub.explain(addr).await.unwrap().layers.iter().any(|l| l.source == source && l.active)
    }
}

async fn wait_for(what: &str, secs: f64, mut f: impl FnMut() -> bool) {
    let end = Instant::now() + Duration::from_secs_f64(secs);
    while Instant::now() < end {
        if f() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {what}");
}

fn near(a: f64, b: f64, tol: f64) -> bool {
    (a - b).abs() <= tol
}

#[tokio::test(flavor = "multi_thread")]
async fn cue_list_with_fades_and_follows_runs_from_an_action() {
    let main = r##"
release = "300ms"
[[cue]]
fade = "600ms"
follow = "200ms"
[cue.set]
all = { intensity = 1.0, color = "palette:warm" }
[[cue]]
fade = "400ms"
[cue.set]
p1 = { intensity = 0.2 }
"##;
    let h = harness(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/palettes", "warm", "kind = \"color\"\n[set]\nall = { color = \"#ff8000\" }"),
        file("lights/cuelists", "main", main),
    ])
    .await;
    let t0 = Instant::now();
    h.act(Origin::Deck, "lights.cue main");
    // mid-fade
    wait_for("fade in progress", 1.0, || {
        let v = h.f("lights.p1.intensity");
        v > 0.15 && v < 0.85
    })
    .await;
    wait_for("cue 1 complete", 1.5, || near(h.f("lights.p1.intensity"), 1.0, 1e-6)).await;
    let el = t0.elapsed().as_millis();
    assert!((550..900).contains(&el), "600 ms fade took {el} ms");
    assert_eq!(h.s("lights.cuelist.main.cue"), "1");
    assert!(h.hub.snapshot.load().bool("lights.cuelist.main.playing"));
    let c = h.color("lights.p2.color");
    assert!(near(c[0] as f64, 1.0, 1e-3) && near(c[1] as f64, 128.0 / 255.0, 1e-2) && near(c[2] as f64, 0.0, 1e-3), "{c:?}");
    // follow: cue 2 starts 200 ms after cue 1 completed; p1 fades to 0.2, p2 tracks at 1.0
    wait_for("follow to cue 2", 1.0, || h.s("lights.cuelist.main.cue") == "2").await;
    let el = t0.elapsed().as_millis();
    assert!((750..1100).contains(&el), "follow fired at {el} ms");
    wait_for("cue 2 fade", 1.5, || near(h.f("lights.p1.intensity"), 0.2, 1e-6)).await;
    assert_eq!(h.f("lights.p2.intensity"), 1.0, "tracked from cue 1");
    // the DMX frame: virtual dimmer × warm colour
    tokio::time::sleep(Duration::from_millis(80)).await;
    let u = h.dmx().await;
    assert_eq!(&u[0..3], &[51, 26, 0], "p1 = 0.2 × #ff8000");
    assert_eq!(&u[3..6], &[255, 128, 0], "p2 = 1.0 × #ff8000");
    // provenance: the playback layer through the core resolver
    let p = h.hub.explain("lights.p1.intensity").await.unwrap();
    assert!(p.layers.iter().any(|l| l.source == "cuelist:main" && l.active), "{:?}", p.layers);
    // playback master scales intensities
    h.act(Origin::Midi, "set lights.cuelist.main.master 0.5");
    wait_for("master", 1.0, || near(h.f("lights.p2.intensity"), 0.5, 1e-6)).await;
    assert!(near(h.f("lights.p1.intensity"), 0.1, 1e-6));
    // query shape used by the Lights view
    let q = h.hub.query("lights.cuelists", Value::Null).await.unwrap();
    let main = q.as_list().unwrap().iter().find(|l| l.get_path("name").and_then(Value::as_str) == Some("main")).unwrap();
    assert_eq!(main.get_path("current").and_then(Value::as_str), Some("2"));
    assert_eq!(main.get_path("cues").and_then(Value::as_list).unwrap().len(), 2);
    // release: fade out, then no override left
    h.act(Origin::Deck, "lights.release cuelist=main");
    wait_for("released", 1.5, || h.f("lights.p2.intensity") == 0.0 && !h.hub.snapshot.load().bool("lights.cuelist.main.playing")).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let p = h.hub.explain("lights.p2.color").await.unwrap();
    assert!(!p.layers.iter().any(|l| l.source == "cuelist:main"), "all overrides released: {:?}", p.layers);
}

#[tokio::test(flavor = "multi_thread")]
async fn beat_synced_chase_follows_synthetic_beat_phase_on_the_wire() {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let port = sock.local_addr().unwrap().port();
    let out = format!("[output]\narmed = true\n[outputs.net]\nkind = \"sacn\"\nuniverses = [1]\ndestination = \"127.0.0.1\"\nport = {port}\n");
    let h = harness(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig(&out)),
        file("lights/effects", "chase", "kind = \"color_chase\"\nunit = \"beats\"\nrate = 2\nspread = 0\ncolors = [\"#ff0000\", \"#0000ff\"]"),
        file("lights/cuelists", "chase", "[[cue]]\neffects = { chase = {} }\n[cue.set]\nall = { intensity = 1.0 }"),
    ])
    .await;
    h.act(Origin::Deck, "lights.cue chase");
    tokio::time::sleep(Duration::from_millis(200)).await;
    // 100 BPM synthetic beat: phase ramps 0→1 every 600 ms
    let hub = h.hub.clone();
    let t0 = Instant::now();
    let beat = move |t: Instant| t.duration_since(t0).as_secs_f64() * 100.0 / 60.0;
    let feeder = std::thread::spawn(move || {
        while t0.elapsed() < Duration::from_millis(4300) {
            hub.signals(vec![("beat.phase".into(), beat(Instant::now()).fract() as f32), ("beat.bpm".into(), 100.0)]);
            std::thread::sleep(Duration::from_millis(5));
        }
    });
    let mut samples: Vec<(f64, bool)> = Vec::new();
    let mut buf = [0u8; 700];
    let mut packets = 0;
    while t0.elapsed() < Duration::from_millis(4200) {
        let Ok(n) = sock.recv(&mut buf) else { continue };
        assert_eq!(n, 638, "full E1.31 data packet");
        assert_eq!(&buf[4..16], b"ASC-E1.17\0\0\0");
        packets += 1;
        // DMP property values: start code at 125, slot 1 at 126
        assert_eq!(buf[125], 0);
        let (r, b) = (buf[126], buf[128]);
        if t0.elapsed() > Duration::from_millis(700) {
            samples.push((beat(Instant::now()), r > 200 && b < 50));
            assert!((r > 200 && b < 50) || (b > 200 && r < 50), "chase colour only: r={r} b={b}");
        }
    }
    feeder.join().unwrap();
    let secs = 3.5;
    assert!(packets as f64 >= 40.0 * 4.0, "{packets} sACN packets in 4.2 s");
    // colour changes happen on beat boundaries and alternate every beat
    let changes: Vec<f64> = samples.windows(2).filter(|w| w[0].1 != w[1].1).map(|w| w[1].0).collect();
    let beats = secs * 100.0 / 60.0;
    assert!((changes.len() as f64 - beats).abs() <= 1.5, "{} colour changes over {beats:.1} beats: {changes:?}", changes.len());
    for b in &changes {
        let off = b.fract().min(1.0 - b.fract());
        assert!(off < 0.12, "change at beat {b:.3} is {off:.3} beats from the boundary");
    }
    for w in changes.windows(2) {
        assert!(near(w[1] - w[0], 1.0, 0.15), "one colour per beat: {w:?}");
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn editing_a_palette_updates_every_cue_using_it() {
    let mut h = harness(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/palettes", "warm", "[set]\nall = { color = \"#ff8000\" }"),
        file("lights/cuelists", "a", "[[cue]]\n[cue.set]\np1 = { intensity = 1.0, color = \"palette:warm\" }"),
        file("lights/cuelists", "b", "[[cue]]\n[cue.set]\np2 = { intensity = 1.0, palette = \"warm\" }"),
    ])
    .await;
    h.act(Origin::Deck, "lights.cue a");
    h.act(Origin::Deck, "lights.cue b");
    wait_for("cues running", 1.0, || {
        near(h.color("lights.p2.color")[1] as f64, 128.0 / 255.0, 0.01) && near(h.color("lights.p1.color")[1] as f64, 128.0 / 255.0, 0.01)
    })
    .await;
    let q = h.hub.query("lights.palettes", Value::Null).await.unwrap();
    let used: Vec<&str> = q.as_list().unwrap()[0].get_path("used_by").and_then(Value::as_list).unwrap().iter().filter_map(Value::as_str).collect();
    assert_eq!(used, vec!["a/1", "b/1"]);
    h.reload("lights/palettes", "warm", "[set]\nall = { color = \"#00ff00\" }\np2 = { color = \"#0000ff\" }");
    wait_for("both running cues follow the edited palette", 2.0, || {
        let a = h.color("lights.p1.color");
        let b = h.color("lights.p2.color");
        a[0] < 0.01 && a[1] > 0.99 && b[2] > 0.99 && b[1] < 0.01
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn htp_ltp_programmer_chat_caps_and_panic() {
    let h = harness(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/cuelists", "a", "priority = 200\n[[cue]]\n[cue.set]\np1 = { intensity = 0.4, color = \"#ff0000\" }"),
        file("lights/cuelists", "b", "priority = 250\n[[cue]]\n[cue.set]\np1 = { intensity = 0.7, color = \"#0000ff\" }"),
        file("lights/cuelists", "hot", "[[cue]]\n[cue.set]\np2 = { intensity = 1.0 }"),
        file("lights/cuelists", "safe", "priority = 1000\n[[cue]]\nstop_effects = [\"all\"]\n[cue.set]\nall = { intensity = 0.6, color = \"#ffe6cc\" }\n[cue.addresses]\n\"lights.master\" = 1.0"),
    ])
    .await;
    // b (priority 250) first, then a (200): priority beats "latest" for LTP attributes
    h.act(Origin::Deck, "lights.cue b");
    tokio::time::sleep(Duration::from_millis(50)).await;
    h.act(Origin::Deck, "lights.cue a");
    wait_for("HTP intensity, LTP by priority", 1.0, || near(h.f("lights.p1.intensity"), 0.7, 1e-6) && h.color("lights.p1.color")[2] > 0.99).await;
    h.act(Origin::Deck, "lights.release cuelist=b fade=0");
    wait_for("a shows again", 1.0, || near(h.f("lights.p1.intensity"), 0.4, 1e-6) && h.color("lights.p1.color")[0] > 0.99).await;
    // the programmer overrides playback until released
    h.act(Origin::Ui, "lights.programmer.select p1");
    h.act(Origin::Ui, "lights.programmer.set attr=color value='#00ff00'");
    wait_for("programmer colour", 1.0, || h.color("lights.p1.color")[1] > 0.99).await;
    assert!(h.hub.snapshot.load().bool("lights.programmer.active"));
    // (bare `x.release` text is the core's release op; the action is sent directly, as the UI does)
    h.hub.command(Command::new(Origin::Ui, Op::Action { name: "lights.programmer.release".into(), args: Value::Null }));
    wait_for("playback colour back", 1.0, || h.color("lights.p1.color")[0] > 0.99).await;
    // chat-origin playback: chat priority, core chat caps clamp intensity
    // chat effects only run while live (core policy)
    h.hub.exec(Command::new(Origin::Ui, Op::ModeSet { mode: "live".into() })).await.unwrap();
    h.act(Origin::Chat, "lights.cue hot");
    wait_for("chat capped", 1.0, || near(h.f("lights.p2.intensity"), 0.5, 1e-6)).await;
    let p = h.hub.explain("lights.p2.intensity").await.unwrap();
    assert!(p.layers.iter().any(|l| l.source.starts_with("chat") && l.priority == Some(100)), "{:?}", p.layers);
    // panic: everything released, safe look on top
    h.act(Origin::Ui, "set lights.master 0.2");
    h.hub.command(Command::new(Origin::Ui, Op::Panic));
    wait_for("safe look", 1.5, || {
        near(h.f("lights.p1.intensity"), 0.6, 1e-6) && near(h.f("lights.p2.intensity"), 0.6, 1e-6) && near(h.f("lights.master"), 1.0, 1e-6)
    })
    .await;
    assert!(h.hub.snapshot.load().bool("lights.cuelist.safe.playing"));
    assert!(!h.hub.snapshot.load().bool("lights.cuelist.a.playing"));
    // automation (e.g. the panic preset ending) cannot release the latched safe look…
    h.hub.command(Command::new(Origin::Rule, Op::Action { name: "lights.release".into(), args: Value::map().with("cue", "safe") }));
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(h.hub.snapshot.load().bool("lights.cuelist.safe.playing"), "safe look latched");
    assert!(near(h.f("lights.p1.intensity"), 0.6, 1e-6));
    // …an operator can
    h.act(Origin::Ui, "lights.release cuelist=safe fade=0");
    wait_for("operator released the safe look", 1.0, || !h.hub.snapshot.load().bool("lights.cuelist.safe.playing") && h.f("lights.p1.intensity") == 0.0).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn flash_spam_is_limited_to_three_flashes_per_second_on_the_wire() {
    let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
    sock.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
    let port = sock.local_addr().unwrap().port();
    let out = format!("[output]\narmed = true\n[outputs.net]\nkind = \"sacn\"\nuniverses = [1]\ndestination = \"127.0.0.1\"\nport = {port}\n");
    let h = harness(vec![file("project", "project", PROJECT), file("lights", "rig", &rig(&out))]).await;
    // a rule/patch spamming `lights.flash` at 10 Hz (the event form patches emit)
    let hub = h.hub.clone();
    let t0 = Instant::now();
    let spam = std::thread::spawn(move || {
        while t0.elapsed() < Duration::from_millis(4000) {
            hub.emit(se_proto::Event::new("lights.flash", Origin::Patch, Value::map().with("color", "#ffffff").with("ms", 40)));
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    let mut levels: Vec<(u64, u8)> = Vec::new();
    let mut buf = [0u8; 700];
    while t0.elapsed() < Duration::from_millis(4000) {
        if let Ok(638) = sock.recv(&mut buf) {
            levels.push((t0.elapsed().as_nanos() as u64, buf[126]));
        }
    }
    spam.join().unwrap();
    let mut onsets = Vec::new();
    let (mut low, mut peak, mut high) = (0u8, 0u8, false);
    for &(t, v) in &levels {
        if high {
            peak = peak.max(v);
            if peak.saturating_sub(v) >= 51 {
                high = false;
                low = v;
            }
        } else {
            low = low.min(v);
            if v.saturating_sub(low) >= 51 && low < 204 {
                onsets.push(t);
                high = true;
                peak = v;
            }
        }
    }
    let worst = onsets.iter().map(|t0| onsets.iter().filter(|t| **t >= *t0 && **t - *t0 < 1_000_000_000).count()).max().unwrap_or(0);
    assert!(worst <= 3, "{worst} flashes within 1 s on the wire ({} total)", onsets.len());
    assert!(onsets.len() >= 8, "flashes still get through at the capped rate ({})", onsets.len());
    let out = h.hub.query("lights.output", Value::Null).await.unwrap();
    assert!(out.get_path("limiter.suppressed").and_then(Value::as_i64).unwrap() > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn running_playbacks_are_restored_after_a_restart() {
    let files = || {
        vec![
            file("project", "project", PROJECT),
            file("lights", "rig", &rig("")),
            file("lights/cuelists", "main", "[[cue]]\n[cue.set]\np1 = { intensity = 0.3 }\n[[cue]]\n[cue.set]\np1 = { intensity = 0.9 }"),
        ]
    };
    let db = se_store::Db::memory().unwrap();
    {
        let h = harness_with_db(files(), db.clone()).await;
        h.act(Origin::Deck, "lights.goto main 2");
        wait_for("cue 2", 1.0, || near(h.f("lights.p1.intensity"), 0.9, 1e-6)).await;
    }
    let h = harness_with_db(files(), db).await;
    wait_for("restored at cue 2", 2.0, || h.s("lights.cuelist.main.cue") == "2" && near(h.f("lights.p1.intensity"), 0.9, 1e-6)).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn released_playback_is_not_restored_during_its_outgoing_fade() {
    let files = || vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/cuelists", "main", "[[cue]]\n[cue.set]\np1 = { intensity = 0.9 }"),
    ];
    let db = se_store::Db::memory().unwrap();
    {
        let h = harness_with_db(files(), db.clone()).await;
        h.act(Origin::Deck, "lights.goto main 1");
        wait_for("manual playback", 1.0, || h.s("lights.cuelist.main.cue") == "1" && near(h.f("lights.p1.intensity"), 0.9, 1e-6)).await;
        h.act(Origin::Deck, "lights.release cuelist=main fade=5s");
        wait_for("release intention", 1.0, || h.s("lights.cuelist.main.cue").is_empty()).await;
        assert!(h.f("lights.p1.intensity") > 0.0, "restart happens before the outgoing fade finishes");
    }
    let h = harness_with_db(files(), db).await;
    assert_eq!(h.s("lights.cuelist.main.cue"), "");
    assert_eq!(h.f("lights.p1.intensity"), 0.0);
    assert!(!h.layer("lights.p1.intensity", "cuelist:main").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn preset_navigation_does_not_inherit_operator_restart_intention() {
    let files = || vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/cuelists", "main", "[[cue]]\n[cue.set]\np1 = { intensity = 0.3 }\n[[cue]]\n[cue.set]\np1 = { intensity = 0.9 }"),
        file("presets", "automated", "do = ['lights.goto main 1']\nuntil_released = true"),
    ];
    let db = se_store::Db::memory().unwrap();
    {
        let h = harness_with_db(files(), db.clone()).await;
        h.act(Origin::Deck, "lights.goto main 2");
        wait_for("operator cue", 1.0, || near(h.f("lights.p1.intensity"), 0.9, 1e-6)).await;
        h.act(Origin::Deck, "preset.fire automated");
        wait_for("automation cue", 1.0, || h.s("lights.cuelist.main.cue") == "1" && near(h.f("lights.p1.intensity"), 0.3, 1e-6)).await;
    }
    let h = harness_with_db(files(), db).await;
    assert_eq!(h.s("lights.cuelist.main.cue"), "");
    assert_eq!(h.f("lights.p1.intensity"), 0.0);
    assert!(!h.layer("lights.p1.intensity", "cuelist:main").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn legacy_playback_generations_are_not_reconstructed() {
    let db = se_store::Db::memory().unwrap();
    db.kv_set("lights", "playbacks", &Value::map().with("main",
        Value::map().with("cue", "2").with("origin", "deck").with("priority", 300_i64)
            .with("key", "layer:color:41").with("epoch", 41_i64)
            .with("applied", Value::map().with("lights.p1.intensity", 0.9_f64))))
        .unwrap();
    let h = harness_with_db(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/cuelists", "main", "[[cue]]\n[cue.set]\np1 = { intensity = 0.3 }\n[[cue]]\n[cue.set]\np1 = { intensity = 0.9 }"),
    ], db).await;
    assert_eq!(h.s("lights.cuelist.main.cue"), "");
    assert_eq!(h.f("lights.p1.intensity"), 0.0);
    assert!(!h.layer("lights.p1.intensity", "cuelist:main").await);
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_intention_tracks_reload_cue_ids_and_drops_deleted_cues() {
    let original = "[[cue]]\nid='low'\n[cue.set]\np1={intensity=0.3}\n[[cue]]\nid='held'\n[cue.set]\np1={intensity=0.9}";
    let reordered = "[[cue]]\nid='held'\n[cue.set]\np1={intensity=0.7}\n[[cue]]\nid='low'\n[cue.set]\np1={intensity=0.3}";
    let removed = "[[cue]]\nid='low'\n[cue.set]\np1={intensity=0.3}";
    let files = |main: &str| vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/cuelists", "main", main),
    ];
    let db = se_store::Db::memory().unwrap();
    {
        let mut h = harness_with_db(files(original), db.clone()).await;
        h.act(Origin::Deck, "lights.goto main held");
        wait_for("held cue", 1.0, || h.s("lights.cuelist.main.cue") == "held" && near(h.f("lights.p1.intensity"), 0.9, 1e-6)).await;
        h.reload("lights/cuelists", "main", reordered);
        wait_for("held cue follows reorder and edit", 2.0, || h.s("lights.cuelist.main.cue") == "held" && near(h.f("lights.p1.intensity"), 0.7, 1e-6)).await;
    }
    {
        let mut h = harness_with_db(files(reordered), db.clone()).await;
        assert_eq!(h.s("lights.cuelist.main.cue"), "held");
        assert!(near(h.f("lights.p1.intensity"), 0.7, 1e-6));
        h.reload("lights/cuelists", "main", removed);
        wait_for("deleted cue stops", 1.0, || h.s("lights.cuelist.main.cue").is_empty() && h.f("lights.p1.intensity") == 0.0).await;
    }
    let h = harness_with_db(files(removed), db).await;
    assert_eq!(h.s("lights.cuelist.main.cue"), "");
    assert_eq!(h.f("lights.p1.intensity"), 0.0);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_look_holds_its_palette_follows_edits_and_releases() {
    let mut h = harness(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/palettes", "warm", "[set]\nall = { color = \"#ff8000\" }\np1 = { intensity = 0.5 }"),
    ])
    .await;
    let mut bus = h.hub.subscribe();
    h.look("lights.cue", "warm", Some(se_proto::PRIORITY_PRESET));
    wait_for("look on", 1.0, || {
        let c = h.color("lights.p2.color");
        near(h.f("lights.p1.intensity"), 0.5, 1e-6) && near(c[0] as f64, 1.0, 1e-3) && near(c[1] as f64, 128.0 / 255.0, 1e-2) && c[2] < 1e-3
    })
    .await;
    assert!(h.layer("lights.p1.color", "look:warm").await);
    assert!(!h.layer("lights.p2.intensity", "look:warm").await, "heads the palette has no value for stay untouched");
    let q = h.hub.query("lights.palettes", Value::Null).await.unwrap();
    assert_eq!(q.as_list().unwrap()[0].get_path("look_active"), Some(&Value::Bool(true)));
    let q = h.hub.query("lights.cuelists", Value::Null).await.unwrap();
    assert!(q.as_list().unwrap().is_empty(), "looks are not cue lists: {q}");
    // palette edit covering other heads: the look is rebuilt (p1 intensity goes, p2's comes)
    h.reload("lights/palettes", "warm", "[set]\nall = { color = \"#00ff00\" }\np2 = { intensity = 0.3 }");
    wait_for("rebuilt look", 2.0, || {
        let c = h.color("lights.p1.color");
        c[1] > 0.99 && c[0] < 0.01 && near(h.f("lights.p2.intensity"), 0.3, 1e-6) && h.f("lights.p1.intensity") == 0.0
    })
    .await;
    assert!(!h.layer("lights.p1.intensity", "look:warm").await);
    // value-only edit
    h.reload("lights/palettes", "warm", "[set]\nall = { color = \"#0000ff\" }\np2 = { intensity = 0.3 }");
    wait_for("edited value", 2.0, || h.color("lights.p2.color")[2] > 0.99 && h.color("lights.p2.color")[1] < 0.01).await;
    // the preset ends
    h.look("lights.release", "warm", None);
    wait_for("look released", 1.0, || h.f("lights.p2.intensity") == 0.0).await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert!(!h.layer("lights.p2.color", "look:warm").await);
    let q = h.hub.query("lights.palettes", Value::Null).await.unwrap();
    assert_eq!(q.as_list().unwrap()[0].get_path("look_active"), Some(&Value::Bool(false)));
    // release-all clears looks; releasing a look that is no longer running is fine
    h.look("lights.cue", "warm", None);
    wait_for("look on again", 1.0, || near(h.f("lights.p2.intensity"), 0.3, 1e-6)).await;
    h.act(Origin::Ui, "lights.release all");
    wait_for("released by release-all", 1.0, || h.f("lights.p2.intensity") == 0.0).await;
    h.look("lights.release", "warm", None);
    // a deleted palette releases its running look
    h.look("lights.cue", "warm", None);
    wait_for("look on once more", 1.0, || near(h.f("lights.p2.intensity"), 0.3, 1e-6)).await;
    h.remove("lights/palettes", "warm");
    wait_for("released with its palette", 2.0, || h.f("lights.p2.intensity") == 0.0).await;
    // unknown looks are errors (no error for the release of an existing, stopped look above)
    h.look("lights.cue", "nope", None);
    let end = Instant::now() + Duration::from_secs(2);
    let mut errors = Vec::new();
    while Instant::now() < end {
        let Ok(Ok(m)) = tokio::time::timeout(Duration::from_millis(100), bus.recv()).await else { continue };
        if let se_hub::Bus::Log { msg, target, .. } = &*m
            && target == "lights"
            && (msg.contains("look") || msg.contains("lights.release"))
        {
            errors.push(msg.clone());
            if msg.contains("nope") {
                break;
            }
        }
    }
    assert_eq!(errors.last().map(String::as_str), Some("lights.cue: unknown look `nope`"), "{errors:?}");
    assert!(!errors.iter().any(|e| e.starts_with("lights.release")), "{errors:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn looks_layer_with_cue_lists_by_priority() {
    let h = harness(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/palettes", "blue", "[set]\np1 = { color = \"#0000ff\" }"),
        file("lights/cuelists", "base", "[[cue]]\n[cue.set]\np1 = { intensity = 1.0, color = \"#ff0000\" }"),
    ])
    .await;
    let red = |h: &H| h.color("lights.p1.color")[0] > 0.99 && h.color("lights.p1.color")[2] < 0.01;
    let blue = |h: &H| h.color("lights.p1.color")[2] > 0.99 && h.color("lights.p1.color")[0] < 0.01;
    h.act(Origin::Deck, "lights.cue base");
    wait_for("cue list red", 1.0, || red(&h)).await;
    // a higher-priority look wins over the running cue list…
    h.look("lights.cue", "blue", Some(se_proto::PRIORITY_PRESET + 50));
    wait_for("look wins", 1.0, || blue(&h)).await;
    // …and the cue list shows again once it is released
    h.look("lights.release", "blue", None);
    wait_for("cue list back", 1.0, || red(&h)).await;
    // a lower-priority look loses to the cue list
    h.look("lights.cue", "blue", Some(se_proto::PRIORITY_PRESET - 50));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let layers = h.hub.explain("lights.p1.color").await.unwrap().layers;
    assert!(layers.iter().any(|l| l.source == "look:blue" && l.priority == Some(se_proto::PRIORITY_PRESET - 50)), "look held underneath: {layers:?}");
    assert!(red(&h), "{:?}", h.color("lights.p1.color"));
    h.act(Origin::Deck, "lights.release cuelist=base fade=0");
    wait_for("look alone shows", 1.0, || blue(&h)).await;
}

const WARM_KNOBS: &str = r##"# the warm wash
label = "Warm"

[set]
all = { color = "#ff8000", intensity = 0.5 } # keep me

[[knob]]
label = "Color"
target = "set.all.color"
kind = "color"

[[knob]]
label = "Brightness"
target = "set.all.intensity"
min = 0.0
max = 1.0
step = 0.05
unit = "%"
"##;

const PULSING: &str = r##"label = "Pulsing"
[[cue]]
effects = { pulse = {} } # breathing
[cue.set]
all = { intensity = 1.0 }

[[knob]]
label = "Speed"
target = "cue.1.effects.pulse.rate"
min = 0.5
max = 8.0
"##;

impl H {
    fn knob(&self, origin: Origin, item: (&str, &str), target: &str, value: Value, save: bool) {
        let args = Value::map().with(item.0, item.1).with("target", target).with("value", value).with("save", save);
        self.hub.command(Command::new(origin, Op::Action { name: "lights.knob".into(), args }));
    }
    fn disk(&self, rel: &str) -> String {
        std::fs::read_to_string(self._dir.path().join(rel)).unwrap()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn knobs_move_running_looks_and_cue_lists_and_the_saved_value_fires_next_time() {
    let pulse = "kind = \"dimmer_sine\"\nunit = \"hz\"\nrate = 1.0\nsize = 0.5";
    let mut h = harness(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/palettes", "warm", WARM_KNOBS),
        file("lights/effects", "pulse", pulse),
        file("lights/cuelists", "pulsing", PULSING),
    ])
    .await;
    let root = h._dir.path().to_path_buf();
    std::fs::write(root.join("project.toml"), PROJECT).unwrap();
    std::fs::create_dir_all(root.join("lights/palettes")).unwrap();
    std::fs::create_dir_all(root.join("lights/cuelists")).unwrap();
    std::fs::write(root.join("lights/palettes/warm.toml"), WARM_KNOBS).unwrap();
    std::fs::write(root.join("lights/cuelists/pulsing.toml"), PULSING).unwrap();
    let rgb = |c: [f32; 4]| c.map(|x| (x * 255.0).round() as i64);

    h.look("lights.cue", "warm", Some(se_proto::PRIORITY_PRESET));
    wait_for("look on", 1.0, || near(h.f("lights.p1.intensity"), 0.5, 1e-6) && rgb(h.color("lights.p1.color"))[..3] == [255, 128, 0]).await;
    // dragging the colour knob: the running look follows, the file doesn't change yet
    h.knob(Origin::Ui, ("look", "warm"), "set.all.color", Value::from("#0000ff"), false);
    wait_for("dragged colour", 2.0, || rgb(h.color("lights.p2.color"))[..3] == [0, 0, 255]).await;
    assert_eq!(h.disk("lights/palettes/warm.toml"), WARM_KNOBS);
    // letting go saves it (comments kept); brightness snaps to its step
    h.knob(Origin::Ui, ("look", "warm"), "set.all.color", Value::from("#00FF00"), true);
    h.knob(Origin::Ui, ("look", "warm"), "set.all.intensity", Value::Float(0.26), true);
    wait_for("saved knobs on the lights", 2.0, || rgb(h.color("lights.p2.color"))[..3] == [0, 255, 0] && near(h.f("lights.p1.intensity"), 0.25, 1e-6)).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    let dmx = h.dmx().await;
    assert!(dmx[0] == 0 && (63..=64).contains(&dmx[1]) && dmx[2] == 0, "green at a quarter on the wire: {:?}", &dmx[..6]);
    let saved = h.disk("lights/palettes/warm.toml");
    assert!(saved.contains("all = { color = \"#00ff00\", intensity = 0.25 } # keep me") && saved.starts_with("# the warm wash\n"), "{saved}");
    let q = h.hub.query("lights.palettes", Value::Null).await.unwrap();
    let knobs = q.as_list().unwrap()[0].get_path("knobs").and_then(Value::as_list).unwrap();
    assert_eq!(knobs.iter().map(|k| k.get_path("value").cloned().unwrap()).collect::<Vec<_>>(), [Value::from("#00ff00"), Value::Float(0.25)]);
    assert_eq!(knobs[1].get_path("unit"), Some(&Value::from("%")));
    // the next firing (after the file reloads) uses the saved values
    h.look("lights.release", "warm", None);
    wait_for("look off", 1.0, || h.f("lights.p1.intensity") == 0.0).await;
    h.reload("lights/palettes", "warm", &saved);
    h.look("lights.cue", "warm", Some(se_proto::PRIORITY_PRESET));
    wait_for("fired with the saved knobs", 2.0, || rgb(h.color("lights.p1.color"))[..3] == [0, 255, 0] && near(h.f("lights.p2.intensity"), 0.25, 1e-6)).await;

    // a cue list's effect speed: the running effect takes the new rate, the file keeps it
    h.act(Origin::Deck, "lights.cue pulsing");
    wait_for("pulsing", 1.0, || h.hub.snapshot.load().bool("lights.effect.pulse.active")).await;
    assert!(near(h.f("lights.effect.pulse.rate"), 1.0, 1e-6));
    let q = h.hub.query("lights.cuelists", Value::Null).await.unwrap();
    assert_eq!(q.as_list().unwrap()[0].get_path("knobs.0.value"), Some(&Value::Float(1.0)), "unset = the effect's own rate");
    h.knob(Origin::Ui, ("cuelist", "pulsing"), "cue.1.effects.pulse.rate", Value::Int(4), true);
    wait_for("new rate", 2.0, || near(h.f("lights.effect.pulse.rate"), 4.0, 1e-6)).await;
    let saved = h.disk("lights/cuelists/pulsing.toml");
    assert!(saved.contains("effects = { pulse = { rate = 4.0 } } # breathing"), "{saved}");
    assert!(h.layer("lights.effect.pulse.rate", "cuelist:pulsing").await);

    // refused: chat, values that don't fit, unknown knobs
    let mut bus = h.hub.subscribe();
    h.knob(Origin::Chat, ("look", "warm"), "set.all.color", Value::from("#ff0000"), true);
    h.knob(Origin::Ui, ("look", "warm"), "set.all.color", Value::from("orange"), true);
    h.knob(Origin::Ui, ("look", "warm"), "set.all.nope", Value::Float(1.0), true);
    let mut logs = Vec::new();
    let end = Instant::now() + Duration::from_secs(2);
    while logs.len() < 2 && Instant::now() < end {
        let Ok(Ok(m)) = tokio::time::timeout(Duration::from_millis(100), bus.recv()).await else { continue };
        if let se_hub::Bus::Log { msg, target, .. } = &*m
            && target == "lights"
            && msg.starts_with("lights.knob")
        {
            logs.push(msg.clone());
        }
    }
    assert_eq!(logs, ["lights.knob: knob “Color” can't take `orange`", "lights.knob: look `warm` has no knob for `set.all.nope`"]);
    assert_eq!(rgb(h.color("lights.p1.color"))[..3], [0, 255, 0], "chat moved no knob");
    assert!(h.disk("lights/palettes/warm.toml").contains("#00ff00"));
}

#[tokio::test(flavor = "multi_thread")]
async fn layer_replacement_expiry_and_coverage_restore_underlying_playback() {
    let h = harness(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/palettes", "red", "[set]\nall = { color = \"#ff0000\", intensity = 0.6 }"),
        file("lights/palettes", "green", "[set]\nall = { color = \"#00ff00\" }"),
        file("lights/palettes", "blue", "[set]\nall = { color = \"#0000ff\" }"),
    ]).await;
    let is_color = |head: &str, channel: usize| {
        let color = h.color(&format!("lights.{head}.color"));
        color[channel] > 0.99 && color[(channel + 1) % 3] < 0.01 && color[(channel + 2) % 3] < 0.01
    };
    h.act(Origin::Ui, "lights.layer.select layer=base palette=red fade=0 owner=base");
    wait_for("red base", 1.0, || is_color("p1", 0) && is_color("p2", 0)).await;
    h.act(Origin::Ui, "lights.layer.select layer=rhythm palette=green coverage=p1 fade=0 owner=rhythm");
    wait_for("green rhythm with red uncovered head", 1.0, || is_color("p1", 1) && is_color("p2", 0)).await;
    h.act(Origin::Ui, "lights.layer.select layer=accent palette=blue coverage=p1 duration=250ms release=0 fade=0 owner=old");
    wait_for("blue accent", 1.0, || is_color("p1", 2)).await;
    h.act(Origin::Ui, "lights.layer.select layer=accent palette=red coverage=p1 duration=600ms release=0 fade=0 owner=new");
    wait_for("replacement accent", 1.0, || is_color("p1", 0)).await;
    tokio::time::sleep(Duration::from_millis(320)).await;
    assert!(is_color("p1", 0), "old accent expiry must not release its replacement");
    h.act(Origin::Ui, "lights.layer.release layer=accent owner=old fade=0");
    tokio::time::sleep(Duration::from_millis(60)).await;
    assert!(is_color("p1", 0), "stale explicit release must not release its replacement");
    wait_for("accent expiry restores rhythm", 1.0, || is_color("p1", 1) && is_color("p2", 0)).await;
    h.act(Origin::Ui, "lights.layer.set layer=rhythm coverage=p2 fade=0");
    wait_for("coverage moves without old channels sticking", 1.0, || is_color("p1", 0) && is_color("p2", 1)).await;
    h.act(Origin::Ui, "lights.layer.release layer=rhythm fade=0");
    wait_for("rhythm release restores base", 1.0, || is_color("p1", 0) && is_color("p2", 0)).await;
    h.wait_dmx(&[153, 0, 0, 153, 0, 0]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn preset_expiry_does_not_release_a_newer_base_selection() {
    let h = harness(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/palettes", "red", "[set]\nall = { color = \"#ff0000\", intensity = 0.4 }"),
        file("lights/palettes", "blue", "[set]\nall = { color = \"#0000ff\", intensity = 0.4 }"),
        file("presets", "old", "hold = \"300ms\"\nlights = { look = \"red\" }"),
        file("presets", "new", "toggle = true\nlights = { look = \"blue\" }"),
    ]).await;
    h.act(Origin::Deck, "preset.fire old");
    wait_for("preset red", 1.0, || h.color("lights.p1.color")[0] > 0.99).await;
    h.act(Origin::Deck, "preset.fire new");
    wait_for("preset blue", 1.0, || h.color("lights.p1.color")[2] > 0.99 && h.color("lights.p1.color")[0] < 0.01).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(h.color("lights.p1.color")[2] > 0.99, "expired old preset must not release new base");
    h.act(Origin::Deck, "preset.fire new");
    wait_for("new preset toggles off", 1.0, || h.f("lights.p1.intensity") == 0.0).await;
    h.wait_dmx(&[0; 6]).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn scene_release_cleans_failed_scene_selection_but_preserves_preset_owner() {
    let h = harness(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/palettes", "red", "[set]\nall = { color = \"#ff0000\", intensity = 0.4 }"),
        file("lights/palettes", "blue", "[set]\nall = { color = \"#0000ff\", intensity = 0.4 }"),
        file("scenes", "a_start", ""),
        file("scenes", "lit", "lights = { look = \"red\" }"),
        file("scenes", "bad", "lights = { look = \"missing\" }"),
        file("scenes", "unlit", ""),
        file("presets", "blue", "toggle = true\nlights = { look = \"blue\" }"),
    ]).await;
    let cut = |scene: &str| h.hub.command(Command::new(Origin::Ui, Op::SceneCut {
        scene: scene.into(), transition: Some("cut".into()),
    }));
    cut("lit");
    wait_for("scene lights on", 1.0, || near(h.f("lights.p1.intensity"), 0.4, 1e-6) && h.color("lights.p1.color")[0] > 0.99).await;
    cut("bad");
    wait_for("bad scene taken", 1.0, || h.s("show.scene.program") == "bad").await;
    tokio::time::sleep(Duration::from_millis(80)).await;
    assert!(h.color("lights.p1.color")[0] > 0.99 && h.f("lights.p1.intensity") > 0.39, "failed content leaves last successful scene look");
    cut("unlit");
    wait_for("unlit scene releases last successful scene owner", 1.0, || h.f("lights.p1.intensity") == 0.0).await;
    h.act(Origin::Deck, "preset.fire blue");
    wait_for("preset takes base", 1.0, || h.color("lights.p1.color")[2] > 0.99 && h.f("lights.p1.intensity") > 0.39).await;
    cut("bad");
    wait_for("bad scene retaken", 1.0, || h.s("show.scene.program") == "bad").await;
    cut("unlit");
    wait_for("unlit scene retaken", 1.0, || h.s("show.scene.program") == "unlit").await;
    tokio::time::sleep(Duration::from_millis(100)).await;
    let bytes = h.dmx().await;
    assert_eq!(&bytes[..6], &[0, 0, 102, 0, 0, 102], "scene release must not clear the newer preset owner");
}

#[tokio::test(flavor = "multi_thread")]
async fn effects_preserve_programmer_color_and_safety_caps() {
    let mut h = harness(vec![
        file("project", "project", &format!("{PROJECT}caps = {{ \"lights.*.intensity\" = [0.0, 0.45] }}\n")),
        file("lights", "rig", &rig("")),
        file("lights/palettes", "red", "[set]\nall = { color = \"#ff0000\", intensity = 0.8 }"),
        file("lights/effects", "chase", "kind = \"color_chase\"\nunit = \"hz\"\nrate = 8\ncolors = [\"#ff0000\", \"#0000ff\"]"),
        file("lights/effects", "sparkle", "kind = \"sparkle\"\nunit = \"hz\"\nrate = 10000\nsize = 1\nspread = 0"),
    ]).await;
    h.act(Origin::Ui, "lights.layer.select layer=base palette=red effects=sparkle fade=0");
    wait_for("sparkle running", 1.0, || h.hub.snapshot.load().bool("lights.effect.sparkle.active")).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let bytes = h.dmx().await;
    assert_eq!(bytes[0], 115, "dim-only sparkle preserves the global 45% intensity cap");
    h.act(Origin::Ui, "lights.layer.select layer=rhythm effects=chase fade=0");
    h.act(Origin::Ui, "lights.programmer.select p1");
    h.act(Origin::Ui, "lights.programmer.set attr=color value='#00ff00'");
    wait_for("programmer green", 1.0, || h.color("lights.p1.color")[1] > 0.99).await;
    h.wait_dmx(&[0, 115, 0]).await;
    for _ in 0..12 {
        tokio::time::sleep(Duration::from_millis(25)).await;
        let bytes = h.dmx().await;
        assert_eq!(&bytes[..3], &[0, 115, 0], "lower-priority color effect must not overwrite programmer green");
    }
    h.act(Origin::Ui, "lights.release all");
    h.hub.command(Command::new(Origin::Ui, Op::Action { name: "lights.programmer.release".into(), args: Value::Null }));
    wait_for("all layers released", 1.0, || h.f("lights.p1.intensity") == 0.0).await;
    h.reload("project", "project", PROJECT);
    h.act(Origin::Ui, "mode.set live");
    tokio::time::sleep(Duration::from_millis(150)).await;
    h.act(Origin::Chat, "lights.layer.select layer=base palette=red effects=sparkle fade=0");
    wait_for("chat sparkle running", 1.0, || h.hub.snapshot.load().bool("lights.effect.sparkle.active") && near(h.f("lights.p1.intensity"), 0.5, 1e-6)).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let bytes = h.dmx().await;
    assert_eq!(bytes[0], 128, "dim-only sparkle preserves the 50% chat intensity cap");
}

#[tokio::test(flavor = "multi_thread")]
async fn quantized_accent_uses_published_signals_and_starts_lifetime_at_boundary() {
    let h = harness(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/palettes", "red", "[set]\nall = { color = \"#ff0000\", intensity = 0.6 }"),
        file("lights/palettes", "blue", "[set]\nall = { color = \"#0000ff\" }"),
    ]).await;
    h.act(Origin::Ui, "lights.layer.select layer=base palette=red fade=0");
    h.wait_dmx(&[153, 0, 0, 153, 0, 0]).await;
    h.hub.signals(vec![
        ("beat.position".into(), 100.25), ("beat.phase".into(), 0.25),
        ("beat.bpm".into(), 60.0), ("beat.confidence".into(), 1.0),
    ]);
    wait_for("shared beat publication", 1.0, || h.hub.snapshot.load().signal("beat.position") == Some(100.25)).await;
    h.act(Origin::Ui, "lights.layer.select layer=accent palette=blue quantize=4 duration=250ms fade=0 release=0");
    wait_for("accent queued on shared boundary", 1.0, || {
        h.hub.snapshot.load().get("lights.layer.accent.state")
            .and_then(|v| v.get_path("pending.beat")).and_then(Value::as_f64) == Some(104.0)
    }).await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    let state = h.hub.snapshot.load();
    assert_eq!(state.get("lights.layer.accent.state").and_then(|v| v.get_path("active")), Some(&Value::Bool(false)));
    assert_eq!(state.get("lights.layer.accent.state").and_then(|v| v.get_path("pending.beat")).and_then(Value::as_f64), Some(104.0), "queued lifetime has not started");
    drop(state);
    h.wait_dmx(&[153, 0, 0, 153, 0, 0]).await;
    h.hub.signals(vec![("beat.position".into(), 104.1), ("beat.phase".into(), 0.1)]);
    h.wait_dmx(&[0, 0, 153, 0, 0, 153]).await;
    h.wait_dmx(&[153, 0, 0, 153, 0, 0]).await;
}

#[tokio::test]
async fn blackout_and_panic_override_rehearsal_on_the_wire() {
    async fn wire(sock: &tokio::net::UdpSocket, expected: [u8; 6], stage: &str) {
        let mut bytes = [0; 1024];
        let mut last = [0; 6];
        while sock.try_recv(&mut bytes).is_ok() {}
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let n = sock.recv(&mut bytes).await.unwrap();
                if n == 638 {
                    last.copy_from_slice(&bytes[126..132]);
                    if last == expected { return; }
                }
            }
        }).await.unwrap_or_else(|_| panic!("{stage}: physical output expected {expected:?}, last {last:?}"));
    }
    let sock = tokio::net::UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let port = sock.local_addr().unwrap().port();
    let out = format!("[output]\narmed = true\n[outputs.net]\nkind = \"sacn\"\nuniverses = [1]\ndestination = \"127.0.0.1\"\nport = {port}\n[safety]\nsafe_look = {{ intensity = 0.0 }}\n");
    let h = harness(vec![file("project", "project", PROJECT), file("lights", "rig", &rig(&out))]).await;
    h.act(Origin::Cli, "set lights.p1.color #ff0000");
    h.act(Origin::Cli, "set lights.p1.intensity 0.8");
    wire(&sock, [204, 0, 0, 0, 0, 0], "live red").await;
    h.act(Origin::Cli, "mode.set rehearsal");
    wait_for("rehearsal hold", 1.0, || h.lights.shared.rehearsal.load(std::sync::atomic::Ordering::Acquire)).await;
    h.act(Origin::Cli, "set lights.p1.color #0000ff");
    h.wait_dmx(&[0, 0, 204, 0, 0, 0]).await;
    wire(&sock, [204, 0, 0, 0, 0, 0], "held red while preview blue").await;
    h.act(Origin::Cli, "set lights.blackout true");
    wire(&sock, [0; 6], "blackout bypasses hold").await;
    h.act(Origin::Cli, "set lights.blackout false");
    h.wait_dmx(&[0, 0, 204, 0, 0, 0]).await;
    wire(&sock, [0; 6], "clearing blackout preserves held darkness").await;
    h.act(Origin::Cli, "mode.set live");
    wire(&sock, [0, 0, 204, 0, 0, 0], "live blue after leaving rehearsal").await;
    h.act(Origin::Cli, "mode.set rehearsal");
    wait_for("second rehearsal hold", 1.0, || h.lights.shared.rehearsal.load(std::sync::atomic::Ordering::Acquire)).await;
    h.act(Origin::Cli, "panic");
    wire(&sock, [0; 6], "panic bypasses held blue").await;
}

fn layer_state(h: &H, slot: &str) -> Value {
    h.hub.snapshot.load().get(&format!("lights.layer.{slot}.state")).cloned().unwrap_or(Value::Null)
}

/// Send `lights.layer.pick` and return the picked palette or cue list once the layer changed.
async fn pick(h: &H, origin: Origin, slot: &str, args: &str) -> String {
    let generation = |h: &H| layer_state(h, slot).get_path("generation").and_then(Value::as_i64);
    let before = generation(h);
    h.act(origin, &format!("lights.layer.pick layer={slot} {args}"));
    wait_for(&format!("pick {slot} {args}"), 1.0, || generation(h) != before).await;
    let sel = layer_state(h, slot).get_path("selection").cloned().unwrap();
    sel.get_path("palette").and_then(Value::as_str).or_else(|| sel.get_path("cuelist").and_then(Value::as_str)).unwrap().to_string()
}

async fn next_lights_log(bus: &mut tokio::sync::broadcast::Receiver<Arc<se_hub::Bus>>, prefix: &str) -> (String, String) {
    let end = Instant::now() + Duration::from_secs(2);
    while Instant::now() < end {
        let Ok(Ok(m)) = tokio::time::timeout(Duration::from_millis(100), bus.recv()).await else { continue };
        if let se_hub::Bus::Log { level, msg, target, .. } = &*m
            && target == "lights"
            && msg.starts_with(prefix)
        {
            return (level.clone(), msg.clone());
        }
    }
    panic!("no lights log starting with {prefix:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn layer_pick_chooses_tagged_content_and_applies_it_like_select() {
    let h = harness(vec![
        file("project", "project", PROJECT),
        file("lights", "rig", &rig("")),
        file("lights/palettes", "red", "tags = [\"heavy\", \"warm\", \"red\"]\n[set]\nall = { color = \"#ff0000\", intensity = 0.6 }"),
        file("lights/palettes", "blue", "tags = [\"heavy\", \"cool\"]\n[set]\nall = { color = \"#0000ff\", intensity = 0.6 }"),
        file("lights/palettes", "green", "tags = [\"chill\", \"cool\"]\n[set]\nall = { color = \"#00ff00\", intensity = 0.6 }"),
        file("lights/palettes", "plain", "[set]\nall = { color = \"#ffffff\" }"),
        file("lights/cuelists", "strobe", "tags = [\"heavy\", \"hit\"]\n[[cue]]\nset = { all = { color = \"#ffffff\", intensity = 1.0 } }"),
        file("lights/effects", "chase", "tags = [\"Hype\", \"heavy\"]\nkind = \"color_chase\"\nrate = 2\ncolors = [\"#ff0000\", \"#0000ff\"]"),
    ]).await;
    let tags = h.hub.query("lights.tags", Value::Null).await.unwrap();
    let names = |t: &str| tags.get_path(t).and_then(Value::as_list).unwrap().iter().filter_map(Value::as_str).map(String::from).collect::<Vec<_>>();
    assert_eq!(names("heavy"), ["blue", "chase", "red", "strobe"]);
    assert_eq!(names("hype"), ["chase"], "tags are lowercased");
    assert_eq!(names("cool"), ["blue", "green"]);

    // Required tags: only `green` is chill.
    assert_eq!(pick(&h, Origin::Ui, "base", "tags=chill fade=0").await, "green");
    // Preferred tags win (history off): red is the only heavy palette that is also red.
    for _ in 0..5 {
        assert_eq!(pick(&h, Origin::Ui, "base", "tags=heavy any=red kind=palette avoid_repeat=0 fade=0").await, "red");
    }
    // Avoid repeats (default 2) on a fresh layer: red first, then both other heavy items, then red.
    let mut seq = Vec::new();
    for _ in 0..4 {
        seq.push(pick(&h, Origin::Ui, "rhythm", "tags=heavy any=red fade=0").await);
    }
    assert_eq!(seq[0], "red");
    let mut middle = vec![seq[1].clone(), seq[2].clone()];
    middle.sort();
    assert_eq!(middle, ["blue", "strobe"], "{seq:?}");
    assert_eq!(seq[3], "red", "{seq:?}");

    // No candidate: Ok no-op with an info line; the layer is untouched.
    let mut bus = h.hub.subscribe();
    let before = layer_state(&h, "base");
    h.act(Origin::Rule, "lights.layer.pick layer=base tags=missing");
    let (level, msg) = next_lights_log(&mut bus, "lights.layer.pick base:").await;
    assert_eq!(level, "info", "{msg}");
    assert!(msg.contains("no usable"), "{msg}");
    assert_eq!(layer_state(&h, "base"), before);

    // Applied like select: owner token, explicit priority, release by owner.
    assert_eq!(pick(&h, Origin::Rule, "accent", "tags=hit duration=5s release=0 fade=0 owner=context priority=230").await, "strobe");
    let accent = layer_state(&h, "accent");
    assert_eq!(accent.get_path("source_owner").and_then(Value::as_str), Some("context"));
    assert_eq!(accent.get_path("priority").and_then(Value::as_i64), Some(230));
    h.act(Origin::Rule, "lights.layer.release layer=accent owner=context priority=230 fade=0");
    wait_for("accent released by owner", 1.0, || layer_state(&h, "accent").get_path("active") == Some(&Value::Bool(false))).await;

    // Ownership: chat cannot pick over the operator's base layer.
    h.act(Origin::Ui, "lights.layer.select layer=base palette=blue fade=0 owner=operator");
    wait_for("operator blue", 1.0, || layer_state(&h, "base").get_path("selection.palette").and_then(Value::as_str) == Some("blue")).await;
    h.act(Origin::Ui, "mode.set live");
    tokio::time::sleep(Duration::from_millis(150)).await;
    let mut bus = h.hub.subscribe();
    h.act(Origin::Chat, "lights.layer.pick layer=base tags=chill fade=0");
    let (level, msg) = next_lights_log(&mut bus, "lights.layer.pick:").await;
    assert_eq!(level, "warn");
    assert!(msg.contains("belongs to"), "{msg}");
    assert_eq!(layer_state(&h, "base").get_path("selection.palette").and_then(Value::as_str), Some("blue"));
}

#[tokio::test(flavor = "multi_thread")]
async fn native_idle_restores_main_after_restart_show_release_and_blackout() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use serde_json::json;
    use std::sync::atomic::{AtomicBool, Ordering};

    // An isolated real TCP EP10 protocol peer starts off, as after a show/crash.
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let on = Arc::new(AtomicBool::new(false));
    let relay = on.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            loop {
                let mut header = [0; 4];
                if stream.read_exact(&mut header).await.is_err() { break; }
                let len = u32::from_be_bytes(header) as usize;
                assert!(len <= 64 * 1024);
                let mut payload = vec![0; len];
                stream.read_exact(&mut payload).await.unwrap();
                let mut key = 0xab;
                for byte in &mut payload {
                    let encrypted = *byte;
                    *byte ^= key;
                    key = encrypted;
                }
                let request: serde_json::Value = serde_json::from_slice(&payload).unwrap();
                let response = if let Some(state) = request.pointer("/system/set_relay_state/state").and_then(serde_json::Value::as_u64) {
                    relay.store(state != 0, Ordering::Release);
                    json!({"system":{"set_relay_state":{"err_code":0}}})
                } else {
                    assert!(request.pointer("/system/get_sysinfo").is_some());
                    json!({"system":{"get_sysinfo":{
                        "err_code":0, "mac":"B4:B0:24:69:F2:7B", "alias":"Main", "model":"EP10(US)",
                        "relay_state":u8::from(relay.load(Ordering::Acquire))
                    }}})
                };
                let payload = serde_json::to_vec(&response).unwrap();
                let mut frame = Vec::with_capacity(4 + payload.len());
                frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
                let mut key = 0xab;
                for byte in payload { key ^= byte; frame.push(key); }
                stream.write_all(&frame).await.unwrap();
            }
        }
    });
    let socket = tokio::net::UdpSocket::bind(("127.0.0.1", 0)).await.unwrap();
    let dmx_port = socket.local_addr().unwrap().port();
    let extra = format!(r##"
[output]
armed = false
[outputs.net]
kind = "sacn"
enabled = true
destination = "127.0.0.1"
port = {dmx_port}
universes = [1]
[main_light]
enabled = true
host = "127.0.0.1"
port = {port}
mac = "B4:B0:24:69:F2:7B"
idle_on = true
[idle]
target = "p1"
color = "#ff69b4"
intensity = 0.3
[safety]
max_intensity = 0.35
safe_look = {{ intensity = 0.0 }}
strobe = "block"
"##);
    let mut h = harness(vec![
        file("project", "", PROJECT),
        file("lights", "rig", &rig(&extra)),
        file("lights/cuelists", "show", "[[cue]]\nfade = 0\n[cue.set]\np2 = { intensity = 0.3, color = \"#0000ff\" }"),
    ]).await;
    h.wait_dmx(&[77, 32, 54, 0, 0, 0]).await;
    assert!(!on.load(Ordering::Acquire), "disarmed preview must not switch the plug");
    h.reload("lights", "rig", &rig(&extra.replace("armed = false", "armed = true")));
    wait_for("Main restored from off at startup", 2.0, || on.load(Ordering::Acquire)).await;
    let mut wire = [0; 1024];
    let len = tokio::time::timeout(Duration::from_secs(1), socket.recv(&mut wire)).await.unwrap().unwrap();
    assert!(len >= 132);
    assert_eq!(&wire[126..132], &[77, 32, 54, 0, 0, 0]);
    h.act(Origin::Ui, "lights.layer.select layer=base cuelist=show fade=0");
    h.wait_dmx(&[0, 0, 0, 0, 0, 77]).await;
    wait_for("Main off for authored show", 2.0, || !on.load(Ordering::Acquire)).await;
    h.act(Origin::Ui, "lights.layer.release layer=base fade=0");
    h.wait_dmx(&[77, 32, 54, 0, 0, 0]).await;
    wait_for("Main on after release", 2.0, || on.load(Ordering::Acquire)).await;
    h.act(Origin::Ui, "set lights.blackout true");
    h.wait_dmx(&[0; 6]).await;
    h.act(Origin::Ui, "set lights.blackout false");
    h.wait_dmx(&[77, 32, 54, 0, 0, 0]).await;
    h.act(Origin::Ui, "lights.panic");
    h.wait_dmx(&[0; 6]).await;
    assert!(on.load(Ordering::Acquire));
    drop(h);
    server.abort();
}
