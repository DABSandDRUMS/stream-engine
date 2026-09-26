//! `[obs]` section of `project.toml`.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FallbackMode {
    /// Never switch automatically (manual `obs.fallback.on|off` still works).
    Off,
    /// Switch only while an OBS output (stream or recording) is active.
    Live,
    /// Switch whenever a visible feed goes stale.
    Always,
}

impl FallbackMode {
    pub fn as_str(self) -> &'static str {
        match self {
            FallbackMode::Off => "off",
            FallbackMode::Live => "live",
            FallbackMode::Always => "always",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "off" => Some(FallbackMode::Off),
            "live" => Some(FallbackMode::Live),
            "always" => Some(FallbackMode::Always),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObsConfig {
    /// obs.sock path (default `$SE_RUNTIME_DIR/obs.sock`, else `$XDG_RUNTIME_DIR/stream-engine/obs.sock`).
    pub socket: PathBuf,
    /// A feed with no frame for this long is stale (plugin side).
    pub stale_ms: u32,
    pub fallback_mode: FallbackMode,
    /// Scene each OBS canvas switches to on a stale feed (created by the plugin only if missing).
    pub fallback_scene: String,
    /// Text of the created fallback scene.
    pub fallback_text: String,
    /// How long an `obs.*` action waits for the plugin's reply.
    pub command_timeout_ms: u64,
}

impl Default for ObsConfig {
    fn default() -> Self {
        ObsConfig {
            socket: default_socket(),
            stale_ms: 500,
            fallback_mode: FallbackMode::Live,
            fallback_scene: "Technical Difficulties".into(),
            fallback_text: "Technical difficulties \u{2014} back in a moment".into(),
            command_timeout_ms: 10_000,
        }
    }
}

/// `$SE_RUNTIME_DIR/obs.sock` (dev instances; the plugin looks there too), else
/// `$XDG_RUNTIME_DIR/stream-engine/obs.sock`.
fn default_socket() -> PathBuf {
    match std::env::var_os("SE_RUNTIME_DIR").filter(|d| !d.is_empty()) {
        Some(dir) => PathBuf::from(dir).join("obs.sock"),
        None => se_proto::wire::runtime_dir().join("obs.sock"),
    }
}

fn duration_ms(key: &str, v: &toml::Value) -> Result<u64, String> {
    match v {
        toml::Value::Integer(i) if *i >= 0 => Ok(*i as u64),
        toml::Value::String(s) => se_proto::parse_duration_ms(s).ok_or_else(|| format!("[obs] {key}: invalid duration `{s}`")),
        _ => Err(format!("[obs] {key}: expected milliseconds or a duration string")),
    }
}

fn string(key: &str, v: &toml::Value) -> Result<String, String> {
    v.as_str().map(str::to_string).ok_or_else(|| format!("[obs] {key}: expected a string"))
}

fn expand_home(s: &str) -> PathBuf {
    match (s.strip_prefix("~/"), std::env::var_os("HOME")) {
        (Some(rest), Some(home)) => PathBuf::from(home).join(rest),
        _ => PathBuf::from(s),
    }
}

impl ObsConfig {
    /// Parses `[obs]`; a missing section yields the defaults.
    pub fn from_section(section: Option<&toml::Value>) -> Result<Self, String> {
        let mut c = ObsConfig::default();
        let Some(v) = section else { return Ok(c) };
        let t = v.as_table().ok_or("[obs] must be a table")?;
        for (k, v) in t {
            match k.as_str() {
                "socket" => c.socket = expand_home(&string(k, v)?),
                "stale_ms" | "stale" => {
                    let ms = duration_ms(k, v)?;
                    if !(50..=60_000).contains(&ms) {
                        return Err(format!("[obs] {k}: {ms} ms is outside 50..60000"));
                    }
                    c.stale_ms = ms as u32;
                }
                "fallback_mode" => {
                    let s = string(k, v)?;
                    c.fallback_mode = FallbackMode::parse(&s).ok_or_else(|| format!("[obs] fallback_mode: `{s}` is not off|live|always"))?;
                }
                "fallback_scene" => {
                    let s = string(k, v)?;
                    if s.trim().is_empty() {
                        return Err("[obs] fallback_scene must not be empty".into());
                    }
                    c.fallback_scene = s;
                }
                "fallback_text" => c.fallback_text = string(k, v)?,
                "command_timeout" | "command_timeout_ms" => {
                    let ms = duration_ms(k, v)?;
                    if ms == 0 {
                        return Err(format!("[obs] {k} must be positive"));
                    }
                    c.command_timeout_ms = ms;
                }
                other => return Err(format!("[obs] unknown key `{other}`")),
            }
        }
        Ok(c)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(s: &str) -> Result<ObsConfig, String> {
        let t: toml::Table = s.parse().unwrap();
        ObsConfig::from_section(t.get("obs"))
    }

    #[test]
    fn defaults_and_overrides() {
        let d = parse("").unwrap();
        assert_eq!(d.stale_ms, 500);
        assert_eq!(d.fallback_mode, FallbackMode::Live);
        assert!(d.socket.ends_with("stream-engine/obs.sock"));
        let c =
            parse("[obs]\nsocket = \"/tmp/x.sock\"\nstale = \"1.5s\"\nfallback_mode = \"always\"\nfallback_scene = \"BRB\"\ncommand_timeout = 2000").unwrap();
        assert_eq!(c.socket, PathBuf::from("/tmp/x.sock"));
        assert_eq!(c.stale_ms, 1500);
        assert_eq!(c.fallback_mode, FallbackMode::Always);
        assert_eq!(c.fallback_scene, "BRB");
        assert_eq!(c.command_timeout_ms, 2000);
    }

    #[test]
    fn rejects_bad_values() {
        assert!(parse("[obs]\nstale_ms = 10").unwrap_err().contains("outside"));
        assert!(parse("[obs]\nfallback_mode = \"sometimes\"").unwrap_err().contains("off|live|always"));
        assert!(parse("[obs]\nfallback_scene = \" \"").is_err());
        assert!(parse("[obs]\nstale_ms = \"soon\"").is_err());
        assert!(parse("[obs]\nsocketpath = \"/x\"").unwrap_err().contains("unknown key"));
        assert!(parse("obs = 3").is_err());
    }
}
