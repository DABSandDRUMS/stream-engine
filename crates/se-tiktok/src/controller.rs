//! The always-on part: one task that watches `[tiktok]` and the `tiktok.*` actions, and
//! starts/stops the supervised client. While disabled this task is all that exists — no
//! client task, no network.

use crate::backoff::{Backoff, Rng};
use crate::config::{self, SIGN_KEY_SECRET, Settings};
use crate::session::{LinkState, Out, Shared, Sink, wait_stop};
use se_hub::EngineCtx;
use se_proto::{Command, Meta, Op, Value};
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// Runs one client until its stop signal flips.
pub type ClientFactory = Arc<dyn Fn(Settings, Out, watch::Receiver<bool>) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// Time a stopping client gets to close its socket before it is aborted.
const STOP_GRACE: Duration = Duration::from_secs(3);
const RESTART_CAP: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, Debug)]
pub struct Tuning {
    /// First delay before restarting a panicked client.
    pub restart_base: Duration,
}

impl Default for Tuning {
    fn default() -> Self {
        Tuning { restart_base: Duration::from_secs(1) }
    }
}

struct Running {
    settings: Settings,
    stop: watch::Sender<bool>,
    supervisor: JoinHandle<()>,
}

struct Controller {
    ctx: EngineCtx,
    factory: Option<ClientFactory>,
    out: Out,
    tuning: Tuning,
    config: Settings,
    /// Last successfully applied `[tiktok]` section (outer `None` = nothing loaded yet).
    raw: Option<Option<toml::Value>>,
    manual: Option<bool>,
    manual_uid: Option<String>,
    running: Option<Running>,
}

fn declare(ctx: &EngineCtx) {
    let hub = &ctx.hub;
    hub.declare("tiktok.connected", Meta::boolean(false).readonly().owner("tiktok").describe("Connected to the TikTok LIVE room"));
    hub.declare("tiktok.room_id", Meta::string("").readonly().owner("tiktok").describe("TikTok LIVE room id (empty when not live)"));
    hub.declare(
        "tiktok.status",
        Meta::enumeration("disabled", &LinkState::OPTIONS).readonly().owner("tiktok").describe("TikTok client state (best-effort, unofficial API)"),
    );
}

/// Start the controller; returns the shared status (tests read it directly).
pub fn start(ctx: EngineCtx, factory: Option<ClientFactory>, tuning: Tuning) -> Arc<Shared> {
    declare(&ctx);
    let shared = Arc::new(Shared::default());
    let sink: Arc<dyn Sink> = ctx.hub.clone();
    let out = Out::new(sink, shared.clone());
    out.set_state(LinkState::Disabled, "", "disabled");
    out.publish_all();
    {
        let shared = shared.clone();
        ctx.hub.register_query(
            "tiktok",
            Arc::new(move |_name, _args| {
                let shared = shared.clone();
                Box::pin(async move { Ok(shared.to_value()) })
            }),
        );
    }
    let actions = ctx.hub.route_actions("tiktok");
    let c = Controller { ctx, factory, out, tuning, config: Settings::default(), raw: None, manual: None, manual_uid: None, running: None };
    tokio::spawn(c.run(actions));
    shared
}

fn panic_message(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>().map(|s| s.to_string()).or_else(|| p.downcast_ref::<String>().cloned()).unwrap_or_else(|| "unknown panic".into())
}

