//! Stream Deck runtime: one worker thread per configured deck. It finds and opens the
//! device (reconnecting on replug), turns key reports into `deck.key` events and actions,
//! derives every key's look from engine state, and pushes only changed key images.

pub mod hid;
pub mod proto;
pub mod render;

use crate::Shared;
use crate::config::{Action, DeckCfg, KeyDef};
use crate::exec::{self, KeyState};
use crossbeam_channel::{Receiver, Sender};
use render::{KeyVisual, Palette, Rgb, mix};
use se_proto::{Event, Id, Meta, Origin, Value};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

const CONFIRM_WINDOW: Duration = Duration::from_secs(3);
const FRAME: Duration = Duration::from_millis(33);

pub enum DeckCmd {
    Config(Arc<DeckCfg>),
    /// Page name, `next`, or `prev`.
    Page(String),
    Press {
        key: u8,
        page: Option<String>,
        origin: Origin,
        causal: Option<Id>,
    },
    Release {
        key: u8,
        page: Option<String>,
        origin: Origin,
        causal: Option<Id>,
    },
    /// Re-send every key image (theme/font change, manual refresh).
    Refresh,
    Stop,
}

/// Latest rendered key images (unrotated RGB) for the UI preview.
#[derive(Clone, Default)]
pub struct Preview {
    pub page: String,
    pub size: u32,
    pub version: u64,
    pub keys: Vec<Option<(u64, Arc<Vec<u8>>)>>,
}

pub struct DeckHandle {
    pub tx: Sender<DeckCmd>,
    pub preview: Arc<parking_lot::Mutex<Preview>>,
    pub status: Arc<parking_lot::Mutex<DeckStatus>>,
    join: Option<std::thread::JoinHandle<()>>,
}

#[derive(Clone, Debug, Default)]
pub struct DeckStatus {
    pub connected: bool,
    pub model: String,
    pub serial: String,
    pub firmware: String,
    pub devnode: String,
    pub usb: String,
    pub page: String,
    pub keys: u8,
    pub error: Option<String>,
    pub images_sent: u64,
    /// Active key cooldowns: (page, key) → (remaining ms, total ms).
    pub cooldowns: HashMap<(String, u8), (u64, u64)>,
}

impl DeckHandle {
    pub fn stop(mut self) {
        let _ = self.tx.send(DeckCmd::Stop);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}

pub fn spawn(shared: Arc<Shared>, cfg: Arc<DeckCfg>) -> DeckHandle {
    let (tx, rx) = crossbeam_channel::unbounded();
    let preview = Arc::new(parking_lot::Mutex::new(Preview::default()));
    let status = Arc::new(parking_lot::Mutex::new(DeckStatus::default()));
    let (p2, s2) = (preview.clone(), status.clone());
    let join = std::thread::Builder::new()
        .name(format!("se-deck-{}", cfg.id))
        .spawn(move || {
            let mut w = Worker::new(shared, cfg, rx, p2, s2);
            w.run();
        })
        .ok();
    DeckHandle { tx, preview, status, join }
}

struct Open {
    dev: hid::HidDev,
    model: &'static proto::Model,
    keys: Vec<bool>,
    /// Hash of the image last pushed per key.
    pushed: Vec<Option<u64>>,
    brightness: Option<u8>,
}

struct Worker {
    sh: Arc<Shared>,
    cfg: Arc<DeckCfg>,
    rx: Receiver<DeckCmd>,
    preview: Arc<parking_lot::Mutex<Preview>>,
    status: Arc<parking_lot::Mutex<DeckStatus>>,
    open: Option<Open>,
    last_attempt: Option<Instant>,
    page: String,
    /// Key held since (for hold-to-fire and pressed rendering), per key index.
    held: HashMap<u8, (Instant, bool)>,
    confirm: HashMap<(String, u8), Instant>,
    cooldown: HashMap<(String, u8), (Instant, u64)>,
    /// Keys whose press action ran (so release runs the matching release action).
    fired: HashMap<u8, (Action, Origin)>,
    last_frame: Instant,
    last_error: Option<String>,
    palette_ver: u64,
}

impl Worker {
    fn new(
        sh: Arc<Shared>,
        cfg: Arc<DeckCfg>,
        rx: Receiver<DeckCmd>,
        preview: Arc<parking_lot::Mutex<Preview>>,
        status: Arc<parking_lot::Mutex<DeckStatus>>,
    ) -> Worker {
        let page = cfg.start_page.clone();
        Worker {
            sh,
            cfg,
            rx,
            preview,
            status,
            open: None,
            last_attempt: None,
            page,
            held: HashMap::new(),
            confirm: HashMap::new(),
            cooldown: HashMap::new(),
            fired: HashMap::new(),
            last_frame: Instant::now() - FRAME,
            last_error: None,
            palette_ver: 0,
        }
    }

