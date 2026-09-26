//! Hyprland monitors: parse `hyprctl monitors all -j`, pick outputs, follow hotplug, place windows.
//!
//! Pure parts (parsing, [`pick`], [`confidence_target`], [`follow`], [`place_lua`], [`place_check`])
//! are cheap and UI-thread safe. Everything that talks to Hyprland ([`Hypr`]) spawns `hyprctl`
//! (bounded by [`HYPRCTL_TIMEOUT`]) and belongs on a background thread: use [`MonitorWatcher`],
//! which owns that thread, follows the event socket, and applies window placements.

use crate::layout::ConfidenceCfg;
use anyhow::{Context as _, Result, anyhow, bail};
use crossbeam_channel::{Receiver, Sender};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

/// Upper bound for one `hyprctl` invocation.
pub const HYPRCTL_TIMEOUT: Duration = Duration::from_secs(2);
/// Fallback poll interval of [`MonitorWatcher`] (the event socket is the primary signal).
pub const POLL_INTERVAL: Duration = Duration::from_secs(5);
/// Hotplug events arrive in bursts (`monitoradded` + `monitoraddedv2` + `configreloaded`).
const EVENT_DEBOUNCE: Duration = Duration::from_millis(150);
const RECONNECT_DELAY: Duration = Duration::from_secs(1);

/// One output from `hyprctl -j monitors all`. Unknown fields are ignored.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Monitor {
    pub id: i64,
    /// Connector name (`DP-1`, `HDMI-A-1`).
    pub name: String,
    /// `"<make> <model> <serial>"` as Hyprland reports it.
    pub description: String,
    pub make: String,
    pub model: String,
    pub serial: String,
    /// Mode size in physical pixels.
    pub width: u32,
    pub height: u32,
    #[serde(rename = "refreshRate")]
    pub refresh: f32,
    /// Position in the global layout (logical pixels).
    pub x: i32,
    pub y: i32,
    pub scale: f32,
    /// wl_output transform (odd values rotate by 90°/270°).
    pub transform: u8,
    /// Listed by `monitors all` but not in use (e.g. the TV parked with `tv off`).
    pub disabled: bool,
    #[serde(rename = "dpmsStatus")]
    pub dpms: bool,
    pub focused: bool,
}

impl Monitor {
    /// Size in logical points (what a fullscreen window gets), honouring scale and rotation.
    pub fn logical_size(&self) -> [f32; 2] {
        let s = if self.scale > 0.0 { self.scale } else { 1.0 };
        let (w, h) = (self.width as f32 / s, self.height as f32 / s);
        if self.transform % 2 == 1 { [h, w] } else { [w, h] }
    }
}

/// Parse the JSON printed by `hyprctl -j monitors all`.
pub fn parse_monitors(json: &str) -> Result<Vec<Monitor>> {
    serde_json::from_str(json).context("parse `hyprctl -j monitors all`")
}

/// One window from `hyprctl -j clients`. Unknown fields are ignored.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Client {
    /// `0x…` window address (usable as `address:0x…` selector).
    pub address: String,
    /// Wayland app-id (`stream-engine.program`).
    pub class: String,
    pub title: String,
    /// Monitor id ([`Monitor::id`]); `-1` while unmapped.
    pub monitor: i64,
    /// Hyprland fullscreen mode bits: 0 none, 1 maximized, 2 fullscreen, 3 both.
    pub fullscreen: u8,
    pub pid: i64,
    pub mapped: bool,
}

impl Client {
    pub fn is_fullscreen(&self) -> bool {
        self.fullscreen & 2 != 0
    }
}

/// Parse the JSON printed by `hyprctl -j clients`.
pub fn parse_clients(json: &str) -> Result<Vec<Client>> {
    serde_json::from_str(json).context("parse `hyprctl -j clients`")
}

/// A monitor selector as written in layouts: `desc:<substring>`, `make:<substring>`,
/// `model:<substring>` (all ASCII case-insensitive), otherwise the connector name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MonitorMatch {
    Desc(String),
    Make(String),
    Model(String),
    Name(String),
}

impl MonitorMatch {
    pub fn parse(s: &str) -> MonitorMatch {
        let s = s.trim();
        let prefixed = |p: &str| s.get(..p.len()).filter(|head| head.eq_ignore_ascii_case(p)).map(|_| s[p.len()..].trim().to_string());
        if let Some(v) = prefixed("desc:") {
            MonitorMatch::Desc(v)
        } else if let Some(v) = prefixed("make:") {
            MonitorMatch::Make(v)
        } else if let Some(v) = prefixed("model:") {
            MonitorMatch::Model(v)
        } else {
            MonitorMatch::Name(s.to_string())
        }
    }

    /// Empty patterns match nothing (an empty `desc:` must not grab an arbitrary output).
    pub fn matches(&self, m: &Monitor) -> bool {
        match self {
            MonitorMatch::Desc(p) => contains_ci(&m.description, p),
            MonitorMatch::Make(p) => contains_ci(&m.make, p),
            MonitorMatch::Model(p) => contains_ci(&m.model, p),
            MonitorMatch::Name(n) => !n.is_empty() && m.name.eq_ignore_ascii_case(n),
        }
    }
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    !needle.is_empty() && hay.as_bytes().windows(needle.len()).any(|w| w.eq_ignore_ascii_case(needle.as_bytes()))
}

impl std::str::FromStr for MonitorMatch {
    type Err = std::convert::Infallible;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(MonitorMatch::parse(s))
    }
}

impl fmt::Display for MonitorMatch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MonitorMatch::Desc(p) => write!(f, "desc:{p}"),
            MonitorMatch::Make(p) => write!(f, "make:{p}"),
            MonitorMatch::Model(p) => write!(f, "model:{p}"),
            MonitorMatch::Name(n) => f.write_str(n),
        }
    }
}

impl Serialize for MonitorMatch {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for MonitorMatch {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        Ok(MonitorMatch::parse(&s))
    }
}

