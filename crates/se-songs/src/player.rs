//! Player control for `web/player.html` (§13.3): two YouTube players ("slots" a/b) so the next
//! song preloads while the current one plays.
//!
//! Engine → page: the desired state, published at `song.player` (and returned from the page's
//! `hello` report). Per slot: `{entry, id, cmd: stop|cue|play|pause, start, seek, seek_t, auto}`.
//! `seek` is a counter: the page seeks to `seek_t` whenever it grows. `auto` on the cued slot
//! lets the page start it the instant the active slot ends (gapless) without a round trip.
//!
//! Page → engine: reports through the named query `song.player`:
//! `{ev: hello|state|progress|ended|error|heartbeat, page, slot, entry, id, state, t, dur, stalled, code}`.

use se_proto::Value;
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    A,
    B,
}

impl Slot {
    fn idx(self) -> usize {
        match self {
            Slot::A => 0,
            Slot::B => 1,
        }
    }
    fn other(self) -> Slot {
        match self {
            Slot::A => Slot::B,
            Slot::B => Slot::A,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Slot::A => "a",
            Slot::B => "b",
        }
    }
    fn parse(s: &str) -> Option<Slot> {
        match s {
            "a" => Some(Slot::A),
            "b" => Some(Slot::B),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Cmd {
    #[default]
    Stop,
    /// Loaded, buffered, paused and muted (the preloaded next song).
    Cue,
    Play,
    Pause,
}

impl Cmd {
    fn as_str(self) -> &'static str {
        match self {
            Cmd::Stop => "stop",
            Cmd::Cue => "cue",
            Cmd::Play => "play",
            Cmd::Pause => "pause",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct Desired {
    entry: i64,
    id: String,
    cmd: Cmd,
    start: f64,
    seek: u64,
    seek_t: f64,
    auto: bool,
}

#[derive(Clone, Debug, Default)]
struct Observed {
    entry: i64,
    /// YouTube player state: `unstarted|buffering|playing|paused|cued|ended`.
    state: String,
    t: f64,
    dur: f64,
    /// Reported playing but media time isn't advancing (an ad, or a stall).
    stalled: bool,
    at: Option<Instant>,
}

/// Something the queue must react to.
#[derive(Clone, Debug, PartialEq)]
pub enum Outcome {
    Ended(i64),
    /// `(entry, YouTube error code)` for the active slot.
    Error(i64, i64),
    /// The preloaded (next) entry can't be played.
    PreloadError(i64, i64),
}

/// Reports older than this mean the page is gone.
const PAGE_TIMEOUT: Duration = Duration::from_secs(6);
/// Never extrapolate position further than this past the last report.
const MAX_EXTRAPOLATE: f64 = 2.0;

#[derive(Debug)]
pub struct Player {
    active: Slot,
    rev: u64,
    slots: [Desired; 2],
    seen: [Observed; 2],
    xfade_ms: u32,
    last_report: Option<Instant>,
    /// Entries whose end/error was already handled (reports may repeat).
    finished: Vec<i64>,
    /// Last entry for which playback start was announced.
    announced: i64,
}

impl Player {
    pub fn new(xfade_ms: u32) -> Player {
        Player { active: Slot::A, rev: 1, slots: Default::default(), seen: Default::default(), xfade_ms, last_report: None, finished: Vec::new(), announced: 0 }
    }

    fn bump(&mut self) {
        self.rev += 1;
    }

    pub fn set_crossfade(&mut self, ms: u32) {
        if self.xfade_ms != ms {
            self.xfade_ms = ms;
            self.bump();
        }
    }

    /// Current entry in the active slot (0 = none).
    pub fn current(&self) -> i64 {
        self.slots[self.active.idx()].entry
    }

    /// Make `entry` the playing (or paused) song. Uses the preloaded slot when it holds it.
    pub fn play(&mut self, entry: i64, id: &str, start: f64, paused: bool) {
        let cmd = if paused { Cmd::Pause } else { Cmd::Play };
        let other = self.active.other();
        if self.slots[other.idx()].entry == entry {
            self.slots[self.active.idx()] = Desired::default();
            self.active = other;
            self.slots[other.idx()].cmd = cmd;
            self.slots[other.idx()].auto = false;
        } else {
            self.slots[self.active.idx()] = Desired { entry, id: id.to_string(), cmd, start: start.max(0.0), ..Default::default() };
            self.seen[self.active.idx()] = Observed::default();
        }
        self.finished.retain(|e| *e != entry);
        self.bump();
    }

    /// Preload the next entry into the idle slot (or clear it).
    pub fn preload(&mut self, next: Option<(i64, &str)>, auto: bool) {
        let s = self.active.other().idx();
        let want = match next {
            Some((entry, id)) if entry != self.current() => {
                let keep = self.slots[s].entry == entry;
                Desired { entry, id: id.to_string(), cmd: Cmd::Cue, auto, ..if keep { self.slots[s].clone() } else { Desired::default() } }
            }
            _ => Desired::default(),
        };
        if self.slots[s] != want {
            if self.slots[s].entry != want.entry {
                self.seen[s] = Observed::default();
            }
            self.slots[s] = want;
            self.bump();
        }
    }

    /// Stop everything (nothing to play).
    pub fn stop(&mut self) {
        if self.slots.iter().any(|d| *d != Desired::default()) {
            self.slots = Default::default();
            self.seen = Default::default();
            self.bump();
        }
    }

    pub fn set_paused(&mut self, paused: bool) {
        let a = self.active.idx();
        if self.slots[a].entry != 0 {
            let cmd = if paused { Cmd::Pause } else { Cmd::Play };
            if self.slots[a].cmd != cmd {
                self.slots[a].cmd = cmd;
                self.bump();
            }
        }
        let o = self.active.other().idx();
        if self.slots[o].entry != 0 && self.slots[o].auto == paused {
            self.slots[o].auto = !paused;
            self.bump();
        }
    }

    pub fn seek(&mut self, t: f64, now: Instant) {
        let a = self.active.idx();
        if self.slots[a].entry == 0 {
            return;
        }
        let t = t.max(0.0);
        self.slots[a].seek += 1;
        self.slots[a].seek_t = t;
        if self.seen[a].entry == self.slots[a].entry {
            self.seen[a].t = t;
            self.seen[a].at = Some(now);
        }
        self.bump();
    }

    /// The page (re)connected: its players are fresh, so resume the active slot at the
    /// current position. Returns the desired state for the page to apply.
    pub fn hello(&mut self, now: Instant) -> Value {
        let a = self.active.idx();
        if self.slots[a].entry != 0 {
            self.slots[a].start = self.position(now);
            self.slots[a].seek = 0;
        }
        self.seen = Default::default();
        self.last_report = Some(now);
        self.bump();
        self.desired()
    }

    /// Apply a page report; returns what the queue must handle.
    pub fn report(&mut self, r: &Value, now: Instant) -> Vec<Outcome> {
        self.last_report = Some(now);
        let ev = r.get_path("ev").and_then(Value::as_str).unwrap_or("");
        let Some(slot) = r.get_path("slot").and_then(Value::as_str).and_then(Slot::parse) else { return Vec::new() };
        let entry = r.get_path("entry").and_then(Value::as_i64).unwrap_or(0);
        let s = slot.idx();
        if entry == 0 || self.slots[s].entry != entry {
            return Vec::new(); // stale report for something no longer in that slot
        }
        let active = slot == self.active;
        match ev {
            "ended" => {
                if active && !self.finished.contains(&entry) {
                    self.finish(entry);
                    return vec![Outcome::Ended(entry)];
                }
            }
            "error" => {
                let code = r.get_path("code").and_then(Value::as_i64).unwrap_or(0);
                if !self.finished.contains(&entry) {
                    self.finish(entry);
                    return vec![if active { Outcome::Error(entry, code) } else { Outcome::PreloadError(entry, code) }];
                }
            }
            "state" | "progress" => {
                let o = &mut self.seen[s];
                if o.entry != entry {
                    *o = Observed { entry, ..Default::default() };
                }
                if let Some(st) = r.get_path("state").and_then(Value::as_str) {
                    o.state = st.to_string();
                }
                if let Some(t) = r.get_path("t").and_then(Value::as_f64).filter(|t| t.is_finite() && *t >= 0.0) {
                    o.t = t;
                }
                if let Some(d) = r.get_path("dur").and_then(Value::as_f64).filter(|d| d.is_finite() && *d > 0.0) {
                    o.dur = d;
                }
                o.stalled = r.get_path("stalled").is_some_and(Value::truthy);
                o.at = Some(now);
                if active && o.state == "ended" && !self.finished.contains(&entry) {
                    self.finish(entry);
                    return vec![Outcome::Ended(entry)];
                }
            }
            _ => {}
        }
        Vec::new()
    }

    fn finish(&mut self, entry: i64) {
        self.finished.push(entry);
        if self.finished.len() > 32 {
            self.finished.remove(0);
        }
    }

    /// True once per entry when the current song is actually playing (not an ad/stall).
    pub fn take_started(&mut self) -> Option<i64> {
        let a = self.active.idx();
        let (d, o) = (&self.slots[a], &self.seen[a]);
        if d.entry != 0 && o.entry == d.entry && o.state == "playing" && !o.stalled && self.announced != d.entry {
            self.announced = d.entry;
            return Some(d.entry);
        }
        None
    }

    /// Seconds into the current song, extrapolated from the last report while playing.
    pub fn position(&self, now: Instant) -> f64 {
        let a = self.active.idx();
        let (d, o) = (&self.slots[a], &self.seen[a]);
        if d.entry == 0 {
            return 0.0;
        }
        if o.entry != d.entry || o.at.is_none() {
            return d.start;
        }
        let mut t = o.t;
        if o.state == "playing" && !o.stalled && d.cmd == Cmd::Play {
            let dt = now.saturating_duration_since(o.at.unwrap_or(now)).as_secs_f64();
            t += dt.min(MAX_EXTRAPOLATE);
        }
        if o.dur > 0.0 { t.min(o.dur) } else { t }
    }

    /// Duration reported by the player (0 until known).
    pub fn duration(&self) -> f64 {
        let a = self.active.idx();
        if self.seen[a].entry == self.slots[a].entry { self.seen[a].dur } else { 0.0 }
    }

    /// `idle|loading|buffering|playing|paused|ad`.
    pub fn state(&self) -> &'static str {
        let a = self.active.idx();
        let (d, o) = (&self.slots[a], &self.seen[a]);
        if d.entry == 0 {
            return "idle";
        }
        if o.entry != d.entry {
            return "loading";
        }
        match o.state.as_str() {
            "playing" if o.stalled => "ad",
            "playing" => "playing",
            "paused" => "paused",
            "buffering" if o.t > 0.0 => "buffering",
            _ => "loading",
        }
    }

    /// The page reported recently.
    pub fn connected(&self, now: Instant) -> bool {
        self.last_report.is_some_and(|t| now.saturating_duration_since(t) < PAGE_TIMEOUT)
    }

    pub fn rev(&self) -> u64 {
        self.rev
    }

    pub fn desired(&self) -> Value {
        let slot = |d: &Desired| {
            Value::map()
                .with("entry", d.entry)
                .with("id", d.id.clone())
                .with("cmd", d.cmd.as_str())
                .with("start", (d.start * 1000.0).round() / 1000.0)
                .with("seek", d.seek as i64)
                .with("seek_t", (d.seek_t * 1000.0).round() / 1000.0)
                .with("auto", d.auto)
        };
        Value::map()
            .with("rev", self.rev as i64)
            .with("active", self.active.as_str())
            .with("xfade_ms", self.xfade_ms as i64)
            .with("a", slot(&self.slots[0]))
            .with("b", slot(&self.slots[1]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(ev: &str, slot: &str, entry: i64) -> Value {
        Value::map().with("ev", ev).with("slot", slot).with("entry", entry)
    }

    fn progress(slot: &str, entry: i64, state: &str, t: f64) -> Value {
        rep("progress", slot, entry).with("state", state).with("t", t).with("dur", 200.0)
    }

    #[test]
    fn gapless_handover_uses_the_preloaded_slot() {
        let now = Instant::now();
        let mut p = Player::new(400);
        p.play(1, "aaaaaaaaaaa", 0.0, false);
        p.preload(Some((2, "bbbbbbbbbbb")), true);
        let d = p.desired();
        assert_eq!(d.get_path("active").unwrap().as_str(), Some("a"));
        assert_eq!(d.get_path("b.cmd").unwrap().as_str(), Some("cue"));
        assert_eq!(d.get_path("b.auto"), Some(&Value::Bool(true)));
        assert_eq!(p.report(&progress("a", 1, "playing", 10.0), now), vec![]);
        assert_eq!(p.take_started(), Some(1));
        assert_eq!(p.take_started(), None, "announced once");
        // the page auto-starts b and reports a's end (twice: must be handled once)
        assert_eq!(p.report(&rep("ended", "a", 1), now), vec![Outcome::Ended(1)]);
        assert_eq!(p.report(&rep("ended", "a", 1), now), vec![]);
        p.report(&progress("b", 2, "playing", 0.2), now);
        p.play(2, "bbbbbbbbbbb", 0.0, false);
        let d = p.desired();
        assert_eq!(d.get_path("active").unwrap().as_str(), Some("b"));
        assert_eq!(d.get_path("b.cmd").unwrap().as_str(), Some("play"));
        assert_eq!(d.get_path("a.cmd").unwrap().as_str(), Some("stop"));
        assert_eq!(p.take_started(), Some(2), "already playing when it became current");
    }

    #[test]
    fn position_extrapolates_only_while_playing() {
        let t0 = Instant::now();
        let mut p = Player::new(0);
        p.play(1, "aaaaaaaaaaa", 30.0, false);
        assert_eq!(p.position(t0), 30.0, "start before the first report");
        assert_eq!(p.state(), "loading");
        p.report(&progress("a", 1, "playing", 42.0), t0);
        assert!((p.position(t0 + Duration::from_millis(500)) - 42.5).abs() < 1e-6);
        assert!((p.position(t0 + Duration::from_secs(60)) - 44.0).abs() < 1e-6, "capped when reports stop");
        p.report(&progress("a", 1, "playing", 42.0).with("stalled", true), t0);
        assert_eq!(p.state(), "ad");
        assert_eq!(p.position(t0 + Duration::from_secs(1)), 42.0, "ads don't advance the song");
        p.report(&progress("a", 1, "paused", 50.0), t0);
        assert_eq!(p.position(t0 + Duration::from_secs(1)), 50.0);
        p.seek(120.0, t0);
        assert_eq!(p.position(t0), 120.0);
        assert_eq!(p.desired().get_path("a.seek").and_then(Value::as_i64), Some(1));
        assert!(p.connected(t0) && !p.connected(t0 + Duration::from_secs(7)));
    }

    #[test]
    fn errors_and_stale_reports() {
        let now = Instant::now();
        let mut p = Player::new(0);
        p.play(5, "aaaaaaaaaaa", 0.0, false);
        p.preload(Some((6, "bbbbbbbbbbb")), true);
        assert_eq!(p.report(&rep("error", "b", 6).with("code", 150), now), vec![Outcome::PreloadError(6, 150)]);
        assert_eq!(p.report(&rep("error", "a", 4).with("code", 101), now), vec![], "stale entry");
        assert_eq!(p.report(&rep("error", "a", 5).with("code", 101), now), vec![Outcome::Error(5, 101)]);
    }

    #[test]
    fn hello_resumes_at_current_position_and_pause_controls_auto() {
        let now = Instant::now();
        let mut p = Player::new(0);
        p.play(1, "aaaaaaaaaaa", 0.0, false);
        p.preload(Some((2, "bbbbbbbbbbb")), true);
        p.report(&progress("a", 1, "playing", 73.0), now);
        let d = p.hello(now);
        assert_eq!(d.get_path("a.start").and_then(Value::as_f64), Some(73.0));
        let rev = p.rev();
        p.set_paused(true);
        assert!(p.rev() > rev);
        let d = p.desired();
        assert_eq!(d.get_path("a.cmd").unwrap().as_str(), Some("pause"));
        assert_eq!(d.get_path("b.auto"), Some(&Value::Bool(false)));
        p.preload(None, false);
        assert_eq!(p.desired().get_path("b.entry").and_then(Value::as_i64), Some(0));
        p.stop();
        assert_eq!(p.state(), "idle");
    }
}
