//! EventSub over WebSocket: session welcome → create subscriptions (within the 10 s window),
//! keepalive timeout detection, `session_reconnect` hand-over without losing events, message
//! de-duplication, revocations, and reconnect with backoff.

use crate::config::SubSource;
use crate::helix::{Helix, Method};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value as J, json};
use std::collections::{HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;

/// One subscription to create for a session.
#[derive(Clone, Debug, PartialEq)]
pub struct SubSpec {
    pub ty: &'static str,
    pub version: &'static str,
    pub condition: J,
}

/// Everything §11 listens to, for `broadcaster` (also the moderator/chat user).
pub fn subscriptions(b: &str, source: SubSource) -> Vec<SubSpec> {
    let bc = json!({ "broadcaster_user_id": b });
    let chat = json!({ "broadcaster_user_id": b, "user_id": b });
    let modr = json!({ "broadcaster_user_id": b, "moderator_user_id": b });
    let mut v = vec![
        SubSpec { ty: "channel.chat.message", version: "1", condition: chat.clone() },
        SubSpec { ty: "channel.chat.message_delete", version: "1", condition: chat.clone() },
        SubSpec { ty: "channel.chat.clear_user_messages", version: "1", condition: chat.clone() },
        SubSpec { ty: "channel.chat.clear", version: "1", condition: chat.clone() },
        SubSpec { ty: "channel.cheer", version: "1", condition: bc.clone() },
        SubSpec { ty: "channel.follow", version: "2", condition: modr.clone() },
        SubSpec { ty: "channel.raid", version: "1", condition: json!({ "to_broadcaster_user_id": b }) },
        SubSpec { ty: "channel.channel_points_custom_reward_redemption.add", version: "1", condition: bc.clone() },
        SubSpec { ty: "automod.message.hold", version: "2", condition: modr.clone() },
        SubSpec { ty: "automod.message.update", version: "2", condition: modr },
        SubSpec { ty: "channel.ban", version: "1", condition: bc.clone() },
        SubSpec { ty: "channel.unban", version: "1", condition: bc.clone() },
        SubSpec { ty: "channel.poll.begin", version: "1", condition: bc.clone() },
        SubSpec { ty: "channel.poll.progress", version: "1", condition: bc.clone() },
        SubSpec { ty: "channel.poll.end", version: "1", condition: bc.clone() },
        SubSpec { ty: "channel.prediction.begin", version: "1", condition: bc.clone() },
        SubSpec { ty: "channel.prediction.progress", version: "1", condition: bc.clone() },
        SubSpec { ty: "channel.prediction.lock", version: "1", condition: bc.clone() },
        SubSpec { ty: "channel.prediction.end", version: "1", condition: bc.clone() },
        SubSpec { ty: "channel.hype_train.begin", version: "2", condition: bc.clone() },
        SubSpec { ty: "channel.hype_train.progress", version: "2", condition: bc.clone() },
        SubSpec { ty: "channel.hype_train.end", version: "2", condition: bc.clone() },
        SubSpec { ty: "channel.ad_break.begin", version: "1", condition: bc.clone() },
        SubSpec { ty: "stream.online", version: "1", condition: bc.clone() },
        SubSpec { ty: "stream.offline", version: "1", condition: bc.clone() },
        SubSpec { ty: "channel.update", version: "2", condition: bc.clone() },
    ];
    match source {
        SubSource::Chat => v.push(SubSpec { ty: "channel.chat.notification", version: "1", condition: chat }),
        SubSource::Eventsub => {
            for ty in ["channel.subscribe", "channel.subscription.gift", "channel.subscription.message"] {
                v.push(SubSpec { ty, version: "1", condition: bc.clone() });
            }
        }
    }
    v
}

#[derive(Clone, Debug, PartialEq)]
pub enum Frame {
    Welcome { session_id: String, keepalive_s: u64 },
    Keepalive,
    Notification { message_id: String, ty: String, version: String, event: J },
    Reconnect { url: String },
    Revocation { ty: String, status: String },
    Other,
}

pub fn parse_frame(text: &str) -> Option<Frame> {
    let v: J = serde_json::from_str(text).ok()?;
    let md = v.get("metadata")?;
    let id = md.get("message_id").and_then(J::as_str).unwrap_or("").to_string();
    let p = v.get("payload").cloned().unwrap_or(J::Null);
    Some(match md.get("message_type").and_then(J::as_str)? {
        "session_welcome" => Frame::Welcome {
            session_id: p.pointer("/session/id").and_then(J::as_str)?.to_string(),
            keepalive_s: p.pointer("/session/keepalive_timeout_seconds").and_then(J::as_u64).unwrap_or(10),
        },
        "session_keepalive" => Frame::Keepalive,
        "notification" => Frame::Notification {
            message_id: id,
            ty: p.pointer("/subscription/type").and_then(J::as_str).or_else(|| md.get("subscription_type").and_then(J::as_str)).unwrap_or("").to_string(),
            version: p
                .pointer("/subscription/version")
                .and_then(J::as_str)
                .or_else(|| md.get("subscription_version").and_then(J::as_str))
                .unwrap_or("1")
                .to_string(),
            event: p.get("event").cloned().unwrap_or(J::Null),
        },
        "session_reconnect" => Frame::Reconnect { url: p.pointer("/session/reconnect_url").and_then(J::as_str)?.to_string() },
        "revocation" => Frame::Revocation {
            ty: p.pointer("/subscription/type").and_then(J::as_str).unwrap_or("").to_string(),
            status: p.pointer("/subscription/status").and_then(J::as_str).unwrap_or("").to_string(),
        },
        _ => Frame::Other,
    })
}

/// Recently seen message ids (Twitch may redeliver).
pub struct Dedup {
    order: VecDeque<String>,
    seen: HashSet<String>,
    cap: usize,
}

impl Dedup {
    pub fn new(cap: usize) -> Dedup {
        Dedup { order: VecDeque::with_capacity(cap), seen: HashSet::with_capacity(cap), cap }
    }
    /// `true` the first time an id is seen.
    pub fn first(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return true;
        }
        if self.seen.contains(id) {
            return false;
        }
        if self.order.len() >= self.cap
            && let Some(old) = self.order.pop_front()
        {
            self.seen.remove(&old);
        }
        self.order.push_back(id.to_string());
        self.seen.insert(id.to_string());
        true
    }
}

