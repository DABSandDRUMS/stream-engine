//! End-to-end tests of the song-request service against a real hub/core and a local HTTP
//! server replaying recorded YouTube Data API responses (tests/fixtures/youtube) and
//! hand-written MusicBrainz responses (tests/fixtures/musicbrainz).

use axum::extract::{Path, Query, State};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use parking_lot::Mutex;
use se_core::{Config, Core, SourceFile};
use se_hub::{Bus, EngineCtx, Hub, RunnerHooks, run_core};
use se_proto::{Actor, Command, Event, Op, Origin, Role, Value};
use se_songs::secrets::SecretStore;
use serde_json::Value as J;
use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc};

// ---- fixtures ----------------------------------------------------------------------------

fn fixture(name: &str) -> J {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/youtube").join(name);
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[derive(Default)]
struct Api {
    videos: AtomicUsize,
    searches: AtomicUsize,
    quota_gone: AtomicBool,
    key_seen: Mutex<Vec<String>>,
    /// MusicBrainz requests (`path?query`) and the User-Agent each one sent.
    mb: Mutex<Vec<(String, String)>>,
}

fn all_videos() -> HashMap<String, J> {
    let mut m = HashMap::new();
    for f in ["videos_ok.json", "videos_search.json", "videos_policy.json", "videos_restricted.json"] {
        for it in fixture(f)["items"].as_array().unwrap() {
            m.insert(it["id"].as_str().unwrap().to_string(), it.clone());
        }
    }
    m
}

async fn videos(State(api): State<Arc<Api>>, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    api.videos.fetch_add(1, Ordering::SeqCst);
    api.key_seen.lock().push(q.get("key").cloned().unwrap_or_default());
    if api.quota_gone.load(Ordering::SeqCst) {
        return (axum::http::StatusCode::FORBIDDEN, Json(fixture("error_quota.json")));
    }
    let all = all_videos();
    let items: Vec<J> = q.get("id").map(|ids| ids.split(',').filter_map(|id| all.get(id).cloned()).collect()).unwrap_or_default();
    (axum::http::StatusCode::OK, Json(serde_json::json!({ "kind": "youtube#videoListResponse", "items": items })))
}

async fn search(State(api): State<Arc<Api>>, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    api.searches.fetch_add(1, Ordering::SeqCst);
    if api.quota_gone.load(Ordering::SeqCst) {
        return (axum::http::StatusCode::FORBIDDEN, Json(fixture("error_quota.json")));
    }
    let body = if q.get("q").is_some_and(|s| s.to_lowercase().contains("bohemian")) { fixture("search_ok.json") } else { fixture("search_empty.json") };
    (axum::http::StatusCode::OK, Json(body))
}

fn mb_fixture(name: &str) -> Option<J> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/musicbrainz").join(name);
    std::fs::read_to_string(p).ok().map(|s| serde_json::from_str(&s).unwrap())
}

fn user_agent(h: &axum::http::HeaderMap) -> String {
    h.get("user-agent").and_then(|v| v.to_str().ok()).unwrap_or("").to_string()
}

async fn mb_search(State(api): State<Arc<Api>>, h: axum::http::HeaderMap, Query(q): Query<HashMap<String, String>>) -> impl IntoResponse {
    let query = q.get("query").cloned().unwrap_or_default();
    api.mb.lock().push((format!("recording?{query}"), user_agent(&h)));
    let file = if query.contains("artist:\"Rick Astley\"") { "search_rick_astley.json" } else { "search_empty.json" };
    Json(mb_fixture(file).unwrap())
}

async fn mb_lookup(State(api): State<Arc<Api>>, h: axum::http::HeaderMap, Path((kind, id)): Path<(String, String)>) -> impl IntoResponse {
    api.mb.lock().push((format!("{kind}/{id}"), user_agent(&h)));
    match mb_fixture(&format!("{kind}_{id}.json")) {
        Some(j) => (axum::http::StatusCode::OK, Json(j)),
        None => (axum::http::StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "Not Found", "help": "For usage, please see: https://musicbrainz.org/development/mmd" }))),
    }
}

async fn api_server() -> (Arc<Api>, String) {
    let api = Arc::new(Api::default());
    let app = Router::new()
        .route("/youtube/v3/videos", get(videos))
        .route("/youtube/v3/search", get(search))
        .route("/ws/2/recording", get(mb_search))
        .route("/ws/2/{kind}/{id}", get(mb_lookup))
        .with_state(api.clone());
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(l, app).await.unwrap() });
    (api, format!("http://127.0.0.1:{}/youtube/v3", addr.port()))
}

#[derive(Default)]
struct MemSecrets(Mutex<BTreeMap<String, String>>);

impl SecretStore for MemSecrets {
    fn get(&self, name: &str) -> anyhow::Result<Option<String>> {
        Ok(self.0.lock().get(name).cloned())
    }
    fn set(&self, name: &str, value: &str) -> anyhow::Result<()> {
        self.0.lock().insert(name.into(), value.into());
        Ok(())
    }
    fn delete(&self, name: &str) -> anyhow::Result<()> {
        self.0.lock().remove(name);
        Ok(())
    }
}

// ---- engine harness ----------------------------------------------------------------------

struct Rig {
    hub: Arc<Hub>,
    api: Arc<Api>,
    chat: mpsc::UnboundedReceiver<Command>,
    twitch: mpsc::UnboundedReceiver<Command>,
    bus: broadcast::Receiver<Arc<Bus>>,
    secrets: Arc<MemSecrets>,
    page: Mutex<String>,
    _dir: tempfile::TempDir,
}

