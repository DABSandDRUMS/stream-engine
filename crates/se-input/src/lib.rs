//! `se-input`: control surfaces (PLAN §10, M8): Stream Deck pages with rendered keys and
//! state feedback, MIDI (ALSA sequencer, 7/14-bit CC, NRPN, pitch-bend, MCU/HUI with banks,
//! LED rings, motor faders, scribble strips, meters; learn; stable device identity), voice
//! push-to-talk with a fixed grammar, and the raw MIDI port API used by Timelines (MTC).
//!
//! State lives under `controllers.*`; signals are `midi.<device>.<control>`; events
//! `deck.key`, `midi.<device>.<control>[.up|.touch]`, `drums.<pad>`, `voice.intent`,
//! `midi.learned`; actions `deck.*`, `midi.*`, `voice.*`. See `docs/controllers.md`.

pub mod config;
pub mod deck;
pub mod exec;
pub mod learn;
pub mod midi;
pub mod voice;

use arc_swap::ArcSwap;
use base64::Engine;
use config::{Controllers, DeckCfg, Parsed, VoiceCfg};
use deck::render::{Fonts, Palette};
use exec::Exec;
use parking_lot::{Mutex, RwLock};
use se_hub::{EngineCtx, Hub};
use se_proto::{Command, Event, Meta, Op, Origin, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

/// State shared by the deck workers, the MIDI thread, the voice worker, and the async tasks.
pub struct Shared {
    pub hub: Arc<Hub>,
    pub exec: Exec,
    pub project_root: PathBuf,
    pub data_dir: PathBuf,
    core: ArcSwap<se_core::Config>,
    ctl: ArcSwap<Controllers>,
    palette: ArcSwap<(Palette, u64)>,
    fonts: RwLock<Option<Arc<Fonts>>>,
    preset_rem: ArcSwap<HashMap<String, u64>>,
    hid_claims: Mutex<HashMap<PathBuf, String>>,
    hid_event: Mutex<Option<Instant>>,
    health_dirty: AtomicBool,
    learn: Mutex<Option<learn::Learn>>,
    learning: AtomicBool,
    meta: Mutex<HashMap<String, Option<[f64; 2]>>>,
    monitor_until: AtomicU64,
    t0: Instant,
    pub monitor: Mutex<VecDeque<midi::MonitorEntry>>,
    pub midi_status: Mutex<Vec<midi::DeviceStatus>>,
    decks: Mutex<BTreeMap<String, deck::DeckHandle>>,
    midi: Mutex<Option<midi::MidiHandle>>,
    watchers: Mutex<Vec<notify::RecommendedWatcher>>,
    voice: Mutex<Option<voice::VoiceHandle>>,
}

impl Shared {
    pub fn core_config(&self) -> Arc<se_core::Config> {
        self.core.load_full()
    }
    pub fn controllers(&self) -> Arc<Controllers> {
        self.ctl.load_full()
    }
    pub fn health_dirty(&self) {
        self.health_dirty.store(true, Ordering::Release);
    }
    pub fn palette(&self) -> (Arc<Palette>, u64) {
        let p = self.palette.load();
        (Arc::new(p.0.clone()), p.1)
    }
    pub fn fonts(&self) -> Option<Arc<Fonts>> {
        self.fonts.read().clone()
    }
    pub fn preset_remaining(&self, name: &str) -> Option<u64> {
        self.preset_rem.load().get(name).copied()
    }
    /// A hidraw node appeared/disappeared after `since`.
    pub fn hid_changed(&self, since: Instant) -> bool {
        self.hid_event.lock().is_some_and(|t| t > since && Instant::now() >= t)
    }
    /// Claim a hidraw node for a deck (identical decks are told apart by serial).
    pub fn claim_hid(&self, path: &Path, deck: &str) -> bool {
        let mut g = self.hid_claims.lock();
        match g.get(path) {
            Some(owner) => owner == deck,
            None => {
                g.insert(path.to_path_buf(), deck.to_string());
                true
            }
        }
    }
    pub fn release_hid(&self, path: &Path) {
        self.hid_claims.lock().remove(path);
    }
    pub fn monitor_on(&self) -> bool {
        (self.t0.elapsed().as_millis() as u64) < self.monitor_until.load(Ordering::Relaxed)
    }

    /// Numeric range of an address (metadata), cached. Called from worker threads.
    pub fn meta_range(&self, addr: &str) -> Option<[f64; 2]> {
        if let Some(r) = self.meta.lock().get(addr) {
            return *r;
        }
        if tokio::runtime::Handle::try_current().is_ok() {
            return None; // never block an async task; the next worker-thread call fills the cache
        }
        let hub = self.hub.clone();
        let a = addr.to_string();
        let entries = self.exec.rt.block_on(async move { tokio::time::timeout(Duration::from_millis(500), hub.get(&a, true)).await.unwrap_or_default() });
        let r = entries.into_iter().find(|e| e.address == addr).and_then(|e| e.meta).and_then(|m| m.range);
        self.meta.lock().insert(addr.to_string(), r);
        r
    }

    // ---- learn ----------------------------------------------------------------------------

    pub fn learning(&self) -> bool {
        self.learning.load(Ordering::Acquire)
    }

    fn arm_learn(&self, target: &str) {
        *self.learn.lock() = Some(learn::Learn { target: target.to_string(), started: Instant::now(), baseline: HashMap::new() });
        self.learning.store(true, Ordering::Release);
        self.hub.publish("controllers.learn.active", Value::Bool(true));
        self.hub.publish("controllers.learn.target", Value::Str(target.to_string()));
        self.hub.log("info", "controllers", format!("learn: move a control (or press a deck key) for `{target}`"));
    }

    fn disarm_learn(&self) -> Option<learn::Learn> {
        let l = self.learn.lock().take();
        self.learning.store(false, Ordering::Release);
        self.hub.publish("controllers.learn.active", Value::Bool(false));
        self.hub.publish("controllers.learn.target", Value::Str(String::new()));
        l
    }

    /// True once an absolute control moved clearly away from where it was first seen.
    /// `switch_like`: a CC that jumps straight to 0 or 127 (footswitch) counts on first sight.
    pub fn learn_moved(&self, dev: &str, idx: usize, value: f32, switch_like: bool) -> bool {
        let mut g = self.learn.lock();
        let Some(l) = g.as_mut() else { return false };
        let key = (dev.to_string(), idx);
        if switch_like && !l.baseline.contains_key(&key) && (value <= 0.0 || value >= 1.0) {
            return true;
        }
        let base = *l.baseline.entry(key).or_insert(value);
        (value - base).abs() >= learn::MOVE_THRESHOLD
    }

    /// A MIDI control moved while learning: write the mapping.
    pub fn learn_complete(self: &Arc<Self>, src: learn::Source) {
        let Some(l) = self.disarm_learn() else { return };
        let me = self.clone();
        self.exec.rt.spawn(async move {
            let meta = me.hub.get(&l.target, true).await.into_iter().find(|e| e.address == l.target).and_then(|e| e.meta);
            let core = me.core_config();
            let plan = match learn::plan(&l.target, meta.as_ref(), &core, &src) {
                Ok(p) => p,
                Err(e) => {
                    me.hub.log("error", "controllers", format!("learn `{}`: {e}", l.target));
                    return;
                }
            };
            let rel = src.file.clone().unwrap_or_else(|| format!("controllers/{}.toml", src.device));
            let root = me.project_root.clone();
            let (p2, s2, r2) = (plan.clone(), src.clone(), rel.clone());
            let res = tokio::task::spawn_blocking(move || -> Result<(), String> {
                let project = se_store::Project::open(&root).map_err(|e| format!("{e:#}"))?;
                let new_file = !root.join(&r2).exists();
                project.edit(&r2, |doc| learn::apply(doc, &p2, &s2, new_file).map_err(|e| anyhow::anyhow!(e))).map_err(|e| format!("{e:#}"))
            })
            .await
            .map_err(|e| e.to_string())
            .and_then(|r| r);
            match res {
                Ok(()) => {
                    let signal = format!("midi.{}.{}", src.device, src.control);
                    me.hub.log("info", "controllers", format!("learned {} → `{}` ({rel})", signal, l.target));
                    me.hub.emit(Event::new(
                        "midi.learned",
                        Origin::Midi,
                        Value::map()
                            .with("target", l.target.clone())
                            .with("signal", signal)
                            .with("device", src.device.clone())
                            .with("control", src.control.clone())
                            .with("file", rel),
                    ));
                }
                Err(e) => me.hub.log("error", "controllers", format!("learn: could not write {rel}: {e}")),
            }
        });
    }

    /// A deck key was pressed while learning: assign the target to that key.
    pub fn learn_deck_key(self: &Arc<Self>, deck: &str, page: &str, key: u8, down: bool) {
        if !down || !self.learning() {
            return;
        }
        let Some(l) = self.disarm_learn() else { return };
        let Some(file) = self.controllers().decks.iter().find(|d| d.id == deck).map(|d| d.file.clone()) else { return };
        let me = self.clone();
        let (deck, page) = (deck.to_string(), page.to_string());
        self.exec.rt.spawn(async move {
            let meta = me.hub.get(&l.target, true).await.into_iter().find(|e| e.address == l.target).and_then(|e| e.meta);
            let entries = learn::deck_entries(&l.target, meta.as_ref(), &me.core_config());
            match me.write_deck_key(&file, &page, key, Some(entries)).await {
                Ok(()) => me.hub.emit(Event::new(
                    "midi.learned",
                    Origin::Deck,
                    Value::map()
                        .with("target", l.target.clone())
                        .with("device", deck)
                        .with("control", format!("key.{key}"))
                        .with("page", page)
                        .with("file", file),
                )),
                Err(e) => me.hub.log("error", "controllers", format!("learn: {e}")),
            }
        });
    }

    async fn write_deck_key(&self, file: &str, page: &str, key: u8, entries: Option<Vec<(String, toml_edit::Value)>>) -> Result<(), String> {
        let root = self.project_root.clone();
        let (file, page) = (file.to_string(), page.to_string());
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            let project = se_store::Project::open(&root).map_err(|e| format!("{e:#}"))?;
            project
                .edit(&file, |doc| learn::assign_key(doc, &page, key, entries.as_deref()).map_err(|e| anyhow::anyhow!(e)))
                .map_err(|e| format!("{file}: {e:#}"))
        })
        .await
        .map_err(|e| e.to_string())?
    }

    fn deck(&self, id: Option<&str>) -> Option<(DeckCfg, Arc<Mutex<deck::DeckStatus>>, Arc<Mutex<deck::Preview>>, crossbeam_channel::Sender<deck::DeckCmd>)> {
        let ctl = self.controllers();
        let cfg = match id {
            Some(i) => ctl.decks.iter().find(|d| d.id == i)?,
            None => ctl.decks.iter().find(|d| d.primary)?,
        };
        let g = self.decks.lock();
        let h = g.get(&cfg.id)?;
        Some((cfg.clone(), h.status.clone(), h.preview.clone(), h.tx.clone()))
    }
}

