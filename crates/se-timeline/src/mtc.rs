//! MTC in/out over the Inputs slice's MIDI ports (`se_input::midi`): raw bytes in, decoded
//! positions to the core as replayable `Input::Timecode`; positions from a core timeline out
//! as quarter frames on a port.

use crate::Health;
use crate::follow::Follower;
use se_clock::timecode::mtc::{MtcDecoder, MtcGenerator};
use se_clock::timecode::{FrameRate, ObsThrottle};
use se_core::Input;
use se_hub::{Hub, Snapshot};
use se_proto::Ts;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// Observations per second sent to the core while locked (discontinuities always pass).
const OBS_INTERVAL: Ts = 60_000_000;

/// Decodes one MIDI input into observations for source `key`.
pub struct MtcIn {
    dec: MtcDecoder,
    thr: ObsThrottle,
}

impl Default for MtcIn {
    fn default() -> Self {
        MtcIn { dec: MtcDecoder::new(), thr: ObsThrottle::new(OBS_INTERVAL, 0.02) }
    }
}

impl MtcIn {
    pub fn feed(&mut self, bytes: &[u8], ts: Ts, mut emit: impl FnMut(se_clock::timecode::TcObs)) {
        let thr = &mut self.thr;
        self.dec.feed(bytes, ts, |u| {
            let o = u.obs();
            if thr.pass(&o) {
                emit(o);
            }
        });
    }

    /// The port went quiet: the next observation (a restart or locate) passes at once.
    pub fn quiet(&mut self) {
        self.thr.reset();
    }
}

pub fn spawn_input(hub: Arc<Hub>, health: Arc<Health>, key: String, pattern: String, stop: Arc<AtomicBool>) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name(format!("se-mtc-in-{key}")).spawn(move || {
        let rx = se_input::midi::subscribe_raw(&pattern);
        let mut input = MtcIn::default();
        let hkey = format!("mtc in `{pattern}`");
        let mut last_msg: Option<Ts> = None;
        let mut checked = 0u32;
        while !stop.load(Ordering::Relaxed) {
            match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(m) => {
                    if last_msg.is_none() {
                        health.set(&hkey, "pass", format!("receiving on {}", m.device));
                    }
                    last_msg = Some(m.ts);
                    input.feed(&m.bytes, m.ts, |obs| hub.submit(Input::Timecode { source: key.clone(), obs }));
                }
                Err(crossbeam_channel::RecvTimeoutError::Timeout) => {
                    input.quiet();
                    // port presence check every ~2 s while idle
                    checked += 1;
                    if checked.is_multiple_of(20) || last_msg.is_none() && checked == 1 {
                        let ports: Vec<String> = se_input::midi::ports()
                            .into_iter()
                            .filter(|p| p.input && p.online && (se_proto::address::matches(&pattern, &p.device) || glob_name(&pattern, &p.name)))
                            .map(|p| p.device)
                            .collect();
                        if ports.is_empty() {
                            health.set(&hkey, "warn", "no MIDI input matches".into());
                        } else if last_msg.is_none_or(|t| se_clock::now().saturating_sub(t) > 2_000_000_000) {
                            health.set(&hkey, "pass", format!("listening on {} (no timecode)", ports.join(", ")));
                        }
                    }
                }
                Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
            }
        }
        health.clear(&hkey);
    })
}

/// ALSA names contain spaces and colons: match them with plain `*` globbing, case-insensitive.
pub fn glob_name(pattern: &str, name: &str) -> bool {
    let (p, n) = (pattern.to_ascii_lowercase(), name.to_ascii_lowercase());
    let parts: Vec<&str> = p.split('*').collect();
    if parts.len() == 1 {
        return p == n;
    }
    let mut rest = n.as_str();
    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            let Some(r) = rest.strip_prefix(part) else { return false };
            rest = r;
        } else if i == parts.len() - 1 {
            return rest.ends_with(part);
        } else {
            let Some(at) = rest.find(part) else { return false };
            rest = &rest[at + part.len()..];
        }
    }
    true
}