const KEY: &str = "AIzaTESTKEY0123456789abcdefghijklmnopq";
const CHANNEL: &str = "UCaaaaaaaaaaaaaaaaaaaaaa";
const DELEGATE: &str = "123456789";

async fn rig(extra_songs: &str, key: Option<&str>) -> Rig {
    let r = rig_unverified(extra_songs, key, true, se_store::Db::memory().unwrap()).await;
    r.connect_verified("test").await;
    r
}

async fn rig_unverified(extra_songs: &str, key: Option<&str>, configured: bool, db: se_store::Db) -> Rig {
    let (api, base) = api_server().await;
    let account = if configured { format!("youtube_channel = \"{CHANNEL}\"\nyoutube_delegate = \"{DELEGATE}\"\n") } else { String::new() };
    let mb = base.replace("/youtube/v3", "/ws/2");
    let project = format!("schema = 1\n[songs]\nregion = \"US\"\napi_base = \"{base}\"\nmetadata_base = \"{mb}\"\ncrossfade = \"0ms\"\n{account}{extra_songs}\n");
    let files = vec![SourceFile { kind: "project".into(), name: "project".into(), path: "project.toml".into(), table: project.parse().unwrap() }];
    let config = Config::build(&files);
    assert!(config.errors.is_empty(), "{:?}", config.errors);
    let (hub, rx) = Hub::new(Arc::new(se_clock::Clock::new()));
    let core = Core::new(config.clone(), se_clock::now());
    let h2 = hub.clone();
    std::thread::spawn(move || run_core(core, h2, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 1, on_runtime: None }));
    let chat = hub.route_actions("bot");
    let twitch = hub.route_actions("twitch");
    let bus = hub.subscribe();
    let dir = tempfile::tempdir().unwrap();
    let (_tx, cfg_rx) = tokio::sync::watch::channel(Arc::new(config));
    std::mem::forget(_tx);
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
    let secrets = Arc::new(MemSecrets::default());
    if let Some(k) = key {
        secrets.set(se_songs::secrets::YOUTUBE_KEY, k).unwrap();
    }
    se_songs::service::spawn(ctx, secrets.clone()).await.unwrap();
    Rig { hub, api, chat, twitch, bus, secrets, page: Mutex::new("test".into()), _dir: dir }
}

fn viewer(name: &str, role: Role) -> Actor {
    Actor { platform: "twitch".into(), id: format!("id-{name}"), name: name.into(), roles: vec![role] }
}

impl Rig {
    fn chat_cmd(&self, name: &str, args: Value, who: &Actor) {
        self.hub.command(Command::new(Origin::Chat, Op::Action { name: name.into(), args }).with_actor(Some(who.clone())));
    }

    fn operator(&self, name: &str, args: Value) {
        self.hub.command(Command::new(Origin::Ui, Op::Action { name: name.into(), args }));
    }

    fn sr(&self, text: &str, who: &Actor) {
        self.chat_cmd("queue.request", Value::map().with("user", who.name.clone()).with("text", text), who);
    }

    async fn said(&mut self) -> String {
        let c = tokio::time::timeout(Duration::from_secs(5), self.chat.recv()).await.expect("no chat reply").unwrap();
        let Op::Action { args, .. } = c.op else { panic!() };
        args.get_path("args.0").and_then(Value::as_str).unwrap().to_string()
    }

    async fn said_matching(&mut self, needle: &str) -> String {
        loop {
            let s = self.said().await;
            if s.contains(needle) {
                return s;
            }
        }
    }

    async fn twitch_action(&mut self) -> (String, Value) {
        let c = tokio::time::timeout(Duration::from_secs(5), self.twitch.recv()).await.expect("no twitch action").unwrap();
        let Op::Action { name, args } = c.op else { panic!() };
        (name, args)
    }

    async fn event(&mut self, ty: &str) -> Event {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            let b = tokio::time::timeout_at(deadline, self.bus.recv()).await.unwrap_or_else(|_| panic!("no {ty} event")).unwrap();
            if let Bus::Event(e) = &*b
                && e.ty == ty
            {
                return e.clone();
            }
        }
    }

    async fn queue(&self) -> Value {
        self.hub.query("queue", Value::Null).await.unwrap()
    }

    async fn report(&self, args: Value) -> Value {
        let args = if args.get_path("page").is_none() { args.with("page", self.page.lock().clone()) } else { args };
        if args.get_path("ev").and_then(Value::as_str) == Some("hello") {
            *self.page.lock() = args.get_path("page").and_then(Value::as_str).unwrap_or("").to_string();
        }
        self.hub.query("song.player", args).await.unwrap()
    }

    async fn connect_verified(&self, page: &str) -> Value {
        self.report(Value::map().with("ev", "hello").with("page", page)).await;
        self.report(Value::map().with("ev", "account").with("verified", true).with("channel", CHANNEL).with("delegate", DELEGATE).with("error", "")).await;
        self.queue().await; // actor barrier: account reports are asynchronous
        self.state("song.player").await
    }

    async fn state(&self, addr: &str) -> Value {
        // publishes are applied on the next core tick
        tokio::time::sleep(Duration::from_millis(30)).await;
        self.hub.get(addr, false).await.into_iter().next().map(|e| e.value).unwrap_or(Value::Null)
    }
}

