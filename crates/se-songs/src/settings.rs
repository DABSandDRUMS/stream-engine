//! Deployment settings from `project.toml` (`[songs]`, `[songs.messages]`, `[relay]`). The
//! queue policy itself is UI-edited and lives in the runtime DB ([`crate::policy`]).
//!
//! ```toml
//! [songs]
//! region = "US"            # ISO 3166-1 alpha-2, for region-restriction checks and search
//! daily_quota = 10000      # units per Pacific day for the key's Google Cloud project
//! link_reserve = 200       # units kept for link lookups when search stops
//! cache_days = 7           # re-check cached metadata older than this (when quota allows)
//! search_results = 5       # candidates per text search (first playable one wins)
//! crossfade = "400ms"      # visual crossfade between the two player slots
//! metadata = true          # artist/genres/year from MusicBrainz (free, no key)
//!
//! [songs.messages]
//! added = "@{user} queued {title} (#{pos})"   # "" silences a message
//!
//! [relay]
//! url = "wss://relay.example.com/link"        # engine link (outbound only)
//! queue_url = "https://relay.example.com/queue"  # optional; derived from url
//! ```

use crate::messages::Messages;
use crate::youtube;
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub region: String,
    /// Exact authenticated YouTube channel ID; empty means playback is locked.
    pub youtube_channel: String,
    /// Delegated Brand-channel session pinned by the native embedded browser.
    pub youtube_delegate: String,
    pub daily_quota: u32,
    pub link_reserve: u32,
    pub cache_days: u32,
    pub search_results: u32,
    pub crossfade_ms: u32,
    pub api_base: String,
    /// Look songs up on MusicBrainz (`[songs] metadata`, default true).
    pub metadata: bool,
    pub metadata_base: String,
    pub messages: Messages,
    pub relay_url: Option<String>,
    pub queue_url: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            region: "US".into(),
            youtube_channel: String::new(),
            youtube_delegate: String::new(),
            daily_quota: 10_000,
            link_reserve: 200,
            cache_days: 7,
            search_results: 5,
            crossfade_ms: 400,
            api_base: youtube::DEFAULT_BASE.into(),
            metadata: true,
            metadata_base: crate::metadata::DEFAULT_BASE.into(),
            messages: Messages::default(),
            relay_url: None,
            queue_url: None,
        }
    }
}

fn int(t: &toml::Table, k: &str, d: u32, max: u32) -> Result<u32, String> {
    match t.get(k) {
        None => Ok(d),
        Some(toml::Value::Integer(i)) if *i >= 0 => Ok((*i as u64).min(max as u64) as u32),
        Some(v) => Err(format!("[songs] {k} must be a non-negative integer, got {v}")),
    }
}

