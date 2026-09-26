//! End to end against the Twitch CLI's mock EventSub WebSocket server plus the local mock of
//! id.twitch.tv/Helix (`se_twitch::mock`): token refresh, subscriptions, normalized events,
//! policy gating (presets fire, a rejected redemption is refunded), a 50-gift bomb, deletion
//! sync, AutoMod holds, an ad break switching modes, `session_reconnect`, and a dropped
//! connection.
//!
//! Needs the CLI binary (`~/.local/share/stream-engine/bin/twitch` or `$TWITCH_CLI`) and its
//! fixed RPC port 44747 free (no other `twitch event websocket start-server` running):
//! `cargo test -p se-twitch --features mock --test mock_e2e -- --ignored --nocapture`

use parking_lot::Mutex;
use se_core::config::SourceFile;
use se_core::{Config, Core};
use se_hub::{Bus, EngineCtx, Hub, RunnerHooks, run_core};
use se_proto::{Command, Event, Op, Origin, Value};
use se_twitch::auth::{MemoryStore, SecretStore};
use se_twitch::mock::{Mock, cli_forward};
use serde_json::{Value as J, json};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn cli() -> PathBuf {
    std::env::var_os("TWITCH_CLI")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap()).join(".local/share/stream-engine/bin/twitch"))
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn file(kind: &str, name: &str, src: &str) -> SourceFile {
    SourceFile { kind: kind.into(), name: name.into(), path: format!("{kind}/{name}.toml"), table: toml::from_str(src).unwrap() }
}