fn upcoming_titles(q: &Value) -> Vec<String> {
    q.get_path("upcoming").and_then(Value::as_list).unwrap().iter().map(|e| e.get_path("title").unwrap().to_string()).collect()
}

// ---- tests -------------------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread")]
async fn sr_link_is_validated_queued_and_played() {
    let mut r = rig("", Some(KEY)).await;
    let fan = viewer("drumfan42", Role::Everyone);
    r.sr("https://youtu.be/dQw4w9WgXcQ?si=share", &fan);
    let reply = r.said().await;
    assert!(reply.starts_with("@drumfan42 added \"Rick Astley - Never Gonna Give You Up"), "{reply}");
    assert!(reply.contains("(3:33)") && reply.contains("#1"), "{reply}");
    assert_eq!(r.api.videos.load(Ordering::SeqCst), 1, "one videos.list call (1 unit)");
    assert_eq!(r.api.key_seen.lock()[0], KEY);
    let requested = r.event("queue.song_requested").await;
    assert_eq!(requested.payload.get_path("source").and_then(Value::as_str), Some("api"));

    // nothing was playing: it became current right away
    let q = r.queue().await;
    assert_eq!(q.get_path("now.video").and_then(Value::as_str), Some("dQw4w9WgXcQ"));
    assert_eq!(q.get_path("now.user").and_then(Value::as_str), Some("drumfan42"));
    assert_eq!(r.state("queue.now.title").await.as_str().map(|s| s.starts_with("Rick Astley")), Some(true));
    assert_eq!(r.state("song.media").await, Value::from("yt:dQw4w9WgXcQ"));
    assert_eq!(q.get_path("quota.used").and_then(Value::as_i64), Some(1));

    // the player page connects and plays it
    let d = r.connect_verified("t").await;
    assert_eq!(d.get_path("active").and_then(Value::as_str), Some("a"));
    assert_eq!(d.get_path("a.id").and_then(Value::as_str), Some("dQw4w9WgXcQ"));
    assert_eq!(d.get_path("a.cmd").and_then(Value::as_str), Some("play"));
    let entry = d.get_path("a.entry").and_then(Value::as_i64).unwrap();
    r.report(Value::map().with("ev", "state").with("slot", "a").with("entry", entry).with("state", "playing").with("t", 0.5).with("dur", 213.0)).await;
    let started = r.event("queue.song_started").await;
    assert_eq!(started.payload.get_path("id").and_then(Value::as_i64), Some(entry));
    assert_eq!(r.state("queue.playing").await, Value::Bool(true));
    assert_eq!(r.state("song.state").await, Value::from("playing"));

    // the same link again: rejected as a duplicate, from the cache (no new API call)
    let other = viewer("snarequeen", Role::Everyone);
    r.sr("dQw4w9WgXcQ", &other);
    let reply = r.said().await;
    assert_eq!(reply, "@snarequeen ✗ that's playing right now");
    assert_eq!(r.api.videos.load(Ordering::SeqCst), 1);

    // ended → song_ended, queue idle
    r.report(Value::map().with("ev", "ended").with("slot", "a").with("entry", entry)).await;
    let ended = r.event("queue.song_ended").await;
    assert_eq!(ended.payload.get_path("reason").and_then(Value::as_str), Some("ended"));
    let q = r.queue().await;
    assert!(q.get_path("now").unwrap().is_null());
    assert_eq!(q.get_path("history.0.status").and_then(Value::as_str), Some("played"));
}

#[tokio::test(flavor = "multi_thread")]
async fn text_search_uses_quota_once_then_the_cache() {
    let mut r = rig("", Some(KEY)).await;
    r.operator("queue.pause", Value::Null);
    let a = viewer("a_user", Role::Everyone);
    r.sr("Bohemian Rhapsody", &a);
    let reply = r.said().await;
    // the first result is embeddable; the second (NoEmbed0001) would be skipped anyway
    assert!(reply.contains("Queen – Bohemian Rhapsody (Official Video Remastered)"), "{reply}");
    assert_eq!((r.api.searches.load(Ordering::SeqCst), r.api.videos.load(Ordering::SeqCst)), (1, 1));
    let q = r.queue().await;
    assert_eq!(q.get_path("quota.used").and_then(Value::as_i64), Some(101));
    assert_eq!(q.get_path("quota.searches").and_then(Value::as_i64), Some(1));

    // the same words, different case/punctuation, from someone else → duplicate, 0 units
    let b = viewer("b_user", Role::Everyone);
    r.sr("  bohemian   RHAPSODY!! ", &b);
    assert_eq!(r.said().await, "@b_user ✗ that's already in the queue (#1)");
    assert_eq!((r.api.searches.load(Ordering::SeqCst), r.api.videos.load(Ordering::SeqCst)), (1, 1));

    // remove it, ask again: cache hit, queued, still no API calls
    r.operator("queue.clear", Value::Null);
    r.sr("bohemian rhapsody", &b);
    let reply = r.said().await;
    assert!(reply.starts_with("@b_user added"), "{reply}");
    assert_eq!((r.api.searches.load(Ordering::SeqCst), r.api.videos.load(Ordering::SeqCst)), (1, 1));
    let requested = r.event("queue.song_requested").await;
    let requested =
        if requested.payload.get_path("user").and_then(Value::as_str) == Some("b_user") { requested } else { r.event("queue.song_requested").await };
    assert_eq!(requested.payload.get_path("source").and_then(Value::as_str), Some("cache"));

    // nothing found
    r.sr("zzqx nonexistent tune", &a);
    assert_eq!(r.said().await, "@a_user ✗ no playable results for that search");
}

