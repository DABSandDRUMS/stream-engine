//! The live guard (AGENTS.md LAW: live streams are never interrupted). Every piece of post-show
//! background work — show indexing, clip jobs, ISO cleanup, archive encodes and moves — waits
//! while the show is LIVE, and stops within 2 s when LIVE begins:
//!
//! * LIVE = `show.mode` is anything but `offline`, OR `twitch.stream.live`, OR
//!   `obs.stream.active`, OR the recorder is active/starting/stopping.
//! * Unknown is LIVE: a missing or unreadable `show.mode` (engine still starting, no snapshot) or
//!   an unreadable flag counts as LIVE. Integrations that never published their flag are not.
//!
//! [`LiveGuard`] polls the state snapshot every 250 ms. Blocking work runs inside
//! [`with_halt`]; FFmpeg children started through [`wait_child`] are killed
//! ([`OnHold::Cancel`], the job is re-queued) or frozen with `SIGSTOP` and resumed with
//! `SIGCONT` ([`OnHold::Pause`], long archive encodes) while the guard holds, and Whisper aborts.

use se_hub::{Hub, Snapshot};
use se_proto::Value;
use std::cell::RefCell;
use std::process::{Child, ExitStatus};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::watch;

/// How often the guard samples the state snapshot.
pub const POLL: Duration = Duration::from_millis(250);
/// Child-process supervision interval (well inside the 2 s budget).
const CHILD_POLL: Duration = Duration::from_millis(50);
/// Error text of work stopped because the show went live.
pub const HALTED: &str = "stopped: the show is live";
/// Error text of work aborted by the operator or a configuration change.
pub const ABORTED: &str = "stopped by the operator";

/// Why the show counts as LIVE right now, or `None` when post-show work may run.
pub fn live_reason(snap: &Snapshot) -> Option<String> {
    match snap.get("show.mode") {
        None | Some(Value::Null) => return Some("show mode unknown".into()),
        Some(Value::Str(m)) if m == "offline" => {}
        Some(Value::Str(m)) => return Some(format!("show mode is {m}")),
        Some(_) => return Some("show mode unreadable".into()),
    }
    for (key, what) in [
        ("twitch.stream.live", "Twitch stream is live"),
        ("obs.stream.active", "OBS is streaming"),
        ("recording.active", "recording"),
        ("recording.starting", "recording is starting"),
        ("recording.stopping", "recording is finishing"),
    ] {
        match snap.get(key) {
            None | Some(Value::Null) | Some(Value::Bool(false)) => {}
            Some(Value::Bool(true)) => return Some(what.into()),
            Some(_) => return Some(format!("{key} unreadable")),
        }
    }
    None
}

/// Shared LIVE state for every post-show worker. Cheap to clone.
#[derive(Clone)]
pub struct LiveGuard {
    rx: watch::Receiver<Option<String>>,
}

impl LiveGuard {
    /// Follow the hub's state snapshot. Starts LIVE ("engine starting") until the first sample.
    pub fn spawn(hub: Arc<Hub>) -> LiveGuard {
        let (tx, rx) = watch::channel(Some("engine starting".to_string()));
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(POLL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tick.tick().await;
                let reason = live_reason(&hub.snapshot.load());
                tx.send_if_modified(|cur| {
                    let changed = *cur != reason;
                    if changed {
                        cur.clone_from(&reason);
                    }
                    changed
                });
                if tx.is_closed() {
                    break;
                }
            }
        });
        LiveGuard { rx }
    }

    /// A guard driven by hand (tests, tools): send `Some(reason)` for LIVE, `None` for offline.
    pub fn manual(reason: Option<&str>) -> (LiveGuard, watch::Sender<Option<String>>) {
        let (tx, rx) = watch::channel(reason.map(String::from));
        (LiveGuard { rx }, tx)
    }

    pub fn reason(&self) -> Option<String> {
        self.rx.borrow().clone()
    }

    pub fn is_live(&self) -> bool {
        self.rx.borrow().is_some()
    }

    /// Wait until the show is offline (returns at once when it already is).
    pub async fn wait_offline(&self) {
        let mut rx = self.rx.clone();
        let _ = rx.wait_for(Option::is_none).await;
    }

    /// Wait until the show goes LIVE.
    pub async fn wait_live(&self) {
        let mut rx = self.rx.clone();
        let _ = rx.wait_for(Option::is_some).await;
    }

    /// Wait for the next LIVE/offline change.
    pub async fn changed(&self) {
        let mut rx = self.rx.clone();
        rx.borrow_and_update();
        let _ = rx.changed().await;
    }

    /// A stop signal for blocking work that holds while the show is LIVE.
    pub fn halt(&self, mode: OnHold) -> Halt {
        Halt { live: Some(self.rx.clone()), hold: Arc::default(), abort: Arc::default(), mode }
    }
}

