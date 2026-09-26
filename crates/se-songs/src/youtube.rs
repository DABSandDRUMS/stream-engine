//! YouTube Data API v3 client: `videos.list` (1 unit per call, up to 50 ids) and
//! `search.list` (100 units). Only an API key is needed (no OAuth).

use serde::{Deserialize, Serialize};
use serde_json::Value as J;
use std::time::Duration;

pub const DEFAULT_BASE: &str = "https://www.googleapis.com/youtube/v3";
pub const SEARCH_COST: u32 = 100;
pub const LIST_COST: u32 = 1;
/// A video id is exactly 11 characters of `[A-Za-z0-9_-]`.
pub const ID_LEN: usize = 11;

/// Everything we need to decide whether a video may be queued and played.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Video {
    pub id: String,
    pub title: String,
    pub channel_id: String,
    pub channel: String,
    pub duration_s: u32,
    pub embeddable: bool,
    /// `public`, `unlisted`, `private`.
    pub privacy: String,
    /// `processed`, `uploaded`, `failed`, `rejected`, `deleted`.
    pub upload_status: String,
    /// `none`, `live`, `upcoming`.
    pub live: String,
    /// When present, the video plays only in these regions.
    #[serde(default)]
    pub region_allowed: Option<Vec<String>>,
    #[serde(default)]
    pub region_blocked: Vec<String>,
    /// `contentRating.ytRating == ytAgeRestricted` (embeds can't play these).
    pub age_restricted: bool,
    /// Mature ratings from rating boards (MPAA R/NC-17, TV-MA) — used by the explicit filter.
    #[serde(default)]
    pub mature: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub category: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ApiError {
    /// Daily quota used up (`quotaExceeded` / `dailyLimitExceeded` / `rateLimitExceeded`).
    QuotaExceeded,
    /// The key is wrong, revoked, or restricted away from this API/IP.
    KeyInvalid(String),
    /// YouTube Data API v3 isn't enabled for the key's Google Cloud project.
    NotEnabled(String),
    /// Any other API error response.
    Rejected {
        status: u16,
        reason: String,
        message: String,
    },
    Network(String),
    Parse(String),
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiError::QuotaExceeded => write!(f, "YouTube API quota exceeded"),
            ApiError::KeyInvalid(m) => write!(f, "YouTube API key rejected: {m}"),
            ApiError::NotEnabled(m) => write!(f, "YouTube Data API v3 is not enabled for this key's project: {m}"),
            ApiError::Rejected { status, reason, message } => write!(f, "YouTube API error {status} {reason}: {message}"),
            ApiError::Network(m) => write!(f, "YouTube API unreachable: {m}"),
            ApiError::Parse(m) => write!(f, "unexpected YouTube API response: {m}"),
        }
    }
}

pub struct Client {
    http: reqwest::Client,
    base: String,
}

impl Client {
    pub fn new(base: &str) -> anyhow::Result<Client> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(10))
            .connect_timeout(Duration::from_secs(5))
            .user_agent(concat!("stream-engine/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Client { http, base: base.trim_end_matches('/').to_string() })
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    async fn get(&self, path: &str, key: &str, params: &[(&str, &str)]) -> Result<J, ApiError> {
        let url = format!("{}/{path}", self.base);
        let resp = self.http.get(&url).query(params).query(&[("key", key)]).send().await.map_err(|e| ApiError::Network(without_key(&e.to_string())))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.map_err(|e| ApiError::Network(without_key(&e.to_string())))?;
        let json: J = serde_json::from_str(&body).map_err(|e| ApiError::Parse(format!("{e} (HTTP {status})")))?;
        if status >= 400 || json.get("error").is_some() {
            return Err(classify_error(status, &json));
        }
        Ok(json)
    }

    /// `videos.list` for up to 50 ids (1 unit). Unknown/deleted ids are simply absent.
    pub async fn videos(&self, key: &str, ids: &[String]) -> Result<Vec<Video>, ApiError> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        let joined = ids.iter().take(50).cloned().collect::<Vec<_>>().join(",");
        let json = self.get("videos", key, &[("part", "snippet,contentDetails,status"), ("id", &joined), ("maxResults", "50")]).await?;
        parse_videos(&json)
    }

    /// `search.list` (100 units): ids of embeddable, syndicated videos in relevance order.
    pub async fn search(&self, key: &str, q: &str, region: &str, max: u32) -> Result<Vec<String>, ApiError> {
        let max = max.clamp(1, 25).to_string();
        let mut params = vec![
            ("part", "id"),
            ("type", "video"),
            ("videoEmbeddable", "true"),
            ("videoSyndicated", "true"),
            ("maxResults", max.as_str()),
            ("q", q),
            ("fields", "items(id(videoId))"),
        ];
        if !region.is_empty() {
            params.push(("regionCode", region));
        }
        let json = self.get("search", key, &params).await?;
        parse_search(&json)
    }
}