#[tokio::test(flavor = "multi_thread")]
async fn quota_exhaustion_degrades_to_links_and_library() {
    let mut r = rig("daily_quota = 350\nlink_reserve = 200", Some(KEY)).await;
    r.operator("queue.pause", Value::Null);
    let a = viewer("a_user", Role::Everyone);
    // 350 units: one search (100) + list (1) leaves 249 ≥ 100 + 200? no → links-only after it
    r.sr("bohemian rhapsody", &a);
    // the reply and the mode announcement race (the ledger is charged before the API call and
    // the actor re-checks the mode every second), so accept either order
    let two = [r.said().await, r.said().await];
    assert!(two.iter().any(|m| m.starts_with("@a_user added")), "{two:?}");
    let announce = two.iter().find(|m| m.contains("YouTube search is used up")).unwrap_or_else(|| panic!("{two:?}"));
    assert!(announce.contains("midnight Pacific"), "{announce}");
    assert_eq!(r.state("queue.lookup").await, Value::from("links"));
    // text now answered from the library only; unknown text is refused without spending
    r.sr("something brand new", &a);
    assert_eq!(r.said().await, "@a_user ✗ song search is used up for today — paste a YouTube link instead");
    assert_eq!(r.api.searches.load(Ordering::SeqCst), 1);
    // links still work (1 unit each)
    let b = viewer("b_user", Role::Everyone);
    r.sr("https://www.youtube.com/watch?v=dQw4w9WgXcQ", &b);
    assert!(r.said().await.starts_with("@b_user added \"Rick Astley"));
    // Google says the quota is gone (other consumers): library only, announced
    r.api.quota_gone.store(true, Ordering::SeqCst);
    r.sr("https://youtu.be/yk3prd8GER4", &b); // cached by the search → from the library, no call needed
    let reply = r.said().await;
    assert!(reply.starts_with("@b_user added \"Bohemian Rhapsody - Panic!"), "{reply}");
    let c = viewer("c_user", Role::Everyone);
    r.sr("https://youtu.be/CleanVer001", &c); // never seen → needs the API
    let reply = r.said_matching("@c_user").await;
    assert_eq!(reply, "@c_user ✗ YouTube lookups are used up for today — only songs already in the library work until midnight Pacific");
    assert_eq!(r.state("queue.lookup").await, Value::from("library"));
    let h = r.state("health.youtube").await;
    assert_eq!(h.get_path("status").and_then(Value::as_str), Some("warn"));
    // library text search still answers known songs
    r.sr("rick astley never gonna give you up", &c);
    assert_eq!(r.said_matching("@c_user").await, "@c_user ✗ that's already in the queue (#2)");
}

#[tokio::test(flavor = "multi_thread")]
async fn policy_gates_platform_checks_and_approval() {
    let mut r = rig("", Some(KEY)).await;
    r.operator("queue.pause", Value::Null);
    let v = viewer("viewer1", Role::Everyone);
    for (link, why) in [
        ("https://youtu.be/NoEmbed0001", "that video can't be played outside YouTube"),
        ("https://youtu.be/AgeGate0001", "that video is age-restricted"),
        ("https://youtu.be/RegionBlk01", "that video is blocked in our region"),
        ("https://youtu.be/LiveNow0001", "live streams and premieres can't be requested"),
        ("https://youtu.be/LongSong001", "that's too long (max 10:00)"),
        ("https://youtu.be/Nope0000000", "couldn't find that video"),
    ] {
        r.sr(link, &v);
        assert_eq!(r.said().await, format!("@viewer1 ✗ {why}"));
    }
    // UI-edited policy: subs only, approval for everyone below VIP
    r.operator("queue.policy.set", Value::map().with("min_role", "sub").with("approval", "role").with("approval_below", "vip").with("max_per_user", 1));
    tokio::time::sleep(Duration::from_millis(50)).await;
    let pol = r.hub.query("queue.policy", Value::Null).await.unwrap();
    assert_eq!(pol.get_path("policy.min_role").and_then(Value::as_str), Some("sub"));
    assert!(pol.get_path("fields").and_then(Value::as_list).unwrap().len() > 15);
    r.sr("https://youtu.be/dQw4w9WgXcQ", &v);
    assert_eq!(r.said().await, "@viewer1 ✗ song requests are for subs and up");
    let sub = viewer("subby", Role::Sub);
    r.sr("https://youtu.be/dQw4w9WgXcQ", &sub);
    let reply = r.said().await;
    assert!(reply.contains("waiting for mod approval"), "{reply}");
    let id = r.queue().await.get_path("pending.0.id").and_then(Value::as_i64).unwrap();
    assert_eq!(r.state("queue.pending").await, Value::Int(1));
    r.sr("https://youtu.be/fJ9rUzIMcZQ", &sub);
    assert_eq!(r.said().await, "@subby ✗ you already have 1 song waiting");
    // a viewer can't approve; a mod can
    r.chat_cmd("queue.approve", Value::map().with("id", format!("#{id}")), &v);
    let m = viewer("moddy", Role::Mod);
    r.chat_cmd("queue.approve", Value::map().with("id", format!("#{id}")), &m);
    let reply = r.said().await;
    assert!(reply.contains("was approved (#1)"), "{reply}");
    assert_eq!(upcoming_titles(&r.queue().await).len(), 1);
    // ban the song: it leaves the queue and can't come back
    r.chat_cmd("queue.ban_song", Value::map().with("id", "dQw4w9WgXcQ"), &m);
    assert!(r.said().await.starts_with("Banned \""));
    assert!(upcoming_titles(&r.queue().await).is_empty());
    let vip = viewer("vippy", Role::Vip);
    r.sr("https://youtu.be/dQw4w9WgXcQ", &vip);
    assert_eq!(r.said().await, "@vippy ✗ that song isn't allowed here");
    // persisted across restart of the service? (policy is in the DB: re-read it)
    let pol = r.hub.query("queue.policy", Value::Null).await.unwrap();
    assert_eq!(pol.get_path("policy.blocked_videos.0").and_then(Value::as_str), Some("dQw4w9WgXcQ"));
}

