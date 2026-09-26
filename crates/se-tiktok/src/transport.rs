//! The network boundary. The session logic only talks to TikTok through [`Transport`], so it
//! can be driven by the real web client (`client` feature) or by in-memory fakes in tests.

use crate::proto::ProtoMessageFetchResult;
use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RoomStatus {
    Live { room_id: String },
    Offline,
}

/// Everything needed to open the webcast socket, as returned by the sign provider.
#[derive(Clone, Debug, PartialEq)]
pub struct SignedConnect {
    /// Complete WebSocket URL (push server + route params + client params).
    pub ws_url: String,
    /// `Cookie` header for the socket (anonymous cookies only).
    pub cookie: String,
    pub user_agent: String,
    /// Room id to join (the provider may correct it).
    pub room_id: String,
    /// The first message batch (backfill).
    pub initial: ProtoMessageFetchResult,
    /// Whether an API key was sent to the provider.
    pub authenticated: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportError {
    /// Transient network/HTTP failure: retry with backoff.
    Network(String),
    /// The sign provider's rate limit; `retry_after` when it said so.
    RateLimited { message: String, retry_after: Option<Duration> },
    /// API key rejected or feature not in the plan (401/402/403).
    Auth(String),
    /// The TikTok user does not exist / can't go live.
    NotFound(String),
    /// TikTok refused us (captcha page, WebSocket handshake rejected).
    Blocked(String),
    /// Unexpected response shape (TikTok or the provider changed something).
    Protocol(String),
}

impl std::fmt::Display for TransportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TransportError::Network(m) => write!(f, "network: {m}"),
            TransportError::RateLimited { message, .. } => write!(f, "sign provider rate limit: {message}"),
            TransportError::Auth(m) => write!(f, "sign provider refused: {m}"),
            TransportError::NotFound(m) => write!(f, "not found: {m}"),
            TransportError::Blocked(m) => write!(f, "blocked by TikTok: {m}"),
            TransportError::Protocol(m) => write!(f, "unexpected response: {m}"),
        }
    }
}

impl std::error::Error for TransportError {}

/// An open webcast socket carrying binary `WebcastPushFrame`s.
pub trait Socket: Send {
    /// Next binary frame; `None` when the connection closed.
    fn recv(&mut self) -> impl Future<Output = Option<Result<Vec<u8>, TransportError>>> + Send;
    fn send(&mut self, frame: Vec<u8>) -> impl Future<Output = Result<(), TransportError>> + Send;
    /// Close politely (bounded in time).
    fn close(&mut self) -> impl Future<Output = ()> + Send;
    /// Heartbeat interval requested by the server (`Handshake-Options: ping-interval=…`).
    fn ping_interval(&self) -> Option<Duration>;
}

pub trait Transport: Send + Sync + 'static {
    type Socket: Socket + 'static;
    /// Is `@unique_id` live, and in which room?
    fn room_status(&self, unique_id: &str) -> impl Future<Output = Result<RoomStatus, TransportError>> + Send;
    /// Ask the sign provider for the signed socket URL, cookies and first batch.
    fn sign(&self, room_id: &str) -> impl Future<Output = Result<SignedConnect, TransportError>> + Send;
    fn open(&self, signed: &SignedConnect) -> impl Future<Output = Result<Self::Socket, TransportError>> + Send;
}
