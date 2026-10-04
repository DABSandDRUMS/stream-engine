//! Recovery allowlist: the only things a Recover button may do, per health check, each with an
//! explicit safety class. Nothing outside this table is ever offered from the alert banner or
//! the Health view. Recovery is judged by watching the check pass, never by the action's Ack.

use crate::health::base;
use se_proto::Value;
use std::time::Duration;

/// What viewers would notice.
#[derive(Clone, Debug, PartialEq)]
pub enum Safety {
    /// Nothing on stream changes: runs on click.
    Safe,
    /// Viewers would notice exactly this: asks first.
    Interrupts(String),
    /// Stops more than the broken part: never offered while on air, asks first off air.
    OffAirOnly(String),
}

/// Helper services outside the engine that may be restarted (both `Restart=always`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Unit {
    /// The local relay / public queue server.
    Queue,
    /// The Cloudflare tunnel that publishes the queue page.
    Tunnel,
}

impl Unit {
    pub fn name(self) -> &'static str {
        match self {
            Unit::Queue => "stream-engine-queue.service",
            Unit::Tunnel => "stream-engine-queue-tunnel.service",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Step {
    /// An engine action with its exact target.
    Action { name: &'static str, args: Value },
    /// `systemctl --user restart <unit>`.
    Restart(Unit),
    /// `systemctl --user enable --now stream-engine` (only while the engine is not running).
    StartEngine,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Recovery {
    /// Button label.
    pub label: String,
    pub step: Step,
    pub safety: Safety,
    /// How long to wait for the check to pass before saying it is still failing.
    pub wait: Duration,
}

/// Default time for a recovery to show up as a passing check.
pub const WAIT: Duration = Duration::from_secs(45);
/// Twitch sign-in waits for the operator to type the code on the Twitch page.
const SIGN_IN_WAIT: Duration = Duration::from_secs(300);

fn arg<'a>(args: &'a Value, k: &str) -> Option<&'a str> {
    args.get_path(k).and_then(Value::as_str).filter(|s| !s.is_empty())
}

/// The safety policy. `label` turns a source/overlay id into the name the operator knows.
pub fn classify(step: &Step, label: &dyn Fn(&str) -> String) -> Safety {
    match step {
        Step::Restart(_) | Step::StartEngine => Safety::Safe,
        Step::Action { name, args } => match *name {
            "devices.rescan" | "mixer.reconnect" | "relay.reconnect" | "twitch.auth.start" => Safety::Safe,
            "web.reload" => match arg(args, "slot") {
                Some("youtube") => Safety::Interrupts("The song playing now stops and the song player reloads: viewers hear the music cut out.".into()),
                Some(slot) => Safety::Interrupts(format!("{} goes blank for a few seconds while it reloads.", label(slot))),
                None => Safety::OffAirOnly("Every web source and overlay reloads at once.".into()),
            },
            "patch.reload" => match arg(args, "id") {
                Some(id) => Safety::Interrupts(format!("The overlay {} disappears for a moment while it reloads.", label(&format!("patch.{id}")))),
                None => Safety::OffAirOnly("Every overlay reloads at once.".into()),
            },
            "source.reopen" => match arg(args, "source") {
                Some(src) => Safety::Interrupts(format!("{} goes black for a moment while it reconnects.", label(src))),
                None => Safety::OffAirOnly("Every camera reconnects.".into()),
            },
            "web.login" => Safety::OffAirOnly("Every web source (overlays, the song player) stops while the sign-in window is open.".into()),
            "audio.reload" => Safety::OffAirOnly("All sound stops for a moment while the sound system is rebuilt.".into()),
            _ => Safety::OffAirOnly("This restarts more than the broken part.".into()),
        },
    }
}

/// The Twitch problem is about the sign-in itself (a new device-code sign-in fixes it).
fn twitch_needs_sign_in(detail: &str) -> bool {
    ["not authorized", "token invalid", "missing scopes", "re-authorize", "revoked", "token expires"].iter().any(|w| detail.contains(w))
}

/// `name: why; name: why` → names whose part matches `bad`.
fn named_parts<'a>(detail: &'a str, bad: impl Fn(&str) -> bool + 'a) -> impl Iterator<Item = &'a str> + 'a {
    detail.split(';').filter_map(move |p| p.trim().split_once(':').filter(|(n, why)| !n.contains(' ') && !n.contains('/') && bad(why)).map(|(n, _)| n.trim()))
}

/// `patches with problems: a (error), b (suspended)` → `a`, `b`.
fn patch_ids(detail: &str) -> Vec<&str> {
    detail.strip_prefix("patches with problems:").map_or_else(Vec::new, |rest| rest.split(',').filter_map(|p| p.split_whitespace().next()).collect())
}

/// What may be offered for a check in its current state. `OffAirOnly` steps are dropped while
/// on air.
pub fn offered(check: &str, detail: &str, on_air: bool, label: &dyn Fn(&str) -> String) -> Vec<Recovery> {
    let act = |name: &'static str, args: Value| Step::Action { name, args };
    let mut steps: Vec<(String, Step, Duration)> = Vec::new();
    match base(check) {
        "devices" | "midi" | "deck" | "dmx" => steps.push(("Look for devices again".into(), act("devices.rescan", Value::Null), WAIT)),
        "mixer" => steps.push(("Reconnect the mixing desk".into(), act("mixer.reconnect", Value::Null), WAIT)),
        "relay" => {
            steps.push(("Reconnect".into(), act("relay.reconnect", Value::Null), WAIT));
            steps.push(("Restart the queue service".into(), Step::Restart(Unit::Queue), WAIT));
        }
        "queue_page" => {
            steps.push(("Restart the tunnel".into(), Step::Restart(Unit::Tunnel), WAIT));
            steps.push(("Restart the queue service".into(), Step::Restart(Unit::Queue), WAIT));
        }
        "twitch" if twitch_needs_sign_in(detail) => {
            steps.push(("Sign in to Twitch again".into(), act("twitch.auth.start", Value::map().with("account", "broadcaster")), SIGN_IN_WAIT));
        }
        "player" if !detail.starts_with("Configure [songs]") => {
            steps.push(("Reload the song player".into(), act("web.reload", Value::map().with("slot", "youtube")), WAIT));
        }
        "sources" => {
            for src in named_parts(detail, |why| why.contains("signal") || why.contains("not capturing") || why.contains("missing")) {
                steps.push((format!("Reconnect {}", label(src)), act("source.reopen", Value::map().with("source", src)), WAIT));
            }
        }
        "patches" => {
            for id in patch_ids(detail) {
                steps.push((format!("Reload {}", label(&format!("patch.{id}"))), act("patch.reload", Value::map().with("id", id)), WAIT));
            }
        }
        "audio" if check == "audio.pipewire" || check == "audio.config" || check.starts_with("audio.input.") => {
            steps.push(("Rebuild the sound system".into(), act("audio.reload", Value::Null), WAIT));
        }
        _ => {}
    }
    steps
        .into_iter()
        .map(|(label_text, step, wait)| {
            let safety = classify(&step, label);
            Recovery { label: label_text, step, safety, wait }
        })
        .filter(|r| !(on_air && matches!(r.safety, Safety::OffAirOnly(_))))
        .collect()
}

/// The engine is unreachable: start it through its user service.
pub fn engine_down() -> Recovery {
    Recovery { label: "Start Stream Engine".into(), step: Step::StartEngine, safety: Safety::Safe, wait: WAIT }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn same(s: &str) -> String {
        s.to_string()
    }

    /// Every check family with details that make each branch offer something.
    const CASES: &[(&str, &str)] = &[
        ("devices", "missing: pads"),
        ("midi.pads", "offline"),
        ("dmx", "transport failed"),
        ("mixer", "disconnected"),
        ("relay", "connection refused — retrying in 8s"),
        ("queue_page", "unreachable; tunnel or DNS"),
        ("twitch", "token invalid (401); run twitch.auth.start"),
        ("player", "Embedded YouTube channel proof expired"),
        ("sources", "cam_3: device missing; 2 live; cam_1: no signal"),
        ("patches", "patches with problems: lower_third (error), chat (suspended)"),
        ("audio.pipewire", "disconnected"),
        ("audio.config", "bad route"),
        ("cef", "CEF host crashed; restarting"),
        ("obs", "not connected"),
    ];

    fn action_name(r: &Recovery) -> Option<&'static str> {
        match &r.step {
            Step::Action { name, .. } => Some(name),
            _ => None,
        }
    }