pub(crate) fn valid_channel_id(channel: &str) -> bool {
    channel.len() == 24 && channel.starts_with("UC") && channel.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

pub(crate) fn valid_delegate(delegate: &str) -> bool {
    !delegate.is_empty() && delegate.bytes().all(|b| b.is_ascii_digit())
}

/// An https API base (plain http only for a localhost test server), without a trailing `/`.
fn base_url(v: &toml::Value, key: &str) -> Result<String, String> {
    let b = v.as_str().ok_or_else(|| format!("[songs] {key} must be a string"))?;
    if !(b.starts_with("https://") || b.starts_with("http://127.0.0.1") || b.starts_with("http://localhost")) {
        return Err(format!("[songs] {key} must be https (or a localhost test server)"));
    }
    Ok(b.trim_end_matches('/').to_string())
}

impl Settings {
    pub fn parse(songs: Option<&toml::Value>, relay: Option<&toml::Value>) -> Result<Settings, String> {
        let mut s = Settings::default();
        if let Some(v) = songs {
            let t = v.as_table().ok_or("[songs] must be a table")?;
            if let Some(r) = t.get("region") {
                let r = r.as_str().ok_or("[songs] region must be a string")?.trim().to_ascii_uppercase();
                if !(r.is_empty() || r.len() == 2 && r.bytes().all(|b| b.is_ascii_uppercase())) {
                    return Err(format!("[songs] region must be a two-letter country code, got `{r}`"));
                }
                s.region = r;
            }
            if let Some(channel) = t.get("youtube_channel") {
                let channel = channel.as_str().ok_or("[songs] youtube_channel must be an exact UC channel ID string")?;
                if !channel.is_empty() && !valid_channel_id(channel) {
                    return Err("[songs] youtube_channel must be an exact 24-character UC channel ID (not a name, handle, or URL)".into());
                }
                s.youtube_channel = channel.to_string();
            }
            if let Some(delegate) = t.get("youtube_delegate") {
                let delegate = delegate.as_str().ok_or("[songs] youtube_delegate must be a decimal identifier string")?;
                if !delegate.is_empty() && !valid_delegate(delegate) {
                    return Err("[songs] youtube_delegate must be a decimal delegated Brand-channel identifier".into());
                }
                s.youtube_delegate = delegate.to_string();
            }
            if !s.youtube_channel.is_empty() && s.youtube_delegate.is_empty() {
                return Err("[songs] youtube_delegate is required when youtube_channel is configured".into());
            }
            s.daily_quota = int(t, "daily_quota", s.daily_quota, 100_000_000)?;
            s.link_reserve = int(t, "link_reserve", s.link_reserve, 100_000)?;
            s.cache_days = int(t, "cache_days", s.cache_days, 3650)?;
            s.search_results = int(t, "search_results", s.search_results, 25)?.max(1);
            if let Some(c) = t.get("crossfade") {
                s.crossfade_ms = match c {
                    toml::Value::Integer(i) if *i >= 0 => (*i as u64).min(10_000) as u32,
                    toml::Value::String(x) => {
                        se_proto::parse_duration_ms(x).ok_or_else(|| format!("[songs] crossfade: bad duration `{x}`"))?.min(10_000) as u32
                    }
                    other => return Err(format!("[songs] crossfade must be a duration, got {other}")),
                };
            }
            if let Some(b) = t.get("api_base") {
                s.api_base = base_url(b, "api_base")?;
            }
            if let Some(m) = t.get("metadata") {
                s.metadata = m.as_bool().ok_or("[songs] metadata must be true or false")?;
            }
            if let Some(b) = t.get("metadata_base") {
                s.metadata_base = base_url(b, "metadata_base")?;
            }
            if let Some(m) = t.get("messages") {
                let m = m.as_table().ok_or("[songs.messages] must be a table")?;
                let mut o = BTreeMap::new();
                for (k, v) in m {
                    if !crate::messages::DEFAULTS.iter().any(|(d, _)| d == k) {
                        return Err(format!("[songs.messages] unknown message `{k}`"));
                    }
                    o.insert(k.clone(), v.as_str().ok_or_else(|| format!("[songs.messages] {k} must be a string"))?.to_string());
                }
                s.messages = Messages::new(o);
            }
        }
        if let Some(v) = relay {
            let t = v.as_table().ok_or("[relay] must be a table")?;
            if let Some(u) = t.get("url") {
                let u = u.as_str().ok_or("[relay] url must be a string")?.trim();
                if !u.is_empty() {
                    if !(u.starts_with("wss://") || u.starts_with("ws://127.0.0.1") || u.starts_with("ws://localhost")) {
                        return Err("[relay] url must be wss:// (ws:// only for a local `wrangler dev`)".into());
                    }
                    s.relay_url = Some(u.to_string());
                }
            }
            if let Some(q) = t.get("queue_url") {
                let q = q.as_str().ok_or("[relay] queue_url must be a string")?.trim();
                if !q.is_empty() {
                    s.queue_url = Some(q.to_string());
                }
            }
        }
        if s.queue_url.is_none() {
            s.queue_url = s.relay_url.as_deref().and_then(derive_queue_url);
        }
        Ok(s)
    }
}

/// `wss://host/link` → `https://host/queue` (same for ws → http).
pub fn derive_queue_url(link: &str) -> Option<String> {
    let (scheme, rest) = match link.strip_prefix("wss://") {
        Some(r) => ("https://", r),
        None => ("http://", link.strip_prefix("ws://")?),
    };
    let host = rest.split('/').next().filter(|h| !h.is_empty())?;
    Some(format!("{scheme}{host}/queue"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(s: &str) -> toml::Value {
        toml::Value::Table(s.parse().unwrap())
    }

    #[test]
    fn parses_and_derives() {
        let s = Settings::parse(
            Some(&t("region = \"gb\"\ncrossfade = \"1.5s\"\n[messages]\nadded = \"ok {title}\"")),
            Some(&t("url = \"wss://relay.example.com/link\"")),
        )
        .unwrap();
        assert_eq!(s.region, "GB");
        assert_eq!(s.crossfade_ms, 1500);
        assert_eq!(s.queue_url.as_deref(), Some("https://relay.example.com/queue"));
        assert_eq!(s.messages.template("added"), "ok {title}");
        assert_eq!(Settings::parse(None, None).unwrap(), Settings::default());
    }

    #[test]
    fn rejects_bad_values() {
        assert!(Settings::parse(Some(&t("region = \"USA\"")), None).is_err());
        assert!(Settings::parse(Some(&t("[messages]\nnope = \"x\"")), None).is_err());
        assert!(Settings::parse(None, Some(&t("url = \"ws://relay.example.com/link\""))).is_err(), "plain ws only for localhost");
        assert!(Settings::parse(None, Some(&t("url = \"ws://127.0.0.1:8787/link\""))).is_ok());
        assert!(Settings::parse(Some(&t("api_base = \"http://evil.example\"")), None).is_err());
        assert!(Settings::parse(Some(&t("metadata_base = \"http://evil.example\"")), None).is_err());
        assert!(Settings::parse(Some(&t("metadata = \"yes\"")), None).is_err());
        assert!(!Settings::parse(Some(&t("metadata = false")), None).unwrap().metadata);
        assert!(Settings::parse(None, None).unwrap().metadata, "on by default");
    }

    #[test]
    fn channel_configuration_requires_exact_identity() {
        let id = "UCz7OyuTD7kJJ6nJHko6r7ZQ";
        assert_eq!(Settings::parse(Some(&t(&format!("youtube_channel = \"{id}\"\nyoutube_delegate = \"123456\""))), None).unwrap().youtube_channel, id);
        assert!(Settings::parse(None, None).unwrap().youtube_channel.is_empty());
        assert!(Settings::parse(Some(&t("youtube_channel = \"\"")), None).unwrap().youtube_channel.is_empty());
        for invalid in ["@covers", "Dabs & Drum Covers", "UCshort", " UCz7OyuTD7kJJ6nJHko6r7ZQ", "UCz7OyuTD7kJJ6nJHko6r7Z!"] {
            assert!(Settings::parse(Some(&t(&format!("youtube_channel = \"{invalid}\""))), None).is_err(), "{invalid}");
        }
        assert!(Settings::parse(Some(&t(&format!("youtube_channel = \"{id}\""))), None).is_err());
        for invalid in ["-123", "12 34", "012x"] {
            assert!(Settings::parse(Some(&t(&format!("youtube_delegate = \"{invalid}\""))), None).is_err());
        }
    }
}
