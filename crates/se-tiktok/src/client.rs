//! The real transport (`client` feature): TikTok web for the room lookup, an
//! Euler-Stream-compatible sign provider for the signed socket URL, and the webcast WebSocket.
//!
//! Flow and constants follow zerodytrash/TikTok-Live-Connector @ `8a923300` (2026-09-22) and
//! isaackogan/TikTokLive 7.0.1 @ `fc73b8f6` (2026-09-09):
//! * room: `GET https://www.tiktok.com/api-live/user/room/?…&uniqueId=<id>&sourceType=54`
//!   (`data.user.roomId`, `data.liveRoom.status`, 4 = offline), falling back to the
//!   `SIGI_STATE` JSON embedded in `https://www.tiktok.com/@<id>/live`;
//! * sign: `GET <sign_url>/webcast/rooms/<room_id>/connect?client=…&user_agent=…&client_enter=true&platform=web`
//!   with `X-Api-Key` when a key is set → protobuf `ProtoMessageFetchResult` body,
//!   `X-Set-TT-Cookie` (cookies for the socket) and `X-Room-Id`; 429 = rate limited,
//!   401/402/403 = key/plan refused;
//! * socket: `push_server?<client params + route params>&version_code=270000` with the
//!   anonymous cookies and the same User-Agent the provider signed with.
//!
//! TLS uses rustls with an explicit `ring` provider and the webpki roots, so it never depends
//! on which crypto provider other crates enable process-wide.

use crate::config::SIGN_KEY_SECRET;
use crate::proto::ProtoMessageFetchResult;
use crate::transport::{RoomStatus, SignedConnect, Socket, Transport, TransportError};
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use prost::Message as _;
use serde_json::Value as Json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

pub const DEFAULT_WEB_BASE: &str = "https://www.tiktok.com";
/// `client` parameter for the sign provider (its metrics only).
const CLIENT_ID: &str = "stream-engine";
/// The browser profile TikTok-Live-Connector uses by default (Edge on macOS).
pub const USER_AGENT: &str =
    "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/129.0.0.0 Safari/537.36 Edg/128.0.2739.79";
const BROWSER_PLATFORM: &str = "MacIntel";
const OS: &str = "mac";
const LANG: &str = "en";
const LANG_COUNTRY: &str = "en-US";
const COUNTRY: &str = "US";
const TZ_NAME: &str = "America/New_York";
const SCREEN: (&str, &str) = ("1920", "1080");
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);
const WS_CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const KEYRING_TIMEOUT: Duration = Duration::from_secs(5);
/// Cookies that identify a logged-in session; never sent (this client is anonymous).
const SESSION_COOKIES: &[&str] = &["sessionid", "sessionid_ss", "sid_tt", "sid_guard", "tt-target-idc"];
const COOKIE_ATTRS: &[&str] = &["path", "domain", "expires", "max-age", "secure", "httponly", "samesite", "priority", "partitioned"];

fn browser_name() -> &'static str {
    USER_AGENT.split_once('/').map_or("Mozilla", |(n, _)| n)
}

fn browser_version() -> &'static str {
    USER_AGENT.split_once('/').map_or("", |(_, v)| v)
}

/// Where the sign-provider API key comes from.
#[derive(Clone, Debug)]
pub enum KeySource {
    /// Keyring secret `tiktok.sign_api_key`, read on every sign request.
    Keyring,
    Fixed(Option<String>),
}

pub struct WebTransport {
    http: reqwest::Client,
    tls: Arc<rustls::ClientConfig>,
    web_base: String,
    sign_url: String,
    key: KeySource,
    device_id: String,
    /// Cookies TikTok web set (ttwid, …), forwarded to the socket.
    cookies: Mutex<BTreeMap<String, String>>,
}

fn tls_config() -> Result<Arc<rustls::ClientConfig>, TransportError> {
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    let cfg = rustls::ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions()
        .map_err(|e| TransportError::Protocol(format!("TLS setup: {e}")))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    Ok(Arc::new(cfg))
}

fn net(e: reqwest::Error) -> TransportError {
    TransportError::Network(e.without_url().to_string())
}

fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n { s.to_string() } else { format!("{}…", s.chars().take(n).collect::<String>()) }
}

/// `name=value` pairs of a cookie list (`a=1; Path=/; b=2`), attributes skipped.
pub fn parse_cookie_list(s: &str) -> Vec<(String, String)> {
    s.split(';')
        .filter_map(|p| {
            let (k, v) = p.trim().split_once('=')?;
            let k = k.trim();
            (!k.is_empty() && !COOKIE_ATTRS.contains(&k.to_ascii_lowercase().as_str())).then(|| (k.to_string(), v.trim().to_string()))
        })
        .collect()
}

