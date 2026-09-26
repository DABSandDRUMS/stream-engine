//! Twitch integration (§11): OAuth device code flow with keyring-stored refresh tokens,
//! EventSub over WebSocket normalized to the engine's event contract (`se_core::sim`), and a
//! Helix client for chat, rewards, moderation, polls/predictions, raids, ads, and markers.
//!
//! The policy decisions on what Twitch activity may do (roles, cooldowns, refunds, veto,
//! approvals) live in the core (`se_core::policy`); this crate feeds it events and executes the
//! resulting `twitch.*` / `mod.*` actions.

pub mod auth;
pub mod chat;
pub mod config;
pub mod emotes;
pub mod eventsub;
pub mod followers;
pub mod helix;
#[cfg(feature = "mock")]
pub mod mock;
pub mod normalize;
pub mod rewards;
pub mod service;
pub mod time;
pub mod users;

use std::sync::Arc;

pub use service::Service;

/// Start the Twitch subsystem (tokens from the system keyring).
pub async fn start(ctx: se_hub::EngineCtx) -> anyhow::Result<Arc<Service>> {
    start_with(ctx, Arc::new(auth::Keyring)).await
}

/// Start with an explicit secret store (tests, dry runs).
pub async fn start_with(ctx: se_hub::EngineCtx, store: Arc<dyn auth::SecretStore>) -> anyhow::Result<Arc<Service>> {
    let svc = Service::new(ctx, store);
    svc.clone().run().await;
    Ok(svc)
}
