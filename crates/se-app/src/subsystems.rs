//! Engine subsystems started by the daemon after the core and API are up.

use crate::daemon::Ctx;
use se_api::Auth;
use std::sync::Arc;

pub async fn start(ctx: &Ctx, _auth: Arc<Auth>) {
    crate::omarchy::start_idle_inhibit(ctx);
}

pub async fn stop(ctx: &Ctx) {
    crate::omarchy::restore_idle(ctx).await;
}

/// Called after every project reload (subsystems that read their own config kinds).
pub fn on_reload(_ctx: &Ctx, _paths: &[String]) {}