#[tokio::test(flavor = "multi_thread")]
async fn gapless_handover_errors_skip_and_redemptions() {
    let mut r = rig("", Some(KEY)).await;
    let a = viewer("a_user", Role::Everyone);
    // a paid (channel points) request: fulfilled only when it actually plays
    r.chat_cmd(
        "queue.request",
        Value::map().with("args", Value::List(vec![Value::from("https://youtu.be/dQw4w9WgXcQ")])).with("redemption_id", "red-1").with("reward_id", "rw-1"),
        &a,
    );
    assert!(r.said().await.contains("added"));
    let b = viewer("b_user", Role::Everyone);
    r.sr("https://youtu.be/fJ9rUzIMcZQ", &b);
    assert!(r.said().await.contains("#1"));
    let d = r.connect_verified("p1").await;
    let (e1, e2) = (d.get_path("a.entry").and_then(Value::as_i64).unwrap(), d.get_path("b.entry").and_then(Value::as_i64).unwrap());
    assert_eq!(d.get_path("b.cmd").and_then(Value::as_str), Some("cue"), "next song preloads in slot b");
    assert_eq!(d.get_path("b.auto"), Some(&Value::Bool(true)));
    r.report(Value::map().with("ev", "state").with("slot", "a").with("entry", e1).with("state", "playing").with("t", 0.1).with("dur", 213.0)).await;
    let (name, args) = r.twitch_action().await;
    assert_eq!(name, "twitch.fulfill");
    assert_eq!(args.get_path("redemption_id").and_then(Value::as_str), Some("red-1"));
    // the page auto-started b when a ended
    r.report(Value::map().with("ev", "ended").with("slot", "a").with("entry", e1)).await;
    r.report(Value::map().with("ev", "state").with("slot", "b").with("entry", e2).with("state", "playing").with("t", 0.0).with("dur", 359.0)).await;
    let _ = r.event("queue.song_ended").await;
    let started = r.event("queue.song_started").await;
    let started = if started.payload.get_path("id").and_then(Value::as_i64) == Some(e2) { started } else { r.event("queue.song_started").await };
    assert_eq!(started.payload.get_path("id").and_then(Value::as_i64), Some(e2));
    let d = r.state("song.player").await;
    assert_eq!(d.get_path("active").and_then(Value::as_str), Some("b"));
    assert_eq!(d.get_path("b.cmd").and_then(Value::as_str), Some("play"));
    // position signal follows the player
    r.report(Value::map().with("ev", "progress").with("slot", "b").with("entry", e2).with("state", "playing").with("t", 42.0).with("dur", 359.0)).await;
    tokio::time::sleep(Duration::from_millis(120)).await;
    let pos = r.hub.snapshot.load().signal("song.position").unwrap();
    assert!((42.0..43.0).contains(&pos), "{pos}");

    // a paid request that fails to embed is refunded and skipped with a chat notice
    let c = viewer("c_user", Role::Everyone);
    r.chat_cmd("queue.request", Value::map().with("text", "https://youtu.be/yk3prd8GER4").with("redemption_id", "red-2"), &c);
    assert!(r.said().await.contains("added"));
    let d = r.state("song.player").await;
    let e3 = d.get_path("a.entry").and_then(Value::as_i64).unwrap();
    r.report(Value::map().with("ev", "error").with("slot", "a").with("entry", e3).with("code", 150)).await;
    let err = r.event("queue.song_error").await;
    assert_eq!(err.payload.get_path("code").and_then(Value::as_i64), Some(150));
    let notice = r.said().await;
    assert!(notice.contains("can't be played here (embedding disabled by the owner)"), "{notice}");
    let (name, args) = r.twitch_action().await;
    assert_eq!((name.as_str(), args.get_path("redemption_id").and_then(Value::as_str)), ("twitch.refund", Some("red-2")));
    // the library remembers: a new request for it is refused without an API call
    let calls = r.api.videos.load(Ordering::SeqCst);
    r.sr("https://youtu.be/yk3prd8GER4", &c);
    assert_eq!(r.said().await, "@c_user ✗ that video can't be played here (embedding disabled by the owner)");
    assert_eq!(r.api.videos.load(Ordering::SeqCst), calls);

    // mods skip; viewers vote-skip once enabled
    r.operator("queue.policy.set", Value::map().with("voteskip_votes", 2));
    r.chat_cmd("queue.skip", Value::Null, &a);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(r.queue().await.get_path("now.id").and_then(Value::as_i64), Some(e2), "viewers can't skip");
    r.chat_cmd("queue.voteskip", Value::Null, &a);
    assert_eq!(r.said().await, "Vote-skip 1/2 for \"Queen – Bohemian Rhapsody (Official Video Remastered)\"");
    r.chat_cmd("queue.voteskip", Value::Null, &a);
    r.chat_cmd("queue.voteskip", Value::Null, &b);
    assert!(r.said().await.starts_with("Vote-skip passed"));
    let ended = r.event("queue.song_ended").await;
    let ended = if ended.payload.get_path("reason").and_then(Value::as_str) == Some("voteskip") { ended } else { r.event("queue.song_ended").await };
    assert_eq!(ended.payload.get_path("reason").and_then(Value::as_str), Some("voteskip"));
}

