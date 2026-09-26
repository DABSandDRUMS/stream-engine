//! Engine side of the Cloudflare relay link (§14.6): one outbound WebSocket to the relay's
//! Durable Object, authenticated with the shared secret (`Authorization: Bearer …`).
//!
//! Frames (JSON text; plus bare `ping`/`pong` keepalive answered by the relay without waking it):
//!
//! | direction | frame |
//! |---|---|
//! | engine → relay | `{t:"hello", v:1, engine}` · `{t:"queue", snapshot}` · `{t:"ack", ids:[message_id]}` · `{t:"mod.res", id, ok, result \| error}` |
//! | relay → engine | `{t:"welcome", v:1, pending, dropped}` · `{t:"kofi", id, received_at, data}` · `{t:"mod.req", id, body}` · `{t:"error", msg}` |
//!
//! Ko-fi payments become `tip` events with the same payload shape as the simulator's tip
//! (`se_core::sim`), deduplicated on `message_id` in the runtime DB (the relay redelivers until
//! acknowledged).

use crate::store::Store;
use crate::text;
use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use se_hub::Hub;
use se_proto::{Actor, Event, Meta, Origin, Value, ValueType};
use serde_json::{Value as J, json};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::{self, Message, client::IntoClientRequest};

pub const PROTOCOL: u32 = 1;
const PING_EVERY: Duration = Duration::from_secs(25);
const PONG_TIMEOUT: Duration = Duration::from_secs(80);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Largest queue snapshot we send (the relay rejects bigger frames).
pub const MAX_SNAPSHOT_BYTES: usize = 200 * 1024;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct LinkConfig {
    pub url: Option<String>,
    pub secret: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub configured: bool,
    pub connected: bool,
    pub url: String,
    pub detail: String,
    pub since: Option<i64>,
}

impl Status {
    pub fn to_value(&self) -> Value {
        Value::map()
            .with("configured", self.configured)
            .with("connected", self.connected)
            .with("url", self.url.clone())
            .with("detail", self.detail.clone())
            .with("since", self.since.map(Value::from).unwrap_or(Value::Null))
    }
}

/// Handle to the link task.
#[derive(Clone)]
pub struct Relay {
    cfg: watch::Sender<LinkConfig>,
    snapshot: watch::Sender<Option<Value>>,
    status: Arc<Mutex<Status>>,
}

impl Relay {
    /// Spawn the link task (idle until configured).
    pub fn spawn(hub: Arc<Hub>, store: Store) -> Relay {
        hub.declare("health.relay", Meta { ty: ValueType::Map, readonly: true, ..Default::default() }.owner("songs").describe("Cloudflare relay link"));
        let (cfg, cfg_rx) = watch::channel(LinkConfig::default());
        let (snapshot, snap_rx) = watch::channel(None);
        let status = Arc::new(Mutex::new(Status::default()));
        let st = status.clone();
        tokio::spawn(async move { run(hub, store, cfg_rx, snap_rx, st).await });
        Relay { cfg, snapshot, status }
    }

    pub fn configure(&self, url: Option<String>, secret: Option<String>) {
        let new = LinkConfig { url, secret };
        self.cfg.send_if_modified(|c| {
            if *c == new {
                return false;
            }
            *c = new;
            true
        });
    }

    /// Force a reconnect (same config).
    pub fn reconnect(&self) {
        self.cfg.send_modify(|_| {});
    }

    /// Latest public queue snapshot; sent now if connected, else on (re)connect.
    pub fn snapshot(&self, v: Value) {
        self.snapshot.send_if_modified(|s| {
            if s.as_ref() == Some(&v) {
                return false;
            }
            *s = Some(v);
            true
        });
    }

    pub fn status(&self) -> Status {
        self.status.lock().clone()
    }
}

fn health(hub: &Hub, status: &str, detail: &str) {
    hub.publish("health.relay", Value::map().with("status", status).with("detail", detail));
}

fn host_of(url: &str) -> String {
    url.split("://").nth(1).and_then(|r| r.split('/').next()).unwrap_or(url).to_string()
}

