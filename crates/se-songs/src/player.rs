//! Player control for `web/player.html` (§13.3): two YouTube players ("slots" a/b) so the next
//! song preloads while the current one plays.
//!
//! Engine → page: the desired state, published at `song.player` (and returned from the page's
//! `hello` report). Per slot: `{entry, id, cmd: stop|cue|play|pause, start, seek, seek_t, auto}`.
//! `seek` is a counter: the page seeks to `seek_t` whenever it grows. `auto` on the cued slot
//! lets the page hand over after the active slot ends, but fresh account verification comes first.
//!
//! Page → engine: reports through the named query `song.player`:
//! `{ev: hello|account|state|progress|ended|error|heartbeat, page, slot, entry, id, state, t, dur, stalled, code}`.

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
/// Channel proof must be renewed even when heartbeat reports continue.
const ACCOUNT_TIMEOUT: Duration = Duration::from_secs(60);
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
    required_channel: String,
    required_delegate: String,
    page: String,
    channel: String,
    delegate: String,
    account_at: Option<Instant>,
    account_error: String,
    /// Entries whose end/error was already handled (reports may repeat).
    finished: Vec<i64>,
    /// Last entry for which playback start was announced.
    announced: i64,
}

impl Player {
    pub fn new(xfade_ms: u32) -> Player {
        Player {
            active: Slot::A,
            rev: 1,
            slots: Default::default(),
            seen: Default::default(),
            xfade_ms,
            last_report: None,
            required_channel: String::new(),
            required_delegate: String::new(),
            page: String::new(),
            channel: String::new(),
            delegate: String::new(),
            account_at: None,
            account_error: "Configure [songs] youtube_channel with the approved exact UC channel ID; playback and requests are locked".into(),
            finished: Vec::new(),
            announced: 0,
        }
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

    pub fn set_required_account(&mut self, channel: &str, delegate: &str, now: Instant) {
        if self.required_channel != channel || self.required_delegate != delegate {
            self.required_channel = channel.to_string();
            self.required_delegate = delegate.to_string();
            self.invalidate_account("Approved channel or delegate changed; verify the embedded YouTube account again", now);
            self.bump();
        }
    }

    /// Freeze the logical queue position but send only blank stopped slots.
    fn invalidate_account(&mut self, error: &str, now: Instant) {
        let a = self.active.idx();
        self.slots[a].start = self.position(now);
        self.slots[a].seek = 0;
        self.seen = Default::default();
        let changed = self.account_at.take().is_some() || self.account_error != error;
        if self.account_error != error {
            self.account_error.clear();
            self.account_error.push_str(error);
        }
        if changed {
            self.bump();
        }
    }

    pub fn account_verified(&self, now: Instant) -> bool {
        crate::settings::valid_channel_id(&self.required_channel)
            && crate::settings::valid_delegate(&self.required_delegate)
            && self.delegate == self.required_delegate
            && !self.page.is_empty()
            && self.channel == self.required_channel
            && self.connected(now)
            && self.account_at.is_some_and(|at| now.saturating_duration_since(at) < ACCOUNT_TIMEOUT)
    }

    pub fn expire_account(&mut self, now: Instant) {
        if self.account_at.is_some() && !self.account_verified(now) {
            self.invalidate_account("Embedded YouTube account proof expired or player disconnected; verify again before resuming", now);
        }
    }

    pub fn account_error(&self) -> &str {
        if self.required_channel.is_empty() {
            "Configure [songs] youtube_channel with the approved exact UC channel ID; playback and requests are locked"
        } else if self.required_delegate.is_empty() {
            "Configure [songs] youtube_delegate for the approved Brand channel; playback and requests are locked"
        } else if self.account_error.is_empty() {
            "Embedded YouTube channel proof expired; verify the approved account before resuming"
        } else {
            &self.account_error
        }
    }

    pub fn account(&self, now: Instant) -> Value {
        let verified = self.account_verified(now);
        Value::map()
            .with("required_channel", self.required_channel.clone())
            .with("required_delegate", self.required_delegate.clone())
            .with("page", self.page.clone())
            .with("verified", verified)
            .with("channel", self.channel.clone())
            .with("delegate", self.delegate.clone())
            .with("error", if verified { "" } else { self.account_error() })
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

    /// A fresh page never inherits the previous page's channel proof.
    pub fn hello(&mut self, page: &str, now: Instant) -> Value {
        let a = self.active.idx();
        if self.slots[a].entry != 0 {
            self.slots[a].start = self.position(now);
            self.slots[a].seek = 0;
        }
        self.invalidate_account("Embedded YouTube channel is unverified; check the approved account before loading any video", now);
        self.page = page.to_string();
        self.channel.clear();
        self.delegate.clear();
        self.seen = Default::default();
        self.last_report = Some(now);
        self.bump();
        self.desired()
    }

    /// Apply a page report; returns what the queue must handle.
    pub fn report(&mut self, r: &Value, now: Instant) -> Vec<Outcome> {
        self.expire_account(now);
        let page = r.get_path("page").and_then(Value::as_str).unwrap_or("");
        if page.is_empty() || page != self.page {
            self.invalidate_account("Player page changed or report has no page identity; reconnect and verify the approved channel", now);
            return Vec::new();
        }
        let ev = r.get_path("ev").and_then(Value::as_str).unwrap_or("");
        // A heartbeat after a disconnect cannot resurrect saved channel proof.
        self.last_report = Some(now);
        if ev == "account" {
            self.channel.clear();
            self.channel.push_str(r.get_path("channel").and_then(Value::as_str).unwrap_or(""));
            self.delegate.clear();
            self.delegate.push_str(r.get_path("delegate").and_then(Value::as_str).unwrap_or(""));
            if r.get_path("verified") == Some(&Value::Bool(true))
                && crate::settings::valid_channel_id(&self.required_channel)
                && self.channel == self.required_channel
                && crate::settings::valid_delegate(&self.required_delegate)
                && self.delegate == self.required_delegate
            {
                let was_verified = self.account_at.is_some();
                self.account_at = Some(now);
                self.account_error.clear();
                if !was_verified {
                    self.bump();
                }
            } else {
                let error = if (!self.channel.is_empty() && self.channel != self.required_channel)
                    || (!self.delegate.is_empty() && self.delegate != self.required_delegate)
                {
                    "Embedded YouTube channel or delegate does not match [songs] youtube_channel/youtube_delegate; select the approved Brand channel in engine login and verify again"
                } else {
                    "Embedded YouTube account could not be verified; sign in to the approved Brand channel in engine login and verify again"
                };
                self.invalidate_account(error, now);
            }
            return Vec::new();
        }
        if !self.account_verified(now) {
            return Vec::new();
        }
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
        if !self.account_verified(Instant::now()) {
            return None;
        }
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
        if self.account_verified(now) && o.state == "playing" && !o.stalled && d.cmd == Cmd::Play {
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
        if !self.account_verified(Instant::now()) {
            return "account_hold";
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
        let now = Instant::now();
        let verified = self.account_verified(now);
        let stopped = Desired::default();
        let slot = |d: &Desired| {
            let d = if verified { d } else { &stopped };
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
            .with("required_channel", self.required_channel.clone())
            .with("required_delegate", self.required_delegate.clone())
            .with("account", self.account(now))
            .with("a", slot(&self.slots[0]))
            .with("b", slot(&self.slots[1]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rep(ev: &str, slot: &str, entry: i64) -> Value {
        Value::map().with("ev", ev).with("page", "test").with("slot", slot).with("entry", entry)
    }

    fn progress(slot: &str, entry: i64, state: &str, t: f64) -> Value {
        rep("progress", slot, entry).with("state", state).with("t", t).with("dur", 200.0)
    }

    const CHANNEL: &str = "UCaaaaaaaaaaaaaaaaaaaaaa";
    const DELEGATE: &str = "123456789";

    fn account(channel: &str, verified: bool) -> Value {
        Value::map().with("ev", "account").with("page", "test").with("verified", verified).with("channel", channel).with("delegate", DELEGATE).with("error", "")
    }

    fn verified_player(ms: u32, now: Instant) -> Player {
        let mut p = Player::new(ms);
        p.set_required_account(CHANNEL, DELEGATE, now);
        p.hello("test", now);
        p.report(&account(CHANNEL, true), now);
        p
    }

    #[test]
    fn gapless_handover_uses_the_preloaded_slot() {
        let now = Instant::now();
        let mut p = verified_player(400, now);
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
        let mut p = verified_player(0, t0);
        p.play(1, "aaaaaaaaaaa", 30.0, false);
        assert_eq!(p.position(t0), 30.0, "start before the first report");
        assert_eq!(p.state(), "loading");
        p.report(&progress("a", 1, "playing", 42.0), t0);
        assert!((p.position(t0 + Duration::from_millis(500)) - 42.5).abs() < 1e-6);
        assert_eq!(p.position(t0 + Duration::from_secs(60)), 42.0, "expired proof never extrapolates playback");
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
        let mut p = verified_player(0, now);
        p.play(5, "aaaaaaaaaaa", 0.0, false);
        p.preload(Some((6, "bbbbbbbbbbb")), true);
        assert_eq!(p.report(&rep("error", "b", 6).with("code", 150), now), vec![Outcome::PreloadError(6, 150)]);
        assert_eq!(p.report(&rep("error", "a", 4).with("code", 101), now), vec![], "stale entry");
        assert_eq!(p.report(&rep("error", "a", 5).with("code", 101), now), vec![Outcome::Error(5, 101)]);
    }

    #[test]
    fn hello_resumes_at_current_position_and_pause_controls_auto() {
        let now = Instant::now();
        let mut p = verified_player(0, now);
        p.play(1, "aaaaaaaaaaa", 0.0, false);
        p.preload(Some((2, "bbbbbbbbbbb")), true);
        p.report(&progress("a", 1, "playing", 73.0), now);
        let d = p.hello("test", now);
        assert_eq!(d.get_path("a.id").and_then(Value::as_str), Some(""), "reconnect must reverify before loading");
        p.report(&account(CHANNEL, true), now);
        let d = p.desired();
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

    fn assert_locked(p: &Player) {
        let d = p.desired();
        for slot in ["a", "b"] {
            assert_eq!(d.get_path(&format!("{slot}.id")).and_then(Value::as_str), Some(""));
            assert_eq!(d.get_path(&format!("{slot}.cmd")).and_then(Value::as_str), Some("stop"));
            assert_eq!(d.get_path(&format!("{slot}.auto")), Some(&Value::Bool(false)));
        }
    }

    #[test]
    fn restored_and_preloaded_entries_require_same_page_exact_identity() {
        let now = Instant::now();
        let mut p = Player::new(0);
        p.play(1, "aaaaaaaaaaa", 38.0, true);
        p.preload(Some((2, "bbbbbbbbbbb")), true);
        assert_locked(&p);
        p.hello("test", now);
        p.report(&account(CHANNEL, true), now);
        assert_locked(&p); // no configured identity is never unrestricted
        p.set_required_account(CHANNEL, DELEGATE, now);
        p.report(&account(CHANNEL, true).with("verified", "true"), now);
        assert_locked(&p);
        p.report(&account(CHANNEL, true).with("channel", ""), now);
        assert_locked(&p);
        p.report(&account(CHANNEL, true).with("delegate", ""), now);
        assert_locked(&p);
        p.report(&account("UCbbbbbbbbbbbbbbbbbbbbbb", true), now);
        assert_locked(&p);
        p.report(&account(CHANNEL, true).with("delegate", "987654321"), now);
        assert_locked(&p);
        p.report(&account(CHANNEL, true).with("page", "other"), now);
        assert_locked(&p);
        p.report(&account(CHANNEL, true), now);
        let d = p.desired();
        assert_eq!(d.get_path("a.id").and_then(Value::as_str), Some("aaaaaaaaaaa"));
        assert_eq!(d.get_path("a.start").and_then(Value::as_f64), Some(38.0));
        assert_eq!(d.get_path("a.cmd").and_then(Value::as_str), Some("pause"));
        assert_eq!(d.get_path("b.cmd").and_then(Value::as_str), Some("cue"));
        p.report(&account(CHANNEL, false), now);
        assert_locked(&p);
        assert_eq!(p.current(), 1, "failure must not skip or remove the logical current entry");
        assert!(p.report(&rep("error", "b", 2).with("code", 150), now).is_empty());
        assert!(p.report(&rep("ended", "a", 1), now).is_empty());
        assert_eq!(p.take_started(), None);
        p.report(&account(CHANNEL, true), now);
        assert_eq!(p.desired().get_path("b.id").and_then(Value::as_str), Some("bbbbbbbbbbb"), "upcoming slot is preserved");
        p.report(&Value::map().with("ev", "heartbeat").with("page", "other"), now);
        assert_locked(&p);
        p.report(&account(CHANNEL, true), now);
        p.set_required_account("UCbbbbbbbbbbbbbbbbbbbbbb", DELEGATE, now);
        assert_locked(&p);
        p.report(&account(CHANNEL, true), now);
        assert_locked(&p);
        p.report(&account("UCbbbbbbbbbbbbbbbbbbbbbb", true), now);
        assert_eq!(p.desired().get_path("a.id").and_then(Value::as_str), Some("aaaaaaaaaaa"));
    }

    #[test]
    fn heartbeat_and_account_expiry_cannot_reuse_old_proof() {
        let now = Instant::now();
        let mut p = verified_player(0, now);
        p.play(1, "aaaaaaaaaaa", 0.0, false);
        p.preload(Some((2, "bbbbbbbbbbb")), true);
        p.report(&progress("a", 1, "playing", 12.0), now);
        p.report(&Value::map().with("ev", "heartbeat").with("page", "test"), now + PAGE_TIMEOUT);
        assert_locked(&p);
        assert_eq!(p.position(now + PAGE_TIMEOUT), 12.0);
        p.report(&account(CHANNEL, true), now + PAGE_TIMEOUT);
        p.hello("new-page", now + PAGE_TIMEOUT);
        assert_locked(&p);
        p.report(&account(CHANNEL, true), now + PAGE_TIMEOUT);
        assert_locked(&p);
        p.report(&account(CHANNEL, true).with("page", "new-page"), now + PAGE_TIMEOUT);
        p.set_required_account(CHANNEL, "987654321", now + PAGE_TIMEOUT);
        assert_locked(&p);
        p.report(&account(CHANNEL, true).with("page", "new-page").with("delegate", "987654321"), now + PAGE_TIMEOUT);
        for seconds in (10..=65).step_by(5) {
            p.report(&Value::map().with("ev", "heartbeat").with("page", "new-page"), now + Duration::from_secs(seconds));
        }
        assert!(p.account_verified(now + Duration::from_secs(65)), "heartbeats keep only the page alive");
        p.report(&Value::map().with("ev", "heartbeat").with("page", "new-page"), now + Duration::from_secs(66));
        assert_locked(&p);
        assert_eq!(p.current(), 1);
    }
}
