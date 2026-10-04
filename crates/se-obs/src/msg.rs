//! obs.sock messages (JSON lines, `docs/frames-protocol.md`).

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Plugin → engine.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum PluginMsg {
    Hello(Hello),
    Status(Box<Status>),
    Event(PluginEvent),
    Reply(Reply),
}

impl PluginMsg {
    /// Message types this engine understands; others are ignored (newer plugin).
    pub const KNOWN: [&'static str; 4] = ["hello", "status", "event", "reply"];
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Hello {
    pub obs: String,
    pub plugin: String,
    pub canvases: Vec<String>,
    pub pid: Option<i64>,
    /// Configuration the plugin persisted from the last engine it talked to.
    pub config: Option<serde_json::Value>,
}

/// Sent once per second and immediately when a feed's staleness changes.
#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Status {
    pub streaming: bool,
    /// Main stream output bitrate over the last interval.
    pub kbps: f64,
    pub dropped: i64,
    pub total: i64,
    pub congestion: f64,
    /// Video the encoder could not keep up with during the last interval (skipped frames ×
    /// frame interval).
    pub lag_ms: f64,
    pub fps: f64,
    pub render_ms: f64,
    pub lagged: i64,
    pub rendered: i64,
    pub skipped: i64,
    pub encoded: i64,
    /// `os_gettime_ns()` and CLOCK_MONOTONIC sampled together.
    pub obs_ns: u64,
    pub mono_ns: u64,
    /// OBS-clock time of the first frame of the main stream (0 = inactive).
    pub stream_start_ns: u64,
    pub scene: String,
    pub stale: BTreeMap<String, bool>,
    pub sources: BTreeMap<String, u32>,
    pub feeds: BTreeMap<String, serde_json::Value>,
    pub fallback: bool,
    pub outputs: Vec<OutputStatus>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct OutputStatus {
    pub name: String,
    pub id: String,
    /// `stream` or `record`.
    pub kind: String,
    pub active: bool,
    pub kbps: f64,
    pub dropped: i64,
    pub total: i64,
    pub congestion: f64,
    /// Engine canvas the output encodes (`wide`, `tall`) or the OBS canvas name.
    pub canvas: String,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct PluginEvent {
    /// `stream_started|stream_stopped|scene_fallback|scene_restored`
    pub name: String,
    pub obs_ns: u64,
    pub mono_ns: u64,
    /// OBS canvas name (fallback events).
    pub canvas: Option<String>,
    pub scene: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    /// Engine canvases shown by the affected scene.
    pub canvases: Vec<String>,
    pub reason: Option<String>,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq)]
#[serde(default)]
pub struct Reply {
    pub id: u64,
    pub ok: bool,
    pub error: Option<String>,
    pub result: serde_json::Value,
}

/// Engine → plugin.
#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(tag = "t", rename_all = "snake_case")]
pub enum EngineMsg {
    Config {
        stale_ms: u32,
        fallback_mode: String,
        fallback_scene: String,
        fallback_text: String,
    },
    Cmd {
        id: u64,
        op: String,
    },
    Error {
        error: String,
    },
}

impl EngineMsg {
    pub fn line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("engine messages always serialize");
        s.push('\n');
        s
    }
}

/// Parses one line. `Ok(None)` for well-formed messages of an unknown type.
pub fn parse(line: &[u8]) -> Result<Option<PluginMsg>, String> {
    let v: serde_json::Value = serde_json::from_slice(line).map_err(|e| format!("invalid JSON: {e}"))?;
    let t = v.get("t").and_then(|t| t.as_str()).ok_or("message without \"t\"")?.to_string();
    if !PluginMsg::KNOWN.contains(&t.as_str()) {
        return Ok(None);
    }
    serde_json::from_value(v).map(Some).map_err(|e| format!("invalid `{t}`: {e}"))
}

/// Maps an OBS-clock timestamp onto the master clock using a pair sampled together.
pub fn obs_to_master(obs: u64, pair_obs: u64, pair_mono: u64) -> u64 {
    let m = pair_mono as i128 + (obs as i128 - pair_obs as i128);
    m.clamp(0, u64::MAX as i128) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_status_with_missing_fields_and_ignores_unknown_types() {
        let s = parse(br#"{"t":"status","streaming":true,"kbps":6000.5,"stale":{"wide":false,"tall":true},"extra":1}"#).unwrap().unwrap();
        let PluginMsg::Status(s) = s else { panic!("not a status") };
        assert!(s.streaming);
        assert_eq!(s.kbps, 6000.5);
        assert_eq!(s.stale.get("tall"), Some(&true));
        assert_eq!(parse(br#"{"t":"future_thing","x":1}"#).unwrap(), None);
        assert!(parse(b"{\"x\":1}").is_err());
        assert!(parse(b"not json").is_err());
        assert!(parse(br#"{"t":"reply","id":"seven"}"#).unwrap_err().contains("reply"));
    }

    #[test]
    fn engine_messages_are_single_lines() {
        let l = EngineMsg::Cmd { id: 7, op: "stream.start".into() }.line();
        assert_eq!(l, "{\"t\":\"cmd\",\"id\":7,\"op\":\"stream.start\"}\n");
        let c = EngineMsg::Config { stale_ms: 500, fallback_mode: "live".into(), fallback_scene: "A\nB".into(), fallback_text: "x".into() }.line();
        assert_eq!(c.matches('\n').count(), 1);
    }

    #[test]
    fn clock_pair_mapping() {
        assert_eq!(obs_to_master(1_500, 1_000, 10_000), 10_500);
        assert_eq!(obs_to_master(500, 1_000, 10_000), 9_500);
        assert_eq!(obs_to_master(0, 1_000_000, 10), 0);
    }
}