/// Never let the key leak into logs via reqwest's URL-bearing error messages.
fn without_key(s: &str) -> String {
    match s.find("key=") {
        Some(i) => {
            let end = s[i..].find(['&', ')', ' ']).map(|e| i + e).unwrap_or(s.len());
            format!("{}key=…{}", &s[..i], &s[end..])
        }
        None => s.to_string(),
    }
}

/// Map a Google API error body to [`ApiError`]. Handles both the classic `errors[].reason`
/// shape and the newer `details[].reason` (`API_KEY_INVALID`, `SERVICE_DISABLED`).
pub fn classify_error(status: u16, json: &J) -> ApiError {
    let err = json.get("error").unwrap_or(&J::Null);
    let message = err.get("message").and_then(J::as_str).unwrap_or("").to_string();
    let mut reasons: Vec<String> = Vec::new();
    for e in err.get("errors").and_then(J::as_array).into_iter().flatten() {
        if let Some(r) = e.get("reason").and_then(J::as_str) {
            reasons.push(r.to_string());
        }
    }
    for d in err.get("details").and_then(J::as_array).into_iter().flatten() {
        if let Some(r) = d.get("reason").and_then(J::as_str) {
            reasons.push(r.to_string());
        }
    }
    let has = |r: &str| reasons.iter().any(|x| x.eq_ignore_ascii_case(r));
    if has("quotaExceeded") || has("dailyLimitExceeded") || has("rateLimitExceeded") || has("RATE_LIMIT_EXCEEDED") {
        ApiError::QuotaExceeded
    } else if has("keyInvalid")
        || has("API_KEY_INVALID")
        || has("keyExpired")
        || has("API_KEY_SERVICE_BLOCKED")
        || has("ipRefererBlocked")
        || has("API_KEY_IP_ADDRESS_BLOCKED")
    {
        ApiError::KeyInvalid(message)
    } else if has("accessNotConfigured") || has("SERVICE_DISABLED") {
        ApiError::NotEnabled(message)
    } else {
        ApiError::Rejected { status, reason: reasons.first().cloned().unwrap_or_default(), message }
    }
}

fn s(v: &J, path: &[&str]) -> String {
    let mut cur = v;
    for p in path {
        cur = match cur.get(p) {
            Some(c) => c,
            None => return String::new(),
        };
    }
    cur.as_str().unwrap_or("").to_string()
}

fn strings(v: Option<&J>) -> Vec<String> {
    v.and_then(J::as_array).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default()
}

/// Parse a `videos.list` response.
pub fn parse_videos(json: &J) -> Result<Vec<Video>, ApiError> {
    let items = json.get("items").and_then(J::as_array).ok_or_else(|| ApiError::Parse("videos: no items".into()))?;
    let mut out = Vec::with_capacity(items.len());
    for it in items {
        let id = s(it, &["id"]);
        if !is_video_id(&id) {
            continue;
        }
        let cd = it.get("contentDetails").unwrap_or(&J::Null);
        let rating = cd.get("contentRating").unwrap_or(&J::Null);
        let rr = cd.get("regionRestriction");
        let mpaa = s(rating, &["mpaaRating"]);
        let tvpg = s(rating, &["tvpgRating"]);
        out.push(Video {
            title: s(it, &["snippet", "title"]),
            channel_id: s(it, &["snippet", "channelId"]),
            channel: s(it, &["snippet", "channelTitle"]),
            duration_s: parse_duration(&s(cd, &["duration"])).unwrap_or(0),
            embeddable: it.get("status").and_then(|st| st.get("embeddable")).and_then(J::as_bool).unwrap_or(false),
            privacy: s(it, &["status", "privacyStatus"]),
            upload_status: s(it, &["status", "uploadStatus"]),
            live: match s(it, &["snippet", "liveBroadcastContent"]) {
                l if l.is_empty() => "none".into(),
                l => l,
            },
            region_allowed: rr.and_then(|r| r.get("allowed")).map(|a| strings(Some(a))),
            region_blocked: strings(rr.and_then(|r| r.get("blocked"))),
            age_restricted: s(rating, &["ytRating"]) == "ytAgeRestricted",
            mature: matches!(mpaa.as_str(), "mpaaR" | "mpaaNc17") || tvpg == "tvpgMa",
            tags: strings(it.get("snippet").and_then(|sn| sn.get("tags"))),
            category: s(it, &["snippet", "categoryId"]),
            id,
        });
    }
    Ok(out)
}

