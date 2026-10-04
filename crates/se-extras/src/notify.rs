//! Desktop notifications (§16.3): background problems reach the desktop as freedesktop
//! notifications (`notify-send`; the Omarchy shell is the daemon) while no Stream Engine window
//! has focus. Only problems, never routine events:
//!
//! * a feed stopped reaching OBS while it matters (streaming, recording, or a show mode),
//! * an expected device was unplugged (`devices.<id>.present` went true → false),
//! * Twitch can't renew its sign-in (`twitch.auth[.bot].status` expired/error, or about to run out),
//! * the project disk is almost full (under 10 GB, the preflight `disk` failure level),
//! * any `health.<check>` that fails for [`HEALTH_HOLD`] (see below),
//! * whatever another subsystem asks for with `notify.send {key, title, body, urgency?, open?}`
//!   (recordings disk, failed backups).
//!
//! Focus: the UI reports `ui.focus {focused, pid}` when its focus changes and when it
//! (re)connects. A UI whose process is gone, or no UI at all, counts as not focused. `ui.focused`
//! mirrors the result.
//!
//! Rules: a problem must last its hold time (feed 5 s, device 3 s, health 10 s, Twitch 60 s)
//! before it counts; each occurrence notifies at most once, and one that became due while a
//! window had focus counts as seen. The same problem isn't repeated within [`REPEAT_AFTER`], and
//! notifications are at least [`MIN_GAP`] apart: problems that become due in between are
//! combined into one notification. Clicking a notification opens (or focuses) the UI on the page
//! that fixes the problem. Each notification is also the event `notify.sent {keys, title, body}`.
//!
//! Health failures: a `health.<check>` that turns `fail` after it was seen passing (or warning),
//! or that is failing while the show matters (streaming or a show mode), notifies critically as
//! "Stream Engine: <check> failed" with the check's detail, **even while a window has focus**.
//! `obs` counts only while the show matters (it fails whenever OBS is closed). A failure that a
//! specific notification above already reports (Twitch sign-in, an unplugged device, a stale OBS
//! feed, the disk/backup requests) is merged into it instead of notified twice. When a notified
//! (or merged) failure passes again, a low-urgency "… recovered" notification follows (subject to
//! focus like other notes). A failure that comes back after it recovered notifies again; one that
//! never recovered is not repeated within [`REPEAT_AFTER`]. Clicking either opens `ui.open
//! {view: "health"}`.

use se_hub::{EngineCtx, Snapshot};
use se_proto::{Event, Meta, Op, Origin, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const TARGET: &str = "notify";

/// Minimum time between two notifications.
pub const MIN_GAP: Duration = Duration::from_secs(30);
/// The same problem (by key) is not notified again within this time.
pub const REPEAT_AFTER: Duration = Duration::from_secs(30 * 60);
/// A `notify.send` request that couldn't be shown yet is dropped after this long.
pub const REQUEST_TTL: Duration = Duration::from_secs(10 * 60);
/// Free space under this on the project disk is a problem (preflight `disk` = fail).
pub const DISK_LOW_GB: f64 = 10.0;
/// A failing health check must stay failing this long before it notifies.
pub const HEALTH_HOLD: Duration = Duration::from_secs(10);
/// A specific notification sent this long before a health failure began still covers it.
const COVER_SLACK: Duration = Duration::from_secs(60);
/// `ui.open` target of health notifications.
pub const HEALTH_VIEW: &str = "health";

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Low,
    Normal,
    Critical,
}

impl Urgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Urgency::Low => "low",
            Urgency::Normal => "normal",
            Urgency::Critical => "critical",
        }
    }
}

/// What makes a problem a failing `health.<check>`.
#[derive(Clone, Debug, PartialEq)]
pub struct HealthFail {
    /// Plain-language check name (`Twitch`, `Public queue page`, …) for the recovery note.
    pub name: String,
    /// Key prefixes of specific problems/requests that already report this failure.
    pub covered_by: &'static [&'static str],
}

/// Something wrong right now, in plain words.
#[derive(Clone, Debug, PartialEq)]
pub struct Problem {
    /// Identity for de-duplication (`feed.wide`, `device.kit_cam`, `twitch.auth`, `disk`, …).
    pub key: String,
    pub title: String,
    pub body: String,
    pub urgency: Urgency,
    /// How long it must last before it's worth a notification.
    pub hold: Duration,
    /// UI panel that fixes it (`ui.open {view}`), e.g. `devices`, `twitch`, `program`.
    pub open: Option<String>,
    /// Set for a failing health check: notified even while a window has focus, and its
    /// recovery is notified too.
    pub health: Option<HealthFail>,
}

/// One notification to show.
#[derive(Clone, Debug, PartialEq)]
pub struct Note {
    pub keys: Vec<String>,
    pub title: String,
    pub body: String,
    pub urgency: Urgency,
    pub open: Option<String>,
}

struct Occurrence {
    since: Instant,
    /// Notified, or seen in the UI, or suppressed as a repeat.
    handled: bool,
    problem: Problem,
}

/// De-duplication and rate limiting (pure; the caller supplies the clock).
#[derive(Default)]
pub struct Notifier {
    open: BTreeMap<String, Occurrence>,
    requests: Vec<(Instant, Problem)>,
    /// "… recovered" notes waiting to be shown.
    recoveries: Vec<(Instant, Problem)>,
    /// Health failures the operator was told about (sent or merged) and that haven't passed
    /// since: key → check name.
    awaiting: BTreeMap<String, String>,
    last_key: HashMap<String, Instant>,
    last_sent: Option<Instant>,
}

