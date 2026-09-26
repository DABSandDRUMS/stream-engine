//! End-to-end tests of the patch loader and the Lua runtime against a real core thread.

use se_core::{Config, Core};
use se_hub::draw::{DrawList, DrawOp, DrawReader};
use se_hub::{Bus, EngineCtx, Hub, RunnerHooks, run_core};
use se_proto::{Command, Op, Origin, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Engine {
    hub: Arc<Hub>,
    dir: tempfile::TempDir,
    readers: HashMap<String, DrawReader>,
    core: Option<std::thread::JoinHandle<()>>,
    patches: se_patch::Patches,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.hub.shutdown();
        if let Some(t) = self.core.take() {
            let _ = t.join();
        }
    }
}

fn repo() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, body).unwrap();
}

async fn engine(files: &[(&str, &str)]) -> Engine {
    let dir = tempfile::tempdir().unwrap();
    for (rel, body) in files {
        write(dir.path(), rel, body);
    }
    let clock = Arc::new(se_clock::Clock::new());
    let (hub, rx) = Hub::new(clock);
    let core = Core::new(Config::default(), se_clock::now());
    let h2 = hub.clone();
    let core = std::thread::spawn(move || run_core(core, h2, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 1, on_runtime: None }));
    if std::env::var_os("SE_TEST_LOG").is_some() {
        let mut bus = hub.subscribe();
        tokio::spawn(async move {
            while let Ok(b) = bus.recv().await {
                if let Bus::Log { level, target, msg, .. } = &*b {
                    eprintln!("[{level}] {target}: {msg}");
                }
            }
        });
    }
    let db = se_store::Db::open(&dir.path().join("runtime.db")).unwrap();
    let (tx, cfg) = tokio::sync::watch::channel(Arc::new(Config::default()));
    std::mem::forget(tx);
    let ctx = EngineCtx {
        hub: hub.clone(),
        db,
        project_root: dir.path().to_path_buf(),
        data_dir: dir.path().join("data"),
        share_dir: repo(),
        config: cfg,
        http: "127.0.0.1:0".parse().unwrap(),
        dev: true,
    };
    let patches = se_patch::start(ctx).await.unwrap();
    Engine { hub, dir, readers: HashMap::new(), core: Some(core), patches }
}