fn json_id(v: &Json) -> Option<String> {
    match v {
        Json::String(s) if !s.is_empty() && s != "0" => Some(s.clone()),
        Json::Number(n) if n.as_u64().is_some_and(|n| n > 0) => Some(n.to_string()),
        _ => None,
    }
}

fn room_from_info(user: &Json, live_room: &Json) -> RoomStatus {
    let Some(room_id) = json_id(&user["roomId"]).or_else(|| json_id(&live_room["roomId"])) else { return RoomStatus::Offline };
    let status = live_room["status"].as_i64().or_else(|| user["status"].as_i64());
    if status == Some(4) { RoomStatus::Offline } else { RoomStatus::Live { room_id } }
}

/// Parse `/api-live/user/room/`.
pub fn parse_api_live(body: &[u8]) -> Result<RoomStatus, TransportError> {
    let v: Json = serde_json::from_slice(body).map_err(|_| TransportError::Blocked("api-live answered with non-JSON (rate limited or captcha)".into()))?;
    let message = v["message"].as_str().unwrap_or("");
    if message == "user_not_found" {
        return Err(TransportError::NotFound("TikTok user does not exist or cannot go LIVE".into()));
    }
    let code = v["statusCode"].as_i64().unwrap_or(0);
    if code != 0 {
        return Err(TransportError::Protocol(format!("api-live error {code} ({message})")));
    }
    let data = &v["data"];
    if data["user"].is_null() {
        return Err(TransportError::Protocol("api-live response has no data.user".into()));
    }
    Ok(room_from_info(&data["user"], &data["liveRoom"]))
}

/// Parse the `SIGI_STATE` blob of `/@<id>/live`.
pub fn parse_live_html(html: &str) -> Result<RoomStatus, TransportError> {
    const TAG: &str = "<script id=\"SIGI_STATE\" type=\"application/json\">";
    let start = html.find(TAG).ok_or_else(|| TransportError::Blocked("live page has no SIGI_STATE (captcha or page layout changed)".into()))? + TAG.len();
    let end = html[start..].find("</script>").ok_or_else(|| TransportError::Protocol("unterminated SIGI_STATE".into()))?;
    let v: Json = serde_json::from_str(&html[start..start + end]).map_err(|_| TransportError::Protocol("SIGI_STATE is not JSON".into()))?;
    let live = &v["LiveRoom"];
    if live.is_null() {
        return Err(TransportError::NotFound("TikTok user has never gone LIVE or does not exist".into()));
    }
    let info = &live["liveRoomUserInfo"];
    if info["user"].is_null() {
        return Err(TransportError::Protocol("SIGI_STATE has no LiveRoom.liveRoomUserInfo.user".into()));
    }
    Ok(room_from_info(&info["user"], &info["liveRoom"]))
}

/// Result of a successful sign request.
#[derive(Debug, PartialEq)]
pub struct SignResponse {
    pub fetch: ProtoMessageFetchResult,
    pub cookies: Vec<(String, String)>,
    pub room_id: Option<String>,
}

fn json_message(body: &[u8]) -> Option<String> {
    let v: Json = serde_json::from_slice(body).ok()?;
    let msg = v["message"].as_str()?.to_string();
    Some(match v["limit_label"].as_str() {
        Some(l) => format!("({l}) {msg}"),
        None => msg,
    })
}

/// Classify the sign provider's answer (`header` looks up a response header).
pub fn parse_sign_response(status: u16, header: impl Fn(&str) -> Option<String>, body: &[u8]) -> Result<SignResponse, TransportError> {
    let text = || json_message(body).unwrap_or_else(|| truncate(String::from_utf8_lossy(body).trim(), 300));
    match status {
        200 => {}
        429 => {
            let secs = header("retry-after").or_else(|| header("ratelimit-reset")).and_then(|s| s.trim().parse::<u64>().ok());
            return Err(TransportError::RateLimited {
                message: json_message(body).unwrap_or_else(|| "too many connections".into()),
                retry_after: secs.map(Duration::from_secs),
            });
        }
        401 | 403 => return Err(TransportError::Auth(format!("HTTP {status}: {}", text()))),
        402 => return Err(TransportError::Auth(format!("needs a paid plan: {}", text()))),
        500.. => return Err(TransportError::Network(format!("sign provider HTTP {status}: {}", text()))),
        _ => return Err(TransportError::Protocol(format!("sign provider HTTP {status}: {}", text()))),
    }
    if body.is_empty() {
        return Err(TransportError::Protocol("sign provider returned an empty body".into()));
    }
    let cookie_header = header("x-set-tt-cookie").filter(|c| !c.trim().is_empty()).ok_or_else(|| {
        let hint = header("handshake-msg").map(|m| format!(" ({m})")).unwrap_or_default();
        TransportError::Protocol(format!("sign provider returned no cookies{hint}"))
    })?;
    let fetch = ProtoMessageFetchResult::decode(body).map_err(|e| TransportError::Protocol(format!("sign response is not a fetch result: {e}")))?;
    if fetch.cursor.is_empty() {
        return Err(TransportError::Protocol("sign response has no cursor".into()));
    }
    if !(fetch.push_server.starts_with("wss://") || fetch.push_server.starts_with("ws://")) {
        return Err(TransportError::Protocol(format!("sign response has no push server (`{}`)", truncate(&fetch.push_server, 80))));
    }
    Ok(SignResponse { fetch, cookies: parse_cookie_list(&cookie_header), room_id: header("x-room-id").filter(|r| !r.is_empty()) })
}