impl Notifier {
    /// A one-shot request (`notify.send`): shown under the same rules as a problem that just
    /// became due.
    pub fn request(&mut self, now: Instant, p: Problem) {
        self.requests.retain(|(_, r)| r.key != p.key);
        self.requests.push((now, p));
    }

    /// `key` (a health problem key, `health.<check>`) passes now with `detail`: if the operator
    /// was told it failed, queue a "recovered" note.
    pub fn recovered(&mut self, now: Instant, key: &str, detail: &str) {
        let Some(name) = self.awaiting.remove(key) else { return };
        let rkey = format!("{key}.recovered");
        self.recoveries.retain(|(_, r)| r.key != rkey);
        self.recoveries.push((
            now,
            Problem {
                key: rkey,
                title: format!("Stream Engine: {name} recovered"),
                body: detail.to_string(),
                urgency: Urgency::Low,
                hold: Duration::ZERO,
                open: Some(HEALTH_VIEW.into()),
                health: None,
            },
        ));
    }

    /// A specific problem or request already tells the operator about this health failure.
    fn covered(&self, key: &str, h: &HealthFail, since: Instant) -> bool {
        let hit = |k: &str| h.covered_by.iter().any(|c| k.starts_with(c));
        self.open.keys().any(|k| k != key && hit(k))
            || self.requests.iter().any(|(_, r)| hit(&r.key))
            || self.last_key.iter().any(|(k, t)| hit(k) && *t + COVER_SLACK >= since)
    }

    /// Feed the problems present now; returns the notification to show, if any.
    pub fn update(&mut self, now: Instant, current: Vec<Problem>, focused: bool) -> Option<Note> {
        let keys: BTreeSet<String> = current.iter().map(|p| p.key.clone()).collect();
        self.open.retain(|k, _| keys.contains(k));
        for p in current {
            match self.open.get_mut(&p.key) {
                Some(o) => o.problem = p,
                None => {
                    self.open.insert(p.key.clone(), Occurrence { since: now, handled: false, problem: p });
                }
            }
        }
        self.requests.retain(|(at, _)| now.duration_since(*at) < REQUEST_TTL);
        // a recovery note for a check that is failing again is stale
        self.recoveries.retain(|(at, r)| now.duration_since(*at) < REQUEST_TTL && !r.key.strip_suffix(".recovered").is_some_and(|k| keys.contains(k)));

        let due: Vec<String> = self.open.iter().filter(|(_, o)| !o.handled && now.duration_since(o.since) >= o.problem.hold).map(|(k, _)| k.clone()).collect();
        let recent = |last: &HashMap<String, Instant>, k: &str| last.get(k).is_some_and(|t| now.duration_since(*t) < REPEAT_AFTER);
        let mut ready: Vec<Problem> = Vec::new();
        for k in due {
            let o = &self.open[&k];
            let health = o.problem.health.clone();
            if let Some(h) = &health
                && self.covered(&k, h, o.since)
            {
                self.awaiting.insert(k.clone(), h.name.clone());
                self.open.get_mut(&k).expect("due key is open").handled = true;
                continue;
            }
            // a health failure that recovered since it was last notified is news again
            let repeat = recent(&self.last_key, &k) && (health.is_none() || self.awaiting.contains_key(&k));
            let o = self.open.get_mut(&k).expect("due key is open");
            if (focused && health.is_none()) || repeat {
                // a window is in front: the UI shows it (health failures notify regardless)
                o.handled = true;
            } else {
                ready.push(o.problem.clone());
            }
        }
        if focused {
            self.requests.clear();
            self.recoveries.clear();
        } else {
            let last_key = &self.last_key;
            self.requests.retain(|(_, r)| !recent(last_key, &r.key));
            for (_, r) in self.requests.iter().chain(&self.recoveries) {
                if !ready.iter().any(|p| p.key == r.key) {
                    ready.push(r.clone());
                }
            }
        }
        if ready.is_empty() || self.last_sent.is_some_and(|t| now.duration_since(t) < MIN_GAP) {
            return None;
        }
        for p in &ready {
            if let Some(o) = self.open.get_mut(&p.key) {
                o.handled = true;
            }
            if let Some(h) = &p.health {
                self.awaiting.insert(p.key.clone(), h.name.clone());
            }
            self.last_key.insert(p.key.clone(), now);
        }
        self.requests.clear();
        self.recoveries.clear();
        self.last_sent = Some(now);
        Some(combine(ready))
    }
}

fn combine(mut ready: Vec<Problem>) -> Note {
    ready.sort_by(|a, b| b.urgency.cmp(&a.urgency).then_with(|| a.key.cmp(&b.key)));
    let urgency = ready[0].urgency;
    let keys = ready.iter().map(|p| p.key.clone()).collect();
    if ready.len() == 1 {
        let p = ready.remove(0);
        return Note { keys, title: p.title, body: p.body, urgency, open: p.open };
    }
    let mut body: Vec<String> = ready.iter().map(|p| format!("• {}", p.title)).collect();
    if urgency == Urgency::Low {
        body.push("No action needed.".into());
        return Note { keys, title: format!("{} problems cleared up", ready.len()), body: body.join("\n"), urgency, open: ready[0].open.clone() };
    }
    body.push("Open Stream Engine to see what to do.".into());
    Note { keys, title: format!("{} things need your attention", ready.len()), body: body.join("\n"), urgency, open: ready[0].open.clone() }
}