    fn addr(&self, leaf: &str) -> String {
        format!("controllers.{}.{leaf}", self.cfg.id)
    }

    fn declare(&self) {
        let h = &self.sh.hub;
        let own = |m: Meta| m.owner("controllers");
        h.declare(&self.addr("connected"), own(Meta::boolean(false).readonly().describe("Stream Deck connected")));
        h.declare(&self.addr("page"), own(Meta::string("").readonly().describe("Current deck page")));
        h.declare(&self.addr("serial"), own(Meta::string("").readonly()));
        h.declare(&self.addr("model"), own(Meta::string("").readonly()));
        h.declare(&self.addr("firmware"), own(Meta::string("").readonly()));
        h.declare(&self.addr("brightness"), own(Meta::int(self.cfg.brightness as i64, [0.0, 100.0]).unit("%").describe("Deck backlight")));
        h.publish(&self.addr("brightness"), Value::Int(self.cfg.brightness as i64));
        if self.cfg.primary {
            h.declare("controllers.page", own(Meta::string("").readonly().describe("Page of the primary deck (the UI pad grid mirrors it)")));
        }
        self.publish_page();
    }

    fn publish_page(&self) {
        self.sh.hub.publish(&self.addr("page"), Value::Str(self.page.clone()));
        if self.cfg.primary {
            self.sh.hub.publish("controllers.page", Value::Str(self.page.clone()));
        }
        self.status.lock().page = self.page.clone();
        self.sh.health_dirty();
    }

    fn run(&mut self) {
        self.declare();
        loop {
            // commands
            loop {
                match self.rx.try_recv() {
                    Ok(DeckCmd::Stop) => {
                        self.close(None);
                        return;
                    }
                    Ok(c) => self.command(c),
                    Err(crossbeam_channel::TryRecvError::Empty) => break,
                    Err(crossbeam_channel::TryRecvError::Disconnected) => {
                        self.close(None);
                        return;
                    }
                }
            }
            if self.open.is_none() && self.last_attempt.is_none_or(|t| t.elapsed() > Duration::from_millis(1500) || self.sh.hid_changed(t)) {
                self.last_attempt = Some(Instant::now());
                self.try_open();
            }
            // input
            let wait = FRAME.saturating_sub(self.last_frame.elapsed()).as_millis().clamp(1, 33) as i32;
            if let Some(o) = self.open.as_mut() {
                match o.dev.wait_readable(wait) {
                    Ok(true) => self.read_reports(),
                    Ok(false) => {}
                    Err(e) => self.close(Some(format!("device lost: {e}"))),
                }
            } else {
                match self.rx.recv_timeout(Duration::from_millis(wait as u64)) {
                    Ok(DeckCmd::Stop) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                    Ok(c) => self.command(c),
                    Err(_) => {}
                }
            }
            if self.last_frame.elapsed() >= FRAME {
                self.last_frame = Instant::now();
                self.tick_holds();
                self.frame();
            }
        }
    }

    fn command(&mut self, c: DeckCmd) {
        match c {
            DeckCmd::Config(cfg) => {
                let primary_changed = cfg.primary != self.cfg.primary;
                self.cfg = cfg;
                if self.cfg.page(&self.page).is_none() {
                    self.page = self.cfg.start_page.clone();
                }
                if primary_changed {
                    self.declare();
                }
                self.publish_page();
                self.invalidate();
            }
            DeckCmd::Page(p) => self.switch_page(&p),
            DeckCmd::Press { key, page, origin, causal } => {
                if let Some(p) = page.filter(|p| *p != self.page) {
                    self.switch_page(&p);
                }
                self.press(key, origin, causal);
            }
            DeckCmd::Release { key, page, origin, causal } => {
                let _ = page;
                self.release(key, origin, causal);
            }
            DeckCmd::Refresh => self.invalidate(),
            DeckCmd::Stop => {}
        }
    }