fn set(params: &mut Vec<(String, String)>, k: &str, v: &str) {
    match params.iter_mut().find(|(pk, _)| pk == k) {
        Some(p) => p.1 = v.to_string(),
        None => params.push((k.to_string(), v.to_string())),
    }
}

/// Socket URL: client params, then the connection params (route params from the provider
/// override), all form-encoded, plus the duplicate `version_code` web clients send.
pub fn build_ws_url(fetch: &ProtoMessageFetchResult, room_id: &str) -> String {
    let mut p: Vec<(String, String)> = [
        ("version_code", "180800"),
        ("aid", "1988"),
        ("app_language", LANG),
        ("app_name", "tiktok_web"),
        ("browser_platform", BROWSER_PLATFORM),
        ("browser_language", LANG_COUNTRY),
        ("browser_name", browser_name()),
        ("browser_version", browser_version()),
        ("browser_online", "true"),
        ("cookie_enabled", "true"),
        ("tz_name", TZ_NAME),
        ("device_platform", "web"),
        ("identity", "audience"),
        ("live_id", "12"),
        ("webcast_language", LANG),
        ("ws_direct", "0"),
        ("sup_ws_ds_opt", "1"),
        ("update_version_code", "2.0.0"),
        ("did_rule", "3"),
        ("screen_height", SCREEN.1),
        ("screen_width", SCREEN.0),
        ("heartbeat_duration", "0"),
        ("resp_content_type", "protobuf"),
        ("history_comment_count", "6"),
        ("client_enter", "1"),
        ("last_rtt", "150"),
    ]
    .iter()
    .map(|(k, v)| (k.to_string(), v.to_string()))
    .collect();
    set(&mut p, "compress", "gzip");
    set(&mut p, "room_id", room_id);
    set(&mut p, "internal_ext", &fetch.internal_ext);
    set(&mut p, "cursor", &fetch.cursor);
    for (k, v) in &fetch.route_params {
        if !v.is_empty() {
            set(&mut p, k, v);
        }
    }
    let query = url::form_urlencoded::Serializer::new(String::new()).extend_pairs(p).finish();
    let sep = if fetch.push_server.contains('?') { '&' } else { '?' };
    format!("{}{sep}{query}&version_code=270000", fetch.push_server)
}

/// `ping-interval` (seconds) from the `Handshake-Options` response header.
pub fn parse_ping_interval(options: &str) -> Option<Duration> {
    parse_cookie_list(options)
        .into_iter()
        .find(|(k, _)| k == "ping-interval")
        .and_then(|(_, v)| v.parse::<f64>().ok())
        .filter(|s| *s > 0.0 && *s < 3600.0)
        .map(Duration::from_secs_f64)
}

fn sign_endpoint(sign_url: &str, room_id: &str) -> Result<url::Url, TransportError> {
    let s = if sign_url.contains("{room_id}") {
        sign_url.replace("{room_id}", room_id)
    } else {
        format!("{}/webcast/rooms/{room_id}/connect", sign_url.trim_end_matches('/'))
    };
    url::Url::parse(&s).map_err(|e| TransportError::Protocol(format!("sign_url: {e}")))
}

impl WebTransport {
    /// Transport signing through `sign_url`, API key from the keyring.
    pub fn new(sign_url: &str) -> Result<Self, TransportError> {
        let tls = tls_config()?;
        let http = reqwest::Client::builder()
            .use_preconfigured_tls((*tls).clone())
            .timeout(HTTP_TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| TransportError::Protocol(format!("HTTP client: {e}")))?;
        let mut rng = crate::backoff::Rng::from_time();
        // 19 random digits (no leading zero), like the web client's device_id
        let device_id = (0..19).map(|i| char::from(b'0' + (rng.next_u64() % if i == 0 { 9 } else { 10 }) as u8 + u8::from(i == 0))).collect();
        Ok(WebTransport {
            http,
            tls,
            web_base: DEFAULT_WEB_BASE.into(),
            sign_url: sign_url.trim_end_matches('/').to_string(),
            key: KeySource::Keyring,
            device_id,
            cookies: Mutex::new(BTreeMap::new()),
        })
    }

