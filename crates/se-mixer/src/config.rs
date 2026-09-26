//! `[mixer]` in `project.toml`.
//!
//! ```toml
//! [mixer]
//! # host = "10.0.0.187"        # skip discovery (ip or ip:port)
//! # serial = "RA1E24110101"     # pick this console when several are found
//! name = "16r"                  # address namespace: mixer.16r.*
//! rate_hz = 50                  # max sends per control per second
//! writable = ["**"]             # mixer addresses the engine may change (others readonly)
//! panic_snapshot = "safe"       # mixes/<name>.toml recalled by `mixer.panic`
//! touch_release = true          # a change on the console releases engine overrides on it
//!
//! [mixer.talk]                  # mic.talking from a channel's input meter
//! channel = 10                  # line channel number or its console label ("VocalMic")
//! threshold_db = -38
//! attack = "40ms"
//! hold = "600ms"
//! ```
//!
//! Level caps for main/monitor outputs live with the other hard caps in `[safety] caps`
//! (e.g. `"mixer.16r.main.fader" = [0.0, 0.8]`) and are enforced by the core's resolver.

use se_proto::parse_duration_ms;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq)]
pub enum ChannelRef {
    Number(u16),
    Label(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TalkConfig {
    pub channel: Option<ChannelRef>,
    pub threshold_db: f32,
    /// Level must stay above the threshold this long to count as talking.
    pub attack: Duration,
    /// Talking ends after the level stayed below (threshold − hysteresis) this long.
    pub hold: Duration,
    pub hysteresis_db: f32,
}

impl Default for TalkConfig {
    fn default() -> Self {
        TalkConfig { channel: None, threshold_db: -38.0, attack: Duration::from_millis(40), hold: Duration::from_millis(600), hysteresis_db: 4.0 }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MixerConfig {
    pub enabled: bool,
    /// Whether `[mixer]` exists in the project (absent = auto-discovery, softer health).
    pub configured: bool,
    pub name: String,
    pub host: Option<String>,
    pub serial: Option<String>,
    pub meters: bool,
    pub meter_port: u16,
    pub rate_hz: f64,
    pub client_name: String,
    pub discovery: Duration,
    pub probe: bool,
    pub writable: Vec<String>,
    pub panic_snapshot: String,
    pub touch_release: bool,
    /// Patterns (below `mixer.<name>.`) captured by `mixer.snapshot.store` by default.
    pub store: Vec<String>,
    pub talk: TalkConfig,
}

impl Default for MixerConfig {
    fn default() -> Self {
        MixerConfig {
            enabled: true,
            configured: false,
            name: "16r".into(),
            host: None,
            serial: None,
            meters: true,
            meter_port: 0,
            rate_hz: 50.0,
            client_name: "stream-engine".into(),
            discovery: Duration::from_secs(4),
            probe: true,
            writable: vec!["**".into()],
            panic_snapshot: "safe".into(),
            touch_release: true,
            store: default_store(),
            talk: TalkConfig::default(),
        }
    }
}

/// Channel strips only: output (main/monitor/FX bus) levels stay out of snapshots unless
/// asked for, so recalling a mix never moves speaker or in-ear levels by surprise.
pub fn default_store() -> Vec<String> {
    ["ch.*.fader", "ch.*.mute", "ch.*.pan", "ret.*.fader", "ret.*.mute", "fxret.*.fader", "fxret.*.mute", "aux.*.*.*.send", "aux.*.tb.send", "fx.*.*.*.send"]
        .map(String::from)
        .to_vec()
}

fn dur(t: &toml::Table, key: &str, default: Duration) -> Result<Duration, String> {
    match t.get(key) {
        None => Ok(default),
        Some(toml::Value::String(s)) => parse_duration_ms(s).map(Duration::from_millis).ok_or_else(|| format!("[mixer] {key}: bad duration `{s}`")),
        Some(toml::Value::Integer(i)) if *i >= 0 => Ok(Duration::from_millis(*i as u64)),
        Some(v) => Err(format!("[mixer] {key}: expected a duration, got {v}")),
    }
}

fn strings(t: &toml::Table, key: &str) -> Result<Option<Vec<String>>, String> {
    match t.get(key) {
        None => Ok(None),
        Some(toml::Value::Array(a)) => {
            a.iter().map(|v| v.as_str().map(String::from).ok_or_else(|| format!("[mixer] {key}: expected strings"))).collect::<Result<Vec<_>, _>>().map(Some)
        }
        Some(v) => Err(format!("[mixer] {key}: expected a list of strings, got {v}")),
    }
}

impl MixerConfig {
    /// Parse `[mixer]` (`None` = section absent → defaults with auto-discovery).
    pub fn parse(section: Option<&toml::Value>) -> Result<MixerConfig, String> {
        let mut c = MixerConfig::default();
        let Some(v) = section else { return Ok(c) };
        let t = v.as_table().ok_or("[mixer] must be a table")?;
        c.configured = true;
        let s = |k: &str| -> Result<Option<String>, String> {
            match t.get(k) {
                None => Ok(None),
                Some(toml::Value::String(s)) => Ok(Some(s.clone())),
                Some(v) => Err(format!("[mixer] {k}: expected a string, got {v}")),
            }
        };
        let b = |k: &str, d: bool| -> Result<bool, String> {
            match t.get(k) {
                None => Ok(d),
                Some(toml::Value::Boolean(x)) => Ok(*x),
                Some(v) => Err(format!("[mixer] {k}: expected true/false, got {v}")),
            }
        };
        c.enabled = b("enabled", true)?;
        c.meters = b("meters", true)?;
        c.probe = b("probe", true)?;
        c.touch_release = b("touch_release", true)?;
        if let Some(n) = s("name")? {
            if n.is_empty() || !n.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-') {
                return Err(format!("[mixer] name `{n}` must be one address segment"));
            }
            c.name = n;
        }
        c.host = s("host")?.filter(|h| !h.is_empty());
        c.serial = s("serial")?.filter(|h| !h.is_empty());
        if let Some(n) = s("client_name")? {
            c.client_name = n;
        }
        if let Some(n) = s("panic_snapshot")? {
            c.panic_snapshot = n;
        }
        match t.get("meter_port") {
            None => {}
            Some(toml::Value::Integer(p)) if (0..=65535).contains(p) => c.meter_port = *p as u16,
            Some(v) => return Err(format!("[mixer] meter_port: expected 0–65535, got {v}")),
        }
        match t.get("rate_hz") {
            None => {}
            Some(v) => {
                let r = v.as_float().or_else(|| v.as_integer().map(|i| i as f64)).ok_or_else(|| format!("[mixer] rate_hz: expected a number, got {v}"))?;
                if !(1.0..=200.0).contains(&r) {
                    return Err(format!("[mixer] rate_hz {r} outside 1–200"));
                }
                c.rate_hz = r;
            }
        }
        c.discovery = dur(t, "discovery", c.discovery)?;
        if let Some(w) = strings(t, "writable")? {
            c.writable = w;
        }
        if let Some(w) = strings(t, "store")? {
            c.store = w;
        }
        if let Some(tv) = t.get("talk") {
            let tt = tv.as_table().ok_or("[mixer.talk] must be a table")?;
            let channel = match tt.get("channel") {
                None => None,
                Some(toml::Value::Integer(n)) if *n >= 1 && *n <= 64 => Some(ChannelRef::Number(*n as u16)),
                Some(toml::Value::String(l)) if !l.is_empty() => Some(ChannelRef::Label(l.clone())),
                Some(v) => return Err(format!("[mixer.talk] channel: expected a channel number or label, got {v}")),
            };
            let num = |k: &str, d: f32| -> Result<f32, String> {
                match tt.get(k) {
                    None => Ok(d),
                    Some(v) => v
                        .as_float()
                        .or_else(|| v.as_integer().map(|i| i as f64))
                        .map(|f| f as f32)
                        .ok_or_else(|| format!("[mixer.talk] {k}: expected a number, got {v}")),
                }
            };
            let d = TalkConfig::default();
            let talk = TalkConfig {
                channel,
                threshold_db: num("threshold_db", d.threshold_db)?,
                hysteresis_db: num("hysteresis_db", d.hysteresis_db)?.max(0.0),
                attack: dur(tt, "attack", d.attack)?,
                hold: dur(tt, "hold", d.hold)?,
            };
            c.talk = talk;
        }
        Ok(c)
    }

    pub fn prefix(&self) -> String {
        format!("mixer.{}", self.name)
    }

    /// Minimum spacing between two sends of the same control.
    pub fn min_interval(&self) -> Duration {
        Duration::from_secs_f64(1.0 / self.rate_hz)
    }

    /// Whether the engine may change `addr` (full address).
    pub fn is_writable(&self, addr: &str) -> bool {
        let prefix = format!("{}.", self.prefix());
        let rel = addr.strip_prefix(&prefix).unwrap_or(addr);
        self.writable.iter().any(|p| se_proto::address::matches(p, rel) || se_proto::address::matches(p, addr))
    }

    /// Explicit console address from `host` (`ip` or `ip:port`).
    pub fn host_addr(&self) -> Result<Option<std::net::SocketAddr>, String> {
        let Some(h) = &self.host else { return Ok(None) };
        if let Ok(a) = h.parse::<std::net::SocketAddr>() {
            return Ok(Some(a));
        }
        h.parse::<std::net::IpAddr>()
            .map(|ip| Some(std::net::SocketAddr::new(ip, crate::ucnet::packet::CONTROL_PORT)))
            .map_err(|_| format!("[mixer] host `{h}` is not an IP address"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<MixerConfig, String> {
        let t: toml::Table = s.parse().unwrap();
        MixerConfig::parse(t.get("mixer"))
    }

    #[test]
    fn absent_section_means_auto_discovery_defaults() {
        let c = MixerConfig::parse(None).unwrap();
        assert!(!c.configured && c.enabled);
        assert_eq!(c.prefix(), "mixer.16r");
        assert_eq!(c.min_interval(), Duration::from_millis(20));
        assert!(c.is_writable("mixer.16r.main.fader"));
    }

    #[test]
    fn parses_full_section() {
        let c = parse(
            "[mixer]\nhost = \"10.0.0.187\"\nrate_hz = 25\nwritable = [\"ch.16.*\"]\n[mixer.talk]\nchannel = \"VocalMic\"\nthreshold_db = -30\nhold = \"1s\"",
        )
        .unwrap();
        assert_eq!(c.host_addr().unwrap(), Some("10.0.0.187:53000".parse().unwrap()));
        assert_eq!(c.min_interval(), Duration::from_millis(40));
        assert!(c.is_writable("mixer.16r.ch.16.fader"));
        assert!(!c.is_writable("mixer.16r.ch.1.fader"));
        assert!(!c.is_writable("mixer.16r.main.fader"));
        assert_eq!(c.talk.channel, Some(ChannelRef::Label("VocalMic".into())));
        assert_eq!(c.talk.threshold_db, -30.0);
        assert_eq!(c.talk.hold, Duration::from_secs(1));
    }

    #[test]
    fn rejects_bad_values() {
        assert!(parse("[mixer]\nrate_hz = 0").is_err());
        assert!(parse("[mixer]\nname = \"a.b\"").is_err());
        assert!(parse("[mixer]\nhost = \"mixer.local\"").unwrap().host_addr().is_err());
        assert!(parse("[mixer.talk]\nchannel = 0").is_err());
        assert!(parse("[mixer]\nwritable = \"**\"").is_err());
        assert!(parse("[mixer]\ndiscovery = \"soon\"").is_err());
    }
}
