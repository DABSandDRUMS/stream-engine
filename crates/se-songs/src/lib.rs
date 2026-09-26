//! Song requests (§13), Ko-fi tips (§14.5) and the engine side of the Cloudflare relay (§14.6).
//!
//! * [`youtube`]: YouTube Data API v3 client (`videos.list` 1 unit, `search.list` 100 units).
//! * [`quota`]: daily quota ledger in the runtime DB, reset at midnight Pacific.
//! * [`store`]: SQLite cache/library (query → ids, id → metadata, play counts), queue rows.
//! * [`policy`]: the UI-editable queue policy (§13.2) and its request/video gates.
//! * [`queue`]: ordering of pending/queued entries.
//! * [`player`]: desired state for the two-slot player page (`web/player.html`) and its reports.
//! * [`relay`]: outbound WebSocket to the relay (queue snapshots out, Ko-fi payments in).
//! * [`service`]: the actor tying it all to the hub (actions `queue.*`, `youtube.*`, `relay.*`).

pub mod lookup;
pub mod messages;
pub mod player;
pub mod policy;
pub mod queue;
pub mod quota;
pub mod relay;
pub mod secrets;
pub mod service;
pub mod settings;
pub mod store;
pub mod text;
pub mod youtube;

use std::sync::Arc;

/// Start the song-request subsystem (reads the YouTube key and relay secret from the keyring).
pub async fn start(ctx: se_hub::EngineCtx) -> anyhow::Result<()> {
    // Both reqwest and tungstenite use rustls; pin one process-wide crypto provider so a second
    // provider enabled elsewhere in the dependency graph can never make TLS setup ambiguous.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    service::spawn(ctx, Arc::new(secrets::Keyring)).await.map(|_| ())
}