/// Start the controllers subsystem. Hardware is opened on worker threads; this returns
/// immediately.
pub async fn start(ctx: EngineCtx) -> anyhow::Result<Arc<Shared>> {
    let cfg0 = ctx.config.borrow().clone();
    let sh = Arc::new(Shared {
        hub: ctx.hub.clone(),
        exec: Exec { hub: ctx.hub.clone(), rt: tokio::runtime::Handle::current() },
        project_root: ctx.project_root.clone(),
        data_dir: ctx.data_dir.clone(),
        core: ArcSwap::new(cfg0.clone()),
        ctl: ArcSwap::from_pointee(Controllers::default()),
        palette: ArcSwap::from_pointee((Palette::load(), 1)),
        fonts: RwLock::new(None),
        preset_rem: ArcSwap::from_pointee(HashMap::new()),
        hid_claims: Mutex::new(HashMap::new()),
        hid_event: Mutex::new(None),
        health_dirty: AtomicBool::new(true),
        learn: Mutex::new(None),
        learning: AtomicBool::new(false),
        meta: Mutex::new(HashMap::new()),
        monitor_until: AtomicU64::new(0),
        t0: Instant::now(),
        monitor: Mutex::new(VecDeque::new()),
        midi_status: Mutex::new(Vec::new()),
        decks: Mutex::new(BTreeMap::new()),
        midi: Mutex::new(None),
        watchers: Mutex::new(Vec::new()),
        voice: Mutex::new(None),
    });
    let h = &sh.hub;
    let own = |m: Meta| m.owner("controllers");
    h.declare("controllers.learn.active", own(Meta::boolean(false).readonly().describe("MIDI learn armed")));
    h.declare("controllers.learn.target", own(Meta::string("").readonly().describe("Address waiting for a control")));

    // fonts load via fontconfig (subprocesses): off the async threads
    {
        let sh = sh.clone();
        tokio::task::spawn_blocking(move || load_fonts(&sh));
    }
    watch_theme(&sh);
    watch_hidraw(&sh);

    // controllers config, decks, MIDI, voice
    let mut last_good = BTreeMap::new();
    apply_config(&sh, &ctx, &cfg0, &mut last_good, true);
    {
        let (sh, mut ctx) = (sh.clone(), ctx.clone());
        tokio::spawn(async move {
            while ctx.config.changed().await.is_ok() {
                let cfg = ctx.config.borrow_and_update().clone();
                apply_config(&sh, &ctx, &cfg, &mut last_good, false);
            }
        });
    }
    route_actions(&sh);
    register_queries(&sh);
    spawn_timers(&sh);
    let _ = INSTANCE.set(sh.clone());
    Ok(sh)
}