/// Connection state reported to the service.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Status {
    pub connected: bool,
    pub session_id: String,
    pub subscriptions: usize,
    /// `(type, error)` for subscriptions Twitch refused (missing scope, unsupported, …).
    pub failed: Vec<(String, String)>,
    pub revoked: Vec<(String, String)>,
    pub reconnects: u64,
    pub last_error: String,
}

/// A notification for the service to normalize.
#[derive(Debug, Clone)]
pub struct Notification {
    pub ty: String,
    pub version: String,
    pub event: J,
}

pub struct Runner {
    pub url: String,
    pub subscriptions_url: String,
    pub specs: Vec<SubSpec>,
    pub helix: Arc<Helix>,
}

async fn read_welcome(ws: &mut Ws) -> anyhow::Result<(String, u64)> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let msg = tokio::time::timeout_at(deadline, ws.next()).await.map_err(|_| anyhow::anyhow!("no session_welcome within 10 s"))?;
        match msg {
            Some(Ok(Message::Text(t))) => {
                if let Some(Frame::Welcome { session_id, keepalive_s }) = parse_frame(&t) {
                    return Ok((session_id, keepalive_s));
                }
            }
            Some(Ok(Message::Close(c))) => anyhow::bail!("closed before welcome: {c:?}"),
            Some(Ok(_)) => {}
            Some(Err(e)) => return Err(e.into()),
            None => anyhow::bail!("closed before welcome"),
        }
    }
}

async fn connect(url: &str) -> anyhow::Result<(Ws, String, u64)> {
    let (mut ws, _) =
        tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(url)).await.map_err(|_| anyhow::anyhow!("connect timed out"))??;
    let (sid, ka) = read_welcome(&mut ws).await?;
    Ok((ws, sid, ka))
}

enum Next {
    /// Reconnect from scratch (new session, subscriptions recreated).
    Fresh(String),
    Stop,
}

impl Runner {
    async fn subscribe(&self, session: &str, status: &mut Status) {
        let futs = self.specs.iter().map(|s| {
            let body = json!({ "type": s.ty, "version": s.version, "condition": s.condition, "transport": { "method": "websocket", "session_id": session } });
            let helix = self.helix.clone();
            let url = self.subscriptions_url.clone();
            async move { (s.ty, helix.call_url(crate::auth::Account::Broadcaster, Method::Post, &url, &[], Some(&body)).await) }
        });
        let results = futures_util::future::join_all(futs).await;
        status.failed.clear();
        status.subscriptions = 0;
        for (ty, r) in results {
            match r {
                Ok(_) => status.subscriptions += 1,
                Err(e) if e.status == 409 => status.subscriptions += 1,
                Err(e) => status.failed.push((ty.to_string(), e.to_string())),
            }
        }
    }