/// Which list a [`Pick`] came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    Preferred,
    Fallback,
    /// Neither list matched: any enabled monitor other than the main one.
    LastResort,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pick<'a> {
    pub monitor: &'a Monitor,
    pub tier: Tier,
}

impl Pick<'_> {
    pub fn is_fallback(&self) -> bool {
        self.tier != Tier::Preferred
    }
}

/// First enabled monitor matching `preferred` (in list order), else `fallback`, else any enabled
/// monitor that `main` doesn't match. Disabled monitors are never picked.
pub fn pick<'a>(monitors: &'a [Monitor], preferred: &[MonitorMatch], fallback: &[MonitorMatch], main: Option<&MonitorMatch>) -> Option<Pick<'a>> {
    let first = |list: &[MonitorMatch]| list.iter().find_map(|mm| monitors.iter().find(|m| !m.disabled && mm.matches(m)));
    if let Some(monitor) = first(preferred) {
        return Some(Pick { monitor, tier: Tier::Preferred });
    }
    if let Some(monitor) = first(fallback) {
        return Some(Pick { monitor, tier: Tier::Fallback });
    }
    monitors.iter().find(|m| !m.disabled && !main.is_some_and(|mm| mm.matches(m))).map(|monitor| Pick { monitor, tier: Tier::LastResort })
}

/// Where the confidence (`stream-engine.program`) window belongs right now: the TV when it is
/// present and enabled, else the configured fallback (DP-2), else any non-main monitor.
pub fn confidence_target<'a>(monitors: &'a [Monitor], cfg: &ConfidenceCfg, main: Option<&MonitorMatch>) -> Option<Pick<'a>> {
    if !cfg.enabled {
        return None;
    }
    pick(monitors, &cfg.monitors, &cfg.fallback, main)
}

/// Why [`follow`] chose a placement.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reason {
    /// A confidence monitor (the TV) is present.
    Preferred,
    /// The TV is absent or disabled: on the fallback monitor.
    Fallback,
    /// Neither the TV nor the fallback exists: on some other non-main monitor.
    LastResort,
    /// Only the main monitor is left: nowhere to put the window.
    NoMonitor,
    /// Confidence is disabled in the layout.
    Disabled,
    /// No monitor list yet (watcher still starting, or not running under Hyprland): keep `prev`.
    Unknown,
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Reason::Preferred => "confidence monitor present",
            Reason::Fallback => "confidence monitor absent, using fallback",
            Reason::LastResort => "confidence and fallback monitors absent, using another monitor",
            Reason::NoMonitor => "no monitor available besides the main one",
            Reason::Disabled => "confidence window disabled",
            Reason::Unknown => "monitor list unknown",
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Placement {
    /// Connector name to put the window on; `None` = no target (close/hide the window).
    pub monitor: Option<String>,
    /// `monitor` differs from `prev`: the window has to move.
    pub changed: bool,
    pub reason: Reason,
}

/// Hotplug follower for the confidence window: TV present → TV; TV absent/disabled → fallback
/// (DP-2); TV back → TV again. `prev` is where the window is (or was last sent).
pub fn follow(prev: Option<&str>, monitors: &[Monitor], cfg: &ConfidenceCfg, main: Option<&MonitorMatch>) -> Placement {
    let (monitor, reason) = if !cfg.enabled {
        (None, Reason::Disabled)
    } else if monitors.is_empty() {
        return Placement { monitor: prev.map(str::to_string), changed: false, reason: Reason::Unknown };
    } else {
        match confidence_target(monitors, cfg, main) {
            Some(p) => (
                Some(p.monitor.name.clone()),
                match p.tier {
                    Tier::Preferred => Reason::Preferred,
                    Tier::Fallback => Reason::Fallback,
                    Tier::LastResort => Reason::LastResort,
                },
            ),
            None => (None, Reason::NoMonitor),
        }
    };
    Placement { changed: monitor.as_deref() != prev, monitor, reason }
}

/// State of a window relative to a desired placement (see [`place_check`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PlaceCheck {
    /// No mapped window with that class (yet).
    Missing,
    /// Already on the target monitor with the wanted fullscreen state (address given).
    Placed(String),
    /// Exists but needs a move and/or fullscreen change (address given).
    Misplaced(String),
}

/// Decide whether `class` needs [`Hypr::place`], so redundant moves are skipped.
pub fn place_check(clients: &[Client], class: &str, target: &Monitor, fullscreen: bool) -> PlaceCheck {
    match clients.iter().find(|c| c.mapped && c.class == class) {
        None => PlaceCheck::Missing,
        Some(c) if c.monitor == target.id && c.is_fullscreen() == fullscreen => PlaceCheck::Placed(c.address.clone()),
        Some(c) => PlaceCheck::Misplaced(c.address.clone()),
    }
}