    fn invalidate(&mut self) {
        if let Some(o) = self.open.as_mut() {
            o.pushed.iter_mut().for_each(|p| *p = None);
        }
        self.preview.lock().keys.iter_mut().for_each(|k| *k = None);
    }

    fn switch_page(&mut self, p: &str) {
        let names: Vec<&str> = self.cfg.pages.iter().map(|p| p.name.as_str()).collect();
        let cur = names.iter().position(|n| *n == self.page).unwrap_or(0);
        let next = match p {
            "next" => names[(cur + 1) % names.len()].to_string(),
            "prev" | "previous" => names[(cur + names.len() - 1) % names.len()].to_string(),
            name if names.contains(&name) => name.to_string(),
            name => {
                self.sh.hub.log("warn", "deck", format!("deck `{}` has no page `{name}`", self.cfg.id));
                return;
            }
        };
        if next == self.page {
            return;
        }
        // held momentary keys on the old page are released
        let fired: Vec<(u8, (Action, Origin))> = self.fired.drain().collect();
        for (_, (a, origin)) in fired {
            self.sh.exec.release(&a, origin, None);
        }
        self.held.clear();
        self.page = next;
        self.publish_page();
    }

    fn key_def(&self, key: u8) -> Option<&KeyDef> {
        self.cfg.page(&self.page).and_then(|p| p.keys.get(&key))
    }

    fn press(&mut self, key: u8, origin: Origin, causal: Option<Id>) {
        let Some(kd) = self.key_def(key).cloned() else { return };
        let ck = (self.page.clone(), key);
        if let Some((t, ms)) = self.cooldown.get(&ck)
            && t.elapsed() < Duration::from_millis(*ms)
        {
            return;
        }
        if kd.b.hold_ms > 0 {
            self.held.insert(key, (Instant::now(), false));
            return;
        }
        self.held.insert(key, (Instant::now(), true));
        if exec::needs_confirm(&kd.action, &kd.b, &self.sh.core_config()) {
            match self.confirm.get(&ck) {
                Some(t) if t.elapsed() < CONFIRM_WINDOW => {
                    self.confirm.remove(&ck);
                }
                _ => {
                    self.confirm.insert(ck, Instant::now());
                    return;
                }
            }
        }
        self.fire(key, &kd, origin, causal);
    }

    fn fire(&mut self, key: u8, kd: &KeyDef, origin: Origin, causal: Option<Id>) {
        if kd.b.cooldown_ms > 0 {
            self.cooldown.insert((self.page.clone(), key), (Instant::now(), kd.b.cooldown_ms));
        }
        match self.sh.exec.press(&kd.action, origin, causal) {
            Some(page) => self.switch_page(&page),
            None => {
                if kd.action.wants_release() {
                    self.fired.insert(key, (kd.action.clone(), origin));
                }
            }
        }
    }

    fn release(&mut self, key: u8, origin: Origin, causal: Option<Id>) {
        self.held.remove(&key);
        if let Some((a, o)) = self.fired.remove(&key) {
            let _ = origin;
            self.sh.exec.release(&a, o, causal);
        }
    }

    /// Hold-to-fire keys: fire once the hold time is reached.
    fn tick_holds(&mut self) {
        let due: Vec<u8> = self
            .held
            .iter()
            .filter(|(k, (t, done))| !*done && self.key_def(**k).is_some_and(|kd| kd.b.hold_ms > 0 && t.elapsed() >= Duration::from_millis(kd.b.hold_ms)))
            .map(|(k, _)| *k)
            .collect();
        for k in due {
            if let Some(h) = self.held.get_mut(&k) {
                h.1 = true;
            }
            if let Some(kd) = self.key_def(k).cloned() {
                self.fire(k, &kd, Origin::Deck, None);
            }
        }
        self.confirm.retain(|_, t| t.elapsed() < CONFIRM_WINDOW);
        self.cooldown.retain(|_, (t, ms)| t.elapsed() < Duration::from_millis(*ms));
        let cds: HashMap<(String, u8), (u64, u64)> =
            self.cooldown.iter().map(|(k, (t, ms))| (k.clone(), (ms.saturating_sub(t.elapsed().as_millis() as u64), *ms))).collect();
        let mut s = self.status.lock();
        if !(cds.is_empty() && s.cooldowns.is_empty()) {
            s.cooldowns = cds;
        }
    }