    /// Run until `stop` flips; notifications go to `sink`, status changes to `report`.
    pub async fn run(self, sink: mpsc::Sender<Notification>, report: impl Fn(&Status) + Send, mut stop: watch::Receiver<bool>) {
        let mut status = Status::default();
        let mut dedup = Dedup::new(2000);
        let mut backoff = 1u64;
        let mut url = self.url.clone();
        loop {
            if *stop.borrow() {
                return;
            }
            let (ws, session, keepalive) = match connect(&url).await {
                Ok(c) => c,
                Err(e) => {
                    status.connected = false;
                    status.last_error = format!("{e:#}");
                    report(&status);
                    tracing::warn!(target: "twitch", "EventSub connect failed: {e:#}; retrying in {backoff}s");
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(backoff)) => {}
                        _ = stop.changed() => return,
                    }
                    backoff = (backoff * 2).min(30);
                    url = self.url.clone();
                    continue;
                }
            };
            backoff = 1;
            status.connected = true;
            status.session_id = session.clone();
            status.last_error.clear();
            self.subscribe(&session, &mut status).await;
            if !status.failed.is_empty() {
                tracing::warn!(target: "twitch", "EventSub: {} subscription(s) refused: {:?}", status.failed.len(), status.failed);
            }
            tracing::info!(target: "twitch", "EventSub session {session}: {} subscriptions", status.subscriptions);
            report(&status);
            match self.session(ws, keepalive, &sink, &report, &mut status, &mut dedup, &mut stop).await {
                Next::Stop => return,
                Next::Fresh(reason) => {
                    status.connected = false;
                    status.reconnects += 1;
                    status.last_error = reason.clone();
                    report(&status);
                    tracing::warn!(target: "twitch", "EventSub: {reason}; reconnecting");
                    url = self.url.clone();
                    tokio::select! {
                        _ = tokio::time::sleep(Duration::from_secs(1)) => {}
                        _ = stop.changed() => return,
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn session(
        &self,
        mut ws: Ws,
        mut keepalive: u64,
        sink: &mpsc::Sender<Notification>,
        report: &(impl Fn(&Status) + Send),
        status: &mut Status,
        dedup: &mut Dedup,
        stop: &mut watch::Receiver<bool>,
    ) -> Next {
        loop {
            let timeout = Duration::from_secs(keepalive + 5);
            let msg = tokio::select! {
                m = tokio::time::timeout(timeout, ws.next()) => m,
                _ = stop.changed() => {
                    let _ = ws.close(None).await;
                    return Next::Stop;
                }
            };
            let text = match msg {
                Err(_) => return Next::Fresh(format!("no message for {}s (keepalive timeout)", keepalive + 5)),
                Ok(None) => return Next::Fresh("connection closed".into()),
                Ok(Some(Err(e))) => return Next::Fresh(format!("websocket error: {e}")),
                Ok(Some(Ok(Message::Close(c)))) => {
                    return Next::Fresh(format!("closed by Twitch: {}", c.map(|c| format!("{} {}", u16::from(c.code), c.reason)).unwrap_or_default()));
                }
                Ok(Some(Ok(Message::Ping(p)))) => {
                    let _ = ws.send(Message::Pong(p)).await;
                    continue;
                }
                Ok(Some(Ok(Message::Text(t)))) => t,
                Ok(Some(Ok(_))) => continue,
            };
            match parse_frame(&text) {
                Some(Frame::Notification { message_id, ty, version, event }) => {
                    if dedup.first(&message_id) && sink.send(Notification { ty, version, event }).await.is_err() {
                        return Next::Stop;
                    }
                }
                Some(Frame::Reconnect { url }) => {
                    // keep reading the old socket until the new one says welcome (no lost events)
                    let fut = connect(&url);
                    tokio::pin!(fut);
                    let deadline = tokio::time::sleep(Duration::from_secs(30));
                    tokio::pin!(deadline);
                    loop {
                        tokio::select! {
                            r = &mut fut => match r {
                                Ok((new_ws, sid, ka)) => {
                                    let _ = ws.close(None).await;
                                    ws = new_ws;
                                    keepalive = ka;
                                    status.session_id = sid;
                                    status.reconnects += 1;
                                    report(status);
                                    tracing::info!(target: "twitch", "EventSub moved to session {} (session_reconnect)", status.session_id);
                                    break;
                                }
                                Err(e) => return Next::Fresh(format!("session_reconnect to {url} failed: {e:#}")),
                            },
                            m = ws.next() => match m {
                                Some(Ok(Message::Text(t))) => {
                                    if let Some(Frame::Notification { message_id, ty, version, event }) = parse_frame(&t)
                                        && dedup.first(&message_id)
                                        && sink.send(Notification { ty, version, event }).await.is_err()
                                    {
                                        return Next::Stop;
                                    }
                                }
                                Some(Ok(_)) => {}
                                _ => {
                                    // old socket gone first: keep waiting for the new welcome
                                    match (&mut fut).await {
                                        Ok((new_ws, sid, ka)) => {
                                            ws = new_ws;
                                            keepalive = ka;
                                            status.session_id = sid;
                                            status.reconnects += 1;
                                            report(status);
                                            break;
                                        }
                                        Err(e) => return Next::Fresh(format!("session_reconnect to {url} failed: {e:#}")),
                                    }
                                }
                            },
                            _ = &mut deadline => return Next::Fresh("session_reconnect timed out".into()),
                            _ = stop.changed() => return Next::Stop,
                        }
                    }
                }
                Some(Frame::Revocation { ty, status: why }) => {
                    tracing::warn!(target: "twitch", "EventSub revoked {ty}: {why}");
                    status.revoked.push((ty, why));
                    status.subscriptions = status.subscriptions.saturating_sub(1);
                    report(status);
                }
                Some(Frame::Welcome { .. } | Frame::Keepalive | Frame::Other) | None => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_from_docs() {
        let welcome = r#"{"metadata":{"message_id":"96a3f3b5","message_type":"session_welcome","message_timestamp":"2023-07-19T14:56:51.634234626Z"},"payload":{"session":{"id":"AQoQILE98gtqShGmLD7AM6yJThAB","status":"connected","connected_at":"2023-07-19T14:56:51.616329898Z","keepalive_timeout_seconds":10,"reconnect_url":null}}}"#;
        assert_eq!(parse_frame(welcome), Some(Frame::Welcome { session_id: "AQoQILE98gtqShGmLD7AM6yJThAB".into(), keepalive_s: 10 }));
        let reconnect = r#"{"metadata":{"message_id":"84c1e79a","message_type":"session_reconnect","message_timestamp":"2022-11-18T09:10:11.634234626Z"},"payload":{"session":{"id":"AQoQexAWVYKSTIu4ec_2VAxyuhAB","status":"reconnecting","keepalive_timeout_seconds":null,"reconnect_url":"wss://eventsub.wss.twitch.tv?...","connected_at":"2022-11-16T10:11:12.634234626Z"}}}"#;
        assert_eq!(parse_frame(reconnect), Some(Frame::Reconnect { url: "wss://eventsub.wss.twitch.tv?...".into() }));
        let note = r#"{"metadata":{"message_id":"befa7b53","message_type":"notification","message_timestamp":"2022-11-16T10:11:12.464757833Z","subscription_type":"channel.follow","subscription_version":"2"},"payload":{"subscription":{"id":"f1c2a387","status":"enabled","type":"channel.follow","version":"2","cost":1,"condition":{"broadcaster_user_id":"12826"},"transport":{"method":"websocket","session_id":"AQoQexAWVYKSTIu4ec_2VAxyuhAB"},"created_at":"2022-11-16T10:11:12.464757833Z"},"event":{"user_id":"1337","user_login":"awesome_user","user_name":"Awesome_User","broadcaster_user_id":"12826","followed_at":"2023-07-15T18:16:11.17106713Z"}}}"#;
        match parse_frame(note) {
            Some(Frame::Notification { message_id, ty, version, event }) => {
                assert_eq!((message_id.as_str(), ty.as_str(), version.as_str()), ("befa7b53", "channel.follow", "2"));
                assert_eq!(event["user_login"], "awesome_user");
            }
            other => panic!("{other:?}"),
        }
        let revoke = r#"{"metadata":{"message_id":"84c1e79a","message_type":"revocation","message_timestamp":"2022-11-16T10:11:12.464757833Z","subscription_type":"channel.follow","subscription_version":"1"},"payload":{"subscription":{"id":"f1c2a387","status":"authorization_revoked","type":"channel.follow","version":"1","cost":1,"condition":{"broadcaster_user_id":"12826"},"transport":{"method":"websocket","session_id":"AQoQexAWVYKSTIu4ec_2VAxyuhAB"},"created_at":"2022-11-16T10:11:12.464757833Z"}}}"#;
        assert_eq!(parse_frame(revoke), Some(Frame::Revocation { ty: "channel.follow".into(), status: "authorization_revoked".into() }));
        assert_eq!(parse_frame(r#"{"metadata":{"message_type":"session_keepalive"},"payload":{}}"#), Some(Frame::Keepalive));
    }

    #[test]
    fn dedup_is_bounded() {
        let mut d = Dedup::new(2);
        assert!(d.first("a"));
        assert!(!d.first("a"));
        assert!(d.first("b"));
        assert!(d.first("c"));
        assert!(d.first("a"), "evicted ids may be seen again");
    }

    #[test]
    fn subscription_sets_by_source() {
        let chat = subscriptions("1", SubSource::Chat);
        assert!(chat.iter().any(|s| s.ty == "channel.chat.notification"));
        assert!(!chat.iter().any(|s| s.ty == "channel.subscribe"));
        let es = subscriptions("1", SubSource::Eventsub);
        assert!(es.iter().any(|s| s.ty == "channel.subscription.gift"));
        assert_eq!(es.iter().find(|s| s.ty == "channel.follow").unwrap().condition["moderator_user_id"], "1");
    }
}
