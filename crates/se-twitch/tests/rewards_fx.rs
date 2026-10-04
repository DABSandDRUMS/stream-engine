//! Effect rewards follow the operator's effects switch: rewards with `fx = true` are paused on
//! Twitch (against the local Helix mock) when `fx.enabled` turns false and resumed when it turns
//! true again; rewards without `fx` are left alone.
//! `cargo test -p se-twitch --features mock --test rewards_fx`

use se_core::config::SourceFile;
use se_core::{Config, Core};
use se_hub::{EngineCtx, Hub, RunnerHooks, run_core};
use se_proto::Value;
use se_twitch::auth::{MemoryStore, SecretStore};
use se_twitch::mock::Mock;
use serde_json::Value as J;
use std::sync::Arc;
use std::time::{Duration, Instant};

fn file(kind: &str, name: &str, src: &str) -> SourceFile {
    SourceFile { kind: kind.into(), name: name.into(), path: format!("{kind}/{name}.toml"), table: toml::from_str(src).unwrap() }
}

/// Last `is_paused` sent to Twitch for the reward titled `title` (create or update).
fn last_paused(mock: &Mock, title: &str, ids: &[(String, String)]) -> Option<bool> {
    let id = ids.iter().find(|(t, _)| t == title).map(|(_, id)| id.as_str());
    mock.calls("/channel_points/custom_rewards")
        .iter()
        .filter(|c| match c.method.as_str() {
            "POST" => c.body["title"] == title,
            "PATCH" => id.is_some_and(|id| c.query.get("id").map(String::as_str) == Some(id)),
            _ => false,
        })
        .filter_map(|c| c.body.get("is_paused").and_then(J::as_bool))
        .next_back()
}

async fn until(what: &str, f: impl Fn() -> bool) {
    let end = Instant::now() + Duration::from_secs(15);
    while !f() {
        assert!(Instant::now() < end, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fx_rewards_pause_while_effects_are_off() {
    let mock = Mock::new("mock-client", "1337", "streamer");
    let api = mock.clone().serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
    // EventSub points nowhere: this test is about Helix only
    let project = format!(
        "schema = 1\n[twitch]\nclient_id = \"mock-client\"\nsecrets = \"twitch-fx\"\nemotes = []\neventsub_url = \"ws://127.0.0.1:9/ws\"\nsubscriptions_url = \"http://127.0.0.1:9/eventsub/subscriptions\"\nhelix_url = \"http://{api}/helix\"\nauth_url = \"http://{api}/oauth2\""
    );
    let files = vec![
        file("project", "project", &project),
        file("rewards", "glitch", "title = \"Glitch\"\ncost = 100\nfx = true"),
        file("rewards", "hydrate", "title = \"Hydrate\"\ncost = 100"),
    ];
    let config = Config::build(&files);
    assert!(config.errors.is_empty(), "{:?}", config.errors);
    let (hub, rx) = Hub::new(Arc::new(se_clock::Clock::new()));
    let core = Core::new(config.clone(), se_clock::now());
    let h2 = hub.clone();
    std::thread::spawn(move || run_core(core, h2, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 4, on_runtime: None }));
    let dir = std::env::temp_dir().join(format!("se-twitch-fx-{}", std::process::id()));
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
    let store = Arc::new(MemoryStore::default());
    store.set("twitch-fx.refresh_token", &mock.grant()).unwrap();
    let _svc = se_twitch::start_with(ctx, store).await.unwrap();

    let snap = |a: &str| hub.snapshot.load().get(a).cloned().unwrap_or_default();
    let ids = || -> Vec<(String, String)> {
        snap("twitch.rewards")
            .as_list()
            .unwrap_or_default()
            .iter()
            .filter_map(|r| Some((r.get_path("title")?.as_str()?.to_string(), r.get_path("id")?.as_str()?.to_string())))
            .collect()
    };
    // effects on (no `fx.enabled` yet counts as on): both created unpaused
    until("initial sync", || ids().len() == 2).await;
    assert_eq!(last_paused(&mock, "Glitch", &ids()), Some(false));
    assert_eq!(last_paused(&mock, "Hydrate", &ids()), Some(false));
    let patches = || mock.calls("/channel_points/custom_rewards").iter().filter(|c| c.method == "PATCH").count();
    assert_eq!(patches(), 0);

    // the core declares `fx.enabled` (default true); a first publish here only creates it
    hub.publish("fx.enabled", Value::Bool(true));
    until("fx.enabled", || snap("fx.enabled") == Value::Bool(true)).await;
    hub.publish("fx.enabled", Value::Bool(false));
    until("fx reward paused", || last_paused(&mock, "Glitch", &ids()) == Some(true)).await;
    assert_eq!(last_paused(&mock, "Hydrate", &ids()), Some(false), "a reward without fx is not touched");
    assert_eq!(patches(), 1, "only the fx reward is updated");

    hub.publish("fx.enabled", Value::Bool(true));
    until("fx reward resumed", || last_paused(&mock, "Glitch", &ids()) == Some(false)).await;
    assert_eq!(patches(), 2, "only the fx reward is updated");
    assert_eq!(last_paused(&mock, "Hydrate", &ids()), Some(false));
}
