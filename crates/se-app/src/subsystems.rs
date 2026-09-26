//! Engine subsystems started by the daemon after the core and API are up.
//!
//! `SE_DISABLE=render,web,…` (comma-separated subsystem names as logged below) skips
//! subsystems for development, e.g. several dev engines on one machine must not all open
//! the GPU and a CEF host.

use crate::daemon::Ctx;
use se_api::Auth;
use std::collections::HashSet;
use std::sync::Arc;

fn disabled() -> HashSet<String> {
    std::env::var("SE_DISABLE").map(|v| v.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect()).unwrap_or_default()
}

pub async fn start(ctx: &Ctx, auth: Arc<Auth>) {
    let off = disabled();
    if !off.is_empty() {
        tracing::warn!("SE_DISABLE: skipping {}", off.iter().cloned().collect::<Vec<_>>().join(", "));
    }
    let on = |name: &str| !off.contains(name);
    crate::omarchy::start_idle_inhibit(ctx);
    if on("devices")
        && let Err(e) = se_devices::start(ctx.engine.clone()).await
    {
        tracing::error!("devices: {e:#}");
    }
    if on("video-in")
        && let Err(e) = se_video_in::start(ctx.engine.clone()).await
    {
        tracing::error!("video-in: {e:#}");
    }
    if on("render")
        && let Err(e) = se_render::start(ctx.engine.clone()).await
    {
        tracing::error!("render: {e:#}");
    }
    if on("extras")
        && let Err(e) = se_extras::start(ctx.engine.clone()).await
    {
        tracing::error!("extras: {e:#}");
    }
    if on("tts")
        && let Err(e) = se_tts::start(ctx.engine.clone()).await
    {
        tracing::error!("tts: {e:#}");
    }
    if on("tiktok")
        && let Err(e) = se_tiktok::start(ctx.engine.clone()).await
    {
        tracing::error!("tiktok: {e:#}");
    }
    if on("clips")
        && let Err(e) = se_clips::start(ctx.engine.clone()).await
    {
        tracing::error!("clips: {e:#}");
    }
    if on("obs")
        && let Err(e) = se_obs::start(ctx.engine.clone()).await
    {
        tracing::error!("obs: {e:#}");
    }
    if on("audio")
        && let Err(e) = se_audio::start(ctx.engine.clone()).await
    {
        tracing::error!("audio: {e:#}");
    }
    if on("patches") {
        match se_patch::start(ctx.engine.clone()).await {
            Ok(patches) => {
                if on("web")
                    && let Err(e) = se_web::start(ctx.engine.clone(), auth.clone(), patches).await
                {
                    tracing::error!("web: {e:#}");
                }
            }
            Err(e) => tracing::error!("patches: {e:#}"),
        }
    }
    if on("twitch")
        && let Err(e) = se_twitch::start(ctx.engine.clone()).await
    {
        tracing::error!("twitch: {e:#}");
    }
    if on("bot")
        && let Err(e) = se_bot::start(ctx.engine.clone()).await
    {
        tracing::error!("bot: {e:#}");
    }
    if on("alerts")
        && let Err(e) = se_alerts::start(ctx.engine.clone()).await
    {
        tracing::error!("alerts: {e:#}");
    }
    if on("lights")
        && let Err(e) = se_dmx::start(ctx.engine.clone()).await
    {
        tracing::error!("lights: {e:#}");
    }
    if on("input")
        && let Err(e) = se_input::start(ctx.engine.clone()).await
    {
        tracing::error!("input: {e:#}");
    }
    if on("timelines")
        && let Err(e) = se_timeline::start(ctx.engine.clone()).await
    {
        tracing::error!("timelines: {e:#}");
    }
    if on("mixer")
        && let Err(e) = se_mixer::start(ctx.engine.clone()).await
    {
        tracing::error!("mixer: {e:#}");
    }
    if on("songs")
        && let Err(e) = se_songs::start(ctx.engine.clone()).await
    {
        tracing::error!("songs: {e:#}");
    }
}

pub async fn stop(ctx: &Ctx) {
    crate::omarchy::restore_idle(ctx).await;
    se_render::stop().await;
    se_input::shutdown().await;
    se_dmx::stop().await;
    se_audio::stop().await;
}