/// What a running child does while the work is held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnHold {
    /// Kill it; the caller re-queues the work.
    Cancel,
    /// Freeze it (`SIGSTOP`) and continue (`SIGCONT`) once the hold lifts.
    Pause,
}

/// Stop signal checked from blocking threads: held while LIVE (or by [`Halt::set_hold`]),
/// aborted by [`Halt::abort`].
#[derive(Clone)]
pub struct Halt {
    live: Option<watch::Receiver<Option<String>>>,
    hold: Arc<AtomicBool>,
    abort: Arc<AtomicBool>,
    mode: OnHold,
}

impl Halt {
    /// A halt without a live source (held/aborted only by hand).
    pub fn manual(mode: OnHold) -> Halt {
        Halt { live: None, hold: Arc::default(), abort: Arc::default(), mode }
    }
    pub fn held(&self) -> bool {
        self.hold.load(Ordering::Relaxed) || self.live.as_ref().is_some_and(|rx| rx.borrow().is_some())
    }
    pub fn aborted(&self) -> bool {
        self.abort.load(Ordering::Relaxed)
    }
    /// Work that can't pause (Whisper, cancel-mode jobs) must stop now.
    pub fn stop_now(&self) -> bool {
        self.aborted() || (self.mode == OnHold::Cancel && self.held())
    }
    pub fn mode(&self) -> OnHold {
        self.mode
    }
    pub fn set_hold(&self, on: bool) {
        self.hold.store(on, Ordering::Relaxed);
    }
    pub fn abort(&self) {
        self.abort.store(true, Ordering::Relaxed);
    }
    /// The error to report for a stop: [`ABORTED`] or [`HALTED`].
    pub fn error(&self) -> String {
        if self.aborted() { ABORTED.into() } else { HALTED.into() }
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Halt>> = const { RefCell::new(None) };
}

/// Run `f` with `halt` as this thread's stop signal ([`current`]).
pub fn with_halt<T>(halt: Halt, f: impl FnOnce() -> T) -> T {
    struct Reset(Option<Halt>);
    impl Drop for Reset {
        fn drop(&mut self) {
            let prev = self.0.take();
            CURRENT.with(|c| *c.borrow_mut() = prev);
        }
    }
    let prev = CURRENT.with(|c| c.borrow_mut().replace(halt));
    let _reset = Reset(prev);
    f()
}

/// This thread's stop signal, if the work runs under [`with_halt`].
pub fn current() -> Option<Halt> {
    CURRENT.with(|c| c.borrow().clone())
}

/// Wait for `child` under `halt`: killed when the work must stop (`Err(HALTED|ABORTED)`),
/// frozen while held in [`OnHold::Pause`] mode.
pub fn wait_child(child: &mut Child, halt: Option<&Halt>) -> Result<ExitStatus, String> {
    let pid = child.id() as libc::pid_t;
    let mut frozen = false;
    loop {
        if let Some(status) = child.try_wait().map_err(|e| e.to_string())? {
            return Ok(status);
        }
        if let Some(h) = halt {
            if h.stop_now() {
                let _ = child.kill();
                let _ = child.wait();
                return Err(h.error());
            }
            let hold = h.held();
            if hold != frozen {
                // SAFETY: `pid` is our unreaped child (try_wait returned None above).
                unsafe { libc::kill(pid, if hold { libc::SIGSTOP } else { libc::SIGCONT }) };
                frozen = hold;
            }
        }
        std::thread::sleep(CHILD_POLL);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn snap(pairs: &[(&str, Value)]) -> Snapshot {
        let index: HashMap<String, usize> = pairs.iter().enumerate().map(|(i, (k, _))| (k.to_string(), i)).collect();
        Snapshot { index: Arc::new(index), values: pairs.iter().map(|(_, v)| v.clone()).collect(), ..Default::default() }
    }

    #[test]
    fn live_truth_table_treats_unknown_as_live() {
        let off = || ("show.mode", Value::Str("offline".into()));
        // unknown / unreadable mode ⇒ LIVE
        assert!(live_reason(&Snapshot::default()).is_some());
        assert!(live_reason(&snap(&[("show.mode", Value::Null)])).is_some());
        assert!(live_reason(&snap(&[("show.mode", Value::Int(3))])).is_some());
        // every mode except offline ⇒ LIVE (preshow and rehearsal too)
        for m in ["preshow", "live", "brb", "ad_break", "outro", "rehearsal"] {
            assert!(live_reason(&snap(&[("show.mode", Value::Str(m.into()))])).is_some(), "{m}");
        }
        assert_eq!(live_reason(&snap(&[off()])), None);
        // offline mode, but streaming or recording ⇒ LIVE
        for key in ["twitch.stream.live", "obs.stream.active", "recording.active", "recording.starting", "recording.stopping"] {
            assert!(live_reason(&snap(&[off(), (key, Value::Bool(true))])).is_some(), "{key}");
            assert_eq!(live_reason(&snap(&[off(), (key, Value::Bool(false))])), None, "{key}");
            assert!(live_reason(&snap(&[off(), (key, Value::Str("?".into()))])).is_some(), "unreadable {key}");
        }
        // integrations that never published their flag do not hold work forever
        assert_eq!(live_reason(&snap(&[off(), ("twitch.stream.live", Value::Null)])), None);
    }

    #[tokio::test]
    async fn guard_waits_and_halts_follow_transitions() {
        let (guard, tx) = LiveGuard::manual(Some("live"));
        let halt = guard.halt(OnHold::Cancel);
        assert!(guard.is_live() && halt.held() && halt.stop_now());
        let g = guard.clone();
        let waiter = tokio::spawn(async move { g.wait_offline().await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        tx.send(None).unwrap();
        tokio::time::timeout(Duration::from_secs(1), waiter).await.unwrap().unwrap();
        assert!(!halt.held() && !halt.stop_now());
        let pause = guard.halt(OnHold::Pause);
        tx.send(Some("live".into())).unwrap();
        assert!(pause.held() && !pause.stop_now(), "paused work is held, not stopped");
        pause.abort();
        assert!(pause.stop_now());
        assert_eq!(pause.error(), ABORTED);
    }

    #[test]
    fn halt_is_scoped_to_the_thread() {
        assert!(current().is_none());
        let h = Halt::manual(OnHold::Cancel);
        with_halt(h.clone(), || {
            assert!(current().is_some());
            std::thread::spawn(|| assert!(current().is_none())).join().unwrap();
        });
        assert!(current().is_none());
    }

    #[test]
    fn cancel_kills_and_pause_freezes_a_child_within_the_budget() {
        let halt = Halt::manual(OnHold::Cancel);
        let mut child = crate::ffmpeg::background_cmd("sleep", 19, &[]).arg("30").spawn().unwrap();
        let h = halt.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            h.set_hold(true);
        });
        let started = std::time::Instant::now();
        assert_eq!(wait_child(&mut child, Some(&halt)).unwrap_err(), HALTED);
        assert!(started.elapsed() < Duration::from_secs(2));
        t.join().unwrap();

        // paused: the child keeps its progress and finishes after the hold lifts
        let halt = Halt::manual(OnHold::Pause);
        let mut child = crate::ffmpeg::background_cmd("sleep", 19, &[]).arg("0.3").spawn().unwrap();
        halt.set_hold(true);
        let h = halt.clone();
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(800));
            h.set_hold(false);
        });
        let started = std::time::Instant::now();
        assert!(wait_child(&mut child, Some(&halt)).unwrap().success());
        assert!(started.elapsed() >= Duration::from_millis(800), "frozen while held");
        t.join().unwrap();
    }
}