static INSTANCE: std::sync::OnceLock<Arc<Shared>> = std::sync::OnceLock::new();

/// Engine shutdown: stop the running subsystem (LEDs off, devices closed).
pub async fn shutdown() {
    if let Some(sh) = INSTANCE.get() {
        stop(sh.clone()).await;
    }
}

/// Stop workers: LEDs off, devices closed.
pub async fn stop(sh: Arc<Shared>) {
    let _ = tokio::task::spawn_blocking(move || {
        let decks: Vec<deck::DeckHandle> = std::mem::take(&mut *sh.decks.lock()).into_values().collect();
        for d in decks {
            d.stop();
        }
        if let Some(m) = sh.midi.lock().take() {
            m.stop();
        }
        if let Some(v) = sh.voice.lock().take() {
            v.stop();
        }
    })
    .await;
}

fn load_fonts(sh: &Shared) {
    match Fonts::load(None) {
        Ok(f) => {
            sh.hub.log("info", "deck", format!("key font: {}", f.family));
            *sh.fonts.write() = Some(Arc::new(f));
            bump_palette(sh, Palette::load());
        }
        Err(e) => {
            sh.hub.log("warn", "deck", format!("no key font ({e}); keys render without text"));
            *sh.fonts.write() = None;
        }
    }
}

fn bump_palette(sh: &Shared, p: Palette) {
    let v = sh.palette.load().1 + 1;
    sh.palette.store(Arc::new((p, v)));
}

/// Omarchy theme changes re-render every key; font changes are polled (`omarchy font current`).
fn watch_theme(sh: &Arc<Shared>) {
    use notify::Watcher;
    let Some(home) = std::env::var_os("HOME") else { return };
    let dir = PathBuf::from(home).join(".local/state/omarchy");
    let (tx, rx) = crossbeam_channel::unbounded::<()>();
    let w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && ev.paths.iter().any(|p| p.to_string_lossy().contains("current"))
        {
            let _ = tx.send(());
        }
    });
    let sh2 = sh.clone();
    let _ = std::thread::Builder::new().name("se-deck-theme".into()).spawn(move || {
        let _watcher = w.ok().and_then(|mut w| w.watch(&dir, notify::RecursiveMode::Recursive).ok().map(|_| w));
        let mut font = deck::render::omarchy_font();
        loop {
            match rx.recv_timeout(Duration::from_secs(5)) {
                Ok(()) => {
                    std::thread::sleep(Duration::from_millis(120));
                    while rx.try_recv().is_ok() {}
                    let p = Palette::load();
                    if p != sh2.palette.load().0 {
                        sh2.hub.log("info", "deck", "Omarchy theme changed; re-rendering keys");
                        bump_palette(&sh2, p);
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    let f = deck::render::omarchy_font();
                    if f.is_some() && f != font {
                        font = f;
                        load_fonts(&sh2);
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
            }
        }
    });
}

/// Replug detection for decks: watch `/dev` for hidraw nodes.
fn watch_hidraw(sh: &Arc<Shared>) {
    use notify::Watcher;
    let sh2 = sh.clone();
    let w = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(ev) = res
            && ev.paths.iter().any(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("hidraw")))
        {
            // udev applies permissions shortly after the node appears
            *sh2.hid_event.lock() = Some(Instant::now() + Duration::from_millis(400));
        }
    });
    match w {
        Ok(mut w) => match w.watch(Path::new("/dev"), notify::RecursiveMode::NonRecursive) {
            Ok(()) => sh.watchers.lock().push(w),
            Err(e) => sh.hub.log("warn", "deck", format!("no /dev watch ({e}); replug detection polls every 1.5 s")),
        },
        Err(e) => sh.hub.log("warn", "deck", format!("no /dev watch ({e}); replug detection polls every 1.5 s")),
    }
}