    fn read_reports(&mut self) {
        let mut buf = [0u8; 512];
        loop {
            let Some(o) = self.open.as_mut() else { return };
            match o.dev.read_report(&mut buf) {
                Ok(Some(n)) => {
                    let Some(states) = proto::parse_keys(&buf[..n], o.model.keys as usize) else { continue };
                    let prev = std::mem::replace(&mut o.keys, states.clone());
                    for (k, (&now, &was)) in states.iter().zip(prev.iter()).enumerate() {
                        if now != was {
                            self.key_event(k as u8, now);
                        }
                    }
                }
                Ok(None) => return,
                Err(e) => {
                    self.close(Some(format!("read failed: {e}")));
                    return;
                }
            }
        }
    }

    fn key_event(&mut self, key: u8, down: bool) {
        let ev = Event::new(
            "deck.key",
            Origin::Deck,
            Value::map().with("page", self.page.clone()).with("key", key as i64).with("down", down).with("deck", self.cfg.id.clone()),
        );
        let id = ev.id;
        self.sh.hub.emit(ev);
        self.sh.learn_deck_key(&self.cfg.id, &self.page, key, down);
        if down {
            self.press(key, Origin::Deck, Some(id));
        } else {
            self.release(key, Origin::Deck, Some(id));
        }
    }

    fn try_open(&mut self) {
        let candidates: Vec<hid::HidInfo> = hid::scan().into_iter().filter(|h| h.vid == proto::VID && proto::model(h.pid).is_some()).collect();
        let mut last_err = None;
        for info in candidates {
            if let Some(want) = &self.cfg.serial
                && !info.uniq.eq_ignore_ascii_case(want)
            {
                continue;
            }
            if !self.sh.claim_hid(&info.devnode, &self.cfg.id) {
                continue;
            }
            match self.open_dev(&info) {
                Ok(()) => return,
                Err(e) => {
                    self.sh.release_hid(&info.devnode);
                    last_err = Some(format!("{}: {e}", info.devnode.display()));
                }
            }
        }
        let msg = last_err.unwrap_or_else(|| match &self.cfg.serial {
            Some(s) => format!("Stream Deck {s} not connected"),
            None => "no Stream Deck connected".into(),
        });
        if self.last_error.as_deref() != Some(msg.as_str()) {
            self.sh.hub.log("warn", "deck", msg.clone());
            self.last_error = Some(msg.clone());
            self.status.lock().error = Some(msg);
            self.sh.health_dirty();
        }
    }

    fn open_dev(&mut self, info: &hid::HidInfo) -> std::io::Result<()> {
        let model = proto::model(info.pid).ok_or_else(|| std::io::Error::other("unsupported model"))?;
        let dev = hid::HidDev::open(&info.devnode)?;
        let serial = dev.get_feature(proto::REPORT_SERIAL, proto::FEATURE_LEN).ok().and_then(|b| proto::parse_serial(&b)).unwrap_or_else(|| info.uniq.clone());
        let firmware = dev.get_feature(proto::REPORT_FIRMWARE, proto::FEATURE_LEN).ok().and_then(|b| proto::parse_firmware(&b)).unwrap_or_default();
        // the engine owns brightness; disable the unit's own sleep timer (older firmware ignores it)
        let _ = dev.set_feature(&proto::sleep_disable_report());
        self.open = Some(Open { dev, model, keys: vec![false; model.keys as usize], pushed: vec![None; model.keys as usize], brightness: None });
        let usb = hid::usb_port(&info.phys).to_string();
        {
            let mut s = self.status.lock();
            s.connected = true;
            s.model = model.name.into();
            s.serial = serial.clone();
            s.firmware = firmware.clone();
            s.devnode = info.devnode.display().to_string();
            s.usb = usb.clone();
            s.keys = model.keys;
            s.error = None;
        }
        self.last_error = None;
        let h = &self.sh.hub;
        h.publish(&self.addr("connected"), Value::Bool(true));
        h.publish(&self.addr("serial"), Value::Str(serial.clone()));
        h.publish(&self.addr("model"), Value::Str(model.name.into()));
        h.publish(&self.addr("firmware"), Value::Str(firmware.clone()));
        h.log("info", "deck", format!("{} {serial} (fw {firmware}) on {} [{usb}] → deck `{}`", model.name, info.devnode.display(), self.cfg.id));
        h.emit(Event::new("deck.connected", Origin::Deck, Value::map().with("deck", self.cfg.id.clone()).with("serial", serial).with("model", model.name)));
        self.sh.health_dirty();
        Ok(())
    }

