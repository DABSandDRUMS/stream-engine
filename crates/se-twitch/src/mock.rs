//! A local stand-in for `id.twitch.tv` (device flow, refresh, validate) and Helix, recording
//! every call. Used with the Twitch CLI's mock EventSub WebSocket server to run the whole
//! subsystem end to end without a Twitch account (`examples/mock_twitch.rs`, `tests/mock_e2e.rs`).
//!
//! `GET /activate` approves the pending device code (the "user" entering it); `GET /_mock/calls`
//! lists recorded Helix calls; `POST /_mock/live?on=true|false` flips the stream state.

use axum::Router;
use axum::extract::{Query, State};
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, get, post};
use parking_lot::Mutex;
use serde_json::{Value as J, json};
use std::collections::HashMap;
use std::sync::Arc;

/// One recorded Helix call.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Call {
    pub method: String,
    pub path: String,
    pub query: HashMap<String, String>,
    pub body: J,
}

#[derive(Default)]
struct Inner {
    calls: Vec<Call>,
    approved: bool,
    access: Vec<String>,
    refresh: Vec<String>,
    seq: u64,
    rewards: Vec<J>,
    live: bool,
}

#[derive(Clone)]
pub struct Mock {
    inner: Arc<Mutex<Inner>>,
    pub client_id: String,
    pub user_id: String,
    pub login: String,
}

fn scopes() -> Vec<&'static str> {
    crate::config::SCOPES.to_vec()
}

impl Mock {
    pub fn new(client_id: &str, user_id: &str, login: &str) -> Mock {
        Mock {
            inner: Arc::new(Mutex::new(Inner { live: true, ..Default::default() })),
            client_id: client_id.into(),
            user_id: user_id.into(),
            login: login.into(),
        }
    }

    /// Recorded Helix calls whose path ends with `suffix`.
    pub fn calls(&self, suffix: &str) -> Vec<Call> {
        self.inner.lock().calls.iter().filter(|c| c.path.ends_with(suffix)).cloned().collect()
    }

    pub fn approve(&self) {
        self.inner.lock().approved = true;
    }

    /// Issue a valid refresh token directly (skip the device flow in tests).
    pub fn grant(&self) -> String {
        let mut i = self.inner.lock();
        i.seq += 1;
        let r = format!("refresh-{}", i.seq);
        i.refresh.push(r.clone());
        r
    }

    pub fn router(&self) -> Router {
        Router::new()
            .route("/oauth2/device", post(device))
            .route("/oauth2/token", post(token))
            .route("/oauth2/validate", get(validate))
            .route("/activate", get(activate))
            .route("/_mock/calls", get(calls))
            .route("/_mock/live", post(live))
            .route("/helix/{*path}", any(helix))
            .with_state(self.clone())
    }

    /// Serve on `addr` until the process exits.
    pub async fn serve(self, addr: std::net::SocketAddr) -> std::io::Result<std::net::SocketAddr> {
        let l = tokio::net::TcpListener::bind(addr).await?;
        let bound = l.local_addr()?;
        let app = self.router();
        tokio::spawn(async move {
            let _ = axum::serve(l, app).await;
        });
        Ok(bound)
    }

    fn tokens(&self, i: &mut Inner) -> J {
        i.seq += 1;
        let (a, r) = (format!("access-{}", i.seq), format!("refresh-{}", i.seq));
        i.access.push(a.clone());
        i.refresh.push(r.clone());
        json!({ "access_token": a, "refresh_token": r, "expires_in": 14_400, "scope": scopes(), "token_type": "bearer" })
    }
}

async fn device(State(m): State<Mock>, axum::Form(f): axum::Form<HashMap<String, String>>) -> Response {
    if f.get("client_id") != Some(&m.client_id) {
        return (StatusCode::BAD_REQUEST, axum::Json(json!({"status": 400, "message": "invalid client"}))).into_response();
    }
    m.inner.lock().approved = false;
    axum::Json(json!({
        "device_code": "mock-device-code",
        "user_code": "MOCK-CODE",
        "verification_uri": "http://mock/activate?public=true&device-code=MOCK-CODE",
        "expires_in": 1800,
        "interval": 1,
    }))
    .into_response()
}