    #[test]
    fn nothing_that_stops_more_than_the_broken_part_is_ever_safe_or_offered_on_air() {
        for &(check, detail) in CASES {
            for on_air in [false, true] {
                for r in offered(check, detail, on_air, &same) {
                    let name = action_name(&r);
                    assert_ne!(r.step, Step::StartEngine, "{check}: engine start is only for an unreachable engine");
                    if matches!(name, Some("web.login" | "audio.reload")) {
                        assert!(matches!(r.safety, Safety::OffAirOnly(_)), "{check}: {name:?} must be off-air only");
                        assert!(!on_air, "{check}: {name:?} offered on air");
                    }
                    if on_air {
                        assert!(!matches!(r.safety, Safety::OffAirOnly(_)), "{check}: {} offered on air", r.label);
                    }
                    if let Step::Action { name: "web.reload" | "patch.reload" | "source.reopen", args } = &r.step {
                        assert!(matches!(r.safety, Safety::Interrupts(_)), "{check}: {} must ask first", r.label);
                        assert!(args.as_map().is_some_and(|m| !m.is_empty()), "{check}: {} must name exactly one target", r.label);
                    }
                }
            }
        }
        for name in ["web.login", "audio.reload", "engine.restart", "panic", "web.reload", "patch.reload", "source.reopen"] {
            assert!(!matches!(classify(&Step::Action { name, args: Value::Null }, &same), Safety::Safe), "{name} without a target is not safe");
        }
    }