fn fixture(name: &str) -> J {
    let path = format!("{}/tests/fixtures/{name}.json", env!("CARGO_MANIFEST_DIR"));
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

async fn trigger(args: &[&str]) {
    let out = tokio::process::Command::new(cli()).args(["event", "trigger"]).args(args).args(["--transport", "websocket"]).output().await.unwrap();
    assert!(out.status.success(), "twitch event trigger {args:?}: {}", String::from_utf8_lossy(&out.stderr));
}

/// Server commands; retried while the CLI still finishes a previous reconnect test (it refuses
/// other commands until the old server has shut down).
async fn ws_command(args: &[&str]) {
    let end = Instant::now() + Duration::from_secs(60);
    loop {
        let out = tokio::process::Command::new(cli()).args(["event", "websocket"]).args(args).output().await.unwrap();
        if out.status.success() {
            return;
        }
        let msg = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
        assert!(msg.contains("reconnect testing is active") && Instant::now() < end, "twitch event websocket {args:?}: {msg}");
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
}

struct Seen(Arc<Mutex<Vec<Event>>>);

impl Seen {
    async fn wait(&self, what: &str, timeout_s: u64, f: impl Fn(&Event) -> bool) -> Event {
        let end = Instant::now() + Duration::from_secs(timeout_s);
        loop {
            if let Some(e) = self.0.lock().iter().find(|e| f(e)) {
                return e.clone();
            }
            assert!(Instant::now() < end, "timed out waiting for {what}; saw: {:?}", self.0.lock().iter().map(|e| e.ty.clone()).collect::<Vec<_>>());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    fn count(&self, f: impl Fn(&Event) -> bool) -> usize {
        self.0.lock().iter().filter(|e| f(e)).count()
    }
}

async fn until(what: &str, timeout_s: u64, f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(timeout_s);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

fn s(e: &Event, k: &str) -> String {
    e.payload.get_path(k).map(|v| v.to_string()).unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "needs the Twitch CLI binary and its RPC port 44747"]
async fn mock_eventsub_end_to_end() {
    assert!(cli().exists(), "Twitch CLI not found at {}", cli().display());
    assert!(
        std::net::TcpStream::connect("127.0.0.1:44747").is_err(),
        "another `twitch event websocket start-server` holds the CLI RPC port 44747; stop it first"
    );
    // --- mock EventSub server (Twitch CLI) and mock id.twitch.tv + Helix ------------------------
    let port = free_port();
    let _server = tokio::process::Command::new(cli())
        .args(["event", "websocket", "start-server", "--port", &port.to_string()])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    until("the CLI mock server", 10, || std::net::TcpStream::connect(("127.0.0.1", port)).is_ok()).await;
    let mock = Mock::new("mock-client", "1337", "streamer");
    let api = mock.clone().serve("127.0.0.1:0".parse().unwrap()).await.unwrap();

    // --- project: rules/presets/rewards/policy wired like project-example -----------------------
    let project = format!(
        r#"schema = 1
start_mode = "offline"
[twitch]
client_id = "mock-client"
secrets = "twitch-e2e"
emotes = []
eventsub_url = "ws://127.0.0.1:{port}/ws"
subscriptions_url = "http://127.0.0.1:{port}/eventsub/subscriptions"
helix_url = "http://{api}/helix"
auth_url = "http://{api}/oauth2"
[policy.veto]
ms = "1s"
min_bits = 100
"#
    );
    let files = vec![
        file("project", "project", &project),
        file("presets", "hype", "hold = \"8s\"\nset = { \"fx.rgb_split.amount\" = 0.6 }"),
        file("presets", "sub_big", "hold = \"6s\"\nset = { \"fx.sub.amount\" = 1.0 }"),
        file("presets", "confetti", "hold = \"2s\"\nset = { \"fx.confetti.amount\" = 1.0 }"),
        file("rules", "cheers", "[[rule]]\nname = \"big cheer\"\nwhen = \"twitch.cheer\"\nif = \"event.bits >= 1000\"\ndo = [\"preset.fire hype\"]"),
        file(
            "rules",
            "subs",
            "[[rule]]\nname = \"tier 2+ sub\"\nwhen = \"twitch.sub\"\nif = \"event.tier >= 2 && mode == 'live' && !event.is_gift\"\ndo = [\"preset.fire sub_big\"]",
        ),
        file(
            "rewards",
            "hype",
            "title = \"HYPE\"\ncost = 2000\ncooldown = \"5m\"\nmax_per_user_per_stream = 3\nfires = \"preset.hype\"\non_reject = \"refund\"",
        ),
    ];
    let config = Config::build(&files);
    assert!(config.errors.is_empty(), "{:?}", config.errors);

    let (hub, rx) = Hub::new(Arc::new(se_clock::Clock::new()));
    let core = Core::new(config.clone(), se_clock::now());
    let h2 = hub.clone();
    let core_thread = std::thread::spawn(move || run_core(core, h2, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 4, on_runtime: None }));
    let seen = Seen(Arc::new(Mutex::new(Vec::new())));
    {
        let (store, mut bus) = (seen.0.clone(), hub.subscribe());
        tokio::spawn(async move {
            while let Ok(b) = bus.recv().await {
                match &*b {
                    Bus::Event(e) => store.lock().push(e.clone()),
                    Bus::Trace(recs) => {
                        for r in recs.iter().filter(|r| r.kind == "error") {
                            eprintln!("trace error: {}", r.label);
                        }
                    }
                    _ => {}
                }
            }
        });
    }
    let dir = std::env::temp_dir().join(format!("se-twitch-e2e-{port}"));
    let (_cfg_tx, cfg_rx) = tokio::sync::watch::channel(Arc::new(config));
    let ctx = EngineCtx {
        hub: hub.clone(),
        db: se_store::Db::memory().unwrap(),
        project_root: dir.clone(),
        data_dir: dir.clone(),
        share_dir: dir,
        config: cfg_rx,
        http: "127.0.0.1:0".parse().unwrap(),
        dev: true,
    };
    // an existing grant in the "keyring": startup refreshes it (single-use refresh tokens)
    let store = Arc::new(MemoryStore::default());
    store.set("twitch-e2e.refresh_token", &mock.grant()).unwrap();
    let svc = se_twitch::start_with(ctx, store.clone()).await.unwrap();
    hub.exec(Command::new(Origin::Ui, Op::ModeSet { mode: "live".into() })).await.unwrap();

    let snap = |a: &str| hub.snapshot.load().get(a).cloned().unwrap_or_default();
    until("EventSub connected", 20, || snap("twitch.eventsub.connected").truthy()).await;
    assert_eq!(snap("twitch.auth.status").as_str(), Some("authorized"));
    assert_eq!(snap("twitch.broadcaster.id").as_str(), Some("1337"));
    assert_ne!(store.get("twitch-e2e.refresh_token").unwrap().as_deref(), Some("refresh-1"), "rotated refresh token stored");
    let subs = snap("twitch.eventsub.subscriptions").as_i64().unwrap_or(0);
    println!("EventSub: {subs} subscriptions; refused by the mock: {}", snap("twitch.eventsub.failed"));
    // CLI 1.1.24 predates the chat/AutoMod/ad-break/hype-train-v2 types and refuses them; its
    // server still forwards every payload to connected clients, which the steps below rely on
    let failed = snap("twitch.eventsub.failed").as_list().map(<[Value]>::len).unwrap_or(0) as i64;
    assert_eq!(subs + failed, se_twitch::eventsub::subscriptions("1337", se_twitch::config::SubSource::Chat).len() as i64);
    assert!(subs >= 15, "the types the CLI knows are accepted");
    until("reward sync", 10, || snap("twitch.rewards").get_path("0.id").is_some_and(|v| !v.to_string().is_empty())).await;
    let reward_id = snap("twitch.rewards").get_path("0.id").unwrap().to_string();
    assert_eq!(snap("twitch.rewards").get_path("0.status").and_then(Value::as_str), Some("created"));
    let created = mock.calls("/channel_points/custom_rewards");
    assert_eq!(created.iter().find(|c| c.method == "POST").unwrap().body["global_cooldown_seconds"], 300);
    println!("reward HYPE synced as {reward_id}");

    // --- cheer (veto window, then the rule fires the preset) ---------------------------------
    trigger(&["channel.cheer", "-C", "1000", "--to-user", "1337"]).await;
    seen.wait("policy.pending for the cheer", 5, |e| e.ty == "policy.pending" && s(e, "type") == "twitch.cheer").await;
    let cheer = seen.wait("twitch.cheer", 5, |e| e.ty == "twitch.cheer").await;
    assert_eq!((s(&cheer, "bits").as_str(), s(&cheer, "vetted").as_str()), ("1000", "true"));
    until("preset hype", 3, || snap("preset.hype.active").truthy()).await;
    println!("cheer → veto window → preset.hype ✓");

    // --- tier-2 sub → rule → preset ----------------------------------------------------------
    // the CLI randomizes `is_gift`; gifted subs must NOT fire the rule, bought ones must
    let sub = loop {
        let before = seen.count(|e| e.ty == "twitch.sub");
        trigger(&["channel.subscribe", "--tier", "2000", "--to-user", "1337"]).await;
        until("twitch.sub tier 2", 5, || seen.count(|e| e.ty == "twitch.sub") > before).await;
        let sub = seen.0.lock().iter().rev().find(|e| e.ty == "twitch.sub").cloned().unwrap();
        assert_eq!(s(&sub, "tier"), "2");
        if s(&sub, "is_gift") == "false" {
            break sub;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!snap("preset.sub_big.active").truthy(), "a gifted sub fired the tier-2 rule");
    };
    assert_eq!(sub.actor.as_ref().unwrap().top_role(), se_proto::Role::Sub);
    until("preset sub_big", 3, || snap("preset.sub_big.active").truthy()).await;
    println!("sub tier 2 → preset.sub_big ✓");

    // --- redemptions: accepted (fires + fulfilled), then cooldown rejection → refund ---------
    hub.exec(Command::new(Origin::Ui, Op::PresetRelease { name: "hype".into() })).await.unwrap();
    trigger(&["channel.channel_points_custom_reward_redemption.add", "-i", &reward_id, "-n", "HYPE", "-C", "2000", "--to-user", "1337"]).await;
    let acc = seen.wait("policy.accepted", 5, |e| e.ty == "policy.accepted" && s(e, "type") == "twitch.redeem").await;
    let first_rid = s(&acc, "redemption_id");
    until("preset hype from the reward", 3, || snap("preset.hype.active").truthy()).await;
    until("fulfill call", 5, || mock.calls("/redemptions").iter().any(|c| c.body["status"] == "FULFILLED" && c.query.get("id") == Some(&first_rid))).await;
    trigger(&["channel.channel_points_custom_reward_redemption.add", "-i", &reward_id, "-n", "HYPE", "-C", "2000", "--to-user", "1337"]).await;
    let rej = seen.wait("policy.rejected", 5, |e| e.ty == "policy.rejected" && s(e, "type") == "twitch.redeem").await;
    assert!(s(&rej, "reason").starts_with("cooldown"), "{:?}", rej.payload);
    let refund_rid = s(&rej, "redemption_id");
    until("refund call", 5, || {
        mock.calls("/channel_points/custom_rewards/redemptions").iter().any(|c| {
            c.method == "PATCH" && c.body["status"] == "CANCELED" && c.query.get("id") == Some(&refund_rid) && c.query.get("reward_id") == Some(&reward_id)
        })
    })
    .await;
    println!("redeem → preset.hype + FULFILLED; second redeem in cooldown → CANCELED (refund) ✓");

    // --- a 50-gift bomb: community gift + 50 linked gifted subs (chat notifications) ---------
    let mut bomb = fixture("channel.chat.notification.community_sub_gift");
    bomb["event"]["community_sub_gift"]["total"] = json!(50);
    bomb["event"]["community_sub_gift"]["id"] = json!("bomb-50");
    cli_forward(&bomb.to_string()).await.unwrap();
    let template = fixture("channel.chat.notification.sub_gift_bomb")[0].clone();
    for i in 0..50 {
        let mut g = template.clone();
        g["event"]["message_id"] = json!(format!("gift-msg-{i}"));
        g["event"]["sub_gift"]["community_gift_id"] = json!("bomb-50");
        g["event"]["sub_gift"]["recipient_user_id"] = json!(format!("rcpt-{i}"));
        g["event"]["sub_gift"]["recipient_user_name"] = json!(format!("Recipient{i}"));
        cli_forward(&g.to_string()).await.unwrap();
    }
    let gift = seen.wait("twitch.gift", 5, |e| e.ty == "twitch.gift" && s(e, "gift_id") == "bomb-50").await;
    assert_eq!(s(&gift, "count"), "50");
    until("50 gifted subs", 10, || seen.count(|e| e.ty == "twitch.sub" && s(e, "gift_id") == "bomb-50" && s(e, "is_gift") == "true") == 50).await;
    assert!(!snap("preset.sub_big.active").truthy() || seen.count(|e| e.ty == "preset.sub_big.fired") == 1, "gifted subs don't fire the tier-2 rule");
    // the CLI's own channel.subscription.gift is normalized too
    trigger(&["channel.subscription.gift", "-C", "50", "--to-user", "1337"]).await;
    seen.wait("CLI gift bomb", 5, |e| e.ty == "twitch.gift" && s(e, "count") == "50" && s(e, "gift_id") != "bomb-50").await;
    println!("gift bomb: twitch.gift count=50 + 50 twitch.sub with gift_id ✓");

    // --- chat, deletion sync, AutoMod ---------------------------------------------------------
    let mut chat = fixture("channel.chat.message");
    chat["event"]["message_id"] = json!("chat-1");
    cli_forward(&chat.to_string()).await.unwrap();
    seen.wait("twitch.chat", 5, |e| e.ty == "twitch.chat" && s(e, "message_id") == "chat-1").await;
    let mut del = fixture("channel.chat.message_delete");
    del["event"]["message_id"] = json!("chat-1");
    cli_forward(&del.to_string()).await.unwrap();
    seen.wait("twitch.chat.delete", 5, |e| e.ty == "twitch.chat.delete" && s(e, "message_id") == "chat-1").await;
    trigger(&["channel.ban", "--to-user", "1337"]).await;
    let ban = seen.wait("twitch.ban", 5, |e| e.ty == "twitch.ban").await;
    seen.wait("twitch.user.purge", 5, |e| e.ty == "twitch.user.purge" && s(e, "user_id") == s(&ban, "user_id")).await;
    let mut hold = fixture("automod.message.hold.v2");
    hold["event"]["message_id"] = json!("held-1");
    cli_forward(&hold.to_string()).await.unwrap();
    until("AutoMod queue", 5, || snap("twitch.automod.held").as_i64() == Some(1)).await;
    let mut sneaky = fixture("channel.chat.message");
    sneaky["event"]["message_id"] = json!("held-1");
    cli_forward(&sneaky.to_string()).await.unwrap();
    seen.wait("held message filtered", 5, |e| e.ty == "policy.filtered" && s(e, "message_id") == "held-1").await;
    assert_eq!(seen.count(|e| e.ty == "twitch.chat" && s(e, "message_id") == "held-1"), 0);
    println!("chat delete → twitch.chat.delete; ban → twitch.user.purge; AutoMod-held message never shown ✓");

    // --- ad break: mode ad_break and back ------------------------------------------------------
    let mut ad = fixture("channel.ad_break.begin");
    ad["event"]["duration_seconds"] = json!(2);
    cli_forward(&ad.to_string()).await.unwrap();
    until("mode ad_break", 5, || snap("show.mode").as_str() == Some("ad_break")).await;
    until("mode back to live", 6, || snap("show.mode").as_str() == Some("live")).await;
    println!("ad break → ad_break → live ✓");

    // --- session_reconnect hand-over, then a dropped connection -------------------------------
    let before = snap("twitch.eventsub.session").to_string();
    ws_command(&["reconnect"]).await;
    until("session_reconnect", 40, || snap("twitch.eventsub.session").to_string() != before && snap("twitch.eventsub.connected").truthy()).await;
    trigger(&["channel.follow", "--to-user", "1337"]).await;
    seen.wait("follow after reconnect", 10, |e| e.ty == "twitch.follow").await;
    let session = snap("twitch.eventsub.session").to_string();
    ws_command(&["close", &format!("--session={session}"), "--reason=4006"]).await;
    until("fresh reconnect", 20, || snap("twitch.eventsub.session").to_string() != session && snap("twitch.eventsub.connected").truthy()).await;
    trigger(&["channel.raid", "--to-user", "1337"]).await;
    seen.wait("raid after the drop", 10, |e| e.ty == "twitch.raid").await;
    println!("session_reconnect + dropped connection: events keep flowing ✓");
    assert_eq!(snap("health.twitch").get_path("status").and_then(Value::as_str).map(|s| s != "fail"), Some(true), "{}", snap("health.twitch"));

    drop(svc);
    hub.shutdown();
    let _ = core_thread.join();
}