    fn close(&mut self, why: Option<String>) {
        let Some(o) = self.open.take() else { return };
        // release anything held
        let fired: Vec<(u8, (Action, Origin))> = self.fired.drain().collect();
        for (_, (a, origin)) in fired {
            self.sh.exec.release(&a, origin, None);
        }
        self.held.clear();
        self.sh.release_hid(&o.dev.path);
        {
            let mut s = self.status.lock();
            s.connected = false;
            s.error = why.clone();
        }
        let h = &self.sh.hub;
        h.publish(&self.addr("connected"), Value::Bool(false));
        if let Some(w) = why {
            h.log("warn", "deck", format!("deck `{}`: {w}; waiting for it to come back", self.cfg.id));
            h.emit(Event::new("deck.disconnected", Origin::Deck, Value::map().with("deck", self.cfg.id.clone()).with("reason", w)));
        }
        self.last_attempt = Some(Instant::now());
        self.sh.health_dirty();
    }

    /// Compute every key's look, push changed images, update the preview.
    fn frame(&mut self) {
        let snap = self.sh.hub.snapshot.load();
        let (palette, pver) = self.sh.palette();
        if pver != self.palette_ver {
            self.palette_ver = pver;
            self.invalidate();
        }
        let fonts = self.sh.fonts();
        let (nkeys, size) = match &self.open {
            Some(o) => (o.model.keys as usize, o.model.key_px),
            None => (15, 72),
        };
        // brightness follows the resolved address
        let want = snap.f32(&self.addr("brightness")).map(|b| b.round().clamp(0.0, 100.0) as u8).unwrap_or(self.cfg.brightness);
        let mut failed = None;
        if let Some(o) = self.open.as_mut()
            && o.brightness != Some(want)
        {
            match o.dev.set_feature(&proto::brightness_report(want)) {
                Ok(()) => o.brightness = Some(want),
                Err(e) => failed = Some(format!("brightness: {e}")),
            }
        }
        let mut changed = false;
        let mut sent = 0u64;
        {
            let mut pv = self.preview.lock();
            if pv.page != self.page || pv.size != size || pv.keys.len() != nkeys {
                pv.page = self.page.clone();
                pv.size = size;
                pv.keys = vec![None; nkeys];
                changed = true;
            }
            for k in 0..nkeys {
                let v = self.visual(k as u8, &snap, &palette);
                let id = v.id();
                let needs_preview = pv.keys[k].as_ref().is_none_or(|(h, _)| *h != id);
                let needs_push = self.open.as_ref().is_some_and(|o| o.pushed[k] != Some(id));
                if !needs_preview && !needs_push {
                    continue;
                }
                let rgb = render::render_rgb(fonts.as_deref(), &v, size, false);
                if needs_push && failed.is_none() {
                    let rotated: Vec<u8> = rgb.chunks(3).rev().flatten().copied().collect();
                    match render::jpeg(&rotated, size) {
                        Ok(j) => {
                            let o = self.open.as_mut().expect("checked above");
                            let mut ok = true;
                            for r in proto::image_reports(k as u8, &j) {
                                if let Err(e) = o.dev.write_report(&r) {
                                    failed = Some(format!("image write failed: {e}"));
                                    ok = false;
                                    break;
                                }
                            }
                            if ok {
                                o.pushed[k] = Some(id);
                                sent += 1;
                            }
                        }
                        Err(e) => self.sh.hub.log("error", "deck", format!("jpeg encode: {e}")),
                    }
                }
                if needs_preview {
                    pv.keys[k] = Some((id, Arc::new(rgb)));
                    changed = true;
                }
            }
            if changed {
                pv.version += 1;
            }
        }
        if sent > 0 {
            self.status.lock().images_sent += sent;
        }
        if let Some(e) = failed {
            self.close(Some(e));
        }
    }