fn apply_config(sh: &Arc<Shared>, ctx: &EngineCtx, cfg: &Arc<se_core::Config>, last_good: &mut BTreeMap<String, Parsed>, first: bool) {
    sh.core.store(cfg.clone());
    sh.meta.lock().clear();
    let files = cfg.other.get("controllers").cloned().unwrap_or_default();
    let (ctl, errors) = config::parse_all(&files, &cfg.files, &sh.project_root, last_good);
    for (file, e) in &errors {
        sh.hub.log("error", "controllers", format!("{file}: {e} (keeping the last good version)"));
    }
    let ctl = Arc::new(ctl);
    let prev = sh.ctl.swap(ctl.clone());
    // decks
    {
        let mut decks = sh.decks.lock();
        let mut stale: Vec<deck::DeckHandle> = Vec::new();
        let ids: Vec<String> = decks.keys().cloned().collect();
        for id in ids {
            if !ctl.decks.iter().any(|d| d.id == id)
                && let Some(h) = decks.remove(&id)
            {
                stale.push(h);
            }
        }
        for d in &ctl.decks {
            match decks.get(&d.id) {
                Some(h) => {
                    if prev.decks.iter().find(|p| p.id == d.id) != Some(d) {
                        let _ = h.tx.send(deck::DeckCmd::Config(Arc::new(d.clone())));
                    }
                }
                None => {
                    decks.insert(d.id.clone(), deck::spawn(sh.clone(), Arc::new(d.clone())));
                }
            }
        }
        if !stale.is_empty() {
            std::thread::spawn(move || stale.into_iter().for_each(deck::DeckHandle::stop));
        }
    }
    // MIDI
    {
        let mut m = sh.midi.lock();
        match m.as_ref() {
            Some(h) => {
                let _ = h.tx.send(midi::MidiCmd::Config(ctl.clone()));
            }
            None if first => match midi::start(sh.clone(), ctl.clone()) {
                Ok(h) => *m = Some(h),
                Err(e) => sh.hub.log("error", "midi", format!("MIDI unavailable: {e}")),
            },
            None => {}
        }
    }
    // voice
    let vcfg = match VoiceCfg::from_value(ctx.project_section("voice").as_ref()) {
        Ok(v) => v,
        Err(e) => {
            sh.hub.log("error", "voice", format!("project.toml [voice]: {e}"));
            VoiceCfg::default()
        }
    };
    let pages: Vec<(String, String)> =
        ctl.decks.iter().find(|d| d.primary).map(|d| d.pages.iter().map(|p| (p.name.clone(), p.label.clone())).collect()).unwrap_or_default();
    let vocab = voice::vocab(cfg, &pages);
    let mut v = sh.voice.lock();
    match v.as_ref() {
        Some(h) => {
            let _ = h.tx.send(voice::VoiceCmd::Config(vcfg, vocab));
        }
        None => *v = Some(voice::spawn(sh.clone(), vcfg, vocab)),
    }
    sh.health_dirty();
}

// ---- actions --------------------------------------------------------------------------------

fn arg<'a>(args: &'a Value, key: &str, pos: usize) -> Option<&'a Value> {
    args.get_path(key).or_else(|| args.get_path("args").and_then(Value::as_list).and_then(|l| l.get(pos)))
}

fn arg_str(args: &Value, key: &str, pos: usize) -> Option<String> {
    arg(args, key, pos).map(|v| match v {
        Value::Str(s) => s.clone(),
        other => other.to_string(),
    })
}

fn route_actions(sh: &Arc<Shared>) {
    for prefix in ["deck", "midi", "voice"] {
        let mut rx = sh.hub.route_actions(prefix);
        let sh = sh.clone();
        tokio::spawn(async move {
            while let Some(c) = rx.recv().await {
                let Op::Action { name, args } = &c.op else { continue };
                if let Err(e) = handle_action(&sh, &c, name, args).await {
                    sh.hub.log("error", "controllers", format!("{name}: {e}"));
                }
            }
        });
    }
}

