//! Extra HTTP routes owned by the engine: web patch folders are served from the project.

use crate::daemon::Ctx;
use axum::Router;
use se_api::Auth;
use std::sync::Arc;

pub fn routes(ctx: &Ctx, _auth: Arc<Auth>) -> Router {
    let patches = tower_http::services::ServeDir::new(ctx.project.root().join("patches"));
    let assets = tower_http::services::ServeDir::new(ctx.project.root().join("assets"));
    Router::new().nest_service("/patches", patches).nest_service("/assets", assets)
}