async fn token(State(m): State<Mock>, axum::Form(f): axum::Form<HashMap<String, String>>) -> Response {
    let mut i = m.inner.lock();
    let bad = |msg: &str| (StatusCode::BAD_REQUEST, axum::Json(json!({"status": 400, "message": msg}))).into_response();
    match f.get("grant_type").map(String::as_str) {
        Some("urn:ietf:params:oauth:grant-type:device_code") => {
            if !i.approved {
                return bad("authorization_pending");
            }
            i.approved = false;
            axum::Json(m.tokens(&mut i)).into_response()
        }
        Some("refresh_token") => {
            let r = f.get("refresh_token").cloned().unwrap_or_default();
            match i.refresh.iter().position(|x| *x == r) {
                Some(pos) => {
                    // public clients: refresh tokens are single use
                    i.refresh.remove(pos);
                    axum::Json(m.tokens(&mut i)).into_response()
                }
                None => bad("Invalid refresh token"),
            }
        }
        _ => bad("unsupported grant_type"),
    }
}

async fn validate(State(m): State<Mock>, h: HeaderMap) -> Response {
    let tok = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("OAuth ")).unwrap_or("").to_string();
    if !m.inner.lock().access.contains(&tok) {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({"status": 401, "message": "invalid access token"}))).into_response();
    }
    axum::Json(json!({ "client_id": m.client_id, "login": m.login, "scopes": scopes(), "user_id": m.user_id, "expires_in": 14_400 })).into_response()
}

async fn activate(State(m): State<Mock>) -> &'static str {
    m.approve();
    "Device approved. You can close this page."
}

async fn calls(State(m): State<Mock>) -> axum::Json<Vec<Call>> {
    axum::Json(m.inner.lock().calls.clone())
}

async fn live(State(m): State<Mock>, Query(q): Query<HashMap<String, String>>) -> StatusCode {
    m.inner.lock().live = q.get("on").is_none_or(|v| v == "true");
    StatusCode::NO_CONTENT
}