    #[test]
    fn interrupting_recoveries_say_what_viewers_notice() {
        let label = |s: &str| if s == "cam_1" { "Camera 1".to_string() } else { s.to_string() };
        let r = offered("sources", "2 live; cam_1: no signal", true, &label);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].label, "Reconnect Camera 1");
        assert_eq!(r[0].safety, Safety::Interrupts("Camera 1 goes black for a moment while it reconnects.".into()));
        let p = offered("player", "Embedded YouTube channel proof expired", true, &same);
        assert!(matches!(&p[0].safety, Safety::Interrupts(t) if t.contains("music cut out")));
    }

    #[test]
    fn scoped_reconnects_are_offered_on_air_and_off() {
        for on_air in [false, true] {
            let safe = |check: &str, detail: &str| -> Vec<Option<&'static str>> {
                offered(check, detail, on_air, &same).iter().filter(|r| r.safety == Safety::Safe).map(action_name).collect()
            };
            assert_eq!(safe("mixer", "gone"), [Some("mixer.reconnect")]);
            assert_eq!(safe("relay", "lost"), [Some("relay.reconnect"), None]);
            assert_eq!(safe("twitch", "missing scopes x: re-authorize"), [Some("twitch.auth.start")]);
        }
        assert!(offered("twitch", "authorized as x; EventSub not connected (timeout)", true, &same).is_empty(), "EventSub reconnects by itself");
        assert!(offered("audio.pipewire", "disconnected", true, &same).is_empty());
        assert_eq!(offered("audio.pipewire", "disconnected", false, &same).len(), 1);
    }
}