async fn handle_action(sh: &Arc<Shared>, c: &Command, name: &str, args: &Value) -> Result<(), String> {
    let deck_id = arg_str(args, "deck", usize::MAX);
    match name {
        "deck.page" => {
            let p = arg_str(args, "page", 0).ok_or("deck.page needs a page (name, next, prev)")?;
            let (_, _, _, tx) = sh.deck(deck_id.as_deref()).ok_or("no such deck")?;
            tx.send(deck::DeckCmd::Page(p)).map_err(|e| e.to_string())
        }
        "deck.press" | "deck.release" => {
            let key = arg(args, "key", 0).and_then(Value::as_i64).ok_or("needs key")?;
            let page = arg_str(args, "page", 1);
            let (_, _, _, tx) = sh.deck(deck_id.as_deref()).ok_or("no such deck")?;
            let key = u8::try_from(key).map_err(|_| "bad key")?;
            let cmd = if name == "deck.press" {
                deck::DeckCmd::Press { key, page, origin: c.origin, causal: Some(c.id) }
            } else {
                deck::DeckCmd::Release { key, page, origin: c.origin, causal: Some(c.id) }
            };
            tx.send(cmd).map_err(|e| e.to_string())
        }
        "deck.brightness" => {
            let v = arg(args, "value", 0).and_then(Value::as_f64).ok_or("needs a value 0–100")?;
            let (cfg, ..) = sh.deck(deck_id.as_deref()).ok_or("no such deck")?;
            sh.hub.command(
                Command::new(
                    c.origin,
                    Op::Set { address: format!("controllers.{}.brightness", cfg.id), value: Value::Int(v.round().clamp(0.0, 100.0) as i64) },
                )
                .caused_by(Some(c.id)),
            );
            Ok(())
        }
        "deck.refresh" => {
            for h in sh.decks.lock().values() {
                let _ = h.tx.send(deck::DeckCmd::Refresh);
            }
            Ok(())
        }
        "deck.assign" => {
            let (cfg, status, ..) = sh.deck(deck_id.as_deref()).ok_or("no such deck")?;
            let key = arg(args, "key", 0).and_then(Value::as_i64).and_then(|k| u8::try_from(k).ok()).ok_or("deck.assign needs key")?;
            let page = arg_str(args, "page", usize::MAX).unwrap_or_else(|| status.lock().page.clone());
            let clear = arg(args, "clear", usize::MAX).is_some_and(Value::truthy);
            let entries = if clear { None } else { Some(assign_entries(args, &sh.project_root)?) };
            sh.write_deck_key(&cfg.file, &page, key, entries).await?;
            sh.hub.log("info", "deck", format!("{}: page {page} key {key} {}", cfg.file, if clear { "cleared" } else { "assigned" }));
            Ok(())
        }
        "midi.learn" => {
            let target = arg_str(args, "target", 0).ok_or("midi.learn needs a target address")?;
            sh.arm_learn(&target);
            Ok(())
        }
        "midi.learn.cancel" => {
            if sh.disarm_learn().is_some() {
                sh.hub.log("info", "controllers", "learn cancelled");
            }
            Ok(())
        }
        "midi.bank" => {
            let device = arg_str(args, "device", 0).ok_or("midi.bank needs a device")?;
            let v = arg(args, "delta", 1).or_else(|| arg(args, "offset", usize::MAX));
            let (delta, set) = match (arg(args, "offset", usize::MAX).and_then(Value::as_i64), v) {
                (Some(o), _) => (None, Some(o.max(0) as u32)),
                (None, Some(Value::Str(s))) => (Some(s.trim_start_matches('+').parse::<i64>().map_err(|_| format!("bad bank step `{s}`"))?), None),
                (None, Some(x)) => (x.as_i64(), None),
                (None, None) => return Err("midi.bank needs a delta (+8/-8) or offset=".into()),
            };
            midi_cmd(sh, midi::MidiCmd::Bank { device, delta, set })
        }
        "midi.send" => {
            let device = arg_str(args, "device", 0).ok_or("midi.send needs a device")?;
            let bytes = match arg(args, "bytes", 1) {
                Some(Value::List(l)) => {
                    l.iter().map(|v| v.as_i64().and_then(|b| u8::try_from(b).ok()).ok_or("bytes must be 0–255")).collect::<Result<Vec<u8>, _>>()?
                }
                Some(Value::Str(s)) => midi::parse::parse_hex(s).ok_or("bad hex")?,
                _ => {
                    // positional hex tokens after the device: midi.send xtouch B0 30 2B
                    let l = args.get_path("args").and_then(Value::as_list).ok_or("midi.send needs bytes")?;
                    // all-digit tokens ("30", "07") arrive as numbers: print them back as two digits
                    let hex: String = l
                        .iter()
                        .skip(1)
                        .map(|v| match v {
                            Value::Int(n) => format!("{n:02}"),
                            other => other.to_string(),
                        })
                        .collect::<Vec<_>>()
                        .join(" ");
                    midi::parse::parse_hex(&hex).ok_or("bad hex bytes")?
                }
            };
            midi_cmd(sh, midi::MidiCmd::Send { device, bytes })
        }
        "midi.record" => {
            let device = arg_str(args, "device", 0).ok_or("midi.record needs a device")?;
            let seconds = arg(args, "seconds", 1).and_then(Value::as_f64).unwrap_or(10.0);
            let path = arg_str(args, "file", 2)
                .map(PathBuf::from)
                .unwrap_or_else(|| sh.data_dir.join("fixtures").join(format!("midi-{device}-{}.txt", se_clock::wall_now_ns() / 1_000_000_000)));
            midi_cmd(sh, midi::MidiCmd::Record { device, seconds, path })
        }
        "midi.refresh" => midi_cmd(sh, midi::MidiCmd::Refresh),
        "midi.unmap" => {
            let device = arg_str(args, "device", 0).ok_or("midi.unmap needs a device")?;
            let control = arg_str(args, "control", 1).ok_or("midi.unmap needs a control")?;
            let file =
                sh.controllers().midi.iter().find(|m| m.id == device).map(|m| m.file.clone()).ok_or_else(|| format!("`{device}` has no controllers file"))?;
            let root = sh.project_root.clone();
            let signal = format!("midi.{device}.{control}");
            tokio::task::spawn_blocking(move || -> Result<(), String> {
                let project = se_store::Project::open(&root).map_err(|e| format!("{e:#}"))?;
                project
                    .edit(&file, |doc| {
                        for (k, field, v) in [("map", "control", control.as_str()), ("binding", "signal", signal.as_str())] {
                            if let Some(a) = doc.get_mut(k).and_then(|i| i.as_array_of_tables_mut()) {
                                a.retain(|t| t.get(field).and_then(|x| x.as_str()) != Some(v));
                            }
                        }
                        Ok(())
                    })
                    .map_err(|e| format!("{e:#}"))
            })
            .await
            .map_err(|e| e.to_string())?
        }
        "voice.ptt" => {
            let op = match arg_str(args, "state", 0).as_deref() {
                Some("start" | "down" | "on" | "1" | "true") => voice::Ptt::Start,
                Some("stop" | "up" | "off" | "0" | "false") => voice::Ptt::Stop,
                None | Some("toggle") => voice::Ptt::Toggle,
                Some(o) => return Err(format!("voice.ptt: unknown `{o}` (start, stop, toggle)")),
            };
            voice_cmd(sh, voice::VoiceCmd::Ptt(op))
        }
        "voice.confirm" => voice_cmd(sh, voice::VoiceCmd::Confirm),
        "voice.cancel" => voice_cmd(sh, voice::VoiceCmd::Cancel),
        "voice.file" => {
            let p = arg_str(args, "path", 0).ok_or("voice.file needs a WAV path")?;
            voice_cmd(sh, voice::VoiceCmd::File(PathBuf::from(p)))
        }
        "voice.model.download" => voice_cmd(sh, voice::VoiceCmd::Download),
        other => Err(format!("unknown action `{other}`")),
    }
}