// ---- what counts as a problem -------------------------------------------------------------

/// Modes in which the feed reaching OBS matters even without a running output.
fn show_mode(mode: &str) -> bool {
    matches!(mode, "preshow" | "live" | "brb" | "ad_break" | "outro")
}

/// Streaming, or a show mode: what goes out matters right now.
fn show_matters(s: &Snapshot) -> bool {
    s.bool("obs.stream.active") || show_mode(s.str("show.mode").unwrap_or("offline"))
}

/// Health checks that fail whenever their app is simply closed: a problem only while the show
/// matters.
const ON_AIR_ONLY: &[&str] = &["obs"];

/// Plain-language name of a health check, and the specific problems (key prefixes) that already
/// notify its failures.
fn check_info(check: &str) -> (&'static str, &'static [&'static str]) {
    match check.split('.').next().unwrap_or(check) {
        "obs" => ("OBS", &["feed."]),
        "twitch" => ("Twitch", &["twitch.auth"]),
        "devices" => ("Devices", &["device."]),
        "sources" => ("Cameras & media", &[]),
        "youtube" => ("Song requests (YouTube)", &[]),
        "player" => ("Song player", &[]),
        "relay" => ("Tips & queue relay", &[]),
        "queue_page" => ("Public queue page", &[]),
        "tiktok" => ("TikTok", &[]),
        "audio" => ("Sound", &[]),
        "mixer" => ("Mixing desk", &[]),
        "dmx" | "lights" => ("Lights", &[]),
        "deck" | "midi" | "input" | "voice" => ("Buttons & pedals", &[]),
        "render" | "gpu" => ("Video", &[]),
        "patches" | "cef" => ("Web sources", &[]),
        "tts" => ("Read-out voice", &[]),
        "timecode" => ("Timelines", &[]),
        "clips" | "recording" => ("Recordings", &[]),
        "recordings" => ("Recordings", &["disk"]),
        "backup" => ("Backups", &["backup"]),
        "versions" => ("Project history", &[]),
        "alerts" => ("Alerts", &[]),
        "bot" => ("Chat bot", &["twitch.auth.bot"]),
        _ => ("", &[]),
    }
}

/// Turns engine state into problems; remembers device presence to spot unplugging and which
/// health checks have been seen working.
#[derive(Default)]
pub struct Watch {
    present: HashMap<String, bool>,
    unplugged: BTreeSet<String>,
    /// Health checks seen `pass`/`warn` since the engine started.
    worked: HashSet<String>,
    /// `(health.<check>, detail)` of the checks passing at the last [`Watch::problems`].
    passing: Vec<(String, String)>,
}

impl Watch {
    pub fn problems(&mut self, s: &Snapshot, disk_free_gb: Option<f64>) -> Vec<Problem> {
        let mut out = Vec::new();
        self.feed(s, &mut out);
        self.devices(s, &mut out);
        twitch(s, &mut out);
        if let Some(gb) = disk_free_gb.filter(|gb| *gb < DISK_LOW_GB) {
            out.push(Problem {
                key: "disk".into(),
                title: "The disk is almost full".into(),
                body: format!("Only {gb:.0} GB left, so recordings may stop. Delete old recordings in Settings → Backups, or free up space."),
                urgency: Urgency::Critical,
                hold: Duration::ZERO,
                open: Some("maintenance".into()),
                health: None,
            });
        }
        self.health(s, &mut out);
        out
    }

    /// Health checks passing at the last [`Watch::problems`]: `(health.<check>, detail)`.
    pub fn passing(&self) -> &[(String, String)] {
        &self.passing
    }

    /// Failing `health.*` checks. One failing since the engine started (never seen working)
    /// counts only while the show matters: preflight shows those.
    fn health(&mut self, s: &Snapshot, out: &mut Vec<Problem>) {
        let matters = show_matters(s);
        self.passing.clear();
        let mut addrs: Vec<&String> = s.index.keys().filter(|a| a.starts_with("health.")).collect();
        addrs.sort();
        for addr in addrs {
            let check = &addr["health.".len()..];
            let Some(v) = s.get(addr) else { continue };
            let detail = v.get_path("detail").and_then(Value::as_str).unwrap_or("");
            match v.get_path("status").and_then(Value::as_str) {
                Some(st @ ("pass" | "warn")) => {
                    self.worked.insert(check.to_string());
                    if st == "pass" {
                        self.passing.push((addr.clone(), detail.to_string()));
                    }
                }
                Some("fail") => {
                    let on_air_only = ON_AIR_ONLY.contains(&check);
                    if !matters && (on_air_only || !self.worked.contains(check)) {
                        continue;
                    }
                    let (name, covered_by) = check_info(check);
                    let name = if name.is_empty() { check.to_string() } else { name.to_string() };
                    out.push(Problem {
                        key: addr.clone(),
                        title: format!("Stream Engine: {name} failed"),
                        body: if detail.is_empty() { "Open Stream Engine to see what's wrong.".into() } else { detail.to_string() },
                        urgency: Urgency::Critical,
                        hold: HEALTH_HOLD,
                        open: Some(HEALTH_VIEW.into()),
                        health: Some(HealthFail { name, covered_by }),
                    });
                }
                _ => {}
            }
        }
    }

    fn feed(&self, s: &Snapshot, out: &mut Vec<Problem>) {
        // with OBS closed "stale" is trivially true; that isn't a feed problem
        if !s.bool("obs.link") || !show_matters(s) {
            return;
        }
        for (canvas, label) in [("wide", "main"), ("tall", "vertical")] {
            if s.bool(&format!("obs.stale.{canvas}")) {
                out.push(Problem {
                    key: format!("feed.{canvas}"),
                    title: format!("OBS stopped getting your {label} video"),
                    body: "Viewers may be seeing the \"Technical difficulties\" screen. Open Stream Engine to check your cameras and video.".into(),
                    urgency: Urgency::Critical,
                    hold: Duration::from_secs(5),
                    open: Some("program".into()),
                    health: None,
                });
            }
        }
    }

    fn devices(&mut self, s: &Snapshot, out: &mut Vec<Problem>) {
        let mut seen: BTreeSet<&str> = BTreeSet::new();
        for addr in s.index.keys() {
            let Some(id) = addr.strip_prefix("devices.").and_then(|r| r.strip_suffix(".present")) else { continue };
            seen.insert(id);
            let now = s.bool(addr);
            if self.present.insert(id.to_string(), now) == Some(true) && !now {
                self.unplugged.insert(id.to_string());
            } else if now {
                self.unplugged.remove(id);
            }
        }
        // devices nobody expects vanish from the state when unplugged: nothing to report
        self.present.retain(|id, _| seen.contains(id.as_str()));
        self.unplugged.retain(|id| seen.contains(id.as_str()));
        for id in &self.unplugged {
            let name = s.str(&format!("devices.{id}.name")).filter(|n| !n.trim().is_empty()).unwrap_or(id);
            out.push(Problem {
                key: format!("device.{id}"),
                title: format!("\u{201c}{name}\u{201d} was disconnected"),
                body: "Check its cable and power. Stream Engine picks it up again by itself when it's back.".into(),
                urgency: Urgency::Critical,
                hold: Duration::from_secs(3),
                open: Some("devices".into()),
                health: None,
            });
        }
    }
}

fn twitch(s: &Snapshot, out: &mut Vec<Problem>) {
    for (prefix, title) in [("twitch.auth", "Twitch needs you to sign in again"), ("twitch.auth.bot", "The chat bot needs to sign in to Twitch again")] {
        let status = s.str(&format!("{prefix}.status")).unwrap_or("none");
        let left = s.get(&format!("{prefix}.expires_in_s")).and_then(Value::as_i64).unwrap_or(0);
        // tokens renew themselves 5 minutes before they run out; still running out means renewing fails
        if matches!(status, "expired" | "error") || (status == "authorized" && left > 0 && left < 240) {
            out.push(Problem {
                key: prefix.into(),
                title: title.into(),
                body: "Stream Engine can't renew its Twitch access. Open Community → Twitch and sign in.".into(),
                urgency: Urgency::Normal,
                hold: Duration::from_secs(60),
                open: Some("twitch".into()),
                health: None,
            });
        }
    }
}

// ---- UI focus -------------------------------------------------------------------------------

/// Process start time (clock ticks since boot) from `/proc/<pid>/stat`: tells a live UI from a
/// recycled pid.
fn proc_start(pid: u32) -> Option<u64> {
    let s = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // fields after the `(comm)`: state is field 3, starttime field 22
    s.get(s.rfind(')')? + 2..)?.split(' ').nth(19)?.parse().ok()
}

/// Focus as reported by each running UI process.
#[derive(Default)]
pub struct Focus {
    uis: HashMap<u32, (u64, bool)>,
}

impl Focus {
    /// Returns true when this pid wasn't known yet (a UI that just connected).
    pub fn report(&mut self, pid: u32, focused: bool) -> bool {
        let Some(start) = proc_start(pid) else { return false };
        self.uis.insert(pid, (start, focused)).is_none_or(|(s, _)| s != start)
    }

    /// Forget UIs that exited.
    pub fn prune(&mut self) {
        self.uis.retain(|pid, (start, _)| proc_start(*pid) == Some(*start));
    }

    pub fn focused(&self) -> bool {
        self.uis.values().any(|(_, f)| *f)
    }

    pub fn any_ui(&self) -> bool {
        !self.uis.is_empty()
    }
}

// ---- engine side ------------------------------------------------------------------------------

fn arg<'a>(args: &'a Value, key: &str, pos: usize) -> Option<&'a Value> {
    args.get_path(key).or_else(|| args.get_path("args").and_then(Value::as_list).and_then(|l| l.get(pos)))
}

