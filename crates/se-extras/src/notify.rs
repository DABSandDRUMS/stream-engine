//! Desktop notifications (§16.3): background problems reach the desktop as freedesktop
//! notifications (`notify-send`; the Omarchy shell is the daemon) while no Stream Engine window
//! has focus. Only problems, never routine events:
//!
//! * a feed stopped reaching OBS while it matters (streaming, recording, or a show mode),
//! * an expected device was unplugged (`devices.<id>.present` went true → false),
//! * Twitch can't renew its sign-in (`twitch.auth[.bot].status` expired/error, or about to run out),
//! * the project disk is almost full (under 10 GB, the preflight `disk` failure level),
//! * whatever another subsystem asks for with `notify.send {key, title, body, urgency?, open?}`
//!   (recordings disk, failed backups).
//!
//! Focus: the UI reports `ui.focus {focused, pid}` when its focus changes and when it
//! (re)connects. A UI whose process is gone, or no UI at all, counts as not focused. `ui.focused`
//! mirrors the result.
//!
//! Rules: a problem must last its hold time (feed 5 s, device 3 s, Twitch 60 s) before it
//! counts; each occurrence notifies at most once, and one that became due while a window had
//! focus counts as seen. The same problem isn't repeated within [`REPEAT_AFTER`], and
//! notifications are at least [`MIN_GAP`] apart: problems that become due in between are
//! combined into one notification. Clicking a notification opens (or focuses) the UI on the page
//! that fixes the problem. Each notification is also the event `notify.sent {keys, title, body}`.

use se_hub::{EngineCtx, Snapshot};
use se_proto::{Event, Meta, Op, Origin, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Urgency {
    Normal,
    Critical,
}

impl Urgency {
    pub fn as_str(self) -> &'static str {
        match self {
            Urgency::Normal => "normal",
            Urgency::Critical => "critical",
        }
    }
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

        let due: Vec<String> = self.open.iter().filter(|(_, o)| !o.handled && now.duration_since(o.since) >= o.problem.hold).map(|(k, _)| k.clone()).collect();
        if focused {
            // a window is in front: the UI shows it
            for k in &due {
                self.open.get_mut(k).expect("due key is open").handled = true;
            }
            self.requests.clear();
            return None;
        }
        let recent = |last: &HashMap<String, Instant>, k: &str| last.get(k).is_some_and(|t| now.duration_since(*t) < REPEAT_AFTER);
        let mut ready: Vec<Problem> = Vec::new();
        for k in due {
            let o = self.open.get_mut(&k).expect("due key is open");
            if recent(&self.last_key, &k) {
                o.handled = true;
            } else {
                ready.push(o.problem.clone());
            }
        }
        let last_key = &self.last_key;
        self.requests.retain(|(_, r)| !recent(last_key, &r.key));
        for (_, r) in &self.requests {
            if !ready.iter().any(|p| p.key == r.key) {
                ready.push(r.clone());
            }
        }
        if ready.is_empty() || self.last_sent.is_some_and(|t| now.duration_since(t) < MIN_GAP) {
            return None;
        }
        for p in &ready {
            if let Some(o) = self.open.get_mut(&p.key) {
                o.handled = true;
            }
            self.last_key.insert(p.key.clone(), now);
        }
        self.requests.clear();
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
    body.push("Open Stream Engine to see what to do.".into());
    Note { keys, title: format!("{} things need your attention", ready.len()), body: body.join("\n"), urgency, open: ready[0].open.clone() }
}

// ---- what counts as a problem -------------------------------------------------------------

/// Modes in which the feed reaching OBS matters even without a running output.
fn show_mode(mode: &str) -> bool {
    matches!(mode, "preshow" | "live" | "brb" | "ad_break" | "outro")
}

/// Turns engine state into problems; remembers device presence to spot unplugging.
#[derive(Default)]
pub struct Watch {
    present: HashMap<String, bool>,
    unplugged: BTreeSet<String>,
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
            });
        }
        out
    }

    fn feed(&self, s: &Snapshot, out: &mut Vec<Problem>) {
        let matters = s.bool("obs.stream.active") || s.bool("obs.record.active") || show_mode(s.str("show.mode").unwrap_or("offline"));
        // with OBS closed "stale" is trivially true; that isn't a feed problem
        if !s.bool("obs.link") || !matters {
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
        Problem { key: key.into(), title: format!("{key} broke"), body: String::new(), urgency: Urgency::Normal, hold: Duration::from_secs(hold), open: None }
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
        let base = |link: bool, mode: &str, rec: bool| {
            snap(&[
                ("obs.link", Value::Bool(link)),
                ("obs.stale.wide", Value::Bool(true)),
                ("obs.stale.tall", Value::Bool(false)),
                ("show.mode", mode.into()),
                ("obs.record.active", Value::Bool(rec)),
            ])
        };
        assert!(w.problems(&base(false, "live", false), None).is_empty(), "OBS closed");
        assert!(w.problems(&base(true, "offline", false), None).is_empty(), "nothing going out");
        assert!(w.problems(&base(true, "rehearsal", false), None).is_empty());
        assert_eq!(keys(&w.problems(&base(true, "offline", true), None)), ["feed.wide"], "recording");
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