/// Exponential backoff 1 s … 60 s with ±20 % jitter.
fn backoff(attempt: u32) -> Duration {
    let base = 2f64.powi(attempt.min(6) as i32).min(60.0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
    let jitter = (nanos % 1000) as f64 / 1000.0 * 0.4 - 0.2;
    Duration::from_secs_f64((base * (1.0 + jitter)).max(0.5))
}

enum End {
    /// Config changed: reconnect immediately.
    Reconfigured,
    /// Connection lost or refused; retry with backoff.
    Lost(String),
    /// The relay refused our secret: retry slowly.
    Unauthorized(String),
}

async fn run(hub: Arc<Hub>, store: Store, mut cfg_rx: watch::Receiver<LinkConfig>, mut snap_rx: watch::Receiver<Option<Value>>, status: Arc<Mutex<Status>>) {
    let _ = store.prune_seen(90);
    let mut attempt = 0u32;
    loop {
        let cfg = cfg_rx.borrow_and_update().clone();
        let (url, secret) = match (cfg.url, cfg.secret) {
            (Some(u), Some(s)) if !s.is_empty() => (u, s),
            (u, _) => {
                let detail = if u.is_none() {
                    "not configured — set [relay] url in project.toml".to_string()
                } else {
                    "no shared secret — run `stream do relay.secret.set <secret>`".to_string()
                };
                *status.lock() = Status { configured: false, connected: false, url: u.unwrap_or_default(), detail: detail.clone(), since: None };
                health(&hub, "warn", &detail);
                if cfg_rx.changed().await.is_err() {
                    return;
                }
                continue;
            }
        };
        {
            let mut s = status.lock();
            s.configured = true;
            s.url = url.clone();
        }
        let started = Instant::now();
        let end = session(&hub, &store, &url, &secret, &mut cfg_rx, &mut snap_rx, &status).await;
        {
            let mut s = status.lock();
            s.connected = false;
            s.since = None;
        }
        let (reason, delay) = match end {
            End::Reconfigured => {
                attempt = 0;
                continue;
            }
            End::Lost(r) => {
                if started.elapsed() > Duration::from_secs(60) {
                    attempt = 0;
                }
                let d = backoff(attempt);
                attempt += 1;
                (r, d)
            }
            End::Unauthorized(r) => (r, Duration::from_secs(60)),
        };
        let detail = format!("{reason} — retrying in {}s", delay.as_secs().max(1));
        status.lock().detail = detail.clone();
        health(&hub, "fail", &detail);
        tracing::warn!("relay: {detail}");
        tokio::select! {
            _ = tokio::time::sleep(delay) => {}
            r = cfg_rx.changed() => if r.is_err() { return },
        }
    }
}

async fn session(
    hub: &Arc<Hub>,
    store: &Store,
    url: &str,
    secret: &str,
    cfg_rx: &mut watch::Receiver<LinkConfig>,
    snap_rx: &mut watch::Receiver<Option<Value>>,
    status: &Arc<Mutex<Status>>,
) -> End {
    let mut req = match url.into_client_request() {
        Ok(r) => r,
        Err(e) => return End::Lost(format!("bad relay url: {e}")),
    };
    let headers = req.headers_mut();
    match format!("Bearer {secret}").parse() {
        Ok(v) => {
            headers.insert(tungstenite::http::header::AUTHORIZATION, v);
        }
        Err(_) => return End::Unauthorized("relay secret contains invalid characters".into()),
    }
    if let Ok(v) = concat!("stream-engine/", env!("CARGO_PKG_VERSION")).parse() {
        headers.insert(tungstenite::http::header::USER_AGENT, v);
    }
    let ws = match tokio::time::timeout(CONNECT_TIMEOUT, tokio_tungstenite::connect_async(req)).await {
        Err(_) => return End::Lost(format!("connecting to {} timed out", host_of(url))),
        Ok(Err(tungstenite::Error::Http(resp))) => {
            let code = resp.status().as_u16();
            return if code == 401 || code == 403 {
                End::Unauthorized(format!("relay rejected the shared secret (HTTP {code})"))
            } else {
                End::Lost(format!("relay answered HTTP {code}"))
            };
        }
        Ok(Err(e)) => return End::Lost(format!("cannot reach {}: {e}", host_of(url))),
        Ok(Ok((ws, _))) => ws,
    };
    let (mut sink, mut stream) = ws.split();
    let hello = json!({ "t": "hello", "v": PROTOCOL, "engine": env!("CARGO_PKG_VERSION") }).to_string();
    if let Err(e) = sink.send(Message::text(hello)).await {
        return End::Lost(format!("send hello: {e}"));
    }
    let (out_tx, mut out_rx) = mpsc::channel::<String>(64);
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ping.tick().await;
    let mut last_pong = Instant::now();
    let mut welcomed = false;
    loop {
        tokio::select! {
            msg = stream.next() => match msg {
                Some(Ok(Message::Text(t))) => {
                    let t = t.as_str();
                    if t == "pong" {
                        last_pong = Instant::now();
                        continue;
                    }
                    last_pong = Instant::now();
                    match handle_frame(hub, store, t, &out_tx) {
                        Frame::Welcome { pending, dropped } => {
                            welcomed = true;
                            let host = host_of(url);
                            let detail = if dropped > 0 {
                                format!("connected to {host}; {dropped} Ko-fi payment(s) were dropped while offline (relay buffer full)")
                            } else {
                                format!("connected to {host}; {pending} buffered payment(s) delivered")
                            };
                            {
                                let mut s = status.lock();
                                s.connected = true;
                                s.detail = detail.clone();
                                s.since = Some(text::now_ms());
                            }
                            health(hub, if dropped > 0 { "warn" } else { "pass" }, &detail);
                            tracing::info!("relay: {detail}");
                            let snap = snap_rx.borrow_and_update().clone();
                            if let Some(s) = snap
                                && let Some(f) = queue_frame(&s)
                                && sink.send(Message::text(f)).await.is_err()
                            {
                                return End::Lost("send failed".into());
                            }
                        }
                        Frame::Reply(r) => {
                            if let Err(e) = sink.send(Message::text(r)).await {
                                return End::Lost(format!("send: {e}"));
                            }
                        }
                        Frame::Fatal(m) => return End::Lost(m),
                        Frame::None => {}
                    }
                }
                Some(Ok(Message::Close(f))) => {
                    let why = f.map(|f| format!("{} {}", u16::from(f.code), f.reason)).unwrap_or_default();
                    return if why.starts_with("4001") { End::Lost("replaced by another engine connection".into()) } else { End::Lost(format!("relay closed the link {why}").trim().to_string()) };
                }
                Some(Ok(_)) => last_pong = Instant::now(),
                Some(Err(e)) => return End::Lost(format!("link error: {e}")),
                None => return End::Lost("link closed".into()),
            },
            r = snap_rx.changed(), if welcomed => {
                if r.is_err() { return End::Lost("engine shutting down".into()); }
                let snap = snap_rx.borrow_and_update().clone();
                if let Some(s) = snap && let Some(f) = queue_frame(&s) && let Err(e) = sink.send(Message::text(f)).await {
                    return End::Lost(format!("send: {e}"));
                }
            }
            Some(f) = out_rx.recv() => {
                if let Err(e) = sink.send(Message::text(f)).await {
                    return End::Lost(format!("send: {e}"));
                }
            }
            _ = ping.tick() => {
                if last_pong.elapsed() > PONG_TIMEOUT {
                    return End::Lost(format!("no answer from the relay for {} s", PONG_TIMEOUT.as_secs()));
                }
                if let Err(e) = sink.send(Message::text("ping")).await {
                    return End::Lost(format!("send: {e}"));
                }
            }
            r = cfg_rx.changed() => {
                let _ = sink.send(Message::Close(None)).await;
                return if r.is_err() { End::Lost("engine shutting down".into()) } else { End::Reconfigured };
            }
        }
    }
}

fn queue_frame(snapshot: &Value) -> Option<String> {
    let f = json!({ "t": "queue", "snapshot": J::from(snapshot) }).to_string();
    if f.len() > MAX_SNAPSHOT_BYTES {
        tracing::warn!("relay: queue snapshot too large ({} bytes); not sent", f.len());
        return None;
    }
    Some(f)
}

enum Frame {
    Welcome { pending: i64, dropped: i64 },
    Reply(String),
    Fatal(String),
    None,
}

fn handle_frame(hub: &Arc<Hub>, store: &Store, raw: &str, out: &mpsc::Sender<String>) -> Frame {
    let Ok(f) = serde_json::from_str::<J>(raw) else {
        tracing::debug!("relay: unparseable frame");
        return Frame::None;
    };
    match f.get("t").and_then(J::as_str).unwrap_or("") {
        "welcome" => {
            let v = f.get("v").and_then(J::as_u64).unwrap_or(0);
            if v != PROTOCOL as u64 {
                return Frame::Fatal(format!("relay speaks protocol v{v}, engine v{PROTOCOL} — redeploy the relay"));
            }
            Frame::Welcome { pending: f.get("pending").and_then(J::as_i64).unwrap_or(0), dropped: f.get("dropped").and_then(J::as_i64).unwrap_or(0) }
        }
        "kofi" => {
            let Some(id) = f.get("id").and_then(J::as_str).filter(|s| !s.is_empty()) else { return Frame::None };
            let data = f.get("data").cloned().unwrap_or(J::Null);
            match store.first_delivery(id) {
                Ok(true) => match kofi_to_tip(&data) {
                    Ok(ev) => {
                        tracing::info!(
                            "relay: Ko-fi {} {} from {}",
                            ev.payload.get_path("amount").map(|v| v.to_string()).unwrap_or_default(),
                            ev.payload.get_path("currency").map(|v| v.to_string()).unwrap_or_default(),
                            ev.payload.get_path("user").map(|v| v.to_string()).unwrap_or_default()
                        );
                        hub.emit(ev);
                    }
                    Err(e) => hub.log("error", "relay", format!("Ko-fi payment {id} ignored: {e}")),
                },
                Ok(false) => tracing::debug!("relay: duplicate Ko-fi delivery {id}"),
                Err(e) => {
                    // don't ack: the relay keeps it and redelivers after reconnect
                    hub.log("error", "relay", format!("Ko-fi dedupe store failed: {e:#}"));
                    return Frame::None;
                }
            }
            Frame::Reply(json!({ "t": "ack", "ids": [id] }).to_string())
        }
        "mod.req" => {
            let (Some(id), body) = (f.get("id").and_then(J::as_str).map(String::from), f.get("body").cloned().unwrap_or(J::Null)) else { return Frame::None };
            let (hub, out) = (hub.clone(), out.clone());
            tokio::spawn(async move {
                let reply = match hub.query("remote_mod.request", Value::from(body)).await {
                    Ok(v) => json!({ "t": "mod.res", "id": id, "ok": true, "result": J::from(&v) }),
                    Err(e) => json!({ "t": "mod.res", "id": id, "ok": false, "error": e }),
                };
                let _ = out.send(reply.to_string()).await;
            });
            Frame::None
        }
        "error" => {
            hub.log("warn", "relay", format!("relay: {}", f.get("msg").and_then(J::as_str).unwrap_or("error")));
            Frame::None
        }
        other => {
            tracing::debug!("relay: ignoring frame `{other}`");
            Frame::None
        }
    }
}

fn jbool(v: Option<&J>, default: bool) -> bool {
    match v {
        Some(J::Bool(b)) => *b,
        Some(J::String(s)) => s.eq_ignore_ascii_case("true"),
        _ => default,
    }
}

/// Normalize a Ko-fi webhook payload into the engine's `tip` event (§14.5). Private supporters
/// (`is_public: false`) become "Anonymous" with no message; e-mail and shipping never leave here.
pub fn kofi_to_tip(d: &J) -> Result<Event, String> {
    let amount = match d.get("amount") {
        Some(J::String(s)) => s.trim().parse::<f64>().map_err(|_| format!("bad amount `{s}`"))?,
        Some(J::Number(n)) => n.as_f64().ok_or("bad amount")?,
        _ => return Err("missing amount".into()),
    };
    if !amount.is_finite() || amount < 0.0 {
        return Err(format!("bad amount {amount}"));
    }
    let currency = d.get("currency").and_then(J::as_str).map(|c| c.trim().to_ascii_uppercase()).filter(|c| !c.is_empty()).unwrap_or_else(|| "USD".into());
    let is_public = jbool(d.get("is_public"), true);
    let from = d.get("from_name").and_then(J::as_str).map(|s| text::display(s, 50)).filter(|s| !s.is_empty()).unwrap_or_else(|| "Someone".into());
    let message = d.get("message").and_then(J::as_str).map(|s| text::display(s, 500)).unwrap_or_default();
    let kind = match d.get("type").and_then(J::as_str).unwrap_or("Donation") {
        "Donation" => "donation".to_string(),
        "Subscription" => "subscription".to_string(),
        "Shop Order" => "shop_order".to_string(),
        "Commission" => "commission".to_string(),
        other => text::fold(other).replace(' ', "_"),
    };
    let user = if is_public { from.clone() } else { "Anonymous".to_string() };
    let opt = |k: &str| d.get(k).and_then(J::as_str).map(|s| Value::from(text::display(s, 100))).unwrap_or(Value::Null);
    let payload = Value::map()
        .with("amount", amount)
        .with("currency", currency)
        .with("message", if is_public { message } else { String::new() })
        .with("is_public", is_public)
        .with("provider", "kofi")
        .with("user", user.clone())
        .with("kind", kind)
        .with("tier", opt("tier_name"))
        .with("subscription", jbool(d.get("is_subscription_payment"), false))
        .with("first_subscription", jbool(d.get("is_first_subscription_payment"), false))
        .with("message_id", opt("message_id"))
        .with("transaction_id", opt("kofi_transaction_id"));
    let actor = Actor { platform: "kofi".into(), id: if is_public { text::login(&from) } else { "anonymous".into() }, name: user, roles: vec![] };
    Ok(Event::new("tip", Origin::Relay, payload).with_actor(actor))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ko-fi's documented test payload.
    pub fn kofi_sample() -> J {
        json!({
            "verification_token": "4f1d0000-0000-0000-0000-000000000000",
            "message_id": "3a1fac0c-f960-4506-a60e-824979a74e74",
            "timestamp": "2026-09-25T14:31:00Z",
            "type": "Donation",
            "is_public": true,
            "from_name": "Jo Example",
            "message": "Good luck with the integration!",
            "amount": "3.00",
            "url": "https://ko-fi.com/Home/CoffeeShop?txid=00000000-1111-2222-3333-444444444444",
            "email": "jo.example@example.com",
            "currency": "USD",
            "is_subscription_payment": false,
            "is_first_subscription_payment": false,
            "kofi_transaction_id": "00000000-1111-2222-3333-444444444444",
            "shop_items": null,
            "tier_name": null,
            "shipping": null
        })
    }

    #[test]
    fn kofi_payload_matches_the_sim_tip_shape() {
        let e = kofi_to_tip(&kofi_sample()).unwrap();
        assert_eq!(e.ty, "tip");
        assert_eq!(e.origin, Origin::Relay);
        let p = &e.payload;
        assert_eq!(p.get_path("amount").and_then(Value::as_f64), Some(3.0));
        assert_eq!(p.get_path("currency").and_then(Value::as_str), Some("USD"));
        assert_eq!(p.get_path("message").and_then(Value::as_str), Some("Good luck with the integration!"));
        assert_eq!(p.get_path("user").and_then(Value::as_str), Some("Jo Example"));
        assert_eq!(p.get_path("provider").and_then(Value::as_str), Some("kofi"));
        assert_eq!(p.get_path("is_public"), Some(&Value::Bool(true)));
        assert_eq!(p.get_path("kind").and_then(Value::as_str), Some("donation"));
        assert_eq!(e.actor.as_ref().unwrap().platform, "kofi");
        // the simulator's tip carries exactly these keys; ours is a superset
        let sim = se_core::sim::events("tip", &Value::Null, &mut se_core::rng::Rng::new(1)).unwrap().remove(0);
        for k in sim.payload.as_map().unwrap().keys() {
            assert!(p.get_path(k).is_some(), "missing sim key {k}");
        }
        let js = serde_json::to_string(&e).unwrap();
        assert!(!js.contains("jo.example@example.com") && !js.contains("verification_token"));
    }

    #[test]
    fn private_supporters_are_anonymous() {
        let mut d = kofi_sample();
        d["is_public"] = json!(false);
        d["type"] = json!("Shop Order");
        d["amount"] = json!("12.5");
        let e = kofi_to_tip(&d).unwrap();
        assert_eq!(e.payload.get_path("user").and_then(Value::as_str), Some("Anonymous"));
        assert_eq!(e.payload.get_path("message").and_then(Value::as_str), Some(""));
        assert_eq!(e.payload.get_path("kind").and_then(Value::as_str), Some("shop_order"));
        assert_eq!(e.actor.unwrap().name, "Anonymous");
        d["amount"] = json!("free");
        assert!(kofi_to_tip(&d).is_err());
    }

    #[test]
    fn backoff_grows_and_caps() {
        assert!(backoff(0) <= Duration::from_secs_f64(1.21));
        assert!(backoff(3) >= Duration::from_secs_f64(6.3));
        assert!(backoff(30) <= Duration::from_secs(72));
        assert_eq!(host_of("wss://relay.example.com/link"), "relay.example.com");
    }
}