    pub fn with_key(mut self, key: KeySource) -> Self {
        self.key = key;
        self
    }

    /// Point the room lookup somewhere else (tests).
    pub fn with_web_base(mut self, base: &str) -> Self {
        self.web_base = base.trim_end_matches('/').to_string();
        self
    }

    async fn api_key(&self) -> Option<String> {
        match &self.key {
            KeySource::Fixed(k) => k.clone().filter(|k| !k.trim().is_empty()),
            KeySource::Keyring => {
                let read = tokio::task::spawn_blocking(|| se_store::secrets::get(SIGN_KEY_SECRET));
                match tokio::time::timeout(KEYRING_TIMEOUT, read).await {
                    Ok(Ok(Ok(k))) => k.filter(|k| !k.trim().is_empty()),
                    Ok(Ok(Err(e))) => {
                        tracing::warn!(target: "tiktok", "keyring unavailable ({e:#}); signing anonymously");
                        None
                    }
                    _ => {
                        tracing::warn!(target: "tiktok", "keyring did not answer; signing anonymously");
                        None
                    }
                }
            }
        }
    }

    fn web_params(&self) -> Vec<(&'static str, String)> {
        [
            ("aid", "1988"),
            ("app_language", LANG),
            ("app_name", "tiktok_web"),
            ("browser_language", LANG_COUNTRY),
            ("browser_name", browser_name()),
            ("browser_online", "true"),
            ("browser_platform", BROWSER_PLATFORM),
            ("browser_version", browser_version()),
            ("cookie_enabled", "true"),
            ("device_platform", "web_pc"),
            ("focus_state", "true"),
            ("from_page", "user"),
            ("history_len", "5"),
            ("is_fullscreen", "false"),
            ("is_page_visible", "true"),
            ("screen_height", SCREEN.1),
            ("screen_width", SCREEN.0),
            ("tz_name", TZ_NAME),
            ("channel", "tiktok_web"),
            ("data_collection_enabled", "true"),
            ("os", OS),
            ("priority_region", COUNTRY),
            ("region", COUNTRY),
            ("user_is_login", "false"),
            ("webcast_language", LANG),
        ]
        .iter()
        .map(|(k, v)| (*k, v.to_string()))
        .chain(std::iter::once(("device_id", self.device_id.clone())))
        .collect()
    }

    fn cookie_header(&self, extra: &[(String, String)]) -> String {
        let mut all = self.cookies.lock().clone();
        for (k, v) in extra {
            all.insert(k.clone(), v.clone());
        }
        all.iter().filter(|(k, _)| !SESSION_COOKIES.contains(&k.as_str())).map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("; ")
    }

    /// GET from TikTok web with browser headers; remembers the cookies it sets.
    async fn tiktok_get(&self, url: url::Url) -> Result<(u16, Vec<u8>), TransportError> {
        let mut cookie = String::from("tt-target-idc=useast1a");
        let jar = self.cookie_header(&[]);
        if !jar.is_empty() {
            cookie.push_str("; ");
            cookie.push_str(&jar);
        }
        let resp = self
            .http
            .get(url)
            .header("User-Agent", USER_AGENT)
            .header("Accept", "text/html,application/json,application/protobuf")
            .header("Accept-Language", "en-US,en;q=0.9")
            .header("Referer", "https://www.tiktok.com/")
            .header("Origin", "https://www.tiktok.com")
            .header("Cache-Control", "max-age=0")
            .header("Sec-Fetch-Site", "same-site")
            .header("Sec-Fetch-Mode", "cors")
            .header("Sec-Fetch-Dest", "empty")
            .header("Sec-Fetch-Ua-Mobile", "?0")
            .header("Cookie", cookie)
            .send()
            .await
            .map_err(net)?;
        {
            let mut jar = self.cookies.lock();
            for v in resp.headers().get_all("set-cookie") {
                if let Some((k, v)) = v.to_str().ok().and_then(|s| parse_cookie_list(s).into_iter().next()) {
                    jar.insert(k, v);
                }
            }
        }
        let status = resp.status().as_u16();
        let body = resp.bytes().await.map_err(net)?.to_vec();
        Ok((status, body))
    }

    async fn room_from_api(&self, uid: &str) -> Result<RoomStatus, TransportError> {
        let mut url = url::Url::parse(&format!("{}/api-live/user/room/", self.web_base)).map_err(|e| TransportError::Protocol(e.to_string()))?;
        url.query_pairs_mut().extend_pairs(self.web_params()).append_pair("uniqueId", uid).append_pair("sourceType", "54");
        let (status, body) = self.tiktok_get(url).await?;
        if status >= 500 {
            return Err(TransportError::Network(format!("api-live HTTP {status}")));
        }
        parse_api_live(&body)
    }

