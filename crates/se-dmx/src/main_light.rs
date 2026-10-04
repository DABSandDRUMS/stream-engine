//! Main-light handoff follows committed DMX output, never offline previews.
use crate::{output::Shared, tplink};
use se_hub::Hub;
use se_proto::{Meta, Value, ValueType};
use serde::{Deserialize, Deserializer};
use std::{sync::{Arc, atomic::Ordering}, time::{Duration, Instant}};

#[derive(Clone, Debug, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    #[serde(default)]
    pub enabled: bool,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(deserialize_with = "deserialize_mac")]
    pub mac: [u8; 6],
    /// Restore Main on initial idle after a successful armed frame (restart recovery).
    #[serde(default)]
    pub idle_on: bool,
}

fn default_port() -> u16 { 9999 }
fn deserialize_mac<'de, D: Deserializer<'de>>(d: D) -> Result<[u8; 6], D::Error> {
    let value = String::deserialize(d)?;
    tplink::parse_mac(&value).map_err(serde::de::Error::custom)
}
impl Config {
    pub(crate) fn validate(&self) -> Result<(), String> {
        if self.host.trim().is_empty() || self.port == 0 {
            return Err("main_light needs a nonempty host and nonzero TCP port".into());
        }
        Ok(())
    }
}

fn mac_text(mac: [u8; 6]) -> String {
    format!("{:02X}:{:02X}:{:02X}:{:02X}:{:02X}:{:02X}", mac[0], mac[1], mac[2], mac[3], mac[4], mac[5])
}

/// Once an off request may have reached the plug, the next stop must request on,
/// even if acknowledgement failed. Configured idle recovery also restores at startup.
#[derive(Default)]
struct Handoff { live: bool, desired: Option<bool> }
impl Handoff {
    fn update(&mut self, live: bool, idle: bool) -> Option<bool> {
        let desired = if live { Some(false) } else if self.live || idle { Some(true) } else { self.desired };
        self.live = live;
        if self.desired == desired { return None; }
        self.desired = desired;
        desired
    }
    fn release(&mut self) -> Option<bool> { self.update(false, false) }
}

