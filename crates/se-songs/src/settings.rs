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
    pub daily_quota: u32,
    pub link_reserve: u32,
    pub cache_days: u32,
    pub search_results: u32,
    pub crossfade_ms: u32,
    pub api_base: String,
    pub messages: Messages,
    pub relay_url: Option<String>,
    pub queue_url: Option<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Settings {
            region: "US".into(),
            daily_quota: 10_000,
            link_reserve: 200,
            cache_days: 7,
            search_results: 5,
            crossfade_ms: 400,
            api_base: youtube::DEFAULT_BASE.into(),
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
                let b = b.as_str().ok_or("[songs] api_base must be a string")?;
                if !(b.starts_with("https://") || b.starts_with("http://127.0.0.1") || b.starts_with("http://localhost")) {
                    return Err("[songs] api_base must be https (or a localhost test server)".into());
                }
                s.api_base = b.trim_end_matches('/').to_string();
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
    }
}