async fn helix(State(m): State<Mock>, method: Method, uri: Uri, h: HeaderMap, Query(q): Query<HashMap<String, String>>, body: String) -> Response {
    let path = uri.path().trim_start_matches("/helix").to_string();
    let tok = h.get("authorization").and_then(|v| v.to_str().ok()).and_then(|v| v.strip_prefix("Bearer ")).unwrap_or("").to_string();
    let cid = h.get("client-id").and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
    let body_json: J = serde_json::from_str(&body).unwrap_or(J::Null);
    let mut i = m.inner.lock();
    if !i.access.contains(&tok) || cid != m.client_id {
        return (StatusCode::UNAUTHORIZED, axum::Json(json!({"error": "Unauthorized", "status": 401, "message": "Invalid OAuth token"}))).into_response();
    }
    i.calls.push(Call { method: method.to_string(), path: path.clone(), query: q.clone(), body: body_json.clone() });
    let ok = |d: J| axum::Json(json!({ "data": d })).into_response();
    match (method.as_str(), path.as_str()) {
        ("GET", "/users") => {
            let login = q.get("login").cloned().unwrap_or_else(|| m.login.clone());
            let id = if login == m.login { m.user_id.clone() } else { format!("id-{login}") };
            ok(json!([{ "id": id, "login": login, "display_name": login.to_uppercase() }]))
        }
        ("GET", "/channel_points/custom_rewards") => ok(J::Array(i.rewards.clone())),
        ("POST", "/channel_points/custom_rewards") => {
            let title = body_json.get("title").and_then(J::as_str).unwrap_or("").to_string();
            if i.rewards.iter().any(|r| r["title"].as_str().is_some_and(|t| t.eq_ignore_ascii_case(&title))) {
                return (StatusCode::BAD_REQUEST, axum::Json(json!({"status": 400, "message": "CREATE_CUSTOM_REWARD_DUPLICATE_REWARD"}))).into_response();
            }
            i.seq += 1;
            let r = reward_from(format!("rw-{}", i.seq), &body_json, None);
            i.rewards.push(r.clone());
            ok(json!([r]))
        }
        ("PATCH", "/channel_points/custom_rewards") => {
            let id = q.get("id").cloned().unwrap_or_default();
            match i.rewards.iter().position(|r| r["id"] == id) {
                Some(pos) => {
                    let r = reward_from(id, &body_json, Some(&i.rewards[pos]));
                    i.rewards[pos] = r.clone();
                    ok(json!([r]))
                }
                None => (StatusCode::NOT_FOUND, axum::Json(json!({"status": 404, "message": "reward not found"}))).into_response(),
            }
        }
        ("PATCH", "/channel_points/custom_rewards/redemptions") => {
            ok(json!([{ "id": q.get("id"), "reward": {"id": q.get("reward_id")}, "status": body_json.get("status") }]))
        }
        ("POST", "/chat/messages") => {
            i.seq += 1;
            ok(json!([{ "message_id": format!("sent-{}", i.seq), "is_sent": true, "drop_reason": null }]))
        }
        ("GET", "/streams") => {
            if i.live {
                ok(json!([{ "viewer_count": 42, "started_at": "2026-09-25T20:00:00Z", "title": "mock stream", "game_name": "Music", "type": "live" }]))
            } else {
                ok(json!([]))
            }
        }
        ("GET", "/channels") => ok(json!([{ "broadcaster_id": m.user_id, "title": "mock stream", "game_name": "Music", "delay": 0 }])),
        ("GET", "/channels/ads") => {
            let next = crate::time::unix_s() + 1200;
            ok(
                json!([{ "next_ad_at": next.to_string(), "last_ad_at": "", "duration": 90, "preroll_free_time": 600, "snooze_count": 3, "snooze_refresh_at": "" }]),
            )
        }
        ("POST", "/streams/markers") => {
            ok(json!([{ "id": "marker-1", "created_at": "2026-09-25T20:10:00Z", "description": body_json.get("description"), "position_seconds": 600 }]))
        }
        ("GET", "/moderation/moderators") => ok(json!([{ "user_id": "id-modfriend", "user_login": "modfriend", "user_name": "ModFriend" }])),
        ("GET", "/channels/vips") => ok(json!([{ "user_id": "id-vipfan", "user_login": "vipfan", "user_name": "VipFan" }])),
        _ => ok(json!([])),
    }
}

fn reward_from(id: String, b: &J, prev: Option<&J>) -> J {
    let g = |k: &str| b.get(k).cloned().or_else(|| prev.and_then(|p| p.get(k).cloned())).unwrap_or(J::Null);
    let nested = |k: &str, flag: &str, val: &str| {
        let e = b.get(flag).cloned().or_else(|| prev.and_then(|p| p.pointer(&format!("/{k}/is_enabled")).cloned())).unwrap_or(J::Bool(false));
        let v = b.get(val).cloned().or_else(|| prev.and_then(|p| p.pointer(&format!("/{k}/{val}")).cloned())).unwrap_or(json!(0));
        json!({ "is_enabled": e, val: v })
    };
    json!({
        "id": id,
        "title": g("title"),
        "cost": g("cost"),
        "prompt": g("prompt"),
        "is_enabled": g("is_enabled"),
        "is_paused": g("is_paused"),
        "is_user_input_required": g("is_user_input_required"),
        "background_color": g("background_color"),
        "should_redemptions_skip_request_queue": g("should_redemptions_skip_request_queue"),
        "max_per_stream_setting": nested("max_per_stream_setting", "is_max_per_stream_enabled", "max_per_stream"),
        "max_per_user_per_stream_setting": nested("max_per_user_per_stream_setting", "is_max_per_user_per_stream_enabled", "max_per_user_per_stream"),
        "global_cooldown_setting": nested("global_cooldown_setting", "is_global_cooldown_enabled", "global_cooldown_seconds"),
    })
}

// ---- Twitch CLI mock EventSub server: forward any EventSub payload -------------------------

