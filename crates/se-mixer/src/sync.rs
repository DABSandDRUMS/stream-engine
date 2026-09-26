//! Two-way sync between the engine's resolved state and the console (pure logic, no IO).
//!
//! * **Console → engine:** every value the console reports is published as the address's
//!   base (readback). A report matching one of our recent sends is an *echo* (the console
//!   echoes our own changes: `PV` for mutes, `MS fdrs` for faders); anything else is an
//!   *external* change (the console, UC Surface, another client).
//! * **Engine → console:** when an address's resolved value changes and differs from what
//!   the console has, it is queued and sent latest-value-wins, at most once per
//!   `min_interval` per control (≤ 50 Hz by default).
//! * **No feedback loops:** resolved changes that merely reflect our own readback publishes
//!   (possibly stale ones: the console moved on while the core caught up) are never sent
//!   back, and echoes never count as external changes.

use crate::map::Control;
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const HISTORY: usize = 32;

#[derive(Debug, Clone, Copy)]
pub struct SyncConfig {
    pub min_interval: Duration,
    /// How long a send waits for its echo, and how long readback values are remembered.
    pub echo_window: Duration,
}

impl Default for SyncConfig {
    fn default() -> Self {
        SyncConfig { min_interval: Duration::from_millis(20), echo_window: Duration::from_secs(2) }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Remote {
    /// Same as what we already know.
    Unchanged,
    /// The console confirming one of our sends.
    Echo,
    /// Changed by someone else.
    External,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outgoing {
    pub id: usize,
    pub path: String,
    pub addr: String,
    pub value: f64,
}

#[derive(Debug)]
pub struct Tracked {
    pub control: Control,
    pub addr: String,
    pub path: String,
    pub writable: bool,
    /// Last value the console reported.
    mirror: Option<f64>,
    /// Our sends awaiting their echo, oldest first.
    sent: VecDeque<(f64, Instant)>,
    /// Readback values we published as base, oldest first (cleared once the core caught up).
    readback: VecDeque<(f64, Instant)>,
    /// Last value published as base (readback or send).
    base: Option<f64>,
    pending: Option<f64>,
    last_sent: Option<Instant>,
}

impl Tracked {
    fn prune(&mut self, now: Instant, window: Duration) -> usize {
        let mut lost = 0;
        while self.sent.front().is_some_and(|(_, t)| now.duration_since(*t) > window) {
            self.sent.pop_front();
            lost += 1;
        }
        while self.readback.front().is_some_and(|(_, t)| now.duration_since(*t) > window) {
            self.readback.pop_front();
        }
        lost
    }
}

#[derive(Debug, Default)]
pub struct SyncEngine {
    items: Vec<Tracked>,
    by_addr: HashMap<String, usize>,
    by_path: HashMap<String, usize>,
    cfg: SyncConfig,
    /// Sends the console never confirmed (since start).
    pub unconfirmed: u64,
}

impl SyncEngine {
    pub fn new(cfg: SyncConfig) -> SyncEngine {
        SyncEngine { cfg, ..Default::default() }
    }

    pub fn set_config(&mut self, cfg: SyncConfig) {
        self.cfg = cfg;
    }

    pub fn clear(&mut self) {
        self.items.clear();
        self.by_addr.clear();
        self.by_path.clear();
    }

    /// Track a control with the console's current value (numeric controls only).
    pub fn track(&mut self, control: Control, addr: String, writable: bool, value: Option<f64>) -> usize {
        let path = control.path();
        let id = self.items.len();
        self.by_addr.insert(addr.clone(), id);
        self.by_path.insert(path.clone(), id);
        self.items.push(Tracked {
            control,
            addr,
            path,
            writable,
            mirror: value,
            sent: VecDeque::new(),
            readback: VecDeque::new(),
            base: value,
            pending: None,
            last_sent: None,
        });
        id
    }

    pub fn get(&self, id: usize) -> &Tracked {
        &self.items[id]
    }
    pub fn by_addr(&self, addr: &str) -> Option<usize> {
        self.by_addr.get(addr).copied()
    }
    pub fn by_path(&self, path: &str) -> Option<usize> {
        self.by_path.get(path).copied()
    }
    pub fn mirror(&self, id: usize) -> Option<f64> {
        self.items[id].mirror
    }
    pub fn len(&self) -> usize {
        self.items.len()
    }
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }
    pub fn iter(&self) -> impl Iterator<Item = (usize, &Tracked)> {
        self.items.iter().enumerate()
    }

    /// The console reported `v` for `path`. For `Echo`/`External` the caller publishes the
    /// value as base when [`Self::needs_publish`] says so, then calls [`Self::note_readback`].
    pub fn remote(&mut self, path: &str, v: f64, now: Instant) -> Option<(usize, Remote)> {
        let id = *self.by_path.get(path)?;
        let window = self.cfg.echo_window;
        let t = &mut self.items[id];
        self.unconfirmed += t.prune(now, window) as u64;
        let ctl = &t.control;
        if t.mirror.is_some_and(|m| ctl.same(m, v)) {
            return Some((id, Remote::Unchanged));
        }
        t.mirror = Some(v);
        if let Some(k) = t.sent.iter().position(|(s, _)| ctl.same(*s, v)) {
            t.sent.drain(..=k);
            return Some((id, Remote::Echo));
        }
        // someone else moved it: our queued value (not yet sent) is obsolete
        t.pending = None;
        Some((id, Remote::External))
    }

    /// Whether `v` differs from the base we last published for `id`.
    pub fn needs_publish(&self, id: usize, v: f64) -> bool {
        let t = &self.items[id];
        !t.base.is_some_and(|b| t.control.same(b, v))
    }

    /// Record that `v` was published as base because the console reported it.
    pub fn note_readback(&mut self, id: usize, v: f64, now: Instant) {
        let t = &mut self.items[id];
        t.base = Some(v);
        if t.readback.len() >= HISTORY {
            t.readback.pop_front();
        }
        t.readback.push_back((v, now));
    }

    /// The engine's resolved value for `addr` changed to `v`. Returns true when a send was
    /// queued.
    pub fn resolved(&mut self, addr: &str, v: f64, now: Instant) -> bool {
        let Some(&id) = self.by_addr.get(addr) else { return false };
        let window = self.cfg.echo_window;
        let t = &mut self.items[id];
        if !t.writable || !t.control.settable() {
            return false;
        }
        self.unconfirmed += t.prune(now, window) as u64;
        let ctl = &t.control;
        if t.readback.back().is_some_and(|(r, _)| ctl.same(*r, v)) {
            // the core caught up with our latest readback: nothing to do, older ones are moot
            t.readback.clear();
            t.pending = None;
            return false;
        }
        if t.readback.iter().any(|(r, _)| ctl.same(*r, v)) {
            // a stale readback value (the console moved on): never push it back
            return false;
        }
        let expected = t.sent.back().map(|s| s.0).or(t.mirror);
        if expected.is_some_and(|e| ctl.same(e, v)) {
            t.pending = None;
            return false;
        }
        t.pending = Some(v);
        true
    }

    /// Sends due now (rate limit per control). Records them as awaiting echo; the caller
    /// transmits them and publishes each value as base.
    pub fn due(&mut self, now: Instant) -> Vec<Outgoing> {
        let min = self.cfg.min_interval;
        let mut out = Vec::new();
        for (id, t) in self.items.iter_mut().enumerate() {
            let Some(v) = t.pending else { continue };
            if t.last_sent.is_some_and(|l| now.duration_since(l) < min) {
                continue;
            }
            t.pending = None;
            t.last_sent = Some(now);
            if t.sent.len() >= HISTORY {
                t.sent.pop_front();
            }
            t.sent.push_back((v, now));
            t.base = Some(v);
            out.push(Outgoing { id, path: t.path.clone(), addr: t.addr.clone(), value: v });
        }
        out
    }

    /// When the next rate-limited send becomes due.
    pub fn next_due(&self) -> Option<Instant> {
        let min = self.cfg.min_interval;
        self.items.iter().filter(|t| t.pending.is_some()).map(|t| t.last_sent.map(|l| l + min).unwrap_or_else(Instant::now)).min()
    }

    /// Drop sends that were never echoed; returns how many were lost since the last call.
    pub fn expire(&mut self, now: Instant) -> u64 {
        let window = self.cfg.echo_window;
        let lost: usize = self.items.iter_mut().map(|t| t.prune(now, window)).sum();
        self.unconfirmed += lost as u64;
        lost as u64
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::{Control, Param, Strip};

    const FADER16: &str = "mixer.16r.ch.16.fader";
    const MUTE16: &str = "mixer.16r.ch.16.mute";

    fn engine() -> (SyncEngine, Instant) {
        let mut s = SyncEngine::new(SyncConfig::default());
        s.track(Control::new(Strip::Line(16), Param::Fader), FADER16.into(), true, Some(0.0));
        s.track(Control::new(Strip::Line(16), Param::Mute), MUTE16.into(), true, Some(0.0));
        s.track(Control::new(Strip::Line(1), Param::Fader), "mixer.16r.ch.1.fader".into(), false, Some(0.75));
        (s, Instant::now())
    }

    fn ms(t: Instant, n: u64) -> Instant {
        t + Duration::from_millis(n)
    }

    #[test]
    fn engine_change_is_sent_and_its_echo_is_not_external() {
        let (mut s, t0) = engine();
        assert!(s.resolved(FADER16, 0.5, t0));
        let out = s.due(t0);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].path, "line/ch16/volume");
        assert_eq!(out[0].value, 0.5);
        // the console echoes a quantized position in the fdrs packet
        let (id, r) = s.remote("line/ch16/volume", 32768.0 / 65535.0, ms(t0, 15)).unwrap();
        assert_eq!(r, Remote::Echo);
        assert!(!s.needs_publish(id, 32768.0 / 65535.0), "echo equals the base we published at send time");
        // the same report again (fdrs carries every fader) is unchanged
        assert_eq!(s.remote("line/ch16/volume", 0.5, ms(t0, 30)).unwrap().1, Remote::Unchanged);
    }

    #[test]
    fn rate_limit_is_latest_value_wins() {
        let (mut s, t0) = engine();
        s.resolved(FADER16, 0.1, t0);
        assert_eq!(s.due(t0).len(), 1);
        // three changes inside one 20 ms window collapse into the last one
        s.resolved(FADER16, 0.2, ms(t0, 5));
        s.resolved(FADER16, 0.3, ms(t0, 10));
        s.resolved(FADER16, 0.4, ms(t0, 15));
        assert!(s.due(ms(t0, 15)).is_empty());
        assert_eq!(s.next_due(), Some(ms(t0, 20)));
        let out = s.due(ms(t0, 20));
        assert_eq!(out.iter().map(|o| o.value).collect::<Vec<_>>(), vec![0.4]);
        // echoes of both sends are recognized even when they arrive late and in order
        assert_eq!(s.remote("line/ch16/volume", 0.1, ms(t0, 30)).unwrap().1, Remote::Echo);
        assert_eq!(s.remote("line/ch16/volume", 0.4, ms(t0, 35)).unwrap().1, Remote::Echo);
    }

    #[test]
    fn external_change_publishes_and_is_never_pushed_back() {
        let (mut s, t0) = engine();
        // UC Surface drags the fader: 0.60, 0.62, 0.65
        for (i, v) in [0.60, 0.62, 0.65].into_iter().enumerate() {
            let now = ms(t0, i as u64 * 10);
            let (id, r) = s.remote("line/ch16/volume", v, now).unwrap();
            assert_eq!(r, Remote::External);
            assert!(s.needs_publish(id, v));
            s.note_readback(id, v, now);
        }
        // the core reports resolved changes for older publishes after the console moved on
        assert!(!s.resolved(FADER16, 0.60, ms(t0, 40)));
        assert!(!s.resolved(FADER16, 0.62, ms(t0, 45)));
        assert!(!s.resolved(FADER16, 0.65, ms(t0, 50)));
        assert!(s.due(ms(t0, 60)).is_empty());
        // once caught up, an engine command equal to an old readback value is honoured
        assert!(s.resolved(FADER16, 0.60, ms(t0, 70)));
        assert_eq!(s.due(ms(t0, 70))[0].value, 0.60);
    }

    #[test]
    fn external_change_cancels_a_queued_send() {
        let (mut s, t0) = engine();
        s.resolved(FADER16, 0.3, t0);
        s.due(t0);
        s.resolved(FADER16, 0.35, ms(t0, 5)); // queued behind the rate limit
        assert_eq!(s.remote("line/ch16/volume", 0.9, ms(t0, 10)).unwrap().1, Remote::External);
        assert!(s.due(ms(t0, 30)).is_empty(), "the console operator wins over a queued value");
    }

    #[test]
    fn engine_value_equal_to_console_is_not_sent() {
        let (mut s, t0) = engine();
        assert!(!s.resolved(FADER16, 0.0, t0));
        assert!(!s.resolved(MUTE16, 0.0, t0));
        assert!(s.resolved(MUTE16, 1.0, t0));
        assert_eq!(s.due(t0)[0].path, "line/ch16/mute");
        assert_eq!(s.remote("line/ch16/mute", 1.0, ms(t0, 5)).unwrap().1, Remote::Echo);
    }

    #[test]
    fn readonly_controls_are_never_sent() {
        let (mut s, t0) = engine();
        assert!(!s.resolved("mixer.16r.ch.1.fader", 0.2, t0));
        assert!(s.due(t0).is_empty());
        assert!(!s.resolved("mixer.16r.ch.99.fader", 0.2, t0));
    }

    #[test]
    fn unconfirmed_sends_expire_and_are_counted() {
        let (mut s, t0) = engine();
        s.resolved(FADER16, 0.5, t0);
        s.due(t0);
        assert_eq!(s.expire(ms(t0, 2500)), 1);
        assert_eq!(s.unconfirmed, 1);
        // a late report of that value is then treated as a console change, not an echo
        assert_eq!(s.remote("line/ch16/volume", 0.5, ms(t0, 2600)).unwrap().1, Remote::External);
    }
}
