//! `[tiktok]` in `project.toml`:
//!
//! ```toml
//! [tiktok]
//! enabled = false                          # default; nothing runs until true (or `tiktok.connect`)
//! unique_id = "yourname"                   # @handle, with or without "@", or the profile/live URL
//! sign_url = "https://api.eulerstream.com" # Euler-Stream-compatible sign provider
//! poll_offline = "60s"                     # how often to check whether an offline stream went live
//! ```
//!
//! `sign_url` is a base URL (`/webcast/rooms/{room_id}/connect` is appended) or a full URL
//! template containing `{room_id}`. The API key is the keyring secret `tiktok.sign_api_key`
//! (set with `stream tiktok.key.set <key>`); without one the provider's anonymous limits apply.

use std::time::Duration;

pub const DEFAULT_SIGN_URL: &str = "https://api.eulerstream.com";
pub const DEFAULT_POLL_OFFLINE: Duration = Duration::from_secs(60);
pub const MIN_POLL_OFFLINE: Duration = Duration::from_secs(30);
pub const MAX_POLL_OFFLINE: Duration = Duration::from_secs(3600);
/// Keyring secret holding the sign-provider API key.
pub const SIGN_KEY_SECRET: &str = "tiktok.sign_api_key";

const KEYS: &[&str] = &["enabled", "unique_id", "sign_url", "poll_offline"];

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    pub enabled: bool,
    /// Normalized handle (no `@`, no URL), empty when unset.
    pub unique_id: String,
    pub sign_url: String,
    pub poll_offline: Duration,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { enabled: false, unique_id: String::new(), sign_url: DEFAULT_SIGN_URL.into(), poll_offline: DEFAULT_POLL_OFFLINE }
    }
}

/// Parsed settings plus non-fatal warnings (unknown keys, clamped values).
#[derive(Debug)]
pub struct Parsed {
    pub settings: Settings,
    pub warnings: Vec<String>,
}

/// Accepts `name`, `@name`, `https://www.tiktok.com/@name[/live]`. TikTok handles are
/// letters, digits, `_` and `.`, at most 24 characters.
pub fn normalize_unique_id(raw: &str) -> Result<String, String> {
    let mut s = raw.trim();
    for p in ["https://", "http://", "www.", "m.", "tiktok.com/"] {
        s = s.strip_prefix(p).unwrap_or(s);
    }
    let s = s.split(['?', '#']).next().unwrap_or("");
    let s = s.trim_end_matches('/');
    let s = s.strip_suffix("/live").unwrap_or(s);
    let s = s.trim_start_matches('@');
    if s.is_empty() {
        return Ok(String::new());
    }
    if s.len() > 24 || !s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.') {
        return Err(format!("`{raw}` is not a TikTok username (letters, digits, `_`, `.`; ≤ 24 chars)"));
    }
    Ok(s.to_string())
}

fn validate_sign_url(s: &str) -> Result<String, String> {
    let s = s.trim().trim_end_matches('/');
    let rest = s.strip_prefix("https://").or_else(|| s.strip_prefix("http://")).ok_or_else(|| format!("sign_url `{s}` must start with https:// or http://"))?;
    let host = rest.split(['/', '?']).next().unwrap_or("");
    if host.is_empty() || host.contains(char::is_whitespace) {
        return Err(format!("sign_url `{s}` has no host"));
    }
    Ok(s.to_string())
}

/// Parse the `[tiktok]` section (`None` = section absent → defaults, disabled).
pub fn parse(section: Option<&toml::Value>) -> Result<Parsed, String> {
    let mut settings = Settings::default();
    let mut warnings = Vec::new();
    let Some(v) = section else { return Ok(Parsed { settings, warnings }) };
    let t = v.as_table().ok_or("[tiktok] must be a table")?;
    for k in t.keys() {
        if !KEYS.contains(&k.as_str()) {
            warnings.push(format!("[tiktok] unknown key `{k}` (known: {})", KEYS.join(", ")));
        }
    }
    if let Some(e) = t.get("enabled") {
        settings.enabled = e.as_bool().ok_or("[tiktok] enabled must be true or false")?;
    }
    if let Some(u) = t.get("unique_id") {
        settings.unique_id = normalize_unique_id(u.as_str().ok_or("[tiktok] unique_id must be a string")?)?;
    }
    if let Some(u) = t.get("sign_url") {
        settings.sign_url = validate_sign_url(u.as_str().ok_or("[tiktok] sign_url must be a string")?)?;
    }
    if let Some(p) = t.get("poll_offline") {
        let d = match p {
            toml::Value::String(s) => {
                Duration::from_millis(se_proto::parse_duration_ms(s).ok_or_else(|| format!("[tiktok] poll_offline `{s}` is not a duration"))?)
            }
            toml::Value::Integer(n) if *n > 0 => Duration::from_secs(*n as u64),
            _ => return Err("[tiktok] poll_offline must be a duration like \"60s\" or whole seconds".into()),
        };
        let c = d.clamp(MIN_POLL_OFFLINE, MAX_POLL_OFFLINE);
        if c != d {
            warnings.push(format!("[tiktok] poll_offline clamped to {}s (allowed 30s–1h)", c.as_secs()));
        }
        settings.poll_offline = c;
    }
    Ok(Parsed { settings, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sec(s: &str) -> toml::Value {
        let t: toml::Table = s.parse().unwrap();
        t["tiktok"].clone()
    }

    #[test]
    fn absent_section_is_disabled_with_public_sign_url() {
        let p = parse(None).unwrap();
        assert_eq!(p.settings, Settings::default());
        assert!(!p.settings.enabled);
        assert_eq!(p.settings.sign_url, "https://api.eulerstream.com");
    }

    #[test]
    fn full_section_parses_and_normalizes() {
        let p = parse(Some(&sec(
            "[tiktok]\nenabled = true\nunique_id = \"https://www.tiktok.com/@Some.User_1/live\"\nsign_url = \"https://sign.example/\"\npoll_offline = \"2m\"",
        )))
        .unwrap();
        assert_eq!(
            p.settings,
            Settings { enabled: true, unique_id: "Some.User_1".into(), sign_url: "https://sign.example".into(), poll_offline: Duration::from_secs(120) }
        );
        assert!(p.warnings.is_empty());
    }

    #[test]
    fn bad_values_are_errors_and_odd_ones_warn() {
        assert!(parse(Some(&sec("[tiktok]\nenabled = \"yes\""))).is_err());
        assert!(parse(Some(&sec("[tiktok]\nunique_id = \"../../etc\""))).is_err());
        assert!(parse(Some(&sec("[tiktok]\nsign_url = \"ftp://x\""))).is_err());
        assert!(parse(Some(&sec("[tiktok]\npoll_offline = \"soon\""))).is_err());
        let p = parse(Some(&sec("[tiktok]\npoll_offline = 5\nenable = true"))).unwrap();
        assert_eq!(p.settings.poll_offline, MIN_POLL_OFFLINE);
        assert!(!p.settings.enabled);
        assert_eq!(p.warnings.len(), 2);
    }

    #[test]
    fn unique_id_forms() {
        assert_eq!(normalize_unique_id("@abc").unwrap(), "abc");
        assert_eq!(normalize_unique_id("tiktok.com/@abc?lang=en").unwrap(), "abc");
        assert_eq!(normalize_unique_id("  ").unwrap(), "");
        assert!(normalize_unique_id("a b").is_err());
        assert!(normalize_unique_id(&"x".repeat(25)).is_err());
    }
}