    async fn room_from_html(&self, uid: &str) -> Result<RoomStatus, TransportError> {
        let url = url::Url::parse(&format!("{}/@{uid}/live", self.web_base)).map_err(|e| TransportError::Protocol(e.to_string()))?;
        let (status, body) = self.tiktok_get(url).await?;
        if status == 404 {
            return Err(TransportError::NotFound("TikTok user does not exist".into()));
        }
        if status >= 500 {
            return Err(TransportError::Network(format!("live page HTTP {status}")));
        }
        parse_live_html(&String::from_utf8_lossy(&body))
    }
}

/// The open webcast socket.
pub struct WsSocket {
    ws: WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>,
    ping: Option<Duration>,
}

impl Socket for WsSocket {
    async fn recv(&mut self) -> Option<Result<Vec<u8>, TransportError>> {
        loop {
            match self.ws.next().await? {
                Ok(Message::Binary(b)) => return Some(Ok(b.to_vec())),
                Ok(Message::Close(_)) => return None,
                Ok(_) => continue,
                Err(e) => return Some(Err(TransportError::Network(e.to_string()))),
            }
        }
    }

    async fn send(&mut self, frame: Vec<u8>) -> Result<(), TransportError> {
        self.ws.send(Message::Binary(frame.into())).await.map_err(|e| TransportError::Network(e.to_string()))
    }

    async fn close(&mut self) {
        let _ = tokio::time::timeout(Duration::from_secs(2), self.ws.close(None)).await;
    }

    fn ping_interval(&self) -> Option<Duration> {
        self.ping
    }
}

fn ws_error(e: tungstenite::Error) -> TransportError {
    match e {
        tungstenite::Error::Http(resp) => {
            let msg = resp.headers().get("handshake-msg").and_then(|v| v.to_str().ok()).unwrap_or("no reason given");
            TransportError::Blocked(format!("socket handshake rejected (HTTP {}): {msg}", resp.status().as_u16()))
        }
        other => TransportError::Network(format!("socket: {other}")),
    }
}

impl Transport for WebTransport {
    type Socket = WsSocket;

    async fn room_status(&self, unique_id: &str) -> Result<RoomStatus, TransportError> {
        let api = match self.room_from_api(unique_id).await {
            Ok(s) => return Ok(s),
            Err(e @ TransportError::NotFound(_)) => return Err(e),
            Err(e) => e,
        };
        match self.room_from_html(unique_id).await {
            Ok(s) => Ok(s),
            Err(e @ TransportError::NotFound(_)) => Err(e),
            Err(html) => {
                let msg = format!("room lookup failed (api-live: {api}; live page: {html})");
                Err(match (api, html) {
                    (TransportError::Network(_), _) | (_, TransportError::Network(_)) => TransportError::Network(msg),
                    (_, TransportError::Blocked(_)) => TransportError::Blocked(msg),
                    _ => TransportError::Protocol(msg),
                })
            }
        }
    }