fn midi_cmd(sh: &Shared, c: midi::MidiCmd) -> Result<(), String> {
    sh.midi.lock().as_ref().ok_or("MIDI is not running")?.tx.send(c).map_err(|e| e.to_string())
}

fn voice_cmd(sh: &Shared, c: voice::VoiceCmd) -> Result<(), String> {
    sh.voice.lock().as_ref().ok_or("voice is not running")?.tx.send(c).map_err(|e| e.to_string())
}

/// `deck.assign` arguments → key table entries (validated like the file parser).
fn assign_entries(args: &Value, root: &Path) -> Result<Vec<(String, toml_edit::Value)>, String> {
    if args.get_path("image").is_some_and(|v| !matches!(v, Value::Str(_))) {
        return Err("deck.assign `image` must be a project-relative PNG path".into());
    }
    for key in ["clock", "disabled"] {
        if args.get_path(key).is_some_and(|v| !matches!(v, Value::Bool(_))) {
            return Err(format!("deck.assign `{key}` must be a boolean"));
        }
    }
    let mut out: Vec<(String, toml_edit::Value)> = Vec::new();
    let mut table = toml::Table::new();
    // `page` selects the page being edited, not the key's navigation action.
    for k in ["preset", "scene", "toggle", "momentary", "target_page", "label", "icon", "color", "state", "hold", "cooldown", "image"] {
        if let Some(Value::Str(s)) = args.get_path(k) {
            let k = if k == "target_page" { "page" } else { k };
            out.push((k.into(), toml_edit::Value::from(s.as_str())));
            table.insert(k.into(), toml::Value::String(s.clone()));
        }
    }
    for k in ["cut", "ptt", "confirm", "clock", "disabled"] {
        if let Some(Value::Bool(b)) = args.get_path(k) {
            out.push((k.into(), toml_edit::Value::from(*b)));
            table.insert(k.into(), toml::Value::Boolean(*b));
        }
    }
    // command lists run on press (`do`) and on key-up (`release`)
    for k in ["do", "release"] {
        let cmds: Vec<String> = match args.get_path(k) {
            Some(Value::Str(s)) => vec![s.clone()],
            Some(Value::List(l)) => l.iter().filter_map(|v| v.as_str().map(String::from)).collect(),
            _ => continue,
        };
        let mut a = toml_edit::Array::new();
        for c in &cmds {
            a.push(c.as_str());
        }
        out.push((k.into(), toml_edit::Value::Array(a)));
        table.insert(k.into(), toml::Value::Array(cmds.into_iter().map(toml::Value::String).collect()));
    }
    config::KeyDef::from_table(&table, root)?;
    if out.is_empty() {
        return Err("deck.assign needs an action or presentation (image=, clock=true, disabled=true, label=), or clear=true".into());
    }
    Ok(out)
}

// ---- queries --------------------------------------------------------------------------------

fn to_value<T: serde::Serialize>(x: &T) -> Value {
    serde_json::to_value(x).map(Value::from).unwrap_or_default()
}