/// Generates MTC for one timeline.
pub struct MtcOut {
    pub generator: MtcGenerator,
    follower: Follower,
    /// Seconds added to the position (compensates output latency of the receiving device).
    pub offset: f64,
}

impl MtcOut {
    pub fn new(timeline: &str, rate: FrameRate, offset: f64) -> Self {
        MtcOut { generator: MtcGenerator::new(rate), follower: Follower::new(timeline), offset }
    }

    /// Emit everything due at `now`.
    pub fn poll(&mut self, snap: &Snapshot, now: Ts, send: impl FnMut(&[u8])) {
        self.follower.update(snap);
        self.follower.tick(now);
        if !self.follower.active() {
            return;
        }
        let pos = self.follower.position(now) + self.offset;
        self.generator.poll(pos, self.follower.running() && self.follower.speed() > 0.0, send);
    }
}

pub fn spawn_output(
    hub: Arc<Hub>,
    health: Arc<Health>,
    timeline: String,
    port: String,
    rate: FrameRate,
    offset: f64,
    stop: Arc<AtomicBool>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new().name(format!("se-mtc-out-{timeline}")).spawn(move || {
        let hkey = format!("mtc out `{timeline}` → `{port}`");
        let mut mtc = MtcOut::new(&timeline, rate, offset);
        let mut out: Option<se_input::midi::MidiOut> = None;
        let mut retry_at = 0;
        let mut errors = 0u32;
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1));
            let now = se_clock::now();
            if out.is_none() && now >= retry_at {
                match se_input::midi::output(&port) {
                    Ok(o) => {
                        health.set(&hkey, "pass", format!("sending {rate} fps on {}", o.device()));
                        out = Some(o);
                    }
                    Err(e) => {
                        health.set(&hkey, "warn", e);
                        retry_at = now + 2_000_000_000;
                    }
                }
            }
            let snap = hub.snapshot.load();
            match &out {
                Some(o) => mtc.poll(&snap, now, |m| {
                    if let Err(e) = o.send(m) {
                        errors += 1;
                        if errors == 1 || errors.is_multiple_of(1000) {
                            health.set(&hkey, "warn", format!("send failed ({errors}×): {e}"));
                        }
                    }
                }),
                None => mtc.poll(&snap, now, |_| {}),
            }
        }
        health.clear(&hkey);
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::follow::snapshot_for;
    use se_clock::timecode::mtc::parse_full_frame;

    #[test]
    fn names_glob() {
        assert!(glob_name("*24c*", "Studio 24c:Studio 24c MIDI 1"));
        assert!(glob_name("studio 24c:*", "Studio 24c:Studio 24c MIDI 1"));
        assert!(!glob_name("*xtouch*", "Studio 24c:Studio 24c MIDI 1"));
    }

    #[test]
    fn mtc_out_follows_the_timeline_and_decodes_back() {
        let ms = 1_000_000u64;
        let mut out = MtcOut::new("show", FrameRate::Fps30, 0.0);
        let mut input = MtcIn::default();
        let mut obs = Vec::new();
        let mut first = None;
        for step in 0..3000u64 {
            let now = step * ms;
            let snap = snapshot_for("show", step / 8, (step / 8) * 8 * ms, 60.0 + ((step / 8) * 8) as f64 / 1000.0, true);
            out.poll(&snap, now, |m| {
                if first.is_none() {
                    first = parse_full_frame(m);
                }
                input.feed(m, now, |o| obs.push(o));
            });
        }
        assert_eq!(first.unwrap().to_string(), "00:01:00:00", "locates the receiver first");
        assert!(obs.len() > 30, "{}", obs.len());
        for o in &obs {
            let truth = 60.0 + o.ts as f64 / 1e9;
            assert!((o.seconds - truth).abs() < 0.04, "{} vs {truth}", o.seconds);
        }
    }
}