/// Escape regex metacharacters (Hyprland window selectors are regexes).
pub fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        if matches!(c, '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// A double-quoted Lua string literal.
pub fn lua_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 || c == '\u{7f}' => out.push_str(&format!("\\{:03}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Hyprland selector for the window whose class (app-id) is exactly `class`.
pub fn class_selector(class: &str) -> String {
    format!("class:^{}$", regex_escape(class))
}

/// The Lua chunk [`Hypr::place`] runs: move without stealing focus, then set/unset fullscreen.
pub fn place_lua(class: &str, monitor: &str, fullscreen: bool) -> String {
    let window = lua_string(&class_selector(class));
    let monitor = lua_string(monitor);
    let action = if fullscreen { "set" } else { "unset" };
    format!(
        "hl.dispatch(hl.dsp.window.move({{ monitor = {monitor}, window = {window}, follow = false }}))\n\
         hl.dispatch(hl.dsp.window.fullscreen({{ mode = \"fullscreen\", action = \"{action}\", window = {window} }}))"
    )
}

/// Handle on one Hyprland instance; every call spawns `hyprctl` (blocking up to [`HYPRCTL_TIMEOUT`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hypr {
    pub signature: String,
    pub runtime_dir: PathBuf,
    hyprctl: PathBuf,
}

impl Hypr {
    /// `$HYPRLAND_INSTANCE_SIGNATURE` if its instance dir exists, else the live instance under
    /// `$XDG_RUNTIME_DIR/hypr/` (or `/run/user/<uid>/hypr/`). `None` when not under Hyprland.
    pub fn detect() -> Option<Hypr> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR").filter(|v| !v.is_empty()).map(PathBuf::from).unwrap_or_else(|| {
            // SAFETY: getuid has no preconditions and cannot fail.
            PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() }))
        });
        let env_sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok();
        Self::detect_in(&runtime, env_sig.as_deref(), PathBuf::from("hyprctl"))
    }

    fn detect_in(runtime: &Path, env_sig: Option<&str>, hyprctl: PathBuf) -> Option<Hypr> {
        let hypr_dir = runtime.join("hypr");
        let make = |sig: &str| Hypr { signature: sig.to_string(), runtime_dir: runtime.to_path_buf(), hyprctl: hyprctl.clone() };
        if let Some(sig) = env_sig.filter(|s| !s.is_empty() && hypr_dir.join(s).is_dir()) {
            return Some(make(sig));
        }
        let mut sigs: Vec<String> = std::fs::read_dir(&hypr_dir)
            .ok()?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().join(".socket.sock").exists())
            .filter_map(|e| e.file_name().into_string().ok())
            .collect();
        if sigs.len() > 1 {
            // Stale dirs survive compositor crashes; keep instances whose control socket answers.
            sigs.retain(|s| UnixStream::connect(hypr_dir.join(s).join(".socket.sock")).is_ok());
        }
        // Signatures are `<hash>_<unix time>_<random>`: newest instance wins.
        let started = |s: &String| s.split('_').nth(1).and_then(|t| t.parse::<u64>().ok()).unwrap_or(0);
        sigs.iter().max_by_key(|s| started(s)).map(|s| make(s))
    }

    /// `$XDG_RUNTIME_DIR/hypr/<signature>`.
    pub fn instance_dir(&self) -> PathBuf {
        self.runtime_dir.join("hypr").join(&self.signature)
    }

    /// The event socket (`.socket2.sock`).
    pub fn event_socket(&self) -> PathBuf {
        self.instance_dir().join(".socket2.sock")
    }

    pub fn monitors(&self) -> Result<Vec<Monitor>> {
        parse_monitors(&self.hyprctl(&["-j", "monitors", "all"])?)
    }

    pub fn clients(&self) -> Result<Vec<Client>> {
        parse_clients(&self.hyprctl(&["-j", "clients"])?)
    }

    /// Move the window with app-id `class` to `monitor` (connector name) without following it, and
    /// set or unset fullscreen. Hyprland silently ignores selectors that match no window, so check
    /// [`place_check`] first when the outcome matters.
    pub fn place(&self, class: &str, monitor: &str, fullscreen: bool) -> Result<()> {
        if class.is_empty() || monitor.is_empty() {
            bail!("place: empty window class or monitor");
        }
        self.eval(&place_lua(class, monitor, fullscreen))
    }

    /// Run a Lua chunk with `hyprctl eval`; errors carry Hyprland's message.
    pub fn eval(&self, lua: &str) -> Result<()> {
        let out = self.hyprctl(&["eval", lua])?;
        match out.trim() {
            "ok" => Ok(()),
            other => Err(anyhow!("hyprctl eval: {other}")),
        }
    }

    fn hyprctl(&self, args: &[&str]) -> Result<String> {
        let what = || format!("hyprctl {}", args.first().copied().unwrap_or(""));
        let mut child = Command::new(&self.hyprctl)
            .args(args)
            .env("HYPRLAND_INSTANCE_SIGNATURE", &self.signature)
            .env("XDG_RUNTIME_DIR", &self.runtime_dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn {}", self.hyprctl.display()))?;
        // Drain both pipes concurrently so a large reply can't fill a pipe and stall the child.
        let drain = |pipe: Option<Box<dyn Read + Send>>| {
            std::thread::spawn(move || {
                let mut buf = Vec::new();
                if let Some(mut p) = pipe {
                    let _ = p.read_to_end(&mut buf);
                }
                buf
            })
        };
        let out = drain(child.stdout.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
        let err = drain(child.stderr.take().map(|p| Box::new(p) as Box<dyn Read + Send>));
        let deadline = Instant::now() + HYPRCTL_TIMEOUT;
        let status = loop {
            if let Some(status) = child.try_wait().with_context(what)? {
                break status;
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                bail!("{} timed out after {:?}", what(), HYPRCTL_TIMEOUT);
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        let out = String::from_utf8_lossy(&out.join().unwrap_or_default()).into_owned();
        let err = String::from_utf8_lossy(&err.join().unwrap_or_default()).into_owned();
        if !status.success() {
            let msg = if out.trim().is_empty() { err.trim() } else { out.trim() };
            bail!("{} failed ({status}): {msg}", what());
        }
        Ok(out)
    }
}

/// Events from `.socket2.sock` the watcher reacts to.
#[derive(Clone, Debug, PartialEq, Eq)]
enum HyprEvent {
    /// Outputs changed (added/removed/config reload): refetch the monitor list.
    Monitors,
    /// A window mapped: maybe one with a pending placement.
    WindowOpened { class: String },
    /// (Re)connected to the event socket: state may have changed while disconnected.
    Connected,
}

fn parse_event(line: &[u8]) -> Option<HyprEvent> {
    let line = std::str::from_utf8(line).ok()?.trim_end_matches(['\n', '\r']);
    let (name, data) = line.split_once(">>")?;
    match name {
        "monitoradded" | "monitorremoved" | "monitoraddedv2" | "monitorremovedv2" | "configreloaded" => Some(HyprEvent::Monitors),
        // openwindow>>ADDRESS,WORKSPACE,CLASS,TITLE (the title may contain commas)
        "openwindow" => data.split(',').nth(2).map(|class| HyprEvent::WindowOpened { class: class.to_string() }),
        _ => None,
    }
}

/// What [`MonitorWatcher::latest`] returns; cheap to clone.
#[derive(Clone, Debug, Default)]
pub struct MonitorSnapshot {
    pub monitors: Arc<Vec<Monitor>>,
    /// Bumps whenever `monitors` changes; recompute placements when it does.
    pub generation: u64,
    /// A Hyprland instance was found (false: no placement is possible).
    pub hyprland: bool,
    /// Last hyprctl failure (fetch or placement), cleared by the next successful fetch.
    pub error: Option<Arc<str>>,
}

enum Cmd {
    Place { class: String, monitor: String, fullscreen: bool },
    Forget(String),
    Refresh,
}

struct Desired {
    monitor: String,
    fullscreen: bool,
    /// Address of the window last verified/placed for this desire.
    done: Option<String>,
}

struct Shared {
    snap: Mutex<MonitorSnapshot>,
    stop: AtomicBool,
    /// Clone of the connected event socket, so `Drop` can unblock the reader.
    events: Mutex<Option<UnixStream>>,
}

/// Background monitor tracker + window placer. Owns two threads: one reads Hyprland's event
/// socket (reconnecting), the other refetches `hyprctl monitors` on hotplug events and every
/// [`POLL_INTERVAL`], publishes snapshots (repainting `ctx` on change), and applies placements.
/// Nothing here blocks the caller.
pub struct MonitorWatcher {
    shared: Arc<Shared>,
    cmd: Sender<Cmd>,
}

impl MonitorWatcher {
    pub fn start(repaint: egui::Context) -> MonitorWatcher {
        Self::spawn(repaint, Arc::new(Hypr::detect))
    }

    fn spawn(repaint: egui::Context, detect: Arc<dyn Fn() -> Option<Hypr> + Send + Sync>) -> MonitorWatcher {
        let shared = Arc::new(Shared { snap: Mutex::new(MonitorSnapshot::default()), stop: AtomicBool::new(false), events: Mutex::new(None) });
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (ev_tx, ev_rx) = crossbeam_channel::unbounded();
        {
            let (shared, detect) = (shared.clone(), detect.clone());
            std::thread::Builder::new().name("hypr-events".into()).spawn(move || event_reader(&shared, &ev_tx, &*detect)).expect("spawn hypr-events thread");
        }
        {
            let shared = shared.clone();
            std::thread::Builder::new()
                .name("hypr-monitors".into())
                .spawn(move || Worker { shared, repaint, detect, hypr: None, desired: BTreeMap::new() }.run(cmd_rx, ev_rx))
                .expect("spawn hypr-monitors thread");
        }
        MonitorWatcher { shared, cmd: cmd_tx }
    }

    /// Latest published state (cheap: `Arc` clones).
    pub fn latest(&self) -> MonitorSnapshot {
        self.shared.snap.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// Keep the window with app-id `class` on `monitor` (connector name) with the given fullscreen
    /// state. Applied in the background once the window exists, re-applied after hotplug and when
    /// the window is recreated; repeated identical requests are free. Manual moves by the user are
    /// left alone until the next hotplug.
    pub fn place(&self, class: &str, monitor: &str, fullscreen: bool) {
        let _ = self.cmd.send(Cmd::Place { class: class.to_string(), monitor: monitor.to_string(), fullscreen });
    }

    /// Stop managing `class` (window closed, or no target monitor).
    pub fn forget(&self, class: &str) {
        let _ = self.cmd.send(Cmd::Forget(class.to_string()));
    }

    /// Refetch now (e.g. after the user toggles the TV from the UI).
    pub fn refresh(&self) {
        let _ = self.cmd.send(Cmd::Refresh);
    }
}

impl Drop for MonitorWatcher {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Some(s) = self.shared.events.lock().unwrap_or_else(PoisonError::into_inner).take() {
            let _ = s.shutdown(std::net::Shutdown::Both);
        }
        // Dropping `cmd` disconnects the worker's command channel, which ends its loop.
    }
}

fn sleep_unless_stopped(shared: &Shared, total: Duration) {
    let end = Instant::now() + total;
    while !shared.stop.load(Ordering::SeqCst) {
        let now = Instant::now();
        if now >= end {
            return;
        }
        std::thread::sleep((end - now).min(Duration::from_millis(100)));
    }
}

fn event_reader(shared: &Shared, tx: &Sender<HyprEvent>, detect: &dyn Fn() -> Option<Hypr>) {
    let mut line = Vec::with_capacity(256);
    while !shared.stop.load(Ordering::SeqCst) {
        let Some(stream) = detect().and_then(|h| UnixStream::connect(h.event_socket()).ok()) else {
            sleep_unless_stopped(shared, RECONNECT_DELAY);
            continue;
        };
        {
            let mut slot = shared.events.lock().unwrap_or_else(PoisonError::into_inner);
            if shared.stop.load(Ordering::SeqCst) {
                return;
            }
            *slot = stream.try_clone().ok();
        }
        if tx.send(HyprEvent::Connected).is_err() {
            return;
        }
        let mut reader = BufReader::new(stream);
        loop {
            line.clear();
            match reader.read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if let Some(ev) = parse_event(&line)
                        && tx.send(ev).is_err()
                    {
                        return;
                    }
                }
            }
        }
        shared.events.lock().unwrap_or_else(PoisonError::into_inner).take();
        sleep_unless_stopped(shared, RECONNECT_DELAY);
    }
}

struct Worker {
    shared: Arc<Shared>,
    repaint: egui::Context,
    detect: Arc<dyn Fn() -> Option<Hypr> + Send + Sync>,
    hypr: Option<Hypr>,
    desired: BTreeMap<String, Desired>,
}

impl Worker {
    fn run(mut self, cmds: Receiver<Cmd>, mut events: Receiver<HyprEvent>) {
        let mut next_poll = Instant::now();
        let mut refresh_at: Option<Instant> = None;
        loop {
            let wake = refresh_at.map_or(next_poll, |r| r.min(next_poll));
            let mut reconcile = false;
            crossbeam_channel::select! {
                recv(cmds) -> cmd => match cmd {
                    Err(_) => return,
                    Ok(Cmd::Refresh) => refresh_at = Some(Instant::now()),
                    Ok(Cmd::Forget(class)) => { self.desired.remove(&class); }
                    Ok(Cmd::Place { class, monitor, fullscreen }) => {
                        let same = self.desired.get(&class).is_some_and(|d| d.monitor == monitor && d.fullscreen == fullscreen);
                        if !same {
                            self.desired.insert(class, Desired { monitor, fullscreen, done: None });
                            reconcile = true;
                        }
                    }
                },
                recv(events) -> ev => match ev {
                    Err(_) => events = crossbeam_channel::never(),
                    Ok(HyprEvent::Connected) => refresh_at = Some(Instant::now()),
                    Ok(HyprEvent::Monitors) => { refresh_at.get_or_insert(Instant::now() + EVENT_DEBOUNCE); }
                    Ok(HyprEvent::WindowOpened { class }) => reconcile |= self.desired.get(&class).is_some_and(|d| d.done.is_none()),
                },
                default(wake.saturating_duration_since(Instant::now())) => {}
            }
            if self.shared.stop.load(Ordering::SeqCst) {
                return;
            }
            let now = Instant::now();
            let poll_due = now >= next_poll;
            if poll_due || refresh_at.is_some_and(|r| now >= r) {
                refresh_at = None;
                next_poll = now + POLL_INTERVAL;
                let changed = self.refresh();
                if changed {
                    // Hotplug may have displaced managed windows: re-verify all of them.
                    self.desired.values_mut().for_each(|d| d.done = None);
                }
                reconcile |= changed || (poll_due && self.desired.values().any(|d| d.done.is_none()));
            }
            if reconcile {
                self.reconcile();
            }
        }
    }

    fn set_error(&self, e: Option<String>) {
        let mut snap = self.shared.snap.lock().unwrap_or_else(PoisonError::into_inner);
        let e: Option<Arc<str>> = e.map(Into::into);
        if snap.error != e {
            snap.error = e;
            drop(snap);
            self.repaint.request_repaint();
        }
    }

    /// Refetch monitors; true when the published list changed.
    fn refresh(&mut self) -> bool {
        if self.hypr.is_none() {
            self.hypr = (self.detect)();
        }
        let result = match &self.hypr {
            None => Err(anyhow!("no Hyprland instance")),
            Some(h) => h.monitors(),
        };
        let mut snap = self.shared.snap.lock().unwrap_or_else(PoisonError::into_inner);
        let before = (snap.generation, snap.hyprland, snap.error.clone());
        snap.hyprland = self.hypr.is_some();
        let changed = match result {
            Ok(list) => {
                snap.error = None;
                if *snap.monitors != list {
                    snap.monitors = Arc::new(list);
                    snap.generation += 1;
                    true
                } else {
                    false
                }
            }
            Err(e) => {
                if self.hypr.is_some() {
                    tracing::warn!("monitors: {e:#}");
                }
                snap.error = self.hypr.is_some().then(|| format!("{e:#}").into());
                // Instance gone (compositor restart)? Detect again next time.
                self.hypr = None;
                false
            }
        };
        let publish = (snap.generation, snap.hyprland, snap.error.clone()) != before;
        drop(snap);
        if publish {
            self.repaint.request_repaint();
        }
        changed
    }

    fn reconcile(&mut self) {
        if self.desired.values().all(|d| d.done.is_some()) {
            return;
        }
        let Some(hypr) = self.hypr.clone() else { return };
        let clients = match hypr.clients() {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("clients: {e:#}");
                return self.set_error(Some(format!("{e:#}")));
            }
        };
        let monitors = self.shared.snap.lock().unwrap_or_else(PoisonError::into_inner).monitors.clone();
        let mut error = None;
        // Verified/placed desires are skipped; hotplug resets them all (see `run`).
        for (class, d) in self.desired.iter_mut().filter(|(_, d)| d.done.is_none()) {
            let Some(target) = monitors.iter().find(|m| !m.disabled && m.name == d.monitor) else {
                d.done = None;
                continue;
            };
            d.done = match place_check(&clients, class, target, d.fullscreen) {
                PlaceCheck::Missing => None,
                PlaceCheck::Placed(addr) => Some(addr),
                PlaceCheck::Misplaced(addr) => match hypr.place(class, &target.name, d.fullscreen) {
                    Ok(()) => {
                        tracing::info!("placed {class} on {} (fullscreen: {})", target.name, d.fullscreen);
                        Some(addr)
                    }
                    Err(e) => {
                        tracing::warn!("place {class}: {e:#}");
                        error = Some(format!("place {class}: {e:#}"));
                        None
                    }
                },
            };
        }
        if error.is_some() {
            self.set_error(error);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{ConfidenceCfg, Layout};

    fn fixture(name: &str) -> Vec<Monitor> {
        let path = format!("{}/tests/fixtures/hyprctl/{name}.json", env!("CARGO_MANIFEST_DIR"));
        parse_monitors(&std::fs::read_to_string(&path).unwrap()).unwrap()
    }

    fn cfg() -> ConfidenceCfg {
        ConfidenceCfg::default()
    }

    fn main_dp1() -> MonitorMatch {
        MonitorMatch::parse("DP-1")
    }

    #[test]
    fn parses_recorded_hyprctl_output() {
        let m = fixture("two-monitors-tv-off");
        assert_eq!(m.len(), 2);
        let dp1 = &m[0];
        assert_eq!((dp1.id, dp1.name.as_str(), dp1.make.as_str(), dp1.model.as_str()), (0, "DP-1", "Microstep", "MSI MAG341CQ"));
        assert_eq!((dp1.width, dp1.height, dp1.x, dp1.y), (3440, 1440, 0, 0));
        assert!((dp1.refresh - 99.998).abs() < 0.01 && dp1.scale == 1.25 && dp1.focused && !dp1.disabled);
        assert_eq!(dp1.logical_size(), [2752.0, 1152.0]);
        assert_eq!((m[1].name.as_str(), m[1].x), ("DP-2", 2752));
        let tv = fixture("tv-disabled").into_iter().find(|m| m.name == "HDMI-A-1").unwrap();
        assert!(tv.disabled);
        assert_eq!(tv.logical_size(), [1920.0, 1080.0]);
    }

    #[test]
    fn rotated_monitor_logical_size_swaps() {
        let m = Monitor { width: 3840, height: 2160, scale: 2.0, transform: 1, ..Default::default() };
        assert_eq!(m.logical_size(), [1080.0, 1920.0]);
    }

    #[test]
    fn monitor_match_forms() {
        let tv = fixture("three-monitors").into_iter().find(|m| m.name == "HDMI-A-1").unwrap();
        assert_eq!(MonitorMatch::parse("desc:Philips FTV"), MonitorMatch::Desc("Philips FTV".into()));
        assert!(MonitorMatch::parse("desc:philips ftv").matches(&tv));
        assert!(MonitorMatch::parse("DESC: Philips").matches(&tv));
        assert!(MonitorMatch::parse("make:philips consumer").matches(&tv));
        assert!(MonitorMatch::parse("model:FTV").matches(&tv));
        assert!(!MonitorMatch::parse("model:Consumer").matches(&tv));
        assert!(MonitorMatch::parse("HDMI-A-1").matches(&tv));
        assert!(MonitorMatch::parse("hdmi-a-1").matches(&tv));
        // A connector name is exact, not a substring.
        assert!(!MonitorMatch::parse("HDMI-A").matches(&tv));
        // Empty patterns never match.
        for s in ["", "desc:", "make:  ", "model:"] {
            assert!(!MonitorMatch::parse(s).matches(&tv), "{s:?}");
        }
        for s in ["desc:Philips FTV", "make:WEH", "model:MSI MAG341CQ", "DP-2"] {
            assert_eq!(MonitorMatch::parse(s).to_string(), s);
        }
    }

    #[test]
    fn pick_prefers_list_order_skips_disabled_and_avoids_main_as_last_resort() {
        let three = fixture("three-monitors");
        let tv = [MonitorMatch::parse("desc:Philips FTV"), MonitorMatch::parse("HDMI-A-1")];
        let dp2 = [MonitorMatch::parse("DP-2")];
        let main = main_dp1();
        let p = pick(&three, &tv, &dp2, Some(&main)).unwrap();
        assert_eq!((p.monitor.name.as_str(), p.tier), ("HDMI-A-1", Tier::Preferred));
        // Earlier matchers win over later ones even when a later one matches an earlier monitor.
        let p = pick(&three, &[MonitorMatch::parse("DP-2"), MonitorMatch::parse("DP-1")], &[], None).unwrap();
        assert_eq!(p.monitor.name, "DP-2");

        let disabled = fixture("tv-disabled");
        let p = pick(&disabled, &tv, &dp2, Some(&main)).unwrap();
        assert_eq!((p.monitor.name.as_str(), p.tier), ("DP-2", Tier::Fallback));
        assert!(p.is_fallback());

        // No preferred, no fallback: any enabled non-main monitor.
        let p = pick(&disabled, &tv, &[MonitorMatch::parse("DP-9")], Some(&main)).unwrap();
        assert_eq!((p.monitor.name.as_str(), p.tier), ("DP-2", Tier::LastResort));
        // Only the main monitor left: nothing.
        assert_eq!(pick(&fixture("tv-only-dp1"), &tv, &dp2, Some(&main)), None);
        // Without a main monitor the last resort may be any enabled one.
        assert_eq!(pick(&fixture("tv-only-dp1"), &tv, &dp2, None).unwrap().monitor.name, "DP-1");
        // An explicit match may still name the main monitor.
        assert_eq!(pick(&fixture("tv-only-dp1"), std::slice::from_ref(&main), &[], Some(&main)).unwrap().tier, Tier::Preferred);
        assert_eq!(pick(&[], &tv, &dp2, None), None);
    }

    #[test]
    fn confidence_target_per_fixture() {
        let main = main_dp1();
        let target = |f: &str| confidence_target(&fixture(f), &cfg(), Some(&main)).map(|p| (p.monitor.name.clone(), p.tier));
        assert_eq!(target("three-monitors"), Some(("HDMI-A-1".into(), Tier::Preferred)));
        assert_eq!(target("two-monitors-tv-off"), Some(("DP-2".into(), Tier::Fallback)));
        assert_eq!(target("tv-disabled"), Some(("DP-2".into(), Tier::Fallback)));
        assert_eq!(target("tv-only-dp1"), None);
        let off = ConfidenceCfg { enabled: false, ..cfg() };
        assert_eq!(confidence_target(&fixture("three-monitors"), &off, Some(&main)), None);
    }

    #[test]
    fn follow_hotplug_sequence() {
        let c = cfg();
        let main = main_dp1();
        let mut at: Option<String> = None;
        let mut log = Vec::new();
        for f in ["two-monitors-tv-off", "three-monitors", "three-monitors", "tv-disabled", "three-monitors", "tv-only-dp1", "two-monitors-tv-off"] {
            let p = follow(at.as_deref(), &fixture(f), &c, Some(&main));
            log.push((f, p.monitor.clone(), p.changed, p.reason));
            at = p.monitor;
        }
        let expect = [
            ("two-monitors-tv-off", Some("DP-2"), true, Reason::Fallback),
            ("three-monitors", Some("HDMI-A-1"), true, Reason::Preferred),
            ("three-monitors", Some("HDMI-A-1"), false, Reason::Preferred),
            ("tv-disabled", Some("DP-2"), true, Reason::Fallback),
            ("three-monitors", Some("HDMI-A-1"), true, Reason::Preferred),
            ("tv-only-dp1", None, true, Reason::NoMonitor),
            ("two-monitors-tv-off", Some("DP-2"), true, Reason::Fallback),
        ];
        let expect: Vec<_> = expect.iter().map(|(f, m, ch, r)| (*f, m.map(String::from), *ch, *r)).collect();
        assert_eq!(log, expect);
    }

    #[test]
    fn follow_keeps_prev_while_monitor_list_unknown_and_reports_disabled() {
        let main = main_dp1();
        let p = follow(Some("HDMI-A-1"), &[], &cfg(), Some(&main));
        assert_eq!(p, Placement { monitor: Some("HDMI-A-1".into()), changed: false, reason: Reason::Unknown });
        let off = ConfidenceCfg { enabled: false, ..cfg() };
        let p = follow(Some("HDMI-A-1"), &fixture("three-monitors"), &off, Some(&main));
        assert_eq!(p, Placement { monitor: None, changed: true, reason: Reason::Disabled });
    }

    #[test]
    fn show_2disp_confidence_stays_on_dp2() {
        let l = Layout::builtin("show-2disp").unwrap();
        let main = l.main.monitor.clone();
        for f in ["three-monitors", "two-monitors-tv-off", "tv-disabled"] {
            let p = follow(None, &fixture(f), &l.confidence, main.as_ref());
            assert_eq!(p.monitor.as_deref(), Some("DP-2"), "{f}");
        }
    }

    #[test]
    fn place_lua_is_exact_and_escaped() {
        assert_eq!(
            place_lua("stream-engine.program", "DP-2", true),
            r#"hl.dispatch(hl.dsp.window.move({ monitor = "DP-2", window = "class:^stream-engine\\.program$", follow = false }))
hl.dispatch(hl.dsp.window.fullscreen({ mode = "fullscreen", action = "set", window = "class:^stream-engine\\.program$" }))"#
        );
        assert!(place_lua("stream-engine.multiview", "HDMI-A-1", false).contains(r#"action = "unset""#));
        // Hostile input can't break out of the string literal or the regex.
        assert_eq!(class_selector("a.b+c(d)|e[f]{g}^h$i*j?k\\l"), r"class:^a\.b\+c\(d\)\|e\[f\]\{g\}\^h\$i\*j\?k\\l$");
        assert_eq!(lua_string("x\" ) os.exit() --\nline\u{1}"), r#""x\" ) os.exit() --\nline\001""#);
    }

    #[test]
    fn clients_fixture_and_place_check() {
        let path = format!("{}/tests/fixtures/hyprctl/clients.json", env!("CARGO_MANIFEST_DIR"));
        let clients = parse_clients(&std::fs::read_to_string(path).unwrap()).unwrap();
        let program = clients.iter().find(|c| c.class == "stream-engine.program").unwrap();
        assert_eq!((program.monitor, program.is_fullscreen(), program.pid, program.address.as_str()), (1, true, 41210, "0x5a1b2c3d4f10"));
        let mons = fixture("three-monitors");
        let (dp2, tv) = (&mons[1], &mons[2]);
        assert_eq!(place_check(&clients, "stream-engine.program", dp2, true), PlaceCheck::Placed("0x5a1b2c3d4f10".into()));
        assert_eq!(place_check(&clients, "stream-engine.program", tv, true), PlaceCheck::Misplaced("0x5a1b2c3d4f10".into()));
        assert_eq!(place_check(&clients, "stream-engine.program", dp2, false), PlaceCheck::Misplaced("0x5a1b2c3d4f10".into()));
        assert_eq!(place_check(&clients, "stream-engine.multiview", dp2, false), PlaceCheck::Placed("0x5a1b2c3d5020".into()));
        // Class match is exact: the main window is not a prefix match for pop-outs.
        assert_eq!(place_check(&clients, "stream-engine.scopes", dp2, false), PlaceCheck::Missing);
        let unmapped: Vec<Client> = clients.iter().cloned().map(|c| Client { mapped: false, ..c }).collect();
        assert_eq!(place_check(&unmapped, "stream-engine.program", dp2, true), PlaceCheck::Missing);
    }

    #[test]
    fn event_lines() {
        assert_eq!(parse_event(b"monitoradded>>HDMI-A-1\n"), Some(HyprEvent::Monitors));
        assert_eq!(parse_event(b"monitorremovedv2>>2,HDMI-A-1,Philips Consumer Electronics Company Philips FTV\n"), Some(HyprEvent::Monitors));
        assert_eq!(parse_event(b"configreloaded>>\n"), Some(HyprEvent::Monitors));
        assert_eq!(
            parse_event(b"openwindow>>5a1b2c3d4f10,2,stream-engine.program,program, wide\n"),
            Some(HyprEvent::WindowOpened { class: "stream-engine.program".into() })
        );
        assert_eq!(parse_event(b"activewindow>>foot,~\n"), None);
        assert_eq!(parse_event(b"focusedmon>>DP-2,2\n"), None);
        assert_eq!(parse_event(b"garbage\n"), None);
        assert_eq!(parse_event(b"\xff\xfe>>x\n"), None);
    }

    #[test]
    fn detect_prefers_env_then_newest_live_instance() {
        let rt = tempfile::tempdir().unwrap();
        let hypr = rt.path().join("hypr");
        assert_eq!(Hypr::detect_in(rt.path(), None, "hyprctl".into()), None);
        let old = "aaaa_1700000000_1";
        let new = "bbbb_1790367909_2";
        let _live: Vec<_> = [old, new]
            .iter()
            .map(|s| {
                std::fs::create_dir_all(hypr.join(s)).unwrap();
                std::os::unix::net::UnixListener::bind(hypr.join(s).join(".socket.sock")).unwrap()
            })
            .collect();
        // A dir without a control socket is not an instance.
        std::fs::create_dir_all(hypr.join("cccc_1999999999_3")).unwrap();
        let h = Hypr::detect_in(rt.path(), None, "hyprctl".into()).unwrap();
        assert_eq!(h.signature, new);
        assert_eq!(h.event_socket(), hypr.join(new).join(".socket2.sock"));
        assert_eq!(Hypr::detect_in(rt.path(), Some(old), "hyprctl".into()).unwrap().signature, old);
        // A stale env signature (dir gone) falls back to discovery.
        assert_eq!(Hypr::detect_in(rt.path(), Some("gone_1_1"), "hyprctl".into()).unwrap().signature, new);
    }

    /// Fake Hyprland: a runtime dir with an event socket we write to, and a `hyprctl` script
    /// that serves `monitors.json`/`clients.json` from the instance dir and logs `eval` calls.
    struct FakeHypr {
        _rt: tempfile::TempDir,
        dir: PathBuf,
        hypr: Hypr,
        events: std::os::unix::net::UnixListener,
    }

    impl FakeHypr {
        fn new() -> FakeHypr {
            use std::os::unix::fs::PermissionsExt;
            let rt = tempfile::tempdir().unwrap();
            let sig = "fake_1790000000_1";
            let dir = rt.path().join("hypr").join(sig);
            std::fs::create_dir_all(&dir).unwrap();
            let _ctl = std::os::unix::net::UnixListener::bind(dir.join(".socket.sock")).unwrap();
            let events = std::os::unix::net::UnixListener::bind(dir.join(".socket2.sock")).unwrap();
            let script = dir.join("hyprctl");
            std::fs::write(
                &script,
                "#!/bin/sh\nd=\"$XDG_RUNTIME_DIR/hypr/$HYPRLAND_INSTANCE_SIGNATURE\"\n\
                 case \"$1 $2\" in\n\
                 \"-j monitors\") cat \"$d/monitors.json\" ;;\n\
                 \"-j clients\") cat \"$d/clients.json\" ;;\n\
                 \"eval \"*) printf '%s\\n' \"$2\" >> \"$d/eval.log\"; echo ok ;;\n\
                 *) echo \"unknown request\"; exit 3 ;;\n\
                 esac\n",
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let hypr = Hypr::detect_in(rt.path(), Some(sig), script).unwrap();
            FakeHypr { _rt: rt, dir, hypr, events }
        }

        fn serve(&self, monitors_fixture: &str) {
            let src = format!("{}/tests/fixtures/hyprctl/{monitors_fixture}.json", env!("CARGO_MANIFEST_DIR"));
            std::fs::copy(src, self.dir.join("monitors.json.tmp")).unwrap();
            std::fs::rename(self.dir.join("monitors.json.tmp"), self.dir.join("monitors.json")).unwrap();
        }

        fn evals(&self) -> String {
            std::fs::read_to_string(self.dir.join("eval.log")).unwrap_or_default()
        }
    }

    fn wait_for(what: &str, mut f: impl FnMut() -> bool) {
        let end = Instant::now() + Duration::from_secs(5);
        while !f() {
            assert!(Instant::now() < end, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn hyprctl_calls_through_fake_binary() {
        let fake = FakeHypr::new();
        fake.serve("three-monitors");
        assert_eq!(fake.hypr.monitors().unwrap().len(), 3);
        fake.hypr.place("stream-engine.program", "HDMI-A-1", true).unwrap();
        assert_eq!(fake.evals().trim_end(), place_lua("stream-engine.program", "HDMI-A-1", true));
        assert!(fake.hypr.place("", "DP-2", true).is_err());
        let err = fake.hypr.hyprctl(&["bogus"]).unwrap_err().to_string();
        assert!(err.contains("unknown request"), "{err}");
    }

    #[test]
    fn hyprctl_timeout_kills_hung_child() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("hyprctl");
        std::fs::write(&script, "#!/bin/sh\nexec sleep 30\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let h = Hypr { signature: "x".into(), runtime_dir: dir.path().into(), hyprctl: script };
        let t = Instant::now();
        let err = h.monitors().unwrap_err().to_string();
        assert!(err.contains("timed out"), "{err}");
        assert!(t.elapsed() < HYPRCTL_TIMEOUT + Duration::from_secs(1));
    }

    #[test]
    fn watcher_follows_hotplug_events_and_places_windows() {
        use std::io::Write;
        let fake = FakeHypr::new();
        fake.serve("two-monitors-tv-off");
        std::fs::copy(format!("{}/tests/fixtures/hyprctl/clients.json", env!("CARGO_MANIFEST_DIR")), fake.dir.join("clients.json")).unwrap();
        let hypr = fake.hypr.clone();
        let w = MonitorWatcher::spawn(egui::Context::default(), Arc::new(move || Some(hypr.clone())));
        let (mut sock, _) = fake.events.accept().unwrap();
        wait_for("first snapshot", || w.latest().generation == 1);
        let s = w.latest();
        assert!(s.hyprland && s.error.is_none());
        assert_eq!(s.monitors.iter().map(|m| m.name.as_str()).collect::<Vec<_>>(), ["DP-1", "DP-2"]);

        // TV plugged in: the event (not the 5 s poll) triggers the refetch.
        fake.serve("three-monitors");
        sock.write_all(b"activewindow>>foot,~\nmonitoradded>>HDMI-A-1\nmonitoraddedv2>>2,HDMI-A-1,Philips\n").unwrap();
        wait_for("TV snapshot", || w.latest().generation == 2);
        let snap = w.latest();
        assert_eq!(follow(Some("DP-2"), &snap.monitors, &cfg(), Some(&main_dp1())).monitor.as_deref(), Some("HDMI-A-1"));

        // Placement: program is on DP-2 in clients.json, so moving it to the TV dispatches once.
        w.place("stream-engine.program", "HDMI-A-1", true);
        wait_for("eval", || !fake.evals().is_empty());
        // Already where we want it: no dispatch.
        w.place("stream-engine.multiview", "DP-2", false);
        w.place("stream-engine.program", "HDMI-A-1", true);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(fake.evals().trim_end(), place_lua("stream-engine.program", "HDMI-A-1", true));

        // TV parked: listed as disabled.
        fake.serve("tv-disabled");
        sock.write_all(b"monitorremoved>>HDMI-A-1\n").unwrap();
        wait_for("disabled snapshot", || w.latest().generation == 3);
        assert!(w.latest().monitors.iter().any(|m| m.name == "HDMI-A-1" && m.disabled));

        // Event socket dropped (compositor restart): reader reconnects and refetches.
        drop(sock);
        fake.serve("three-monitors");
        let (_sock2, _) = fake.events.accept().unwrap();
        wait_for("refetch after reconnect", || w.latest().generation == 4);
        drop(w);
    }
}