fn hex_rgb(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn page_value(sh: &Shared, cfg: &DeckCfg, page: &str) -> Result<Value, String> {
    let p = cfg.page(page).ok_or_else(|| format!("deck `{}` has no page `{page}`", cfg.id))?;
    let cooldowns = sh.deck(Some(&cfg.id)).map(|(_, st, ..)| st.lock().cooldowns.clone()).unwrap_or_default();
    let snap = sh.hub.snapshot.load();
    let core = sh.core_config();
    let (pal, _) = sh.palette();
    let keys: Vec<Value> = p
        .keys
        .iter()
        .map(|(k, kd)| {
            let st = exec::state(&kd.action, &kd.b, &snap, page);
            let (label, icon, color) = deck::key_look(kd, &core, cfg, &pal, st.on);
            let mut v = Value::map()
                .with("key", *k as i64)
                .with("kind", kd.action.kind())
                .with("label", label)
                .with("icon", icon)
                .with(
                    "color",
                    kd.b.color
                        .as_ref()
                        .map(|_| hex_rgb(color))
                        .unwrap_or_else(|| if matches!(kd.action, config::Action::Preset(_)) { hex_rgb(color) } else { String::new() }),
                )
                .with("action", kd.action.describe())
                .with("image", kd.image.as_ref().map(|image| image.path.clone()).unwrap_or_default())
                .with("clock", kd.clock)
                .with("disabled", kd.disabled)
                .with("available", kd.available())
                .with("active", st.on)
                .with("program", st.program)
                .with("preview", st.preview)
                .with("hold_ms", if kd.available() { kd.b.hold_ms as i64 } else { 0 })
                .with("confirm", kd.available() && exec::needs_confirm(&kd.action, &kd.b, &core));
            if let Some((rem, total)) = cooldowns.get(&(page.to_string(), *k)) {
                v = v.with("cooldown_ms", *rem as i64).with("cooldown_total_ms", *total as i64);
            }
            match &kd.action {
                config::Action::Preset(n) => v = v.with("preset", n.clone()),
                config::Action::Scene { name, .. } => v = v.with("scene", name.clone()),
                config::Action::Toggle(a) | config::Action::Momentary(a) => v = v.with("address", a.clone()),
                config::Action::Page(pg) => v = v.with("page", pg.clone()),
                config::Action::Commands { press, release } => v = v.with("do", press.clone()).with("release", release.clone()),
                _ => {}
            }
            v
        })
        .collect();
    Ok(Value::map().with("deck", cfg.id.clone()).with("page", page).with("label", p.label.clone()).with("keys", keys))
}

fn register_queries(sh: &Arc<Shared>) {
    let sh2 = sh.clone();
    sh.hub.register_query(
        "controllers",
        Arc::new(move |name: String, args: Value| {
            let sh = sh2.clone();
            Box::pin(async move { query(&sh, &name, &args).await })
        }),
    );
}

async fn query(sh: &Arc<Shared>, name: &str, args: &Value) -> Result<Value, String> {
    let deck_id = args.get_path("deck").and_then(Value::as_str).map(String::from);
    match name {
        "controllers.page" => {
            let (cfg, status, ..) = sh.deck(deck_id.as_deref()).ok_or("no deck configured")?;
            let page = args.get_path("page").and_then(Value::as_str).map(String::from).unwrap_or_else(|| status.lock().page.clone());
            page_value(sh, &cfg, if page.is_empty() { &cfg.start_page } else { &page })
        }
        "controllers.pages" => {
            let (cfg, ..) = sh.deck(deck_id.as_deref()).ok_or("no deck configured")?;
            Ok(Value::List(
                cfg.pages.iter().map(|p| Value::map().with("name", p.name.clone()).with("label", p.label.clone()).with("keys", p.keys.len())).collect(),
            ))
        }
        "controllers.deck" => {
            let ctl = sh.controllers();
            let mut decks = Vec::new();
            for d in &ctl.decks {
                let Some((cfg, status, ..)) = sh.deck(Some(&d.id)) else { continue };
                let s = status.lock().clone();
                let pages: Vec<Value> = cfg.pages.iter().filter_map(|p| page_value(sh, &cfg, &p.name).ok()).collect();
                decks.push(
                    Value::map()
                        .with("id", cfg.id.clone())
                        .with("file", cfg.file.clone())
                        .with("primary", cfg.primary)
                        .with("connected", s.connected)
                        .with("model", s.model)
                        .with("serial", s.serial)
                        .with("firmware", s.firmware)
                        .with("devnode", s.devnode)
                        .with("usb", s.usb)
                        .with("keys", if s.keys == 0 { 15 } else { s.keys as i64 })
                        .with("cols", if s.keys == 32 { 8 } else { 5 })
                        .with("page", s.page)
                        .with("error", s.error.map(Value::Str).unwrap_or_default())
                        .with("images_sent", s.images_sent as i64)
                        .with(
                            "brightness",
                            sh.hub.snapshot.load().f32(&format!("controllers.{}.brightness", cfg.id)).map(|b| b as f64).unwrap_or(cfg.brightness as f64),
                        )
                        .with("pages", pages),
                );
            }
            Ok(Value::List(decks))
        }
        "controllers.deck.preview" => {
            let (cfg, _, preview, _) = sh.deck(deck_id.as_deref()).ok_or("no deck configured")?;
            let pv = preview.lock().clone();
            if args.get_path("since").and_then(Value::as_i64) == Some(pv.version as i64) {
                return Ok(Value::map().with("version", pv.version as i64).with("unchanged", true));
            }
            let b64 = base64::engine::general_purpose::STANDARD;
            let keys: Vec<Value> = pv
                .keys
                .iter()
                .enumerate()
                .map(|(k, e)| {
                    let jpeg = e.as_ref().and_then(|(_, rgb)| deck::render::jpeg(rgb, pv.size).ok());
                    Value::map().with("key", k as i64).with("jpeg", jpeg.map(|j| Value::Str(b64.encode(j))).unwrap_or_default())
                })
                .collect();
            Ok(Value::map().with("deck", cfg.id).with("page", pv.page).with("size", pv.size as i64).with("version", pv.version as i64).with("keys", keys))
        }
        "controllers.midi" => Ok(to_value(&*sh.midi_status.lock())),
        "controllers.midi.ports" => Ok(to_value(&midi::ports())),
        "controllers.midi.monitor" => {
            // watching turns the monitor on for 30 s
            sh.monitor_until.store(sh.t0.elapsed().as_millis() as u64 + 30_000, Ordering::Relaxed);
            let n = args.get_path("n").and_then(Value::as_i64).unwrap_or(100).clamp(1, 500) as usize;
            let dev = args.get_path("device").and_then(Value::as_str);
            let g = sh.monitor.lock();
            let items: Vec<&midi::MonitorEntry> = g.iter().rev().filter(|e| dev.is_none_or(|d| e.device == d)).take(n).collect();
            Ok(to_value(&items))
        }
        "controllers.voice" => {
            let v = sh.voice.lock();
            let st = v.as_ref().map(|h| h.status.lock().clone()).unwrap_or_default();
            Ok(to_value(&st))
        }
        "controllers.learn" => {
            let g = sh.learn.lock();
            Ok(match g.as_ref() {
                Some(l) => Value::map().with("active", true).with("target", l.target.clone()).with("elapsed_ms", l.started.elapsed().as_millis() as i64),
                None => Value::map().with("active", false),
            })
        }
        other => Err(format!("unknown query `{other}`")),
    }
}

// ---- timers: preset hold progress, learn timeout, health ----------------------------------

fn health(status: &str, detail: impl Into<String>) -> Value {
    Value::map().with("status", status).with("detail", detail.into())
}

fn spawn_timers(sh: &Arc<Shared>) {
    let sh = sh.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        let mut last_health = Instant::now() - Duration::from_secs(10);
        let mut prev: HashMap<String, Value> = HashMap::new();
        loop {
            tick.tick().await;
            // remaining hold of active presets (key progress bars)
            let core = sh.core_config();
            let snap = sh.hub.snapshot.load();
            let any = core.presets.values().any(|p| p.hold.is_some() && snap.bool(&format!("preset.{}.active", p.name)));
            if any {
                if let Ok(Value::List(items)) = sh.hub.query("active", Value::Null).await {
                    let m: HashMap<String, u64> = items
                        .iter()
                        .filter(|i| i.get_path("kind").and_then(Value::as_str) == Some("preset"))
                        .filter_map(|i| Some((i.get_path("name")?.as_str()?.to_string(), i.get_path("remaining_ms")?.as_i64()?.max(0) as u64)))
                        .collect();
                    sh.preset_rem.store(Arc::new(m));
                }
            } else if !sh.preset_rem.load().is_empty() {
                sh.preset_rem.store(Arc::new(HashMap::new()));
            }
            // learn timeout
            let timed_out = sh.learn.lock().as_ref().is_some_and(|l| l.started.elapsed() > Duration::from_secs(30));
            if timed_out && let Some(l) = sh.disarm_learn() {
                sh.hub.log("info", "controllers", format!("learn for `{}` timed out", l.target));
                sh.hub.emit(Event::new("midi.learn.timeout", Origin::Midi, Value::map().with("target", l.target)));
            }
            // health
            if sh.health_dirty.swap(false, Ordering::AcqRel) || last_health.elapsed() > Duration::from_secs(2) {
                last_health = Instant::now();
                for (k, v) in health_values(&sh) {
                    if prev.get(&k) != Some(&v) {
                        sh.hub.publish(&k, v.clone());
                        prev.insert(k, v);
                    }
                }
            }
        }
    });
}

