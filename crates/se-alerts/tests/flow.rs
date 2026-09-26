//! End to end through a real core + hub: simulator events → alerts/bot → bus/actions.

use se_core::{Config, Core};
use se_hub::{Bus, EngineCtx, Hub, RunnerHooks, run_core};
use se_proto::{Command, Event, Op, Origin, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::Receiver;

const PROJECT: &str = r#"schema = 1
name = "test"
[goals.subs]
target = 10
counts = "subs"
"#;

struct Engine {
    hub: Arc<Hub>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Engine {
    fn drop(&mut self) {
        self.hub.shutdown();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Only these tests opt in to commands, alert reactions and queue policy.
fn project(dir: &Path) {
    std::fs::write(dir.join("project.toml"), PROJECT).unwrap();
    std::fs::create_dir_all(dir.join("commands")).unwrap();
    std::fs::create_dir_all(dir.join("alerts")).unwrap();
    std::fs::write(
        dir.join("commands/test.toml"),
        r#"[[command]]
name = "!discord"
reply = "Join the Discord: https://discord.gg/xxxx"

[[command]]
name = "!addcom"
builtin = "addcom"
role = "mod"
min_args = 2
"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("alerts/test.toml"),
        r#"
[queue]
pause_modes = ["ad_break", "brb"]
veto_window = "3s"
gift_window = "3s"

[[alert]]
name = "gift"
when = "twitch.gift"
title = "{user} gifted {count} subs!"
sound = "alert_gift"
duration = "7s"
priority = 60
[[alert.variation]]
name = "bomb"
if = "event.count >= 50"
title = "{user} DROPPED {count} GIFT SUBS!"
duration = "12s"
priority = 95

[[alert]]
name = "cheer"
when = "twitch.cheer"
title = "{user} cheered {amount} bits!"
message = "{message}"
duration = "4s"
priority = 20

[[alert]]
name = "raid"
when = "twitch.raid"
title = "{user} raided"
duration = "7s"
priority = 60

[[alert]]
name = "follow"
when = "twitch.follow"
title = "{user} followed"
duration = "4s"
priority = 10
"#,
    )
    .unwrap();
}

async fn start(dir: &Path, db: se_store::Db) -> Engine {
    let p = se_store::Project::open(dir).unwrap();
    let loaded = p.load();
    assert!(loaded.errors.is_empty(), "{:?}", loaded.errors);
    let config = Config::build(&loaded.files);
    let (hub, rx) = Hub::new(Arc::new(se_clock::Clock::new()));
    let core = Core::new(config.clone(), se_clock::now());
    let h = hub.clone();
    let thread = std::thread::spawn(move || run_core(core, h, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 1, on_runtime: None }));
    let (_tx, cfg) = tokio::sync::watch::channel(Arc::new(config));
    std::mem::forget(_tx);
    let ctx = EngineCtx {
        hub: hub.clone(),
        db,
        project_root: dir.to_path_buf(),
        data_dir: dir.join("data"),
        share_dir: dir.to_path_buf(),
        config: cfg,
        http: "127.0.0.1:0".parse().unwrap(),
        dev: true,
    };
    se_bot::start(ctx.clone()).await.unwrap();
    se_alerts::start(ctx).await.unwrap();
    Engine { hub, thread: Some(thread) }
}

async fn run(hub: &Hub, text: &str) {
    hub.exec(Command::new(Origin::Cli, Op::parse(text).unwrap())).await.unwrap_or_else(|e| panic!("{text}: {e}"));
}

/// Events matching `ty` seen within `dur`.
async fn events(bus: &mut Receiver<Arc<Bus>>, ty: &str, dur: Duration) -> Vec<Event> {
    let end = tokio::time::Instant::now() + dur;
    let mut out = Vec::new();
    while let Ok(Ok(b)) = tokio::time::timeout_at(end, bus.recv()).await {
        if let Bus::Event(e) = &*b
            && se_proto::address::matches(ty, &e.ty)
        {
            out.push(e.clone());
        }
    }
    out
}

async fn action(rx: &mut tokio::sync::mpsc::UnboundedReceiver<Command>, name: &str, dur: Duration) -> Option<Value> {
    let end = tokio::time::Instant::now() + dur;
    while let Ok(Some(c)) = tokio::time::timeout_at(end, rx.recv()).await {
        if let Op::Action { name: n, args } = &c.op
            && n == name
        {
            return Some(args.clone());
        }
    }
    None
}

fn state(hub: &Hub, a: &str) -> Option<Value> {
    hub.snapshot.load().get(a).cloned()
}

#[tokio::test(flavor = "multi_thread")]
async fn gift_bomb_of_50_is_exactly_one_alert() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let e = start(dir.path(), se_store::Db::memory().unwrap()).await;
    let mut bus = e.hub.subscribe();
    let mut audio = e.hub.route_actions("audio");
    run(&e.hub, "sim.gift_bomb count=50").await;
    let ev = events(&mut bus, "alert.**", Duration::from_millis(1500)).await;
    let shows: Vec<&Event> = ev.iter().filter(|e| e.ty == "alert.show").collect();
    assert_eq!(shows.len(), 1, "{:#?}", ev.iter().map(|e| (&e.ty, &e.payload)).collect::<Vec<_>>());
    let p = &shows[0].payload;
    assert_eq!(p.get_path("kind"), Some(&Value::Str("gift".into())));
    assert_eq!(p.get_path("variation"), Some(&Value::Str("bomb".into())));
    assert!(p.get_path("title").and_then(Value::as_str).unwrap().contains("50 GIFT SUBS"));
    // recipients arrive as updates on the same alert, never as separate alerts
    let cur = state(&e.hub, "alerts.current").unwrap();
    assert_eq!(cur.get_path("id"), p.get_path("id"));
    assert_eq!(cur.get_path("recipients").and_then(Value::as_list).map(|l| l.len()), Some(50));
    assert_eq!(e.hub.query("alerts", Value::Null).await.unwrap().get_path("queue").and_then(Value::as_list).map(|l| l.len()), Some(0));
    // one sound, and the 50 gifted subs still count as 50 subs
    let snd = action(&mut audio, "audio.play", Duration::from_millis(300)).await.unwrap();
    assert_eq!(snd.get_path("sound"), Some(&Value::Str("alert_gift".into())));
    assert_eq!(state(&e.hub, "stats.session.subs"), Some(Value::Int(50)));
    assert_eq!(state(&e.hub, "stats.session.gifts"), Some(Value::Int(50)));
    assert_eq!(state(&e.hub, "goals.subs.current"), Some(Value::Float(50.0)));
}

#[tokio::test(flavor = "multi_thread")]
async fn bot_replies_and_addcom_writes_the_file() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let e = start(dir.path(), se_store::Db::memory().unwrap()).await;
    let mut twitch = e.hub.route_actions("twitch");
    run(&e.hub, "sim.chat message='!discord'").await;
    let sent = action(&mut twitch, "twitch.chat.send", Duration::from_secs(2)).await.expect("bot replied");
    assert_eq!(sent.get_path("text"), Some(&Value::Str("Join the Discord: https://discord.gg/xxxx".into())));
    // rules and other subsystems speak through bot.say
    run(&e.hub, "bot.say 'hello from a rule'").await;
    let sent = action(&mut twitch, "twitch.chat.send", Duration::from_secs(2)).await.unwrap();
    assert_eq!(sent.get_path("text"), Some(&Value::Str("hello from a rule".into())));
    // a mod adds a command from chat
    run(&e.hub, "sim.chat message='!addcom !kit Tama Starclassic, Zildjian K' role=mod").await;
    let sent = action(&mut twitch, "twitch.chat.send", Duration::from_secs(2)).await.unwrap();
    assert!(sent.get_path("text").and_then(Value::as_str).unwrap().ends_with("added !kit"), "{sent:?}");
    let custom = std::fs::read_to_string(dir.path().join("commands/custom.toml")).unwrap();
    assert!(custom.contains("name = \"!kit\"") && custom.contains("# added from chat by"));
    run(&e.hub, "sim.chat message='!kit'").await;
    let sent = action(&mut twitch, "twitch.chat.send", Duration::from_secs(2)).await.unwrap();
    assert_eq!(sent.get_path("text"), Some(&Value::Str("Tama Starclassic, Zildjian K".into())));
    // viewers can't
    run(&e.hub, "sim.chat message='!addcom !nope x'").await;
    assert!(action(&mut twitch, "twitch.chat.send", Duration::from_millis(500)).await.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn veto_window_pause_replay_and_deletion_sync() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let e = start(dir.path(), se_store::Db::memory().unwrap()).await;
    let mut bus = e.hub.subscribe();
    run(&e.hub, "mode.set live").await;
    // a cheer with viewer text waits in its veto window; the mod kills it
    run(&e.hub, "sim.cheer bits=50 message='hello chat'").await; // below the policy's veto hold threshold
    tokio::time::sleep(Duration::from_millis(300)).await;
    let q = e.hub.query("alerts", Value::Null).await.unwrap();
    let id = q.get_path("queue.0.id").and_then(Value::as_i64).expect("queued");
    assert!(q.get_path("queue.0.veto_remaining_ms").and_then(Value::as_i64).unwrap() > 0);
    let countdown = state(&e.hub, &format!("alerts.veto.{id}")).and_then(|v| v.as_f64()).expect("veto countdown state");
    assert!(countdown > 0.0 && countdown <= 3.0);
    run(&e.hub, &format!("alerts.veto {id}")).await;
    let ev = events(&mut bus, "alert.show", Duration::from_millis(3500)).await;
    assert!(ev.is_empty(), "vetoed alert never shows");
    assert!(state(&e.hub, &format!("alerts.veto.{id}")).is_none(), "countdown removed");

    // pause in brb: the on-screen alert goes down and replays after
    run(&e.hub, "sim.raid viewers=40").await;
    let shows = events(&mut bus, "alert.show", Duration::from_millis(500)).await;
    assert_eq!(shows.len(), 1);
    let raid = shows[0].payload.get_path("id").cloned();
    run(&e.hub, "mode.set brb").await;
    let hides = events(&mut bus, "alert.hide", Duration::from_millis(500)).await;
    assert_eq!(hides[0].payload.get_path("reason"), Some(&Value::Str("paused".into())));
    run(&e.hub, "sim.follow").await;
    assert!(events(&mut bus, "alert.show", Duration::from_millis(1500)).await.is_empty(), "held in brb");
    assert_eq!(state(&e.hub, "alerts.paused"), Some(Value::Bool(true)));
    run(&e.hub, "mode.set live").await;
    let shows = events(&mut bus, "alert.show", Duration::from_millis(500)).await;
    assert_eq!(shows[0].payload.get_path("id"), raid.as_ref(), "the raid replays first");

    // deletion sync: chat message removed from the feed and overlays told
    run(&e.hub, "sim.chat message='delete me please'").await;
    let msgs = events(&mut bus, "chat.message", Duration::from_millis(500)).await;
    let mid = msgs[0].payload.get_path("id").and_then(Value::as_str).unwrap().to_string();
    assert!(e.hub.query("chat.recent", Value::Null).await.unwrap().as_list().unwrap().iter().any(|m| m.get_path("id").and_then(Value::as_str) == Some(&mid)));
    e.hub
        .exec(Command::new(Origin::Twitch, Op::Emit { ty: "twitch.chat.delete".into(), payload: Value::map().with("message_id", mid.clone()) }))
        .await
        .unwrap();
    let del = events(&mut bus, "chat.delete", Duration::from_millis(500)).await;
    assert_eq!(del[0].payload.get_path("message_id"), Some(&Value::Str(mid.clone())));
    assert!(!e.hub.query("chat.recent", Value::Null).await.unwrap().as_list().unwrap().iter().any(|m| m.get_path("id").and_then(Value::as_str) == Some(&mid)));
}

#[tokio::test(flavor = "multi_thread")]
async fn goals_persist_across_restart() {
    let dir = tempfile::tempdir().unwrap();
    project(dir.path());
    let db_path = dir.path().join("runtime.db");
    {
        let e = start(dir.path(), se_store::Db::open(&db_path).unwrap()).await;
        run(&e.hub, "sim.sub").await;
        run(&e.hub, "sim.sub tier=2").await;
        tokio::time::sleep(Duration::from_millis(400)).await;
        assert_eq!(state(&e.hub, "goals.subs.current"), Some(Value::Float(2.0)));
    }
    let e = start(dir.path(), se_store::Db::open(&db_path).unwrap()).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(state(&e.hub, "goals.subs.current"), Some(Value::Float(2.0)));
    assert_eq!(state(&e.hub, "goals.subs.target"), Some(Value::Float(10.0)));
    run(&e.hub, "goals.add subs amount=3").await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(state(&e.hub, "goals.subs.current"), Some(Value::Float(5.0)));
}