/// Keep one client alive: a panic is logged and the client restarted with backoff; it never
/// propagates. Ends (within [`STOP_GRACE`]) once `stop` flips.
async fn supervise(factory: ClientFactory, settings: Settings, out: Out, mut stop: watch::Receiver<bool>, tuning: Tuning) {
    let mut backoff = Backoff::new(tuning.restart_base, RESTART_CAP, Rng::from_time());
    loop {
        let mut child = tokio::spawn(factory(settings.clone(), out.clone(), stop.clone()));
        let res = tokio::select! {
            r = &mut child => r,
            _ = wait_stop(&mut stop) => match tokio::time::timeout(STOP_GRACE, &mut child).await {
                Ok(r) => r,
                Err(_) => {
                    child.abort();
                    let _ = child.await;
                    return;
                }
            },
        };
        match res {
            Err(e) if e.is_panic() => {
                let msg = panic_message(e.into_panic());
                out.shared.counts.panics.fetch_add(1, Ordering::Relaxed);
                let delay = backoff.next_delay();
                out.error("error", format!("TikTok client panicked: {msg}; restarting in {:.1}s", delay.as_secs_f32()));
                out.set_state(LinkState::Backoff, "", format!("client crashed ({msg}); restarting"));
                if !crate::session::sleep_or_stop(&mut stop, delay).await {
                    return;
                }
            }
            _ => return,
        }
    }
}

impl Controller {
    async fn run(mut self, mut actions: mpsc::UnboundedReceiver<Command>) {
        let mut cfg = self.ctx.config.clone();
        let _ = cfg.borrow_and_update();
        self.load_config();
        self.reconcile(false).await;
        loop {
            tokio::select! {
                r = cfg.changed() => {
                    if r.is_err() {
                        break;
                    }
                    let _ = cfg.borrow_and_update();
                    if self.load_config() {
                        self.reconcile(false).await;
                    }
                }
                c = actions.recv() => {
                    let Some(c) = c else { break };
                    let force = self.action(c).await;
                    self.reconcile(force).await;
                }
            }
        }
        self.stop_running().await;
    }

    /// Re-read `[tiktok]`; true when the section changed. A broken section is logged and the
    /// last good settings are kept.
    fn load_config(&mut self) -> bool {
        let raw = self.ctx.project_section("tiktok");
        if self.raw.as_ref() == Some(&raw) {
            return false;
        }
        match config::parse(raw.as_ref()) {
            Ok(p) => {
                for w in p.warnings {
                    self.ctx.hub.log("warn", "tiktok", w);
                }
                self.raw = Some(raw);
                self.config = p.settings;
                true
            }
            Err(e) => {
                self.ctx.hub.log("error", "tiktok", format!("{e}; keeping the previous [tiktok] settings"));
                false
            }
        }
    }

    fn effective(&self) -> Settings {
        let mut s = self.config.clone();
        s.enabled = self.manual.unwrap_or(s.enabled);
        if let Some(u) = &self.manual_uid {
            s.unique_id = u.clone();
        }
        s
    }

    /// Handle one `tiktok.*` action; true when the client must restart.
    async fn action(&mut self, c: Command) -> bool {
        let Op::Action { name, args } = c.op else { return false };
        let arg = |k: &str| args.get_path(k).or_else(|| args.get_path("args.0")).and_then(Value::as_str).map(str::to_string);
        match name.as_str() {
            "tiktok.connect" => {
                if let Some(u) = arg("unique_id") {
                    match config::normalize_unique_id(&u) {
                        Ok(u) if !u.is_empty() => self.manual_uid = Some(u),
                        Ok(_) => {}
                        Err(e) => {
                            self.ctx.hub.log("error", "tiktok", e);
                            return false;
                        }
                    }
                }
                self.manual = Some(true);
                self.ctx.hub.log("info", "tiktok", "connect requested (session override)");
                false
            }
            "tiktok.disconnect" => {
                self.manual = Some(false);
                self.ctx.hub.log("info", "tiktok", "disconnect requested (session override; tiktok.connect resumes)");
                false
            }
            "tiktok.key.set" => {
                let key = arg("key").unwrap_or_default().trim().to_string();
                let clear = key.is_empty();
                let res =
                    tokio::task::spawn_blocking(
                        move || if clear { se_store::secrets::delete(SIGN_KEY_SECRET) } else { se_store::secrets::set(SIGN_KEY_SECRET, &key) },
                    )
                    .await;
                match res {
                    Ok(Ok(())) => {
                        let msg = if clear { "sign API key removed (anonymous limits apply)" } else { "sign API key stored in the keyring" };
                        self.ctx.hub.log("info", "tiktok", msg);
                        true
                    }
                    Ok(Err(e)) => {
                        self.ctx.hub.log("error", "tiktok", format!("could not store the sign API key: {e:#}"));
                        false
                    }
                    Err(e) => {
                        self.ctx.hub.log("error", "tiktok", format!("could not store the sign API key: {e}"));
                        false
                    }
                }
            }
            other => {
                self.ctx.hub.log("warn", "tiktok", format!("unknown action `{other}` (tiktok.connect [unique_id], tiktok.disconnect, tiktok.key.set <key>)"));
                false
            }
        }
    }

