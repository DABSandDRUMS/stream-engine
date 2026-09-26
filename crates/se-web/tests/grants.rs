//! Web patch page tokens follow the manifest's `grants` (§19), including on hot reload, without
//! the CEF runtime (the token is minted before any browser opens).

use se_api::Auth;
use se_api::auth::Scope;
use se_core::{Config, Core, SourceFile};
use se_hub::{EngineCtx, Hub, RunnerHooks, run_core};
use se_patch::{Manifest, PatchInfo, PatchSet};
use se_proto::{Op, Value};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

fn set_with(grants: &str, generation: u64) -> Arc<PatchSet> {
    let m = Manifest::parse(Path::new("/p/patches/deck"), &format!("kind = \"web\"\nlayer = \"overlay\"\ngrants = {grants}")).unwrap();
    let mut set = PatchSet::new();
    set.insert("deck".into(), PatchInfo { manifest: Arc::new(m), enabled: true, generation });
    Arc::new(set)
}

async fn page_scope(auth: &Auth, want: impl Fn(&Scope) -> bool) -> Scope {
    let t0 = Instant::now();
    loop {
        let web: Vec<Scope> = auth.devices().into_iter().filter(|(n, _)| n == "web.deck").map(|(_, s)| s).collect();
        if let [s] = web.as_slice()
            && want(s)
        {
            return s.clone();
        }
        assert!(web.len() <= 1, "one token per page, got {web:?}");
        assert!(t0.elapsed() < Duration::from_secs(10), "page token never matched; have {web:?}");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn page_token_follows_manifest_grants() {
    // no CEF host anywhere: sources stay "unavailable" but still get their tokens
    let home = tempfile::tempdir().unwrap();
    // SAFETY: this test binary has a single test; nothing else reads the environment concurrently.
    unsafe {
        std::env::set_var("HOME", home.path());
        std::env::remove_var("STREAM_ENGINE_WEB_HOST");
    }
    let (project, data, share) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let files = vec![SourceFile {
        kind: "project".into(),
        name: "project".into(),
        path: "project.toml".into(),
        table: "schema = 1\n[canvas.wide]\nwidth = 1920\nheight = 1080\n".parse().unwrap(),
    }];
    let cfg = Config::build(&files);
    let (hub, rx) = Hub::new(Arc::new(se_clock::Clock::new()));
    let core = Core::new(cfg.clone(), se_clock::now());
    {
        let hub = hub.clone();
        std::thread::spawn(move || run_core(core, hub, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 2, on_runtime: None }));
    }
    let (_config_tx, config_rx) = watch::channel(Arc::new(cfg));
    let (patches_tx, patches_rx) = watch::channel(set_with("[\"lights.*\"]", 1));
    let ctx = EngineCtx {
        hub: hub.clone(),
        db: se_store::Db::memory().unwrap(),
        project_root: project.path().to_path_buf(),
        data_dir: data.path().to_path_buf(),
        share_dir: share.path().to_path_buf(),
        config: config_rx,
        http: "127.0.0.1:9".parse().unwrap(),
        dev: true,
    };
    let auth = Arc::new(Auth::new(None));
    se_web::start_with(ctx, auth.clone(), patches_rx).await.unwrap();

    let act = |n: &str| Op::Action { name: n.into(), args: Value::Null };
    let s = page_scope(&auth, |s| matches!(s, Scope::Patch(id, g) if id == "deck" && g == &["lights.*"])).await;
    assert!(s.allows(&act("lights.cue")) && s.allows(&Op::Set { address: "patch.deck.count".into(), value: Value::Int(1) }));
    assert!(!s.allows(&act("mixer.mute")));

    // hot reload: the same token now carries the new grants (old ones gone)
    patches_tx.send(set_with("[\"mixer.*\"]", 2)).unwrap();
    let s = page_scope(&auth, |s| matches!(s, Scope::Patch(_, g) if g == &["mixer.*"])).await;
    assert!(s.allows(&act("mixer.mute")) && !s.allows(&act("lights.cue")));

    // grants removed → back to its own namespace only
    patches_tx.send(set_with("[]", 3)).unwrap();
    let s = page_scope(&auth, |s| matches!(s, Scope::Patch(_, g) if g.is_empty())).await;
    assert!(!s.allows(&act("mixer.mute")));
}