/// `notify.send {key, title, body, urgency?, open?}` → a problem to show once.
pub fn parse_request(args: &Value) -> Result<Problem, String> {
    let s = |k: &str, pos: usize| arg(args, k, pos).map(|v| v.to_string()).filter(|v| !v.trim().is_empty());
    let title = s("title", 0).ok_or("notify.send needs a title")?;
    let urgency = match s("urgency", usize::MAX).as_deref() {
        None | Some("normal") => Urgency::Normal,
        Some("critical") => Urgency::Critical,
        Some(o) => return Err(format!("notify.send: urgency must be normal or critical, not `{o}`")),
    };
    Ok(Problem {
        key: s("key", usize::MAX).unwrap_or_else(|| title.clone()),
        body: s("body", 1).unwrap_or_default(),
        title,
        urgency,
        hold: Duration::ZERO,
        open: s("open", usize::MAX),
        health: None,
    })
}

/// `stream-engine-launch-or-focus`: on PATH, else next to the engine binary.
fn launcher() -> Option<PathBuf> {
    const NAME: &str = "stream-engine-launch-or-focus";
    let on_path = std::env::var_os("PATH").and_then(|p| std::env::split_paths(&p).map(|d| d.join(NAME)).find(|p| p.is_file()));
    on_path.or_else(|| std::env::current_exe().ok().map(|e| e.with_file_name(NAME)).filter(|p| p.is_file()))
}

