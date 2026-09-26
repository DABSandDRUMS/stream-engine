//! Users/roles cache against the local Helix mock: once authorized, the moderator and VIP
//! lists are read into the runtime DB and answer role lookups for viewers without badges.
//! `cargo test -p se-twitch --features mock --test users_cache`

use se_core::config::SourceFile;
use se_core::{Config, Core};
use se_hub::{EngineCtx, Hub, RunnerHooks, run_core};
use se_proto::Value;
use se_twitch::auth::{MemoryStore, SecretStore};
use se_twitch::mock::Mock;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moderator_and_vip_lists_fill_the_roles_cache() {
    let mock = Mock::new("mock-client", "1337", "streamer");
    let api = mock.clone().serve("127.0.0.1:0".parse().unwrap()).await.unwrap();
    // EventSub points nowhere: this test is about Helix only
    let project = format!(
        "schema = 1\n[twitch]\nclient_id = \"mock-client\"\nsecrets = \"twitch-users\"\nemotes = []\neventsub_url = \"ws://127.0.0.1:9/ws\"\nsubscriptions_url = \"http://127.0.0.1:9/eventsub/subscriptions\"\nhelix_url = \"http://{api}/helix\"\nauth_url = \"http://{api}/oauth2\""
    );
    let files = vec![SourceFile { kind: "project".into(), name: "project".into(), path: "project.toml".into(), table: toml::from_str(&project).unwrap() }];
    let config = Config::build(&files);
    assert!(config.errors.is_empty(), "{:?}", config.errors);
    let (hub, rx) = Hub::new(Arc::new(se_clock::Clock::new()));
    let core = Core::new(config.clone(), se_clock::now());
    let h2 = hub.clone();
    std::thread::spawn(move || run_core(core, h2, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 4, on_runtime: None }));
    let dir = std::env::temp_dir().join(format!("se-twitch-users-{}", std::process::id()));
    let (_cfg_tx, cfg_rx) = tokio::sync::watch::channel(Arc::new(config));
    let db = se_store::Db::memory().unwrap();
    let ctx = EngineCtx {
        hub: hub.clone(),
        db: db.clone(),
        project_root: dir.clone(),
        data_dir: dir.clone(),
        share_dir: dir,
        config: cfg_rx,
        http: "127.0.0.1:0".parse().unwrap(),
        dev: true,
    };
    let store = Arc::new(MemoryStore::default());
    store.set("twitch-users.refresh_token", &mock.grant()).unwrap();
    let _svc = se_twitch::start_with(ctx, store).await.unwrap();

    let deadline = Instant::now() + Duration::from_secs(15);
    let users = loop {
        let v = hub.query("twitch.users", Value::map()).await.unwrap();
        if v.get_path("vips.0").is_some() && v.get_path("moderators.0").is_some() {
            break v;
        }
        assert!(Instant::now() < deadline, "lists never arrived: {v}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(users.get_path("moderators.0.login").and_then(Value::as_str), Some("modfriend"));
    assert_eq!(users.get_path("vips.0.id").and_then(Value::as_str), Some("id-vipfan"));
    assert!(users.get_path("vips_synced_at").and_then(Value::as_i64).unwrap_or(0) > 0);
    // a VIP who never chatted still gets the role (e.g. for a redemption without badges)
    let vip = hub.query("twitch.users", Value::map().with("user_id", "id-vipfan")).await.unwrap();
    assert_eq!(vip.get_path("roles"), Some(&Value::from(vec!["vip".to_string()])));
    let nobody = hub.query("twitch.users", Value::map().with("user_id", "id-nobody")).await.unwrap();
    assert_eq!(nobody.get_path("known"), Some(&Value::Bool(false)));
    // kept in the runtime DB
    let rows: Vec<(String, bool, bool)> = db
        .with(|c| {
            let mut st = c.prepare("SELECT user_id, is_mod, is_vip FROM twitch_users ORDER BY user_id")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
            rows.collect()
        })
        .unwrap();
    assert_eq!(rows, vec![("id-modfriend".to_string(), true, false), ("id-vipfan".to_string(), false, true)]);
}