/// Parse a `search.list` response into video ids (non-video results are skipped).
pub fn parse_search(json: &J) -> Result<Vec<String>, ApiError> {
    let items = json.get("items").and_then(J::as_array).ok_or_else(|| ApiError::Parse("search: no items".into()))?;
    let mut ids: Vec<String> = Vec::with_capacity(items.len());
    for it in items {
        let id = s(it, &["id", "videoId"]);
        if is_video_id(&id) && !ids.contains(&id) {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// ISO 8601 duration (`PT4M13S`, `PT1H2M`, `P1DT2H`, `P0D`) → whole seconds.
pub fn parse_duration(s: &str) -> Option<u32> {
    let rest = s.strip_prefix('P')?;
    let (mut total, mut num, mut in_time, mut any) = (0u64, String::new(), false, false);
    for c in rest.chars() {
        match c {
            'T' => in_time = true,
            '0'..='9' | '.' => num.push(c),
            unit => {
                let n: f64 = num.parse().ok()?;
                num.clear();
                any = true;
                let mult = match (unit, in_time) {
                    ('W', false) => 604_800.0,
                    ('D', false) => 86_400.0,
                    ('H', true) => 3_600.0,
                    ('M', true) => 60.0,
                    ('S', true) => 1.0,
                    _ => return None,
                };
                total += (n * mult) as u64;
            }
        }
    }
    (any && num.is_empty()).then(|| total.min(u32::MAX as u64) as u32)
}

pub fn is_video_id(s: &str) -> bool {
    s.len() == ID_LEN && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// Find a video id in request text: any YouTube URL form (`watch?v=`, `youtu.be/`, `shorts/`,
/// `embed/`, `live/`, music/mobile/nocookie hosts) or a bare id that can't be a plain word.
pub fn extract_video_id(text: &str) -> Option<String> {
    let text = text.trim();
    for tok in text.split_whitespace() {
        let tok = tok.trim_matches(|c| c == '<' || c == '>' || c == '(' || c == ')' || c == '"' || c == '\'');
        if let Some(id) = id_from_url(tok) {
            return Some(id);
        }
    }
    // A lone token that is id-shaped and not an ordinary word.
    let mut toks = text.split_whitespace();
    if let (Some(t), None) = (toks.next(), toks.next())
        && is_video_id(t)
        && looks_like_id(t)
    {
        return Some(t.to_string());
    }
    None
}

fn looks_like_id(t: &str) -> bool {
    t.bytes().any(|b| b.is_ascii_digit() || b == b'-' || b == b'_')
        || t.bytes().skip(1).any(|b| b.is_ascii_uppercase()) && t.bytes().any(|b| b.is_ascii_lowercase())
}

fn id_from_url(tok: &str) -> Option<String> {
    let lower = tok.to_ascii_lowercase();
    let after_scheme = lower.strip_prefix("https://").or_else(|| lower.strip_prefix("http://")).unwrap_or(&lower);
    let offset = tok.len() - after_scheme.len();
    let i = after_scheme.find('/')?;
    let (host, path) = (&after_scheme[..i], &tok[offset + i..]);
    let host = host.strip_prefix("www.").unwrap_or(host);
    let take = |s: &str| -> Option<String> {
        let id: String = s.chars().take_while(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_').collect();
        is_video_id(&id).then_some(id)
    };
    match host {
        "youtu.be" => take(path.trim_start_matches('/')),
        "youtube.com" | "m.youtube.com" | "music.youtube.com" | "youtube-nocookie.com" | "gaming.youtube.com" => {
            let (p, query) = match path.find('?') {
                Some(i) => (&path[..i], &path[i + 1..]),
                None => (path, ""),
            };
            if p == "/watch" || p == "/watch/" {
                for kv in query.split('&') {
                    if let Some(v) = kv.strip_prefix("v=") {
                        return take(v);
                    }
                }
                return None;
            }
            for prefix in ["/shorts/", "/embed/", "/live/", "/v/", "/e/"] {
                if let Some(rest) = p.strip_prefix(prefix) {
                    return take(rest);
                }
            }
            None
        }
        _ => None,
    }
}

/// Canonical watch link for chat and the UI.
pub fn watch_url(id: &str) -> String {
    format!("https://youtu.be/{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> J {
        let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/youtube").join(name);
        serde_json::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap()
    }

    #[test]
    fn durations() {
        assert_eq!(parse_duration("PT3M34S"), Some(214));
        assert_eq!(parse_duration("PT1H2M3S"), Some(3723));
        assert_eq!(parse_duration("PT45S"), Some(45));
        assert_eq!(parse_duration("PT10M"), Some(600));
        assert_eq!(parse_duration("P1DT1S"), Some(86_401));
        assert_eq!(parse_duration("P0D"), Some(0));
        assert_eq!(parse_duration("PT"), None);
        assert_eq!(parse_duration("3:34"), None);
        assert_eq!(parse_duration("PT3X"), None);
    }

    #[test]
    fn ids_from_links() {
        let id = Some("dQw4w9WgXcQ".to_string());
        for t in [
            "https://www.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtube.com/watch?feature=share&v=dQw4w9WgXcQ&t=42s",
            "youtube.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/dQw4w9WgXcQ?si=abc",
            "http://m.youtube.com/watch?v=dQw4w9WgXcQ",
            "https://music.youtube.com/watch?v=dQw4w9WgXcQ&list=RDAMVM",
            "https://www.youtube.com/shorts/dQw4w9WgXcQ",
            "https://www.youtube.com/embed/dQw4w9WgXcQ?start=3",
            "https://www.youtube-nocookie.com/embed/dQw4w9WgXcQ",
            "https://www.youtube.com/live/dQw4w9WgXcQ",
            "please play <https://youtu.be/dQw4w9WgXcQ> thanks",
            "dQw4w9WgXcQ",
        ] {
            assert_eq!(extract_video_id(t), id, "{t}");
        }
        for t in [
            "never gonna give you up",
            "Beautifully",
            "https://example.com/watch?v=dQw4w9WgXcQ",
            "https://youtu.be/short",
            "https://www.youtube.com/@RickAstleyYT",
        ] {
            assert_eq!(extract_video_id(t), None, "{t}");
        }
    }

    #[test]
    fn parses_recorded_videos_list() {
        let v = parse_videos(&fixture("videos_ok.json")).unwrap();
        assert_eq!(v.len(), 1);
        let v = &v[0];
        assert_eq!(v.id, "dQw4w9WgXcQ");
        assert_eq!(v.duration_s, 213);
        assert!(v.embeddable && !v.age_restricted);
        assert_eq!(v.channel, "Rick Astley");
        assert_eq!(v.privacy, "public");
        assert_eq!(v.live, "none");
        assert!(v.region_allowed.is_none() && v.region_blocked.is_empty());
    }

    #[test]
    fn parses_restrictions() {
        let v = parse_videos(&fixture("videos_restricted.json")).unwrap();
        let by = |id: &str| v.iter().find(|x| x.id == id).unwrap().clone();
        assert!(!by("NoEmbed0001").embeddable);
        assert!(by("AgeGate0001").age_restricted);
        assert_eq!(by("RegionBlk01").region_blocked, vec!["US".to_string(), "DE".to_string()]);
        assert_eq!(by("RegionAlw01").region_allowed, Some(vec!["JP".to_string()]));
        assert_eq!(by("LiveNow0001").live, "live");
        assert_eq!(by("LiveNow0001").duration_s, 0);
        assert!(by("MatureR0001").mature);
    }

    #[test]
    fn parses_search() {
        let ids = parse_search(&fixture("search_ok.json")).unwrap();
        assert_eq!(ids, vec!["fJ9rUzIMcZQ", "NoEmbed0001", "yk3prd8GER4"]);
    }

    #[test]
    fn classifies_errors() {
        assert_eq!(classify_error(403, &fixture("error_quota.json")), ApiError::QuotaExceeded);
        assert!(matches!(classify_error(400, &fixture("error_key_invalid.json")), ApiError::KeyInvalid(_)));
        assert!(matches!(classify_error(403, &fixture("error_not_enabled.json")), ApiError::NotEnabled(_)));
    }

    #[test]
    fn key_is_scrubbed_from_errors() {
        let s = without_key("error sending request for url (https://x/youtube/v3/videos?part=id&key=AIzaSECRET)");
        assert!(!s.contains("SECRET"), "{s}");
    }
}