fn health_values(sh: &Shared) -> Vec<(String, Value)> {
    let mut out = Vec::new();
    let ctl = sh.controllers();
    // deck
    let mut ok = Vec::new();
    let mut bad = Vec::new();
    for d in &ctl.decks {
        let Some((cfg, status, ..)) = sh.deck(Some(&d.id)) else { continue };
        let s = status.lock();
        if s.connected {
            ok.push(format!("{} {} ({}, page {})", s.model, s.serial, cfg.id, s.page));
        } else {
            bad.push(format!("{}: {}", cfg.id, s.error.clone().unwrap_or_else(|| "not connected".into())));
        }
    }
    let deck = if ctl.decks.is_empty() {
        let present = deck::hid::scan().iter().any(|h| h.vid == deck::proto::VID && deck::proto::model(h.pid).is_some());
        health("warn", if present { "a Stream Deck is connected but no controllers/*.toml deck is configured" } else { "no Stream Deck configured" })
    } else if bad.is_empty() {
        health("pass", ok.join("; "))
    } else {
        health("fail", bad.join("; "))
    };
    out.push(("health.deck".to_string(), deck));
    // MIDI (configured devices)
    for d in sh.midi_status.lock().iter().filter(|d| d.configured) {
        let v = if d.online {
            health("pass", format!("{} ({})", d.client, d.usb.clone().unwrap_or_else(|| "virtual".into())))
        } else {
            health("fail", d.error.clone().unwrap_or_else(|| "not connected".into()))
        };
        out.push((format!("health.midi.{}", d.id), v));
    }
    // voice
    if let Some(v) = sh.voice.lock().as_ref() {
        let s = v.status.lock();
        let h = match s.state.as_str() {
            "disabled" => health("pass", "voice disabled ([voice] enabled = false)"),
            "error" => health("fail", s.error.clone().unwrap_or_else(|| "voice unavailable".into())),
            "downloading" => health("warn", format!("downloading whisper model {} ({:.0}%)", s.model, s.download.unwrap_or(0.0) * 100.0)),
            "loading" => health("warn", format!("loading whisper model {}", s.model)),
            _ => health("pass", format!("whisper {} ready; capture `{}`", s.model, s.device)),
        };
        out.push(("health.voice".to_string(), h));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assign_keeps_edit_page_separate_from_key_action() {
        let root = Path::new(".");
        let saved_action = Value::map().with("page", "apple_music").with("preset", "apple_music_pause");
        let navigation = Value::map().with("page", "apple_music").with("target_page", "main");
        for (args, expected) in [
            (saved_action, config::Action::Preset("apple_music_pause".into())),
            (navigation, config::Action::Page("main".into())),
        ] {
            let entries = assign_entries(&args, root).unwrap();
            let mut doc = toml_edit::DocumentMut::new();
            learn::assign_key(&mut doc, "apple_music", 4, Some(&entries)).unwrap();
            let stored: toml::Table = doc.to_string().parse().unwrap();
            let key = stored["page"]["apple_music"]["key"]["4"].as_table().unwrap();
            assert_eq!(config::Action::from_table(key).unwrap(), expected);
        }
        assert!(assign_entries(&Value::map().with("page", "apple_music"), root).is_err());
    }

    #[test]
    fn assign_writes_press_and_release_lists() {
        let list = |c: &str| Value::List(vec![Value::from(c)]);
        let args = Value::map().with("do", list("preset.fire hype")).with("release", list("preset.release hype")).with("label", "Hype");
        let entries = assign_entries(&args, Path::new(".")).unwrap();
        let text = entries.iter().map(|(k, v)| format!("{k} = {v}")).collect::<Vec<_>>().join("\n");
        let table: toml::Table = text.parse().unwrap();
        assert_eq!(
            config::Action::from_table(&table).unwrap(),
            config::Action::Commands { press: vec!["preset.fire hype".into()], release: vec!["preset.release hype".into()] }
        );
        // a release list alone is a valid key too (nothing on press)
        assert!(assign_entries(&Value::map().with("release", list("scene.take")), Path::new(".")).is_ok());
        assert!(assign_entries(&Value::map().with("release", list("set")), Path::new(".")).is_err(), "release lines are checked like press lines");
    }
}