/// Minimal `encoding/gob` writer for the two request structs `net/rpc` sends.
mod gob {
    pub fn uint(out: &mut Vec<u8>, v: u64) {
        if v < 128 {
            out.push(v as u8);
            return;
        }
        let bytes = v.to_be_bytes();
        let skip = bytes.iter().take_while(|b| **b == 0).count();
        out.push((256 - (8 - skip)) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }

    pub fn int(out: &mut Vec<u8>, v: i64) {
        let u = if v < 0 { ((!v as u64) << 1) | 1 } else { (v as u64) << 1 };
        uint(out, u);
    }

    pub fn string(out: &mut Vec<u8>, s: &str) {
        uint(out, s.len() as u64);
        out.extend_from_slice(s.as_bytes());
    }

    /// One framed message: byte count, type id, value.
    pub fn message(out: &mut Vec<u8>, type_id: i64, value: &[u8]) {
        let mut body = Vec::new();
        int(&mut body, type_id);
        body.extend_from_slice(value);
        uint(out, body.len() as u64);
        out.extend_from_slice(&body);
    }

    /// `CommonType{Name, Id}`.
    fn common(out: &mut Vec<u8>, name: &str, id: i64) {
        uint(out, 1);
        string(out, name);
        uint(out, 1);
        int(out, id);
        uint(out, 0);
    }

    /// `wireType{StructT: &structType{CommonType, Field: [{Name, Id}]}}`.
    pub fn struct_type(name: &str, id: i64, fields: &[(&str, i64)]) -> Vec<u8> {
        let mut v = Vec::new();
        uint(&mut v, 3); // wireType.StructT (field 2)
        uint(&mut v, 1); // structType.CommonType
        common(&mut v, name, id);
        uint(&mut v, 1); // structType.Field
        uint(&mut v, fields.len() as u64);
        for (n, t) in fields {
            uint(&mut v, 1);
            string(&mut v, n);
            uint(&mut v, 1);
            int(&mut v, *t);
            uint(&mut v, 0);
        }
        uint(&mut v, 0);
        uint(&mut v, 0);
        v
    }

    /// `wireType{MapT: &mapType{CommonType, Key, Elem}}`.
    pub fn map_type(name: &str, id: i64, key: i64, elem: i64) -> Vec<u8> {
        let mut v = Vec::new();
        uint(&mut v, 4); // wireType.MapT (field 3)
        uint(&mut v, 1);
        common(&mut v, name, id);
        uint(&mut v, 1);
        int(&mut v, key);
        uint(&mut v, 1);
        int(&mut v, elem);
        uint(&mut v, 0);
        uint(&mut v, 0);
        v
    }

    /// Read one unsigned integer.
    pub fn read_uint(b: &[u8], i: &mut usize) -> Option<u64> {
        let first = *b.get(*i)?;
        *i += 1;
        if first < 128 {
            return Some(first as u64);
        }
        let n = 256 - first as usize;
        let mut v = 0u64;
        for _ in 0..n {
            v = (v << 8) | *b.get(*i)? as u64;
            *i += 1;
        }
        Some(v)
    }

    pub fn read_int(b: &[u8], i: &mut usize) -> Option<i64> {
        let u = read_uint(b, i)?;
        Some(if u & 1 == 1 { !(u >> 1) as i64 } else { (u >> 1) as i64 })
    }
}

/// The CLI's fixed RPC port (`twitch event trigger --transport=websocket` talks to it).
pub const CLI_RPC: &str = "127.0.0.1:44747";

/// Forward an EventSub payload (`{subscription, event}`, the webhook shape the docs show) to
/// every client of the running `twitch event websocket start-server`, exactly like
/// `twitch event trigger … --transport=websocket` does for the events it knows.
pub async fn cli_forward(body: &str) -> Result<(), String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    const STRING: i64 = 6;
    const UINT: i64 = 3;
    let mut s = tokio::net::TcpStream::connect(CLI_RPC).await.map_err(|e| format!("Twitch CLI mock server RPC ({CLI_RPC}): {e}"))?;
    s.write_all(b"CONNECT /_goRPC_ HTTP/1.0\n\n").await.map_err(|e| e.to_string())?;
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\n\n") {
        s.read_exact(&mut byte).await.map_err(|e| e.to_string())?;
        head.push(byte[0]);
        if head.len() > 256 {
            return Err("unexpected RPC handshake".into());
        }
    }
    if !String::from_utf8_lossy(&head).contains("200") {
        return Err(format!("RPC handshake failed: {}", String::from_utf8_lossy(&head)));
    }
    let mut out = Vec::new();
    // net/rpc Request{ServiceMethod, Seq}
    gob::message(&mut out, -65, &gob::struct_type("Request", 65, &[("ServiceMethod", STRING), ("Seq", UINT)]));
    let mut req = Vec::new();
    gob::uint(&mut req, 1);
    gob::string(&mut req, "RPCHandler.ExecuteGenericRPC");
    gob::uint(&mut req, 1);
    gob::uint(&mut req, 1); // Seq = 1
    gob::uint(&mut req, 0);
    gob::message(&mut out, 65, &req);
    // RPCArgs{RPCName, Body, Variables map[string]string}
    gob::message(&mut out, -66, &gob::map_type("map[string]string", 66, STRING, STRING));
    gob::message(&mut out, -67, &gob::struct_type("RPCArgs", 67, &[("RPCName", STRING), ("Body", STRING), ("Variables", 66)]));
    let mut args = Vec::new();
    gob::uint(&mut args, 1);
    gob::string(&mut args, "EventSubWebSocketForwardEvent");
    gob::uint(&mut args, 1);
    gob::string(&mut args, body);
    gob::uint(&mut args, 1);
    gob::uint(&mut args, 1);
    gob::string(&mut args, "ClientName");
    gob::string(&mut args, "");
    gob::uint(&mut args, 0);
    gob::message(&mut out, 67, &args);
    s.write_all(&out).await.map_err(|e| e.to_string())?;
    // Response{ServiceMethod, Seq, Error} then RPCResponse{ResponseCode, DetailedInfo}
    let mut values = Vec::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
    while values.len() < 2 {
        let mut len_buf = Vec::new();
        loop {
            tokio::time::timeout_at(deadline, s.read_exact(&mut byte)).await.map_err(|_| "RPC reply timed out".to_string())?.map_err(|e| e.to_string())?;
            len_buf.push(byte[0]);
            let mut i = 0;
            if gob::read_uint(&len_buf, &mut i).is_some() {
                break;
            }
        }
        let mut i = 0;
        let len = gob::read_uint(&len_buf, &mut i).unwrap_or(0) as usize;
        let mut msg = vec![0u8; len];
        tokio::time::timeout_at(deadline, s.read_exact(&mut msg)).await.map_err(|_| "RPC reply timed out".to_string())?.map_err(|e| e.to_string())?;
        let mut i = 0;
        let id = gob::read_int(&msg, &mut i).ok_or("bad RPC reply")?;
        if id > 0 {
            values.push(msg[i..].to_vec());
        }
    }
    // Response.Error (field 2) set → RPC error
    let parse = |v: &[u8], string_fields: &[i64]| -> Vec<(i64, String, i64)> {
        let (mut i, mut field, mut out) = (0usize, -1i64, Vec::new());
        while let Some(delta) = gob::read_uint(v, &mut i) {
            if delta == 0 {
                break;
            }
            field += delta as i64;
            if string_fields.contains(&field) {
                let n = gob::read_uint(v, &mut i).unwrap_or(0) as usize;
                out.push((field, String::from_utf8_lossy(v.get(i..i + n).unwrap_or(&[])).to_string(), 0));
                i += n;
            } else {
                out.push((field, String::new(), gob::read_int(v, &mut i).unwrap_or(0)));
            }
        }
        out
    };
    if let Some((_, e, _)) = parse(&values[0], &[0, 2]).into_iter().find(|(f, _, _)| *f == 2) {
        return Err(format!("RPC error: {e}"));
    }
    let reply = parse(&values[1], &[1]);
    let code = reply.iter().find(|(f, _, _)| *f == 0).map(|r| r.2).unwrap_or(0);
    if code != 0 {
        let info = reply.iter().find(|(f, _, _)| *f == 1).map(|r| r.1.clone()).unwrap_or_default();
        return Err(format!("mock server refused the event ({code}): {info}"));
    }
    Ok(())
}