#[tokio::test(flavor = "multi_thread")]
async fn key_management_and_no_key_mode() {
    let mut r = rig("", None).await;
    assert_eq!(r.state("queue.lookup").await, Value::from("off"));
    let a = viewer("a_user", Role::Everyone);
    r.sr("https://youtu.be/dQw4w9WgXcQ", &a);
    assert_eq!(r.said().await, "@a_user ✗ song lookup isn't set up yet");
    r.operator("youtube.key.set", Value::map().with("key", KEY));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while r.secrets.get(se_songs::secrets::YOUTUBE_KEY).unwrap().is_none() {
        assert!(tokio::time::Instant::now() < deadline, "key never stored");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(r.api.videos.load(Ordering::SeqCst), 1, "the key is verified with one videos.list call");
    assert_eq!(r.state("queue.lookup").await, Value::from("full"));
    let st = r.hub.query("youtube.status", Value::Null).await.unwrap();
    assert_eq!(st.get_path("key_set"), Some(&Value::Bool(true)));
    r.sr("https://youtu.be/dQw4w9WgXcQ", &a);
    assert!(r.said().await.contains("added"));
    // a chat viewer can't touch keys
    r.chat_cmd("youtube.key.clear", Value::Null, &a);
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(r.secrets.get(se_songs::secrets::YOUTUBE_KEY).unwrap().is_some());
}

fn assert_stopped_slots(desired: &Value) {
    for slot in ["a", "b"] {
        assert_eq!(desired.get_path(&format!("{slot}.id")).and_then(Value::as_str), Some(""));
        assert_eq!(desired.get_path(&format!("{slot}.cmd")).and_then(Value::as_str), Some("stop"));
        assert_eq!(desired.get_path(&format!("{slot}.auto")), Some(&Value::Bool(false)));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn unconfigured_or_unverified_account_blocks_operator_and_chat_without_lookup() {
    for configured in [false, true] {
        let mut r = rig_unverified("", Some(KEY), configured, se_store::Db::memory().unwrap()).await;
        r.operator("queue.close", Value::Null);
        r.operator("queue.pause", Value::Null);
        r.queue().await;
        let hello = r.report(Value::map().with("ev", "hello")).await;
        assert_stopped_slots(&hello);
        assert_eq!(hello.get_path("required_channel").and_then(Value::as_str), Some(if configured { CHANNEL } else { "" }));
        r.operator("queue.open", Value::Null);
        r.operator("queue.resume", Value::Null);
        r.operator("queue.policy.set", Value::map().with("open", true));
        r.operator("queue.request", Value::map().with("text", "dQw4w9WgXcQ"));
        let fan = viewer("locked", Role::Everyone);
        r.sr("https://youtu.be/dQw4w9WgXcQ", &fan);
        r.said().await; // queue-action FIFO barrier; queries use a separate route.
        let q = r.queue().await;
        assert_eq!(q.get_path("open"), Some(&Value::Bool(false)));
        assert_eq!(q.get_path("paused"), Some(&Value::Bool(true)));
        assert_eq!(q.get_path("now"), Some(&Value::Null));
        assert!(q.get_path("upcoming").and_then(Value::as_list).unwrap().is_empty());
        assert_eq!(r.api.videos.load(Ordering::SeqCst), 0);
        assert_eq!(r.api.searches.load(Ordering::SeqCst), 0);
        assert_eq!(r.state("health.player").await.get_path("status").and_then(Value::as_str), Some("warn"));
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn restored_rows_stay_held_through_mismatch_reconnect_and_exact_channel_release() {
    use se_songs::queue::{Entry, Paid, Status};
    use se_songs::store::Store;
    let db = se_store::Db::memory().unwrap();
    let store = Store::open(db.clone()).unwrap();
    store.kv_set("paused", &Value::Bool(true)).unwrap();
    let mut current = Entry {
        id: 0,
        video: "dQw4w9WgXcQ".into(),
        title: "Restored current".into(),
        channel: "Artist".into(),
        duration_s: 213,
        user: "fan".into(),
        user_id: Some("fan-id".into()),
        role: Role::Everyone,
        status: Status::Playing,
        paid: Some(Paid { redemption_id: Some("untouched-redemption".into()), ..Default::default() }),
        requested_at: 1,
        started_at: Some(2),
        ended_at: None,
        note: None,
    };
    current.id = store.insert_entry(&current).unwrap();
    store.update_entry(&current).unwrap();
    let mut next = current.clone();
    next.id = 0;
    next.video = "fJ9rUzIMcZQ".into();
    next.title = "Restored upcoming".into();
    next.status = Status::Queued;
    next.started_at = None;
    next.paid = None;
    next.id = store.insert_entry(&next).unwrap();
    store.save_order(&[next.id]).unwrap();
    store.kv_set("position", &Value::map().with("entry", current.id).with("t", 27.0)).unwrap();
    let mut r = rig_unverified("", Some(KEY), true, db).await;
    assert_stopped_slots(&r.state("song.player").await);
    let hello = r.report(Value::map().with("ev", "hello")).await;
    assert_stopped_slots(&hello);
    r.report(Value::map().with("ev", "account").with("verified", true).with("channel", "UCbbbbbbbbbbbbbbbbbbbbbb").with("delegate", DELEGATE)).await;
    r.queue().await;
    assert_stopped_slots(&r.state("song.player").await);
    r.operator("queue.resume", Value::Null);
    r.sr("https://youtu.be/dQw4w9WgXcQ", &viewer("held", Role::Everyone));
    r.said().await; // the rejected request follows resume on the same queue action route.
    r.report(Value::map().with("ev", "ended").with("slot", "a").with("entry", current.id)).await;
    r.report(Value::map().with("ev", "error").with("slot", "b").with("entry", next.id).with("code", 150)).await;
    let held = r.queue().await;
    assert_eq!(held.get_path("paused"), Some(&Value::Bool(true)));
    assert_eq!(r.api.videos.load(Ordering::SeqCst), 0);
    assert_eq!(held.get_path("now.id").and_then(Value::as_i64), Some(current.id));
    assert_eq!(held.get_path("upcoming.0.id").and_then(Value::as_i64), Some(next.id));
    assert!(held.get_path("history").and_then(Value::as_list).unwrap().is_empty());
    assert!(r.twitch.try_recv().is_err(), "identity failure must not refund or fulfill stored rows");
    let (stored_current, stored_upcoming, _) = store.load_active().unwrap();
    assert_eq!(stored_current, Some(current.clone()));
    assert_eq!(stored_upcoming, vec![next.clone()]);
    let released = r.connect_verified("test").await;
    assert_eq!(released.get_path("a.id").and_then(Value::as_str), Some(current.video.as_str()));
    assert_eq!(released.get_path("a.start").and_then(Value::as_f64), Some(27.0));
    assert_eq!(released.get_path("a.cmd").and_then(Value::as_str), Some("pause"));
    assert_eq!(released.get_path("b.id").and_then(Value::as_str), Some(next.video.as_str()));
    assert_eq!(released.get_path("b.cmd").and_then(Value::as_str), Some("cue"));
    assert_eq!(released.get_path("b.auto"), Some(&Value::Bool(false)));
    let reconnected = r.report(Value::map().with("ev", "hello").with("page", "new-page")).await;
    assert_stopped_slots(&reconnected);
    r.report(Value::map().with("ev", "account").with("page", "test").with("verified", true).with("channel", CHANNEL).with("delegate", DELEGATE)).await;
    r.queue().await;
    assert_stopped_slots(&r.state("song.player").await);
    r.report(Value::map().with("ev", "account").with("verified", true).with("channel", CHANNEL).with("delegate", DELEGATE)).await;
    r.queue().await;
    assert_eq!(r.state("song.player").await.get_path("a.id").and_then(Value::as_str), Some(current.video.as_str()));
    while let Ok(bus) = r.bus.try_recv() {
        if let Bus::Event(event) = &*bus {
            assert!(!matches!(event.ty.as_str(), "queue.song_started" | "queue.song_ended" | "queue.song_error"));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn transient_identity_failure_after_restart_resumes_when_verified() {
    use se_songs::queue::{Entry, Status};
    use se_songs::store::Store;
    let db = se_store::Db::memory().unwrap();
    let store = Store::open(db.clone()).unwrap();
    let mut current = Entry {
        id: 0,
        video: "dQw4w9WgXcQ".into(),
        title: "Restored current".into(),
        channel: "Artist".into(),
        duration_s: 213,
        user: "fan".into(),
        user_id: Some("fan-id".into()),
        role: Role::Everyone,
        status: Status::Playing,
        paid: None,
        requested_at: 1,
        started_at: Some(2),
        ended_at: None,
        note: None,
    };
    current.id = store.insert_entry(&current).unwrap();
    store.update_entry(&current).unwrap();
    let r = rig_unverified("", Some(KEY), true, db).await;
    r.report(Value::map().with("ev", "hello")).await;
    r.report(Value::map().with("ev", "account").with("verified", false).with("channel", "").with("delegate", "").with("error", "signing in")).await;
    assert_eq!(r.queue().await.get_path("paused"), Some(&Value::Bool(true)), "unverified identity holds playback");
    let released = r.connect_verified("test").await;
    assert_eq!(released.get_path("a.id").and_then(Value::as_str), Some(current.video.as_str()));
    assert_eq!(released.get_path("a.cmd").and_then(Value::as_str), Some("play"));
    assert_eq!(r.queue().await.get_path("paused"), Some(&Value::Bool(false)));
    assert_eq!(store.kv_get("paused"), None, "an identity hold is never saved as an operator pause");
}

/// Poll a state address until it equals `want` (metadata arrives asynchronously).
async fn wait_state(r: &Rig, addr: &str, want: Value) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let v = r.state(addr).await;
        if v == want {
            return;
        }
        assert!(tokio::time::Instant::now() < deadline, "{addr} stayed {v:?}, wanted {want:?}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn musicbrainz_metadata_reaches_entries_and_state_without_touching_playback() {
    let db = se_store::Db::memory().unwrap();
    let mut r = rig_unverified("", Some(KEY), true, db.clone()).await;
    r.connect_verified("test").await;
    let fan = viewer("drumfan42", Role::Everyone);
    r.sr("https://youtu.be/dQw4w9WgXcQ", &fan);
    r.said().await;
    // first sight: nothing cached yet; artist/title come from the video title
    let requested = r.event("queue.song_requested").await;
    assert_eq!(requested.payload.get_path("artist").and_then(Value::as_str), Some("Rick Astley"));
    assert_eq!(requested.payload.get_path("genres"), Some(&Value::List(vec![])));
    assert!(requested.payload.get_path("year").unwrap().is_null());
    let desired = r.state("song.player").await;
    let before = r.queue().await;

    wait_state(&r, "song.current.year", Value::Int(1987)).await;
    assert_eq!(r.state("song.current.title").await, Value::from("Never Gonna Give You Up"));
    assert_eq!(r.state("song.current.artist").await, Value::from("Rick Astley"));
    assert_eq!(r.state("song.current.genres").await, Value::from(vec!["dance-pop", "synth-pop", "pop"]));
    let q = r.queue().await;
    assert_eq!(q.get_path("now.artist").and_then(Value::as_str), Some("Rick Astley"));
    assert_eq!(q.get_path("now.genres.0").and_then(Value::as_str), Some("dance-pop"));
    assert_eq!(q.get_path("now.year").and_then(Value::as_i64), Some(1987));
    // metadata only: the player and the queue are exactly as before
    assert_eq!(r.state("song.player").await, desired);
    for k in ["now.id", "now.status", "now.title", "upcoming", "pending"] {
        assert_eq!(q.get_path(k), before.get_path(k), "{k}");
    }
    // one search, one recording lookup, polite User-Agent
    let mb = r.api.mb.lock().clone();
    assert_eq!(mb.len(), 2, "{mb:?}");
    assert_eq!(mb[0].0, "recording?recording:\"Never Gonna Give You Up\" AND artist:\"Rick Astley\"");
    assert_eq!(mb[1].0, "recording/c95fd3f9-1a2b-4c3d-8e4f-5a6b7c8d9e01");
    assert!(mb.iter().all(|(_, ua)| ua.starts_with("stream-engine/") && ua.contains("https://github.com/DABSandDRUMS/stream-engine")), "{mb:?}");
    // playback events carry the fields
    let d = r.connect_verified("t").await;
    let entry = d.get_path("a.entry").and_then(Value::as_i64).unwrap();
    r.report(Value::map().with("ev", "state").with("slot", "a").with("entry", entry).with("state", "playing").with("t", 0.5).with("dur", 213.0)).await;
    let started = r.event("queue.song_started").await;
    assert_eq!(started.payload.get_path("genres.1").and_then(Value::as_str), Some("synth-pop"));

    // after a restart the restored song is answered from the runtime-DB cache: no requests
    let r2 = rig_unverified("", Some(KEY), true, db).await;
    assert_eq!(r2.state("song.current.genres").await, Value::from(vec!["dance-pop", "synth-pop", "pop"]));
    assert_eq!(r2.state("song.current.year").await, Value::Int(1987));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(r2.api.mb.lock().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn metadata_off_reads_the_title_only_and_state_clears_between_songs() {
    let mut r = rig("metadata = false", Some(KEY)).await;
    let fan = viewer("drumfan42", Role::Everyone);
    r.sr("https://youtu.be/dQw4w9WgXcQ", &fan);
    r.said().await;
    assert_eq!(r.state("song.current.title").await, Value::from("Never Gonna Give You Up"));
    assert_eq!(r.state("song.current.artist").await, Value::from("Rick Astley"));
    assert_eq!(r.state("song.current.genres").await, Value::List(vec![]));
    assert_eq!(r.state("song.current.year").await, Value::Null);
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(r.api.mb.lock().is_empty(), "no MusicBrainz requests with metadata = false");
    // nothing playing: cleared
    r.operator("queue.skip", Value::Null);
    wait_state(&r, "song.current.title", Value::from("")).await;
    assert_eq!(r.state("song.current.artist").await, Value::from(""));
    assert_eq!(r.state("song.current.genres").await, Value::List(vec![]));
    assert_eq!(r.state("song.current.year").await, Value::Null);
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_theme_survives_restart_without_releasing_the_player_hold() {
    let db = se_store::Db::memory().unwrap();
    let r = rig_unverified("metadata = false", None, true, db.clone()).await;
    let before = r.state("song.player").await;
    r.operator("queue.theme.set", Value::map().with("theme", "modern"));
    wait_state(&r, "queue.theme", Value::from("modern")).await;
    assert_eq!(r.queue().await.get_path("theme").and_then(Value::as_str), Some("modern"));
    assert_eq!(r.state("song.player").await, before);
    assert_eq!(r.queue().await.get_path("paused"), Some(&Value::Bool(true)));

    let restarted = rig_unverified("metadata = false", None, true, db).await;
    wait_state(&restarted, "queue.theme", Value::from("modern")).await;
    assert_eq!(restarted.queue().await.get_path("theme").and_then(Value::as_str), Some("modern"));
    restarted.operator("queue.theme.set", Value::map().with("theme", "win31"));
    wait_state(&restarted, "queue.theme", Value::from("win31")).await;
    assert_eq!(restarted.queue().await.get_path("paused"), Some(&Value::Bool(true)));
}