    async fn stop_running(&mut self) {
        if let Some(r) = self.running.take() {
            let _ = r.stop.send(true);
            let _ = r.supervisor.await;
        }
    }

    async fn reconcile(&mut self, force: bool) {
        let want = self.effective();
        {
            let mut s = self.out.shared.status.lock();
            s.enabled = want.enabled;
            s.manual = self.manual;
            s.unique_id = want.unique_id.clone();
        }
        let blocked = if !want.enabled {
            Some((LinkState::Disabled, if self.manual == Some(false) { "disconnected (tiktok.connect resumes)" } else { "disabled" }.to_string()))
        } else if want.unique_id.is_empty() {
            Some((LinkState::Failed, "enabled but no unique_id: set [tiktok] unique_id or run tiktok.connect <name>".to_string()))
        } else if self.factory.is_none() {
            Some((LinkState::Failed, "se-tiktok was built without the `client` feature".to_string()))
        } else {
            None
        };
        if let Some((state, detail)) = blocked {
            self.stop_running().await;
            self.out.viewers(0);
            self.out.set_state(state, "", detail);
            return;
        }
        if !force && self.running.as_ref().is_some_and(|r| r.settings == want && !r.supervisor.is_finished()) {
            return;
        }
        self.stop_running().await;
        let Some(factory) = self.factory.clone() else { return };
        let (stop, stop_rx) = watch::channel(false);
        let supervisor = tokio::spawn(supervise(factory, want.clone(), self.out.clone(), stop_rx, self.tuning));
        self.running = Some(Running { settings: want, stop, supervisor });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::testing::Fake;
    use crate::transport::{RoomStatus, TransportError};
    use parking_lot::Mutex;
    use se_hub::{Bus, Hub, RunnerHooks, run_core};
    use std::collections::VecDeque;
    use std::sync::atomic::AtomicUsize;

    struct Engine {
        ctx: EngineCtx,
        cfg_tx: watch::Sender<Arc<se_core::Config>>,
        core: Option<std::thread::JoinHandle<()>>,
    }

    impl Drop for Engine {
        fn drop(&mut self) {
            self.ctx.hub.shutdown();
            if let Some(t) = self.core.take() {
                let _ = t.join();
            }
        }
    }

    fn config(project: &str) -> se_core::Config {
        let files = vec![se_core::SourceFile { kind: "project".into(), name: "project".into(), path: "project.toml".into(), table: project.parse().unwrap() }];
        se_core::Config::build(&files)
    }

    fn engine(project: &str) -> Engine {
        let cfg = config(project);
        let (hub, rx) = Hub::new(Arc::new(se_clock::Clock::new()));
        let core = se_core::Core::new(cfg.clone(), se_clock::now());
        let h = hub.clone();
        let t = std::thread::spawn(move || run_core(core, h, rx, RunnerHooks { on_applied: Box::new(|_| {}), snapshot_every: 1, on_runtime: None }));
        let (cfg_tx, cfg_rx) = watch::channel(Arc::new(cfg));
        let ctx = EngineCtx {
            hub,
            db: se_store::Db::memory().unwrap(),
            project_root: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            share_dir: std::env::temp_dir(),
            config: cfg_rx,
            http: "127.0.0.1:0".parse().unwrap(),
            dev: false,
        };
        Engine { ctx, cfg_tx, core: Some(t) }
    }

    /// A factory whose clients use a scripted transport; counts client starts and every
    /// transport call.
    fn counting_factory(rooms: Vec<Result<RoomStatus, TransportError>>, panic_first: bool) -> (ClientFactory, Arc<AtomicUsize>, Arc<Mutex<Vec<&'static str>>>) {
        let starts = Arc::new(AtomicUsize::new(0));
        let calls: Arc<Mutex<Vec<&'static str>>> = Arc::default();
        let (s2, c2) = (starts.clone(), calls.clone());
        let f: ClientFactory = Arc::new(move |settings, out, stop| {
            let n = s2.fetch_add(1, Ordering::SeqCst);
            let fake = Fake { rooms: Mutex::new(VecDeque::from(rooms.clone())), signs: Mutex::default(), sockets: Mutex::default(), calls: c2.clone() };
            Box::pin(async move {
                if panic_first && n == 0 {
                    panic!("boom in client");
                }
                crate::session::run(fake, settings, out, stop).await
            })
        });
        (f, starts, calls)
    }

    async fn state(e: &Engine, addr: &str) -> Value {
        e.ctx.hub.get(addr, false).await.into_iter().next().map(|s| s.value).unwrap_or_default()
    }

    /// Hub state lags the controller by a core tick; poll until it matches.
    async fn wait_state(e: &Engine, addr: &str, want: impl Fn(&Value) -> bool) -> Value {
        for _ in 0..200 {
            let v = state(e, addr).await;
            if want(&v) {
                return v;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{addr} never reached the expected value (last: {})", state(e, addr).await);
    }

    async fn wait_for(mut f: impl FnMut() -> bool) {
        for _ in 0..200 {
            if f() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("condition not reached");
    }

    fn action(e: &Engine, text: &str) {
        e.ctx.hub.command(Command::new(se_proto::Origin::Cli, Op::parse(text).unwrap()));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disabled_by_default_starts_no_client_and_touches_no_network() {
        let e = engine("schema = 1\n[twitch]\nchannel = \"x\"");
        let (factory, starts, calls) = counting_factory(vec![Ok(RoomStatus::Offline)], false);
        let shared = start(e.ctx.clone(), Some(factory), Tuning::default());
        // unrelated hot reloads don't wake it either
        e.cfg_tx.send(Arc::new(config("schema = 1\n[twitch]\nchannel = \"y\""))).unwrap();
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(starts.load(Ordering::SeqCst), 0, "no client task may start while disabled");
        assert!(calls.lock().is_empty(), "no network call may happen while disabled");
        assert_eq!(state(&e, "tiktok.connected").await, Value::Bool(false));
        assert_eq!(state(&e, "tiktok.status").await, Value::from("disabled"));
        let h = state(&e, "health.tiktok").await;
        assert_eq!((h.get_path("status").and_then(Value::as_str), h.get_path("detail").and_then(Value::as_str)), (Some("pass"), Some("disabled")));
        let q = e.ctx.hub.query("tiktok", Value::Null).await.unwrap();
        assert_eq!(q.get_path("enabled"), Some(&Value::Bool(false)));
        assert_eq!(q.get_path("connected"), Some(&Value::Bool(false)));
        assert_eq!(q.get_path("counts.chat").and_then(Value::as_i64), Some(0));
        assert_eq!(shared.status.lock().state, LinkState::Disabled);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn config_and_session_overrides_start_and_stop_the_client() {
        let e = engine("schema = 1");
        let (factory, starts, calls) = counting_factory(vec![Ok(RoomStatus::Offline)], false);
        let shared = start(e.ctx.clone(), Some(factory), Tuning::default());
        // hot reload enables it
        e.cfg_tx.send(Arc::new(config("schema = 1\n[tiktok]\nenabled = true\nunique_id = \"@streamer\""))).unwrap();
        wait_for(|| shared.status.lock().state == LinkState::Offline).await;
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(shared.status.lock().unique_id, "streamer");
        // a broken section is rejected and the running client is kept
        e.cfg_tx.send(Arc::new(config("schema = 1\n[tiktok]\nenabled = \"yes\""))).unwrap();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(starts.load(Ordering::SeqCst), 1);
        assert_eq!(shared.status.lock().state, LinkState::Offline);
        // manual disconnect wins over the config for this session
        action(&e, "tiktok.disconnect");
        wait_for(|| shared.status.lock().state == LinkState::Disabled).await;
        let n = calls.lock().len();
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(calls.lock().len(), n);
        // …and connect resumes, here with a different account
        action(&e, "tiktok.connect other_user");
        wait_for(|| shared.status.lock().state == LinkState::Offline).await;
        assert_eq!(starts.load(Ordering::SeqCst), 2);
        let q = e.ctx.hub.query("tiktok", Value::Null).await.unwrap();
        assert_eq!(q.get_path("unique_id").and_then(Value::as_str), Some("other_user"));
        assert_eq!(q.get_path("manual"), Some(&Value::Bool(true)));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn connect_without_config_forces_a_session_and_needs_a_username() {
        let e = engine("schema = 1");
        let (factory, starts, _) = counting_factory(vec![Ok(RoomStatus::Offline)], false);
        let shared = start(e.ctx.clone(), Some(factory), Tuning::default());
        action(&e, "tiktok.connect");
        wait_for(|| shared.status.lock().state == LinkState::Failed).await;
        assert_eq!(starts.load(Ordering::SeqCst), 0);
        let h = wait_state(&e, "health.tiktok", |h| h.get_path("status").and_then(Value::as_str) == Some("fail")).await;
        assert!(h.get_path("detail").and_then(Value::as_str).unwrap().contains("unique_id"));
        action(&e, "tiktok.connect unique_id=@someone");
        wait_for(|| shared.status.lock().state == LinkState::Offline).await;
        assert_eq!(starts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn enabled_without_a_client_reports_a_failed_preflight() {
        let e = engine("schema = 1\n[tiktok]\nenabled = true\nunique_id = \"streamer\"");
        let shared = start(e.ctx.clone(), None, Tuning::default());
        wait_for(|| shared.status.lock().state == LinkState::Failed).await;
        let h = wait_state(&e, "health.tiktok", |h| h.get_path("status").and_then(Value::as_str) == Some("fail")).await;
        assert!(h.get_path("detail").and_then(Value::as_str).unwrap().contains("`client` feature"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_panicking_client_is_logged_and_restarted() {
        let e = engine("schema = 1\n[tiktok]\nenabled = true\nunique_id = \"streamer\"");
        let mut bus = e.ctx.hub.subscribe();
        let (factory, starts, _) = counting_factory(vec![Ok(RoomStatus::Offline)], true);
        let tuning = Tuning { restart_base: Duration::from_millis(50) };
        let shared = start(e.ctx.clone(), Some(factory), tuning);
        wait_for(|| starts.load(Ordering::SeqCst) == 2 && shared.status.lock().state == LinkState::Offline).await;
        assert_eq!(shared.counts.panics.load(Ordering::Relaxed), 1);
        let mut saw = false;
        while let Ok(m) = bus.try_recv() {
            if let Bus::Log { level, target, msg, .. } = &*m {
                saw |= level == "error" && target == "tiktok" && msg.contains("boom in client");
            }
        }
        assert!(saw, "the panic must be logged");
        // the engine is unaffected: the hub still answers
        wait_state(&e, "tiktok.status", |v| v.as_str() == Some("offline")).await;
    }
}