async fn wait_for(what: &str, timeout: Duration, mut f: impl FnMut() -> bool) {
    let t0 = Instant::now();
    while !f() {
        assert!(t0.elapsed() < timeout, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

impl Engine {
    fn state(&self, id: &str) -> String {
        self.hub.snapshot.load().str(&format!("patch.{id}.state")).unwrap_or_default().to_string()
    }

    fn error(&self, id: &str) -> String {
        self.hub.snapshot.load().str(&format!("patch.{id}.error")).unwrap_or_default().to_string()
    }

    async fn wait_state(&self, id: &str, st: &str, timeout: Duration) {
        let t0 = Instant::now();
        while self.state(id) != st {
            if t0.elapsed() > timeout {
                let all = self.hub.get(&format!("patch.{id}.**"), false).await;
                panic!("timed out waiting for patch.{id}.state = {st} (now `{}`: {}); state: {all:?}", self.state(id), self.error(id));
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn draw(&mut self, id: &str) -> Option<DrawList> {
        for (n, r) in self.hub.draw.take_new() {
            self.readers.insert(n, r);
        }
        self.readers.get_mut(&format!("patch.{id}")).map(|r| r.latest().clone())
    }

    async fn wait_draw(&mut self, id: &str, timeout: Duration, mut f: impl FnMut(&DrawList) -> bool) -> DrawList {
        let t0 = Instant::now();
        loop {
            if let Some(d) = self.draw(id)
                && f(&d)
            {
                return d;
            }
            assert!(t0.elapsed() < timeout, "timed out waiting for a draw list from {id} (latest {:?})", self.draw(id).map(|d| (d.seq, d.ops.len())));
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    async fn query(&self, id: &str) -> Value {
        let v = self.hub.query("patches", Value::map().with("id", id)).await.unwrap();
        v.as_list().and_then(|l| l.first().cloned()).unwrap_or(Value::Null)
    }

    async fn run(&self, text: &str) {
        self.hub.exec(Command::new(Origin::Cli, Op::parse(text).unwrap())).await.unwrap();
    }
}

fn circles(d: &DrawList) -> usize {
    d.ops.iter().filter(|o| matches!(o, DrawOp::Circle { .. })).count()
}

const T: Duration = Duration::from_secs(3);

#[tokio::test(flavor = "multi_thread")]
async fn sub_meteors_trigger_produces_draw_lists_and_flash() {
    let ex = repo().join("templates/patches/script/sub_meteors");
    let toml = std::fs::read_to_string(ex.join("patch.toml")).unwrap();
    let lua = std::fs::read_to_string(ex.join("main.lua")).unwrap();
    let mut e = engine(&[("patches/sub_meteors/patch.toml", &toml), ("patches/sub_meteors/main.lua", &lua)]).await;
    e.wait_state("sub_meteors", "loaded", T).await;

    let q = e.query("sub_meteors").await;
    assert_eq!(q.get_path("kind").and_then(Value::as_str), Some("script"));
    assert_eq!(q.get_path("layer").and_then(Value::as_str), Some("overlay"));
    assert_eq!(q.get_path("trigger"), Some(&Value::Bool(true)));
    let names: Vec<&str> = q.get_path("params").and_then(Value::as_list).unwrap().iter().filter_map(|p| p.get_path("name").and_then(Value::as_str)).collect();
    assert_eq!(names, vec!["count", "speed"]);
    // params are declared with their Meta, the trigger envelope exists
    assert_eq!(e.hub.snapshot.load().get("patch.sub_meteors.count"), Some(&Value::Int(40)));
    assert_eq!(e.hub.snapshot.load().f32("patch.sub_meteors.env"), Some(0.0));

    let mut bus = e.hub.subscribe();
    e.run("patch.sub_meteors.trigger tier=2").await;

    // 40 × tier 2 meteors, each a circle in the accent color at alpha = env
    let d = e.wait_draw("sub_meteors", T, |d| circles(d) == 80).await;
    assert!(matches!(d.ops[0], DrawOp::Clear(c) if c == [0.0; 4]), "frame starts with a clear: {:?}", d.ops[0]);
    let DrawOp::Circle { center, radius, color, .. } = &d.ops[1] else { panic!("expected circles, got {:?}", d.ops[1]) };
    assert!((0.0..=1.0).contains(&center[0]) && center[1] < 0.2, "meteors start above the top: {center:?}");
    assert!((radius - 0.006).abs() < 1e-6);
    let accent = se_patch::script::vm::PALETTE[0].2;
    assert_eq!(&color[..3], &accent[..3]);
    // after the 100 ms attack the envelope (alpha) is full
    let d = e.wait_draw("sub_meteors", T, |d| d.ops.iter().any(|o| matches!(o, DrawOp::Circle { color, .. } if (color[3] - accent[3]).abs() < 1e-3))).await;
    assert_eq!(circles(&d), 80);
    // meteors fall
    let y0 = match &d.ops[1] {
        DrawOp::Circle { center, .. } => center[1],
        _ => unreachable!(),
    };
    tokio::time::sleep(Duration::from_millis(200)).await;
    let d2 = e.draw("sub_meteors").unwrap();
    assert!(d2.seq > d.seq);
    let y1 = match &d2.ops[1] {
        DrawOp::Circle { center, .. } => center[1],
        other => panic!("{other:?}"),
    };
    assert!(y1 > y0, "meteors move down ({y0} → {y1})");

    // the handler fired lights.flash with the palette accent (origin patch)
    let t0 = Instant::now();
    let flash = loop {
        assert!(t0.elapsed() < T, "no lights.flash event");
        match tokio::time::timeout(Duration::from_millis(100), bus.recv()).await {
            Ok(Ok(b)) => {
                if let Bus::Event(ev) = &*b
                    && ev.ty == "lights.flash"
                {
                    break ev.clone();
                }
            }
            _ => continue,
        }
    };
    assert_eq!(flash.origin, Origin::Patch);
    assert_eq!(flash.payload.get_path("ms"), Some(&Value::Int(400)));
    assert_eq!(flash.payload.get_path("color").and_then(Value::as_color).map(|c| c[0]), Some(accent[0]));
}

const STEADY: &str = "function frame(dt, s)\n  draw.clear()\n  draw.rect(0, 0, 0.5, 0.5, \"#ff0000\")\nend\n";
const MANIFEST: &str = "kind = \"script\"\nlayer = \"overlay\"\ntrigger = { hold = \"1s\" }\n";

fn rect_color(d: &DrawList) -> Option<[f32; 4]> {
    d.ops.iter().find_map(|o| match o {
        DrawOp::Rect { color, .. } => Some(*color),
        _ => None,
    })
}

#[tokio::test(flavor = "multi_thread")]
async fn broken_script_keeps_old_version_live_and_reports_the_line() {
    let mut e = engine(&[("patches/bl/patch.toml", MANIFEST), ("patches/bl/main.lua", STEADY)]).await;
    e.wait_state("bl", "loaded", T).await;
    e.wait_draw("bl", T, |d| rect_color(d) == Some([1.0, 0.0, 0.0, 1.0])).await;

    // syntax error on line 3
    write(e.dir.path(), "patches/bl/main.lua", "function frame(dt, s)\n  draw.clear()\n  local x = = 1\nend\n");
    e.wait_state("bl", "error", T).await;
    let err = e.error("bl");
    assert!(err.starts_with("main.lua:3:"), "{err}");
    let q = e.query("bl").await;
    assert_eq!(q.get_path("error_file").and_then(Value::as_str), Some("main.lua"));
    assert_eq!(q.get_path("error_line"), Some(&Value::Int(3)));
    assert_eq!(q.get_path("live"), Some(&Value::Bool(true)));
    // the old version keeps drawing
    let before = e.draw("bl").unwrap().seq;
    let d = e.wait_draw("bl", T, |d| d.seq > before + 5).await;
    assert_eq!(rect_color(&d), Some([1.0, 0.0, 0.0, 1.0]));

    // fixed → the new version replaces it
    write(e.dir.path(), "patches/bl/main.lua", &STEADY.replace("#ff0000", "#0000ff"));
    e.wait_state("bl", "loaded", T).await;
    assert_eq!(e.error("bl"), "");
    e.wait_draw("bl", T, |d| rect_color(d) == Some([0.0, 0.0, 1.0, 1.0])).await;

    // runtime error: reported with its line, the patch keeps running
    write(e.dir.path(), "patches/bl/main.lua", "function frame(dt, s)\n  missing_fn()\nend\n");
    e.wait_state("bl", "error", T).await;
    let err = e.error("bl");
    assert!(err.starts_with("main.lua:2:") && err.contains("missing_fn"), "{err}");

    // errors inside API calls point at the calling line
    write(e.dir.path(), "patches/bl/main.lua", "function frame(dt, s)\n  draw.clear()\n\n  draw.circle(0.5, 0.5, 0.1, \"not a color\")\nend\n");
    wait_for("draw.circle error", T, || e.error("bl").starts_with("main.lua:4:")).await;
    assert!(e.error("bl").contains("bad color"), "{}", e.error("bl"));

    // a broken manifest also keeps the running version and reports patch.toml:<line>
    write(e.dir.path(), "patches/bl/main.lua", STEADY);
    e.wait_state("bl", "loaded", T).await;
    write(e.dir.path(), "patches/bl/patch.toml", "kind = \"script\"\nlayer = \n");
    e.wait_state("bl", "error", T).await;
    let q = e.query("bl").await;
    assert_eq!(q.get_path("error_file").and_then(Value::as_str), Some("patch.toml"));
    assert_eq!(q.get_path("error_line"), Some(&Value::Int(2)));
    let before = e.draw("bl").unwrap().seq;
    e.wait_draw("bl", T, |d| d.seq > before + 5).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn infinite_loop_is_suspended_without_stalling_others() {
    let looping = "on(\"trigger\", function(e)\n  while true do end\nend)\nfunction frame(dt, s) draw.clear() draw.circle(0.5, 0.5, 0.1, \"#00ff00\") end\n";
    let co = "on(\"trigger\", function(e)\n  local c = coroutine.wrap(function() while true do end end)\n  pcall(c)\nend)\n";
    let mut e = engine(&[
        ("patches/looper/patch.toml", MANIFEST),
        ("patches/looper/main.lua", looping),
        ("patches/co/patch.toml", MANIFEST),
        ("patches/co/main.lua", co),
        ("patches/steady/patch.toml", MANIFEST),
        ("patches/steady/main.lua", STEADY),
    ])
    .await;
    for id in ["looper", "co", "steady"] {
        e.wait_state(id, "loaded", T).await;
    }
    e.wait_draw("steady", T, |d| d.seq > 3).await;
    e.wait_draw("looper", T, |d| circles(d) == 1).await;

    // sample the steady patch's frame sequence while the others loop forever
    let t0 = Instant::now();
    e.run("patch.looper.trigger").await;
    e.run("patch.co.trigger").await;
    let mut seqs = Vec::new();
    while t0.elapsed() < Duration::from_millis(600) {
        seqs.push((t0.elapsed(), e.draw("steady").unwrap().seq));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    e.wait_state("looper", "suspended", T).await;
    e.wait_state("co", "suspended", T).await;
    assert!(e.error("looper").contains("budget"), "{}", e.error("looper"));
    assert!(e.error("co").contains("budget"), "{}", e.error("co"));
    // steady kept rendering at ~60 fps the whole time (no gap longer than 100 ms)
    let frames = seqs.last().unwrap().1 - seqs.first().unwrap().1;
    assert!(frames >= 20, "steady produced only {frames} frames in 600 ms");
    let mut last_change = seqs[0];
    for w in seqs.windows(2) {
        if w[1].1 != w[0].1 {
            assert!(w[1].0 - last_change.0 < Duration::from_millis(100), "steady stalled: {:?}", w);
            last_change = w[1];
        }
    }
    // a suspended patch's output is cleared; enabling resumes it
    e.wait_draw("looper", T, |d| d.ops.is_empty()).await;
    e.run("patch.enable looper").await;
    e.wait_state("looper", "loaded", T).await;
    e.wait_draw("looper", T, |d| circles(d) == 1).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sustained_overrun_is_suspended_and_top_level_loops_fail_the_load() {
    // ~20k interpreter iterations per frame (≈0.1 ms, far below the 25 ms hard cap) against a
    // 0.02 ms budget
    let heavy = "function frame(dt, s)\n  local x = 0\n  for i = 1, 20000 do x = x + i end\n  draw.clear()\nend\n";
    let e = engine(&[
        ("patches/heavy/patch.toml", "kind = \"script\"\nbudget = { cpu_ms = 0.02 }\n"),
        ("patches/heavy/main.lua", heavy),
        ("patches/hang/patch.toml", "kind = \"script\"\n"),
        ("patches/hang/main.lua", "local n = 0\nwhile true do n = n + 1 end\n"),
    ])
    .await;
    e.wait_state("heavy", "suspended", T).await;
    assert!(e.error("heavy").contains("over CPU budget"), "{}", e.error("heavy"));
    e.wait_state("hang", "error", T).await;
    assert!(e.error("hang").contains("top-level code over budget"), "{}", e.error("hang"));
    assert_eq!(e.query("hang").await.get_path("live"), Some(&Value::Bool(false)));
}

#[tokio::test(flavor = "multi_thread")]
async fn sandbox_api_signals_and_modules() {
    let main = r##"
assert(os == nil and io == nil and debug == nil and package == nil and ffi == nil and jit == nil)
assert(load == nil and loadstring == nil and dofile == nil and loadfile == nil and string.dump == nil)
assert(not pcall(require, "../escape"))
assert(not pcall(string.rep, "x", 1e9))
local util = require("util")
assert(util.double(21) == 42)
local got = {}
on("demo.*", function(p, ev)
  got[#got + 1] = ev.type
  set("patch.api.speed", p.speed)
  emit("api.seen", { n = #got, from = ev.origin, user = ev.actor and ev.actor.name })
end)
function frame(dt, s)
  draw.clear()
  signal("level", s.band.kick.value + s.band.kick.env)
  draw.text(tostring(params.speed), 0.1, 0.1, 0.05, palette.foreground, 1, { align = "center" })
  draw.push({ x = 0.5, y = 0.5, rotate = 1, scale = 2 })
  draw.line(0, 0, 1, 1, 0.01, { 1, 1, 1 })
  draw.path({ 0, 0, 1, 0, 1, 1 }, "#ffffff80", 1, { close = true, stroke = 0.002 })
  draw.image("logo.png", 0, 0, 0.1, 0.1, 0.5)
end
"##;
    let manifest = "kind = \"script\"\nparams.speed = { type = \"float\", default = 1.5, range = [0, 10] }\n";
    let mut e = engine(&[]).await;
    // signals exist before the patch loads (a leaf `band.kick` and a subtree `band.kick.env`)
    e.hub.signals(vec![("band.kick".into(), 0.25), ("band.kick.env".into(), 0.5)]);
    wait_for("signals", T, || e.hub.snapshot.load().signal("band.kick.env").is_some()).await;
    write(e.dir.path(), "patches/api/util.lua", "return { double = function(x) return x * 2 end }\n");
    write(e.dir.path(), "patches/api/main.lua", main);
    write(e.dir.path(), "patches/api/patch.toml", manifest);
    e.wait_state("api", "loaded", T).await;

    let d = e.wait_draw("api", T, |d| d.ops.len() >= 7).await;
    let DrawOp::Text { text, align, .. } = &d.ops[1] else { panic!("{:?}", d.ops) };
    assert_eq!(text, "1.5");
    assert_eq!(*align, se_hub::draw::TextAlign::Center);
    assert!(matches!(d.ops[2], DrawOp::Push { rotate, scale, .. } if rotate == 1.0 && scale == [2.0, 2.0]));
    assert!(matches!(&d.ops[4], DrawOp::Path { cmds, paint: se_hub::draw::Paint::Stroke(_), .. } if cmds.len() == 4));
    assert!(matches!(&d.ops[5], DrawOp::Image { path, opacity, .. } if path == "logo.png" && *opacity == 0.5));
    // the unbalanced push is closed automatically
    assert!(matches!(d.ops.last(), Some(DrawOp::Pop)));
    // leaf + subtree signal names coexist (`band.kick` as `.value`), published under the patch
    wait_for("patch.api.level signal", T, || e.hub.snapshot.load().signal("patch.api.level").is_some_and(|v| (v - 0.75).abs() < 1e-6)).await;

    // events matching `on` patterns reach the handler with payload + event info
    let mut bus = e.hub.subscribe();
    e.run("emit demo.ping speed=3").await;
    wait_for("set via patch", T, || e.hub.snapshot.load().f32("patch.api.speed") == Some(3.0)).await;
    let t0 = Instant::now();
    loop {
        assert!(t0.elapsed() < T, "no api.seen event");
        if let Ok(Ok(b)) = tokio::time::timeout(Duration::from_millis(100), bus.recv()).await
            && let Bus::Event(ev) = &*b
            && ev.ty == "api.seen"
        {
            assert_eq!(ev.payload.get_path("n"), Some(&Value::Int(1)));
            assert_eq!(ev.payload.get_path("from").and_then(Value::as_str), Some("cli"));
            assert!(ev.causal.is_some(), "commands from handlers carry the causing event");
            break;
        }
    }
    e.wait_draw("api", T, |d| matches!(&d.ops.get(1), Some(DrawOp::Text { text, .. }) if text == "3")).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn folders_hot_load_disable_and_new_patch() {
    let mut e = engine(&[]).await;
    // drop a new folder in
    write(e.dir.path(), "patches/late/main.lua", STEADY);
    write(e.dir.path(), "patches/late/patch.toml", MANIFEST);
    e.wait_state("late", "loaded", T).await;
    let all = e.hub.query("patches", Value::Null).await.unwrap();
    assert_eq!(all.as_list().unwrap().len(), 1);
    e.wait_draw("late", T, |d| rect_color(d).is_some()).await;

    // disable → cleared output, state disabled, persisted; enable → back
    e.run("patch.disable late").await;
    e.wait_state("late", "disabled", T).await;
    e.wait_draw("late", T, |d| d.ops.is_empty()).await;
    assert_eq!(e.query("late").await.get_path("enabled"), Some(&Value::Bool(false)));
    e.run("patch.enable late").await;
    e.wait_state("late", "loaded", T).await;
    e.wait_draw("late", T, |d| rect_color(d).is_some()).await;

    // New patch from the shipped templates
    e.hub.exec(Command::new(Origin::Ui, Op::Action { name: "patch.new".into(), args: Value::map().with("id", "fresh").with("kind", "script") })).await.unwrap();
    e.wait_state("fresh", "loaded", T).await;
    assert!(e.dir.path().join("patches/fresh/main.lua").is_file());
    let q = e.query("fresh").await;
    assert_eq!(q.get_path("label").and_then(Value::as_str), Some("Fresh"));

    // removing the folder removes the patch and its addresses
    std::fs::remove_dir_all(e.dir.path().join("patches/late")).unwrap();
    wait_for("late removed", T, || e.hub.snapshot.load().get("patch.late.state").is_none()).await;
    let all = e.hub.query("patches", Value::Null).await.unwrap();
    let ids: Vec<&str> = all.as_list().unwrap().iter().filter_map(|p| p.get_path("id").and_then(Value::as_str)).collect();
    assert_eq!(ids, vec!["fresh"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn optional_confetti_and_hype_meter_templates_run() {
    let ex = repo().join("templates/patches/script");
    let mut files: Vec<(String, String)> = Vec::new();
    for id in ["confetti", "hype_meter"] {
        for f in ["patch.toml", "main.lua"] {
            files.push((format!("patches/{id}/{f}"), std::fs::read_to_string(ex.join(id).join(f)).unwrap()));
        }
    }
    let refs: Vec<(&str, &str)> = files.iter().map(|(a, b)| (a.as_str(), b.as_str())).collect();
    let mut e = engine(&refs).await;
    e.wait_state("confetti", "loaded", T).await;
    e.wait_state("hype_meter", "loaded", T).await;

    // confetti: nothing until triggered, then rotated pieces (push → rect → pop)
    let d = e.wait_draw("confetti", T, |d| d.seq > 2).await;
    assert_eq!(d.ops.len(), 1, "idle confetti only clears: {:?}", d.ops);
    e.run("patch.confetti.trigger count=50").await;
    let d = e.wait_draw("confetti", T, |d| d.ops.iter().filter(|o| matches!(o, DrawOp::Rect { .. })).count() == 50).await;
    let pushes = d.ops.iter().filter(|o| matches!(o, DrawOp::Push { .. })).count();
    let pops = d.ops.iter().filter(|o| matches!(o, DrawOp::Pop)).count();
    assert_eq!((pushes, pops), (50, 50));
    // a cheer-sized payload scales the burst
    e.run("patch.confetti.trigger bits=1000").await;
    e.wait_draw("confetti", T, |d| d.ops.iter().filter(|o| matches!(o, DrawOp::Rect { .. })).count() > 50 + 160).await;

    // hype meter: chat + band signals raise the score, drawn as a bar and published as a signal
    let d = e.wait_draw("hype_meter", T, |d| d.ops.iter().any(|o| matches!(o, DrawOp::Text { .. }))).await;
    assert!(matches!(&d.ops[0], DrawOp::Clear(_)));
    for _ in 0..40 {
        e.hub.signals(vec![("twitch.chat_rate".into(), 8.0), ("band.level".into(), 0.8)]);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait_for("hype level rises", T, || e.hub.snapshot.load().signal("patch.hype_meter.level").is_some_and(|v| v > 0.5)).await;
    let d = e.draw("hype_meter").unwrap();
    let text = d.ops.iter().find_map(|o| match o {
        DrawOp::Text { text, .. } => Some(text.clone()),
        _ => None,
    });
    assert!(text.as_deref().is_some_and(|t| t.starts_with("HYPE ") && t != "HYPE 0%"), "{text:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn hosted_kinds_derive_state_from_their_runtime_and_publish_the_patch_set() {
    let shader = "kind = \"shader\"\nlayer = \"source\"\nparams.speed = { type = \"float\", default = 1.0 }\n";
    let e = engine(&[
        ("patches/sh/patch.toml", shader),
        ("patches/sh/main.wgsl", "@fragment fn fs(in: SeVsOut) -> @location(0) vec4<f32> { return vec4<f32>(1.0); }\n"),
    ])
    .await;
    e.wait_state("sh", "loaded", T).await;
    let mut set = e.patches.subscribe();
    let first = set.borrow_and_update().clone();
    let info = &first["sh"];
    assert!(info.enabled);
    assert_eq!(info.manifest.kind, se_patch::Kind::Shader);
    let g0 = info.generation;

    // the renderer reports a compile error → state error; fixed → loaded
    e.hub.publish("patch.sh.error", Value::Str("main.wgsl:3:5: unknown identifier".into()));
    e.wait_state("sh", "error", T).await;
    let q = e.query("sh").await;
    assert_eq!(q.get_path("error_file").and_then(Value::as_str), Some("main.wgsl"));
    assert_eq!(q.get_path("error_line"), Some(&Value::Int(3)));
    e.hub.publish("patch.sh.error", Value::Str(String::new()));
    e.wait_state("sh", "loaded", T).await;

    // editing the entry bumps the generation so hosts reload it
    write(e.dir.path(), "patches/sh/main.wgsl", "@fragment fn fs(in: SeVsOut) -> @location(0) vec4<f32> { return vec4<f32>(0.5); }\n");
    tokio::time::timeout(T, set.changed()).await.unwrap().unwrap();
    assert!(set.borrow_and_update()["sh"].generation > g0);

    // a broken manifest: loader-owned error with its line; the last good manifest stays in the set
    write(e.dir.path(), "patches/sh/patch.toml", "kind = \"shader\"\nparams.speed = { type = \"float\", default = }\n");
    e.wait_state("sh", "error", T).await;
    assert!(e.error("sh").starts_with("patch.toml:2:"), "{}", e.error("sh"));
    assert!(e.patches.current().contains_key("sh"));
    write(e.dir.path(), "patches/sh/patch.toml", shader);
    e.wait_state("sh", "loaded", T).await;
    assert_eq!(e.error("sh"), "");

    // disable is visible to hosts
    e.run("patch.disable sh").await;
    e.wait_state("sh", "disabled", T).await;
    wait_for("set shows disabled", T, || !e.patches.current()["sh"].enabled).await;
}