#[derive(Default)]
struct Status {
    on: Option<bool>,
    alias: String,
    model: String,
    error: Option<String>,
    published_config: Option<Config>,
    published_flags: Option<(&'static str, bool, bool)>,
    dirty: bool,
}
impl Status {
    fn publish(&mut self, hub: &Hub, config: Option<&Config>, phase: &'static str, live: bool, sent: bool) {
        let flags = (phase, live, sent);
        if !self.dirty && self.published_config.as_ref() == config && self.published_flags == Some(flags) { return; }
        let state = Value::map()
            .with("configured", config.is_some())
            .with("enabled", config.is_some_and(|c| c.enabled))
            .with("host", config.map(|c| Value::from(c.host.clone())).unwrap_or(Value::Null))
            .with("mac", config.map(|c| Value::from(mac_text(c.mac))).unwrap_or(Value::Null))
            .with("phase", if self.error.is_some() { "error" } else { phase })
            .with("in_use", live)
            .with("output_sent", sent)
            .with("on", self.on.map(Value::Bool).unwrap_or(Value::Null))
            .with("alias", self.alias.clone())
            .with("model", self.model.clone())
            .with("detail", self.error.clone().map(Value::from).unwrap_or_else(|| Value::from(match phase {
                "dmx" => "Main light off while committed DMX lighting owns the room",
                "restoring" => "Restoring main light after DMX stop or blackout",
                "switching" => "Switching main light off after committed DMX output",
                "disabled" => "Automatic main-light handoff disabled",
                _ if config.is_some_and(|c| c.idle_on) && self.on == Some(true) => "Idle room: Main light on; ambient DMX does not claim takeover",
                _ => "No live DMX takeover; main light is left unchanged",
            })));
        hub.publish("lights.main_light.state", state);
        self.published_config = config.cloned();
        self.published_flags = Some(flags);
        self.dirty = false;
    }
    async fn power(&mut self, hub: &Hub, config: &Config, on: bool) {
        self.dirty = true;
        match tplink::set_power(&config.host, config.port, config.mac, on).await {
            Ok(device) => {
                self.on = Some(device.on);
                self.alias = device.alias;
                self.model = device.model;
                self.error = None;
            }
            Err(error) => {
                self.on = None;
                hub.log("error", "lights.main_light", error.clone());
                self.error = Some(error);
            }
        }
    }
}

pub(crate) fn spawn(hub: Arc<Hub>, shared: Arc<Shared>) -> tokio::task::JoinHandle<()> {
    hub.declare("lights.main_light.state", Meta { ty: ValueType::Map, default: Value::map(), readonly: true, ..Default::default() }.owner("lights").describe("Identity-pinned main-light handoff from committed DMX output"));
    tokio::spawn(async move {
        let mut target: Option<Config> = None;
        let mut handoff = Handoff::default();
        let mut status = Status::default();
        let mut tick = tokio::time::interval(Duration::from_millis(100));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut retry_at = Instant::now();
        loop {
            tick.tick().await;
            let plan = shared.plan.load_full();
            let config = plan.rig.main_light.as_ref();
            let selected = config.filter(|c| c.enabled);
            if target.as_ref() != selected {
                if let Some(old) = target.as_ref() && handoff.release().is_some() {
                    status.publish(&hub, Some(old), "restoring", false, shared.output_sent.load(Ordering::Acquire));
                    status.power(&hub, old, true).await;
                }
                target = selected.cloned();
                handoff = Handoff::default();
                status.error = None;
                status.on = None;
                status.alias.clear();
                status.model.clear();
                status.dirty = true;
            }
            let sent = shared.output_sent.load(Ordering::Acquire);
            let stopping = shared.stop.load(Ordering::Acquire) || !shared.alive.load(Ordering::Acquire);
            let live = !stopping && sent && shared.output_in_use.load(Ordering::Acquire);
            if let Some(target) = target.as_ref() {
                let idle = !stopping && sent && plan.rig.output_armed && target.idle_on;
                let changed = handoff.update(live, idle);
                let retry = !stopping && status.error.is_some() && Instant::now() >= retry_at;
                if let Some(on) = changed.or_else(|| retry.then_some(handoff.desired).flatten()) {
                    status.publish(&hub, Some(target), if on { "restoring" } else { "switching" }, live, sent);
                    status.power(&hub, target, on).await;
                    retry_at = Instant::now() + Duration::from_secs(1);
                    // Re-read immediately after a network request. Stop/config changes
                    // during it must not leave a stale off request as the final state.
                    continue;
                }
            }
            status.publish(&hub, config, if target.is_none() { "disabled" } else if live { "dmx" } else { "standby" }, live, sent);
            if stopping { break; }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::Handoff;
    #[test]
    fn idle_does_not_take_over_and_each_live_session_restores_once() {
        let mut h = Handoff::default();
        assert_eq!(h.update(false, false), None);
        assert_eq!(h.release(), None);
        assert_eq!(h.update(true, false), Some(false));
        assert_eq!(h.update(true, false), None);
        assert_eq!(h.update(false, false), Some(true));
        assert_eq!(h.update(false, false), None);
        assert_eq!(h.update(true, false), Some(false));
        assert_eq!(h.release(), Some(true));
        assert_eq!(h.release(), None);
    }
    #[test]
    fn unacknowledged_off_still_requires_restoration() {
        let mut h = Handoff::default();
        assert_eq!(h.update(true, false), Some(false));
        // No success callback is required: the device may have accepted a timed-out write.
        assert_eq!(h.release(), Some(true));
    }
    #[test]
    fn restart_idle_restores_only_after_guard_and_then_hands_off() {
        let mut h = Handoff::default();
        assert_eq!(h.update(false, false), None, "disarmed or unsuccessful output is not ownership");
        assert_eq!(h.update(false, true), Some(true), "successful idle must restore a previously off plug");
        assert_eq!(h.update(false, true), None);
        assert_eq!(h.update(true, true), Some(false));
        assert_eq!(h.update(false, true), Some(true));
    }
}