/// Show `note`; resolves to its `open` target when the user clicked it.
async fn show(note: Note) -> Option<Option<String>> {
    let mut cmd = tokio::process::Command::new("notify-send");
    cmd.args(["-a", "Stream Engine", "-i", "stream-engine", "-u", note.urgency.as_str(), "-A", "default=Open Stream Engine", &note.title, &note.body])
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    // `-A` waits until the notification is clicked or dismissed
    let out = match tokio::time::timeout(Duration::from_secs(3600), cmd.output()).await {
        Ok(Ok(o)) => o,
        Ok(Err(e)) => {
            tracing::warn!(target: "notify", "notify-send: {e}");
            return None;
        }
        Err(_) => return None,
    };
    (String::from_utf8_lossy(&out.stdout).trim() == "default").then_some(note.open)
}

pub fn start(ctx: EngineCtx) {
    ctx.hub.declare("ui.focused", Meta::boolean(false).readonly().owner(TARGET).describe("A Stream Engine window has keyboard focus"));
    let mut focus_rx = ctx.hub.route_actions("ui.focus");
    let mut notify_rx = ctx.hub.route_actions("notify");
    let (click_tx, mut click_rx) = tokio::sync::mpsc::unbounded_channel::<Option<String>>();
    tokio::spawn(async move {
        let hub = ctx.hub.clone();
        let mut focus = Focus::default();
        let mut watch = Watch::default();
        let mut notifier = Notifier::default();
        let mut published: Option<bool> = None;
        // a click launched the UI: open this panel once it reports in
        let mut pending_open: Option<(String, Instant)> = None;
        let mut disk: (Option<Instant>, Option<f64>) = (None, None);
        let mut tick = tokio::time::interval(Duration::from_secs(2));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let open_panel = |view: &str| hub.emit(Event::new("ui.open", Origin::System, Value::map().with("view", view)));
        loop {
            tokio::select! {
                c = focus_rx.recv() => {
                    let Some(c) = c else { break };
                    let Op::Action { args, .. } = &c.op else { continue };
                    let pid = arg(args, "pid", 1).and_then(Value::as_i64).and_then(|p| u32::try_from(p).ok());
                    let focused = arg(args, "focused", 0).is_some_and(Value::truthy);
                    match pid {
                        Some(pid) => {
                            if focus.report(pid, focused)
                                && let Some((view, until)) = pending_open.take()
                                && Instant::now() < until
                            {
                                open_panel(&view);
                            }
                        }
                        None => hub.log("warn", TARGET, "ui.focus without a pid is ignored"),
                    }
                }
                c = notify_rx.recv() => {
                    let Some(c) = c else { break };
                    let Op::Action { name, args } = &c.op else { continue };
                    match (name.as_str(), parse_request(args)) {
                        ("notify.send", Ok(p)) => notifier.request(Instant::now(), p),
                        ("notify.send", Err(e)) => hub.log("warn", TARGET, e),
                        (other, _) => hub.log("warn", TARGET, format!("unknown action {other}")),
                    }
                }
                c = click_rx.recv() => {
                    let Some(open) = c else { continue };
                    match launcher() {
                        Some(l) => {
                            if let Err(e) = tokio::process::Command::new(&l).stdin(std::process::Stdio::null()).spawn() {
                                hub.log("warn", TARGET, format!("could not open the UI ({}): {e}", l.display()));
                            }
                        }
                        None => hub.log("warn", TARGET, "stream-engine-launch-or-focus not found; can't open the UI"),
                    }
                    if let Some(view) = open {
                        if focus.any_ui() {
                            open_panel(&view);
                        } else {
                            pending_open = Some((view, Instant::now() + Duration::from_secs(30)));
                        }
                    }
                }
                _ = tick.tick() => {
                    let now = Instant::now();
                    if disk.0.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(30)) {
                        disk = (Some(now), crate::maintenance::free_bytes(&ctx.project_root).map(|b| b as f64 / 1e9));
                    }
                    focus.prune();
                    let focused = focus.focused();
                    if published != Some(focused) {
                        published = Some(focused);
                        hub.publish("ui.focused", Value::Bool(focused));
                    }
                    let problems = watch.problems(&hub.snapshot.load(), disk.1);
                    for (key, detail) in watch.passing() {
                        notifier.recovered(now, key, detail);
                    }
                    if let Some(note) = notifier.update(now, problems, focused) {
                        tracing::info!(target: "notify", "desktop notification: {} ({})", note.title, note.keys.join(", "));
                        hub.emit(Event::new(
                            "notify.sent",
                            Origin::System,
                            Value::map().with("keys", note.keys.clone()).with("title", note.title.as_str()).with("body", note.body.as_str()),
                        ));
                        let tx = click_tx.clone();
                        tokio::spawn(async move {
                            if let Some(open) = show(note).await {
                                let _ = tx.send(open);
                            }
                        });
                    }
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn p(key: &str, hold: u64) -> Problem {
        Problem {
            key: key.into(),
            title: format!("{key} broke"),
            body: String::new(),
            urgency: Urgency::Normal,
            hold: Duration::from_secs(hold),
            open: None,
            health: None,
        }
    }

    fn at(t0: Instant, s: u64) -> Instant {
        t0 + Duration::from_secs(s)
    }

    #[test]
    fn a_problem_notifies_once_after_its_hold_time() {
        let t0 = Instant::now();
        let mut n = Notifier::default();
        assert_eq!(n.update(t0, vec![p("feed.wide", 5)], false), None);
        assert_eq!(n.update(at(t0, 4), vec![p("feed.wide", 5)], false), None, "still within the hold time");
        let note = n.update(at(t0, 5), vec![p("feed.wide", 5)], false).expect("due");
        assert_eq!(note.keys, vec!["feed.wide".to_string()]);
        assert_eq!(n.update(at(t0, 600), vec![p("feed.wide", 5)], false), None, "never repeated while it lasts");
    }

    #[test]
    fn a_blip_shorter_than_the_hold_time_is_ignored() {
        let t0 = Instant::now();
        let mut n = Notifier::default();
        n.update(t0, vec![p("device.cam", 3)], false);
        n.update(at(t0, 2), vec![], false);
        assert_eq!(n.update(at(t0, 4), vec![p("device.cam", 3)], false), None, "the hold time restarts");
        assert!(n.update(at(t0, 7), vec![p("device.cam", 3)], false).is_some());
    }

    #[test]
    fn a_problem_seen_while_focused_is_not_notified_later() {
        let t0 = Instant::now();
        let mut n = Notifier::default();
        assert_eq!(n.update(t0, vec![p("disk", 0)], true), None);
        assert_eq!(n.update(at(t0, 60), vec![p("disk", 0)], false), None, "the UI showed it when it happened");
        // a new occurrence after it cleared is new
        n.update(at(t0, 61), vec![], false);
        assert!(n.update(at(t0, 62), vec![p("disk", 0)], false).is_some());
    }

    #[test]
    fn a_recurring_problem_waits_for_the_repeat_interval() {
        let t0 = Instant::now();
        let mut n = Notifier::default();
        assert!(n.update(t0, vec![p("device.cam", 0)], false).is_some());
        n.update(at(t0, 10), vec![], false);
        assert_eq!(n.update(at(t0, 100), vec![p("device.cam", 0)], false), None, "flapping device: once is enough");
        n.update(at(t0, 101), vec![], false);
        let later = t0 + REPEAT_AFTER + Duration::from_secs(1);
        assert!(n.update(later, vec![p("device.cam", 0)], false).is_some());
    }

    #[test]
    fn problems_due_together_or_within_the_gap_are_combined() {
        let t0 = Instant::now();
        let mut n = Notifier::default();
        let both = n.update(t0, vec![p("device.a", 0), p("device.b", 0)], false).expect("due");
        assert_eq!(both.keys.len(), 2);
        assert_eq!(both.title, "2 things need your attention");
        // two more within the gap wait, the one that clears meanwhile is never sent
        assert_eq!(n.update(at(t0, 5), vec![p("device.a", 0), p("device.b", 0), p("disk", 0), p("twitch.auth", 0)], false), None);
        let rest = n.update(at(t0, 31), vec![p("device.a", 0), p("device.b", 0), p("disk", 0)], false).expect("gap over");
        assert_eq!(rest.keys, vec!["disk".to_string()]);
    }

    #[test]
    fn critical_problems_lead_a_combined_notification() {
        let t0 = Instant::now();
        let mut n = Notifier::default();
        let mut crit = p("feed.wide", 0);
        crit.urgency = Urgency::Critical;
        crit.open = Some("program".into());
        let note = n.update(t0, vec![p("twitch.auth", 0), crit], false).unwrap();
        assert_eq!(note.urgency, Urgency::Critical);
        assert_eq!(note.open.as_deref(), Some("program"));
        assert!(note.body.starts_with("• feed.wide broke"), "{}", note.body);
    }

    #[test]
    fn requests_follow_focus_repeat_and_expiry_rules() {
        let t0 = Instant::now();
        let mut n = Notifier::default();
        n.request(t0, p("backup", 0));
        assert_eq!(n.update(t0, vec![], true), None, "focused: the UI shows it");
        assert_eq!(n.update(at(t0, 1), vec![], false), None, "and it's gone");
        n.request(at(t0, 2), p("backup", 0));
        assert!(n.update(at(t0, 2), vec![], false).is_some());
        n.request(at(t0, 100), p("backup", 0));
        assert_eq!(n.update(at(t0, 100), vec![], false), None, "same key within the repeat interval");
        let mut m = Notifier::default();
        m.request(t0, p("disk", 0));
        assert!(m.update(at(t0, 1), vec![p("device.x", 0)], false).is_some());
        m.request(at(t0, 2), p("budget", 0));
        assert_eq!(m.update(at(t0, 2) + REQUEST_TTL, vec![], false), None, "expired while waiting for the gap");
    }

    fn snap(vals: &[(&str, Value)]) -> Snapshot {
        let index: HashMap<String, usize> = vals.iter().enumerate().map(|(i, (a, _))| (a.to_string(), i)).collect();
        Snapshot { index: Arc::new(index), values: vals.iter().map(|(_, v)| v.clone()).collect(), ..Default::default() }
    }

    fn keys(ps: &[Problem]) -> Vec<&str> {
        ps.iter().map(|p| p.key.as_str()).collect()
    }

    #[test]
    fn only_an_expected_device_going_away_is_a_problem() {
        let mut w = Watch::default();
        let missing_at_start = snap(&[("devices.cam.present", Value::Bool(false)), ("devices.cam.name", "Kit cam".into())]);
        assert!(w.problems(&missing_at_start, None).is_empty(), "never seen present: preflight's job");
        let here = snap(&[("devices.cam.present", Value::Bool(true)), ("devices.cam.name", "Kit cam".into())]);
        assert!(w.problems(&here, None).is_empty());
        let gone = w.problems(&missing_at_start, None);
        assert_eq!(keys(&gone), ["device.cam"]);
        assert_eq!(gone[0].title, "\u{201c}Kit cam\u{201d} was disconnected");
        assert!(!w.problems(&missing_at_start, None).is_empty(), "still gone");
        assert!(w.problems(&here, None).is_empty(), "back");
        w.problems(&here, None);
        assert!(w.problems(&snap(&[]), None).is_empty(), "an unexpected device vanishing is not reported");
    }

    #[test]
    fn a_stale_feed_counts_only_while_obs_is_open_and_it_matters() {
        let mut w = Watch::default();
        let base = |link: bool, mode: &str, streaming: bool| {
            snap(&[
                ("obs.link", Value::Bool(link)),
                ("obs.stale.wide", Value::Bool(true)),
                ("obs.stale.tall", Value::Bool(false)),
                ("show.mode", mode.into()),
                ("obs.stream.active", Value::Bool(streaming)),
            ])
        };
        assert!(w.problems(&base(false, "live", false), None).is_empty(), "OBS closed");
        assert!(w.problems(&base(true, "offline", false), None).is_empty(), "nothing going out");
        assert!(w.problems(&base(true, "rehearsal", false), None).is_empty());
        assert_eq!(keys(&w.problems(&base(true, "offline", true), None)), ["feed.wide"], "streaming");
        assert_eq!(keys(&w.problems(&base(true, "brb", false), None)), ["feed.wide"]);
    }

    #[test]
    fn twitch_sign_in_problems_and_disk() {
        let mut w = Watch::default();
        let auth = |status: &str, left: i64| snap(&[("twitch.auth.status", status.into()), ("twitch.auth.expires_in_s", Value::Int(left))]);
        assert!(w.problems(&auth("authorized", 250), None).is_empty(), "renews by itself");
        assert_eq!(keys(&w.problems(&auth("authorized", 200), None)), ["twitch.auth"], "renewing is failing");
        assert_eq!(keys(&w.problems(&auth("expired", 0), None)), ["twitch.auth"]);
        assert!(w.problems(&auth("none", 0), None).is_empty(), "never set up: not a background problem");
        assert!(w.problems(&snap(&[]), Some(DISK_LOW_GB)).is_empty());
        assert_eq!(keys(&w.problems(&snap(&[]), Some(9.4))), ["disk"]);
    }

    fn health(status: &str, detail: &str) -> Value {
        Value::map().with("status", status).with("detail", detail)
    }

    /// One tick of the engine loop: watch → recoveries → notifier.
    fn tick(w: &mut Watch, n: &mut Notifier, now: Instant, s: &Snapshot, focused: bool) -> Option<Note> {
        let problems = w.problems(s, None);
        for (k, d) in w.passing().to_vec() {
            n.recovered(now, &k, &d);
        }
        n.update(now, problems, focused)
    }

    #[test]
    fn a_health_failure_notifies_once_even_when_focused_and_its_recovery_follows() {
        let t0 = Instant::now();
        let (mut w, mut n) = (Watch::default(), Notifier::default());
        let ok = snap(&[("health.mixer", health("pass", "console connected"))]);
        let bad = snap(&[("health.mixer", health("fail", "console unreachable at 10.0.0.5"))]);
        assert_eq!(tick(&mut w, &mut n, t0, &ok, true), None);
        assert_eq!(tick(&mut w, &mut n, at(t0, 1), &bad, true), None);
        assert_eq!(tick(&mut w, &mut n, at(t0, 10), &bad, true), None, "within the hold time");
        let note = tick(&mut w, &mut n, at(t0, 11), &bad, true).expect("fails notify while focused");
        assert_eq!(note.title, "Stream Engine: Mixing desk failed");
        assert_eq!(note.body, "console unreachable at 10.0.0.5");
        assert_eq!((note.urgency, note.open.as_deref()), (Urgency::Critical, Some(HEALTH_VIEW)));
        assert_eq!(tick(&mut w, &mut n, at(t0, 300), &bad, false), None, "no duplicate while it lasts");
        let back = tick(&mut w, &mut n, at(t0, 400), &ok, false).expect("recovered");
        assert_eq!((back.title.as_str(), back.urgency), ("Stream Engine: Mixing desk recovered", Urgency::Low));
        assert_eq!(tick(&mut w, &mut n, at(t0, 500), &ok, false), None, "recovery is said once");
        // breaking again after it recovered is news, even within the repeat interval
        tick(&mut w, &mut n, at(t0, 600), &bad, false);
        assert!(tick(&mut w, &mut n, at(t0, 610), &bad, false).is_some());
    }

    #[test]
    fn health_blips_and_unresolved_failures_stay_quiet() {
        let t0 = Instant::now();
        let (mut w, mut n) = (Watch::default(), Notifier::default());
        let s = |st: &str| snap(&[("health.dmx", health(st, "adapter"))]);
        tick(&mut w, &mut n, t0, &s("pass"), false);
        tick(&mut w, &mut n, at(t0, 1), &s("fail"), false);
        assert_eq!(tick(&mut w, &mut n, at(t0, 5), &s("pass"), false), None, "a blip shorter than the hold");
        assert_eq!(tick(&mut w, &mut n, at(t0, 6), &s("pass"), false), None, "nothing to recover from");
        tick(&mut w, &mut n, at(t0, 100), &s("fail"), false);
        assert!(tick(&mut w, &mut n, at(t0, 110), &s("fail"), false).is_some());
        // fail → warn → fail without ever passing: not repeated within the repeat interval
        assert_eq!(tick(&mut w, &mut n, at(t0, 200), &s("warn"), false), None, "warn is not a recovery");
        tick(&mut w, &mut n, at(t0, 300), &s("fail"), false);
        assert_eq!(tick(&mut w, &mut n, at(t0, 310), &s("fail"), false), None);
    }

    #[test]
    fn a_different_check_failing_soon_after_is_delayed_not_dropped() {
        let t0 = Instant::now();
        let (mut w, mut n) = (Watch::default(), Notifier::default());
        let s = |a: &str, b: &str| snap(&[("health.mixer", health(a, "m")), ("health.relay", health(b, "r"))]);
        tick(&mut w, &mut n, t0, &s("pass", "pass"), false);
        tick(&mut w, &mut n, at(t0, 1), &s("fail", "pass"), false);
        assert!(tick(&mut w, &mut n, at(t0, 11), &s("fail", "pass"), false).is_some());
        tick(&mut w, &mut n, at(t0, 12), &s("fail", "fail"), false);
        assert_eq!(tick(&mut w, &mut n, at(t0, 22), &s("fail", "fail"), false), None, "inside the minimum gap");
        let note = tick(&mut w, &mut n, at(t0, 41), &s("fail", "fail"), false).expect("after the gap");
        assert_eq!(note.keys, vec!["health.relay".to_string()]);
    }

    #[test]
    fn checks_failing_since_start_or_obs_closed_count_only_while_the_show_matters() {
        let mut w = Watch::default();
        let s =
            |mode: &str| snap(&[("show.mode", mode.into()), ("health.cef", health("fail", "host down")), ("health.obs", health("fail", "OBS is not running"))]);
        assert!(w.problems(&s("offline"), None).is_empty(), "never seen working: preflight's job");
        assert_eq!(keys(&w.problems(&s("live"), None)), ["health.cef", "health.obs"]);
        w.problems(&snap(&[("health.cef", health("pass", "")), ("health.obs", health("pass", ""))]), None);
        assert_eq!(keys(&w.problems(&s("offline"), None)), ["health.cef"], "OBS closed after the show is not a failure");
    }

    #[test]
    fn failures_a_specific_notification_reports_are_merged() {
        let t0 = Instant::now();
        let (mut w, mut n) = (Watch::default(), Notifier::default());
        let ok = snap(&[("twitch.auth.status", "authorized".into()), ("twitch.auth.expires_in_s", Value::Int(9000)), ("health.twitch", health("pass", "ok"))]);
        let expired = snap(&[("twitch.auth.status", "expired".into()), ("health.twitch", health("fail", "token expired"))]);
        tick(&mut w, &mut n, t0, &ok, false);
        tick(&mut w, &mut n, at(t0, 1), &expired, false);
        assert_eq!(tick(&mut w, &mut n, at(t0, 20), &expired, false), None, "health.twitch merged into the sign-in problem");
        let note = tick(&mut w, &mut n, at(t0, 61), &expired, false).expect("the sign-in problem");
        assert_eq!(note.keys, vec!["twitch.auth".to_string()]);
        assert!(tick(&mut w, &mut n, at(t0, 200), &ok, false).is_some_and(|r| r.title == "Stream Engine: Twitch recovered"));
        // a backup failure already requested by maintenance
        let mut m = Notifier::default();
        m.request(t0, p("backup", 0));
        let mut fail = p("health.backup", 0);
        fail.health = Some(HealthFail { name: "Backups".into(), covered_by: &["backup"] });
        let note = m.update(t0, vec![fail], false).expect("the request");
        assert_eq!(note.keys, vec!["backup".to_string()]);
    }

    #[test]
    fn requests_parse_with_defaults_and_reject_bad_urgency() {
        let r = parse_request(&Value::map().with("title", "Backup failed").with("body", "disk full")).unwrap();
        assert_eq!((r.key.as_str(), r.urgency, r.hold), ("Backup failed", Urgency::Normal, Duration::ZERO));
        assert!(parse_request(&Value::map().with("body", "x")).is_err());
        assert!(parse_request(&Value::map().with("title", "x").with("urgency", "loud")).unwrap_err().contains("urgency"));
    }

    #[test]
    fn focus_follows_live_ui_processes() {
        let mut f = Focus::default();
        assert!(!f.focused(), "no UI: not focused");
        let me = std::process::id();
        assert!(f.report(me, true), "first report from a UI");
        assert!(!f.report(me, true));
        f.prune();
        assert!(f.focused());
        f.report(me, false);
        assert!(!f.focused());
        assert!(!f.report(u32::MAX - 1, true), "no such process");
        assert!(!f.focused());
    }
}