    /// The look of one key from its definition and the current engine state.
    fn visual(&self, key: u8, snap: &se_hub::Snapshot, pal: &Palette) -> KeyVisual {
        let Some(kd) = self.key_def(key) else { return KeyVisual { blank: true, ..Default::default() } };
        let cfg = self.sh.core_config();
        let st: KeyState = exec::state(&kd.action, &kd.b, snap, &self.page);
        let ck = (self.page.clone(), key);
        let held = self.held.get(&key);
        let (label, icon, color) = key_look(kd, &cfg, &self.cfg, pal, st.on);
        let mut v = KeyVisual { icon, label, ..Default::default() };
        // state → colors
        match &kd.action {
            Action::Scene { .. } => {
                if st.program {
                    v.bg = mix(pal.bg_dark, pal.bright_red, 0.55);
                    v.edge = Some((pal.bright_red, 5));
                } else if st.preview {
                    v.bg = mix(pal.bg_dark, pal.yellow, 0.3);
                    v.edge = Some((pal.yellow, 4));
                } else {
                    v.bg = color;
                    v.edge = Some((pal.muted, 1));
                }
                v.fg = render::contrast(v.bg);
            }
            Action::Ptt => {
                let vs = snap.str("controllers.voice.state").unwrap_or("idle");
                let (bg, text) = match vs {
                    "listening" => (pal.bright_red, "LISTEN"),
                    "transcribing" => (pal.yellow, "…"),
                    "confirm" => (pal.yellow, "CONFIRM?"),
                    "loading" | "downloading" => (mix(pal.bg_dark, pal.magenta, 0.3), "LOADING"),
                    "error" => (mix(pal.bg_dark, pal.red, 0.5), "VOICE ERR"),
                    _ => (mix(pal.bg_dark, color, 0.35), ""),
                };
                v.bg = bg;
                if !text.is_empty() && kd.b.label.is_none() {
                    v.label = text.into();
                }
                v.fg = render::contrast(bg);
            }
            _ => {
                if st.on {
                    v.bg = color;
                    v.fg = render::contrast(color);
                    v.edge = Some((mix(color, [255, 255, 255], 0.5), 2));
                } else {
                    v.bg = mix(pal.bg_dark, color, 0.3);
                    v.fg = mix(color, pal.fg, 0.55);
                }
            }
        }
        // preset hold remaining
        if let Action::Preset(p) = &kd.action
            && st.on
            && let (Some(rem), Some(total)) = (self.sh.preset_remaining(p), cfg.presets.get(p).and_then(|d| d.hold).map(|h| h.ms()))
            && total > 0
        {
            v.progress = Some((((rem as f64 / total as f64).clamp(0.0, 1.0) * 64.0).round() as u8, v.fg));
        }
        // hold-to-fire progress
        if let Some((t, done)) = held
            && kd.b.hold_ms > 0
            && !done
        {
            let f = (t.elapsed().as_millis() as f64 / kd.b.hold_ms as f64).clamp(0.0, 1.0);
            v.progress = Some(((f * 64.0).round() as u8, pal.bright_red));
            v.edge = Some((pal.bright_red, 3));
        }
        // cooldown sweep
        if let Some((t, ms)) = self.cooldown.get(&ck) {
            let rem = 1.0 - (t.elapsed().as_millis() as f64 / *ms as f64).clamp(0.0, 1.0);
            if rem > 0.0 {
                v.progress = Some(((rem * 64.0).round() as u8, pal.muted));
                v.dim = true;
            }
        }
        // confirmation pending: blink yellow
        if let Some(t) = self.confirm.get(&ck) {
            let phase = (t.elapsed().as_millis() / 400) % 2 == 0;
            v.bg = if phase { pal.yellow } else { mix(pal.bg_dark, pal.yellow, 0.35) };
            v.fg = render::contrast(v.bg);
            v.label = "CONFIRM?".into();
            v.icon = render::icon_glyph("warn").unwrap_or_default().into();
            v.edge = Some((pal.yellow, 3));
        }
        v.pressed = held.is_some() && kd.b.hold_ms == 0;
        v
    }
}

/// Default label/icon/color of a command key from its first command.
fn command_look(cmds: &[String], pal: &Palette) -> (String, String, Rgb) {
    let first = cmds.first().map(|s| s.as_str()).unwrap_or("");
    let verb = first.split_whitespace().next().unwrap_or("");
    let arg = first.split_whitespace().nth(1).unwrap_or("");
    match verb {
        "panic" => ("PANIC".into(), "panic".into(), pal.bright_red),
        "clean" => ("CLEAN".into(), "clean".into(), pal.cyan),
        "scene.take" => ("TAKE".into(), "take".into(), pal.accent),
        "scene.next" => ("NEXT".into(), "next".into(), pal.blue),
        "scene.prev" => ("PREV".into(), "prev".into(), pal.blue),
        "scene.go" | "scene.cut" => (arg.to_string(), "scene".into(), pal.bg_light),
        "preset.fire" | "preset.toggle" => (arg.to_uppercase(), "preset".into(), pal.accent),
        "mode.set" => (arg.to_uppercase(), "live".into(), pal.magenta),
        "session.marker" | "twitch.marker" => ("MARKER".into(), "marker".into(), pal.yellow),
        "lights.cue" => (arg.to_uppercase(), "light".into(), pal.yellow),
        "audio.play" => (arg.to_uppercase(), "music".into(), pal.green),
        "voice.confirm" => ("YES".into(), "check".into(), pal.green),
        "voice.cancel" => ("NO".into(), "cross".into(), pal.red),
        "deck.page" => (arg.to_uppercase(), "page".into(), pal.blue),
        _ => (verb.rsplit('.').next().unwrap_or(verb).to_uppercase(), "play".into(), pal.accent),
    }
}

/// Label, icon glyph, and base color of a key (explicit settings, else from its target).
pub fn key_look(kd: &KeyDef, cfg: &se_core::Config, deck: &DeckCfg, pal: &Palette, on: bool) -> (String, String, Rgb) {
    let upper = |s: &str| s.to_uppercase();
    let (mut label, mut icon, mut color): (String, String, Rgb) = match &kd.action {
        Action::Preset(p) => {
            let d = cfg.presets.get(p);
            (
                d.and_then(|d| d.label.clone()).unwrap_or_else(|| p.to_uppercase()),
                d.and_then(|d| d.icon.clone()).unwrap_or_else(|| "preset".into()),
                d.and_then(|d| d.color.as_deref()).and_then(|c| pal.color(c)).unwrap_or(pal.accent),
            )
        }
        Action::Scene { name, .. } => {
            let d = cfg.scenes.get(name);
            (d.and_then(|d| d.label.clone()).unwrap_or_else(|| name.clone()), "scene".into(), pal.bg_light)
        }
        Action::Toggle(a) => {
            let skip = usize::from(a.ends_with(".enabled") || a.ends_with(".on"));
            (upper(a.rsplit('.').nth(skip).unwrap_or(a)), if on { "toggle_on" } else { "toggle_off" }.into(), pal.green)
        }
        Action::Momentary(a) => (upper(a.rsplit('.').next().unwrap_or(a)), "hand".into(), pal.orange),
        Action::Commands { press, .. } => command_look(press, pal),
        Action::Page(p) => (deck.page(p).map(|pg| pg.label.clone()).unwrap_or_else(|| p.to_uppercase()), "page".into(), pal.blue),
        Action::Ptt => ("TALK".into(), "mic".into(), pal.magenta),
        Action::None => (String::new(), String::new(), pal.bg_light),
    };
    if let Some(l) = &kd.b.label {
        label = l.clone();
    }
    if let Some(i) = &kd.b.icon {
        icon = i.clone();
    }
    if let Some(c) = kd.b.color.as_deref().and_then(|c| pal.color(c)) {
        color = c;
    }
    let icon = render::icon_glyph(&icon).map(String::from).unwrap_or(icon);
    (label, icon, color)
}