    async fn sign(&self, room_id: &str) -> Result<SignedConnect, TransportError> {
        let key = self.api_key().await;
        let mut url = sign_endpoint(&self.sign_url, room_id)?;
        url.query_pairs_mut()
            .append_pair("client", CLIENT_ID)
            .append_pair("user_agent", USER_AGENT)
            .append_pair("client_enter", "true")
            .append_pair("platform", "web");
        let mut req =
            self.http.get(url).header("Accept", "application/protobuf,application/json").header("User-Agent", concat!("se-tiktok/", env!("CARGO_PKG_VERSION")));
        if let Some(k) = &key {
            req = req.header("X-Api-Key", k);
        }
        let resp = req.send().await.map_err(net)?;
        let status = resp.status().as_u16();
        let headers = resp.headers().clone();
        let body = resp.bytes().await.map_err(net)?;
        let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok()).map(str::to_string);
        let signed = parse_sign_response(status, header, &body)?;
        let room_id = signed.room_id.unwrap_or_else(|| room_id.to_string());
        Ok(SignedConnect {
            ws_url: build_ws_url(&signed.fetch, &room_id),
            cookie: self.cookie_header(&signed.cookies),
            user_agent: USER_AGENT.into(),
            room_id,
            initial: signed.fetch,
            authenticated: key.is_some(),
        })
    }

    async fn open(&self, signed: &SignedConnect) -> Result<WsSocket, TransportError> {
        let mut req = signed.ws_url.as_str().into_client_request().map_err(|e| TransportError::Protocol(format!("socket URL: {e}")))?;
        let h = req.headers_mut();
        h.insert("User-Agent", signed.user_agent.parse().map_err(|_| TransportError::Protocol("bad user agent".into()))?);
        if !signed.cookie.is_empty() {
            h.insert("Cookie", signed.cookie.parse().map_err(|_| TransportError::Protocol("bad cookie header".into()))?);
        }
        let connect = tokio_tungstenite::connect_async_tls_with_config(req, None, false, Some(Connector::Rustls(self.tls.clone())));
        let (ws, resp) = tokio::time::timeout(WS_CONNECT_TIMEOUT, connect)
            .await
            .map_err(|_| TransportError::Network("socket connect timed out".into()))?
            .map_err(ws_error)?;
        let ping = resp.headers().get("handshake-options").and_then(|v| v.to_str().ok()).and_then(parse_ping_interval);
        Ok(WsSocket { ws, ping })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Settings;
    use crate::frame;
    use crate::proto::*;
    use crate::session::testing::Recorder;
    use crate::session::{LinkState, Out, Shared};
    use axum::extract::ws::{Message as AxMessage, WebSocketUpgrade};
    use axum::extract::{Query, State};
    use axum::http::{HeaderMap, StatusCode};
    use axum::response::{IntoResponse, Response};
    use axum::routing::get;
    use se_proto::Value;
    use std::collections::HashMap;
    use tokio::sync::{mpsc, watch};

    const ROOM: &str = "7140000000000000001";

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/fixtures/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn api_live_statuses() {
        let live = br#"{"statusCode":0,"data":{"user":{"roomId":"7140000000000000001","status":2},"liveRoom":{"status":2}}}"#;
        assert_eq!(parse_api_live(live).unwrap(), RoomStatus::Live { room_id: ROOM.into() });
        let off = br#"{"statusCode":0,"data":{"user":{"roomId":"7140000000000000001"},"liveRoom":{"status":4}}}"#;
        assert_eq!(parse_api_live(off).unwrap(), RoomStatus::Offline);
        let never = br#"{"statusCode":0,"data":{"user":{"roomId":""}}}"#;
        assert_eq!(parse_api_live(never).unwrap(), RoomStatus::Offline);
        assert!(matches!(parse_api_live(br#"{"statusCode":19881007,"message":"user_not_found"}"#), Err(TransportError::NotFound(_))));
        assert!(matches!(parse_api_live(b"<html>captcha</html>"), Err(TransportError::Blocked(_))));
    }

    #[test]
    fn live_page_sigi_state() {
        let html = |state: &str| format!("<html><body><script id=\"SIGI_STATE\" type=\"application/json\">{state}</script></body></html>");
        let live = html(r#"{"LiveRoom":{"liveRoomUserInfo":{"user":{"roomId":"7140000000000000001"},"liveRoom":{"status":2}}}}"#);
        assert_eq!(parse_live_html(&live).unwrap(), RoomStatus::Live { room_id: ROOM.into() });
        let off = html(r#"{"LiveRoom":{"liveRoomUserInfo":{"user":{"roomId":"7140000000000000001","status":4}}}}"#);
        assert_eq!(parse_live_html(&off).unwrap(), RoomStatus::Offline);
        assert!(matches!(parse_live_html(&html(r#"{"AppContext":{}}"#)), Err(TransportError::NotFound(_))));
        assert!(matches!(parse_live_html("<html>verify you are human</html>"), Err(TransportError::Blocked(_))));
    }

    #[test]
    fn sign_responses_are_classified() {
        let none = |_: &str| None;
        let r = parse_sign_response(429, |h| (h == "ratelimit-reset").then(|| "42".into()), br#"{"message":"slow down","limit_label":"minute"}"#).unwrap_err();
        assert_eq!(r, TransportError::RateLimited { message: "(minute) slow down".into(), retry_after: Some(Duration::from_secs(42)) });
        assert!(matches!(parse_sign_response(401, none, br#"{"message":"Invalid API key"}"#), Err(TransportError::Auth(m)) if m.contains("Invalid API key")));
        assert!(matches!(parse_sign_response(402, none, b"{}"), Err(TransportError::Auth(_))));
        assert!(matches!(parse_sign_response(503, none, b"down"), Err(TransportError::Network(_))));
        let body = fixture("sign_response.bin");
        assert!(matches!(parse_sign_response(200, none, &body), Err(TransportError::Protocol(m)) if m.contains("no cookies")));
        let ok = parse_sign_response(200, |h| (h == "x-set-tt-cookie").then(|| "ttwid=1%7Cabc; Path=/; Secure; tt_chain_token=z".into()), &body).unwrap();
        assert_eq!(ok.cookies, vec![("ttwid".into(), "1%7Cabc".into()), ("tt_chain_token".into(), "z".into())]);
        assert_eq!(ok.fetch.push_server, "wss://webcast-ws.tiktok.com/webcast/im/ws_proxy/ws_reuse_supplement/");
    }

    #[test]
    fn socket_url_carries_cursor_route_params_and_encoding() {
        let fetch = ProtoMessageFetchResult {
            cursor: "t-1_r-1".into(),
            internal_ext: "fetch_time:1|seq:2".into(),
            push_server: "wss://push.example/ws/".into(),
            route_params: [("wrss".to_string(), "abc".to_string()), ("user_agent".to_string(), "A B (C)".to_string()), ("empty".to_string(), String::new())]
                .into(),
            ..Default::default()
        };
        let u = build_ws_url(&fetch, ROOM);
        assert!(u.starts_with("wss://push.example/ws/?version_code=180800&aid=1988&"));
        assert!(u.ends_with("&version_code=270000"));
        let q: HashMap<String, String> = url::Url::parse(&u).unwrap().query_pairs().map(|(k, v)| (k.into(), v.into())).collect();
        assert_eq!(q["room_id"], ROOM);
        assert_eq!(q["cursor"], "t-1_r-1");
        assert_eq!(q["internal_ext"], "fetch_time:1|seq:2");
        assert_eq!(q["wrss"], "abc");
        assert_eq!(q["compress"], "gzip");
        assert_eq!(q["user_agent"], "A B (C)");
        assert!(!q.contains_key("empty"));
        assert!(!u.contains(' '), "no raw spaces in the request target");
    }

    #[test]
    fn handshake_options_ping_interval() {
        assert_eq!(parse_ping_interval("ping-interval=10; foo=bar"), Some(Duration::from_secs(10)));
        assert_eq!(parse_ping_interval("foo=bar"), None);
        assert_eq!(parse_ping_interval("ping-interval=abc"), None);
    }

    #[test]
    fn sign_endpoint_template_or_base() {
        assert_eq!(sign_endpoint("https://api.eulerstream.com", "5").unwrap().as_str(), "https://api.eulerstream.com/webcast/rooms/5/connect");
        assert_eq!(sign_endpoint("https://s.example/fetch?room_id={room_id}", "5").unwrap().as_str(), "https://s.example/fetch?room_id=5");
    }

    // ---- end-to-end against a local fake of TikTok web + sign provider + push server ----

    struct Mock {
        port: u16,
        seen: mpsc::UnboundedSender<String>,
        frames_to_client: parking_lot::Mutex<Option<mpsc::UnboundedReceiver<Vec<u8>>>>,
        from_client: mpsc::UnboundedSender<Vec<u8>>,
    }

    async fn api_live(State(m): State<Arc<Mock>>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap) -> Response {
        let _ = m.seen.send(format!(
            "api-live uniqueId={} cookie={}",
            q.get("uniqueId").cloned().unwrap_or_default(),
            headers.get("cookie").and_then(|v| v.to_str().ok()).unwrap_or("")
        ));
        let body = format!(r#"{{"statusCode":0,"data":{{"user":{{"roomId":"{ROOM}"}},"liveRoom":{{"status":2}}}}}}"#);
        ([("set-cookie", "ttwid=web-cookie; Path=/; Domain=.tiktok.com")], body).into_response()
    }

    async fn sign(
        State(m): State<Arc<Mock>>,
        axum::extract::Path(room): axum::extract::Path<String>,
        Query(q): Query<HashMap<String, String>>,
        headers: HeaderMap,
    ) -> Response {
        let key = headers.get("x-api-key").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        let _ = m.seen.send(format!("sign room={room} key={key} client_enter={}", q.get("client_enter").cloned().unwrap_or_default()));
        if key != "test-key" {
            return (StatusCode::UNAUTHORIZED, r#"{"message":"Invalid API key"}"#).into_response();
        }
        let initial = ProtoMessageFetchResult {
            cursor: "cursor-1".into(),
            internal_ext: "ext-1".into(),
            push_server: format!("ws://127.0.0.1:{}/ws", m.port),
            route_params: [("wrss".to_string(), "signed".to_string())].into(),
            messages: vec![BaseProtoMessage {
                method: "WebcastRoomUserSeqMessage".into(),
                payload: WebcastRoomUserSeqMessage { total: 321, ..Default::default() }.encode_to_vec(),
                ..Default::default()
            }],
            ..Default::default()
        };
        ([("x-set-tt-cookie", "tt_chain_token=sign-cookie; Path=/"), ("x-room-id", ROOM)], initial.encode_to_vec()).into_response()
    }

    async fn ws(State(m): State<Arc<Mock>>, Query(q): Query<HashMap<String, String>>, headers: HeaderMap, up: WebSocketUpgrade) -> Response {
        let _ = m.seen.send(format!(
            "ws room_id={} cursor={} wrss={} cookie={} ua={}",
            q.get("room_id").cloned().unwrap_or_default(),
            q.get("cursor").cloned().unwrap_or_default(),
            q.get("wrss").cloned().unwrap_or_default(),
            headers.get("cookie").and_then(|v| v.to_str().ok()).unwrap_or(""),
            headers.get("user-agent").is_some_and(|v| v.as_bytes() == USER_AGENT.as_bytes()),
        ));
        let mut rx = m.frames_to_client.lock().take().expect("one socket");
        let from_client = m.from_client.clone();
        up.on_upgrade(move |mut sock| async move {
            loop {
                tokio::select! {
                    f = rx.recv() => match f {
                        Some(f) => if sock.send(AxMessage::Binary(f.into())).await.is_err() { break },
                        None => break,
                    },
                    m = sock.recv() => match m {
                        Some(Ok(AxMessage::Binary(b))) => { let _ = from_client.send(b.to_vec()); }
                        Some(Ok(_)) => {}
                        _ => break,
                    },
                }
            }
        })
        .into_response()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn real_client_against_local_fake_servers() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (seen_tx, mut seen) = mpsc::unbounded_channel();
        let (to_client, frames_rx) = mpsc::unbounded_channel();
        let (from_client_tx, mut from_client) = mpsc::unbounded_channel();
        let mock = Arc::new(Mock { port, seen: seen_tx, frames_to_client: parking_lot::Mutex::new(Some(frames_rx)), from_client: from_client_tx });
        let app = axum::Router::new()
            .route("/api-live/user/room/", get(api_live))
            .route("/webcast/rooms/{room}/connect", get(sign))
            .route("/ws", get(ws))
            .with_state(mock);
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = format!("http://127.0.0.1:{port}");

        // the wrong key is refused → health fail, no socket
        let t = WebTransport::new(&base).unwrap().with_web_base(&base).with_key(KeySource::Fixed(Some("wrong".into())));
        assert!(matches!(t.sign(ROOM).await, Err(TransportError::Auth(m)) if m.contains("Invalid API key")));
        seen.recv().await.unwrap();

        let t = WebTransport::new(&base).unwrap().with_web_base(&base).with_key(KeySource::Fixed(Some("test-key".into())));
        let rec = Arc::new(Recorder::default());
        let out = Out::new(rec.clone(), Arc::new(Shared::default()));
        let (stop_tx, stop_rx) = watch::channel(false);
        let settings = Settings { enabled: true, unique_id: "streamer".into(), sign_url: base.clone(), ..Default::default() };
        let task = tokio::spawn(crate::session::run(t, settings, out.clone(), stop_rx));

        assert_eq!(seen.recv().await.unwrap(), "api-live uniqueId=streamer cookie=tt-target-idc=useast1a");
        assert_eq!(seen.recv().await.unwrap(), format!("sign room={ROOM} key=test-key client_enter=true"));
        assert_eq!(
            seen.recv().await.unwrap(),
            format!("ws room_id={ROOM} cursor=cursor-1 wrss=signed cookie=tt_chain_token=sign-cookie; ttwid=web-cookie ua=true")
        );

        let enter = frame::decode_push_frame(&from_client.recv().await.unwrap()).unwrap();
        assert_eq!(enter.payload_type, "im_enter_room");
        assert_eq!(WebcastImEnterRoomMessage::decode(enter.payload.as_slice()).unwrap().room_id.to_string(), ROOM);
        assert_eq!(frame::decode_push_frame(&from_client.recv().await.unwrap()).unwrap().payload_type, "hb");

        // the captured-style fixture: gzip batch with chat, streak, like, follow, share, join, sub, viewers
        to_client.send(fixture("push_frame_events.bin")).unwrap();
        let ack = loop {
            let f = frame::decode_push_frame(&from_client.recv().await.unwrap()).unwrap();
            if f.payload_type == "ack" {
                break f;
            }
        };
        assert_eq!((ack.log_id, String::from_utf8(ack.payload).unwrap()), (7_400_000_000_000_000_123, "fetch_time:1758835200000|start:0|seq:42".to_string()));
        tokio::time::sleep(Duration::from_millis(1300)).await;
        let types: Vec<String> = rec.events().iter().map(|e| e.ty.clone()).collect();
        assert_eq!(types, ["tiktok.chat", "tiktok.gift", "tiktok.follow", "tiktok.share", "tiktok.join", "tiktok.sub", "tiktok.gift", "tiktok.like"]);
        assert_eq!(rec.signals("tiktok.viewers"), vec![321.0, 1234.0]);
        assert_eq!(out.state(), LinkState::Connected);

        to_client.send(fixture("push_frame_end.bin")).unwrap();
        for _ in 0..100 {
            if out.state() == LinkState::Offline {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(out.state(), LinkState::Offline);
        assert_eq!(rec.published("tiktok.connected"), vec![Value::Bool(true), Value::Bool(false)]);
        assert_eq!(out.shared.status.lock().sign_auth, "api_key");
        stop_tx.send(true).unwrap();
        task.await.unwrap();
    }
}
