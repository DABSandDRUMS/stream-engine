//! Health checks as the operator sees them (`health.<check>` = `{status, detail}`): which ones
//! raise the alert banner, their names in plain words, and how long each has been in its
//! current status (the engine publishes no timestamps, so the UI tracks transitions itself).

use crate::model::Model;
use se_proto::Value;
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// A check's status, most urgent first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Status {
    Fail,
    Warn,
    /// Published without a known status word.
    Unknown,
    Pass,
}

impl Status {
    pub fn parse(s: &str) -> Status {
        match s {
            "pass" => Status::Pass,
            "warn" => Status::Warn,
            "fail" => Status::Fail,
            _ => Status::Unknown,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Status::Fail => "fail",
            Status::Warn => "warn",
            Status::Unknown => "unknown",
            Status::Pass => "pass",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Check {
    /// Name without the `health.` prefix (`relay`, `audio.mic`, `midi.pads`).
    pub name: String,
    pub status: Status,
    pub detail: String,
}

impl Check {
    pub fn new(name: &str, status: Status, detail: &str) -> Check {
        Check { name: name.into(), status, detail: detail.into() }
    }
}

/// Every published check, most urgent first, then by name.
pub fn checks(m: &Model) -> Vec<Check> {
    let mut out: Vec<Check> = m
        .under("health")
        .map(|(a, v)| {
            let s = |k: &str| v.get_path(k).and_then(Value::as_str).unwrap_or("");
            Check::new(a.trim_start_matches("health."), Status::parse(s("status")), s("detail"))
        })
        .collect();
    out.sort_by(|a, b| (a.status, &a.name).cmp(&(b.status, &b.name)));
    out
}

/// First segment of a check name (`audio` for `audio.mic`).
pub fn base(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

/// The detail says an optional integration was simply never set up (not a fault). Matches the
/// producers' wording: se-songs relay/YouTube/player, se-twitch, the queue page probe.
pub fn not_set_up(name: &str, detail: &str) -> bool {
    match base(name) {
        "relay" => detail.starts_with("not configured") || detail.starts_with("no shared secret"),
        "youtube" => detail.starts_with("no YouTube API key"),
        "player" => detail.starts_with("Configure [songs]"),
        "twitch" => detail.starts_with("no Client ID") || detail.starts_with("not authorized"),
        "queue_page" => detail.contains("not configured"),
        _ => false,
    }
}

/// Checks whose `warn` viewers notice while on air (the others stay in the health pill).
const ON_AIR_WARN: &[&str] = &["obs", "sources", "cef", "patches", "twitch", "relay", "player", "queue_page", "audio.pipewire"];

fn on_air_warn(name: &str) -> bool {
    ON_AIR_WARN.iter().any(|w| name == *w || base(name) == *w)
}

/// Does this check raise the alert banner? Every `fail` does, optional integrations included,
/// except "not set up yet" while off air (that is setup, shown in the health pill). Warnings
/// alert only while on air, for checks viewers would notice.
pub fn alerting(c: &Check, on_air: bool) -> bool {
    match c.status {
        Status::Fail => on_air || !not_set_up(&c.name, &c.detail),
        Status::Warn => on_air && on_air_warn(&c.name) && !not_set_up(&c.name, &c.detail),
        Status::Unknown | Status::Pass => false,
    }
}

/// The checks the alert banner shows, failures first.
pub fn alerts(checks: &[Check], on_air: bool) -> Vec<&Check> {
    let mut out: Vec<&Check> = checks.iter().filter(|c| alerting(c, on_air)).collect();
    out.sort_by(|a, b| (a.status, &a.name).cmp(&(b.status, &b.name)));
    out
}

/// Plain-language name of a check.
pub fn title(name: &str) -> String {
    let fixed = match name {
        "obs" => "OBS",
        "twitch" => "Twitch",
        "youtube" => "YouTube song lookups",
        "player" => "Song player",
        "relay" => "Tips & queue link",
        "queue_page" => "Public song list page",
        "tiktok" => "TikTok",
        "sources" => "Cameras & media",
        "devices" => "Devices",
        "audio.mic" => "Microphone",
        "audio.obs" => "OBS sound inputs",
        "audio.pipewire" => "Sound system",
        "audio.rt" => "Sound timing",
        "audio.xruns" => "Sound glitches",
        "audio.config" => "Sound settings",
        "mixer" => "Mixing desk",
        "dmx" => "Lights box",
        "deck" => "Stream deck",
        "voice" => "Voice control",
        "render" => "Video rendering",
        "patches" => "Overlays",
        "cef" => "Web sources",
        "tts" => "Read-out voice",
        "timecode" => "Timelines",
        "clips" => "Clips",
        "recording" => "Recording",
        "recordings" => "Recording space",
        "archive" => "Archive",
        "backup" => "Backups",
        "versions" => "Project history",
        "alerts" => "Alerts",
        "bot" => "Chat bot",
        "disk" => "Disk space",
        "gpu" => "Graphics card",
        "idle_inhibitor" => "Screen saver",
        "night_light" => "Night light",
        _ => "",
    };
    if !fixed.is_empty() {
        return fixed.into();
    }
    if let Some(input) = name.strip_prefix("audio.input.") {
        return format!("Sound input “{input}”");
    }
    if let Some(dev) = name.strip_prefix("midi.") {
        return format!("MIDI “{dev}”");
    }
    name.replace(['_', '.'], " ")
}

/// "45 s", "3 min", "2 h 5 min".
pub fn ago(d: Duration) -> String {
    let s = d.as_secs();
    if s < 60 {
        format!("{s} s")
    } else if s < 3600 {
        format!("{} min", s / 60)
    } else {
        format!("{} h {} min", s / 3600, (s / 60) % 60)
    }
}

/// When a check entered its current status.
#[derive(Clone, Copy, Debug)]
pub struct Seen {
    pub status: Status,
    pub since: Instant,
    /// First seen right after the UI (re)connected: it may have been like this for longer.
    pub at_least: bool,
}

impl Seen {
    /// "for 3 min" / "for at least 3 min".
    pub fn for_how_long(&self, now: Instant) -> String {
        format!("for {}{}", if self.at_least { "at least " } else { "" }, ago(now.saturating_duration_since(self.since)))
    }
}

/// Status transitions of every check, as observed by this UI.
#[derive(Default)]
pub struct Tracker {
    seen: HashMap<String, Seen>,
    /// Connection generation and when it was first observed.
    conn: (u64, Option<Instant>),
}

/// Checks first seen this soon after connecting were already in that state before.
const CONNECT_BURST: Duration = Duration::from_secs(3);

impl Tracker {
    pub fn update(&mut self, checks: &[Check], conn_gen: u64, now: Instant) {
        if self.conn.0 != conn_gen || self.conn.1.is_none() {
            self.conn = (conn_gen, Some(now));
        }
        let burst = self.conn.1.is_some_and(|c| now.saturating_duration_since(c) < CONNECT_BURST);
        self.seen.retain(|name, _| checks.iter().any(|c| &c.name == name));
        for c in checks {
            match self.seen.get_mut(&c.name) {
                Some(s) if s.status == c.status => {}
                Some(s) => *s = Seen { status: c.status, since: now, at_least: false },
                None => {
                    self.seen.insert(c.name.clone(), Seen { status: c.status, since: now, at_least: burst });
                }
            }
        }
    }

    pub fn get(&self, name: &str) -> Option<&Seen> {
        self.seen.get(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(name: &str, status: Status, detail: &str) -> Check {
        Check::new(name, status, detail)
    }

    #[test]
    fn banner_alerts_every_failure_including_optional_integrations() {
        let list = [
            c("relay", Status::Fail, "connection refused — retrying in 8s"),
            c("youtube", Status::Fail, "API key invalid"),
            c("queue_page", Status::Fail, "public queue page unreachable 3 times; tunnel or DNS"),
            c("player", Status::Warn, "Embedded YouTube channel proof expired; verify the approved account before resuming"),
            c("twitch", Status::Fail, "authorized as x; EventSub not connected (connecting)"),
            c("tiktok", Status::Fail, "failed"),
            c("disk", Status::Pass, "300 GB free"),
        ];
        for on_air in [false, true] {
            let names: Vec<&str> = alerts(&list, on_air).iter().map(|c| c.name.as_str()).collect();
            for f in ["relay", "youtube", "queue_page", "twitch", "tiktok"] {
                assert!(names.contains(&f), "{f} fail must alert (on_air={on_air}): {names:?}");
            }
            assert!(!names.contains(&"disk"));
            assert_eq!(names.contains(&"player"), on_air, "player warn alerts only on air");
        }
    }

    #[test]
    fn banner_is_empty_when_everything_passes_or_is_merely_not_set_up() {
        let pass = [c("obs", Status::Pass, "ok"), c("twitch", Status::Pass, "authorized"), c("relay", Status::Pass, "connected")];
        assert!(alerts(&pass, true).is_empty());
        assert!(alerts(&pass, false).is_empty());
        let unset = [
            c("relay", Status::Warn, "not configured — set [relay] url in project.toml"),
            c("youtube", Status::Warn, "no YouTube API key — requests use the library only"),
            c("player", Status::Warn, "Configure [songs] youtube_channel with the approved exact UC channel ID"),
            c("audio.obs", Status::Warn, "check OBS inputs"),
        ];
        assert!(alerts(&unset, true).is_empty(), "optional extras that were never set up and routine warns never alert");
        assert!(alerts(&[c("twitch", Status::Fail, "no Client ID: set [twitch] client_id")], false).is_empty(), "off-air setup");
        assert_eq!(alerts(&[c("twitch", Status::Fail, "not authorized: run twitch.auth.start")], true).len(), 1, "on air Twitch is never setup-only");
    }

    #[test]
    fn tracker_keeps_the_start_of_the_current_status() {
        let t0 = Instant::now();
        let mut t = Tracker::default();
        t.update(&[c("relay", Status::Fail, "a")], 1, t0);
        assert!(t.get("relay").unwrap().at_least, "already failing when the UI connected");
        let later = t0 + Duration::from_secs(30);
        t.update(&[c("relay", Status::Fail, "b"), c("mixer", Status::Fail, "x")], 1, later);
        assert_eq!(t.get("relay").unwrap().since, t0, "detail changes don't reset the clock");
        assert!(!t.get("mixer").unwrap().at_least, "appeared while watching");
        t.update(&[c("relay", Status::Pass, "ok")], 1, later + Duration::from_secs(5));
        assert_eq!(t.get("relay").unwrap().since, later + Duration::from_secs(5));
        assert!(t.get("mixer").is_none(), "gone checks are forgotten");
    }
}
