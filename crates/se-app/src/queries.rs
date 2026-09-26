//! Engine-level named queries (sessions, audit log, engine info).

use crate::daemon::Ctx;
use se_proto::Value;
use std::sync::Arc;

pub fn register(ctx: &Ctx) {
    let db = ctx.db.clone();
    ctx.hub.register_query(
        "sessions",
        Arc::new(move |_, args| {
            let db = db.clone();
            Box::pin(async move {
                let n = args.get_path("n").and_then(Value::as_i64).unwrap_or(50).clamp(1, 1000) as usize;
                db.sessions(n).map(Value::List).map_err(|e| e.to_string())
            })
        }),
    );
    let db = ctx.db.clone();
    ctx.hub.register_query(
        "audit",
        Arc::new(move |_, args| {
            let db = db.clone();
            Box::pin(async move {
                let n = args.get_path("n").and_then(Value::as_i64).unwrap_or(200).clamp(1, 5000) as usize;
                db.audit_recent(n).map(Value::List).map_err(|e| e.to_string())
            })
        }),
    );
    let hub = ctx.hub.clone();
    let project = ctx.project.root().display().to_string();
    ctx.hub.register_query(
        "engine.info",
        Arc::new(move |_, _| {
            let info = hub.info.read().clone();
            let project = project.clone();
            Box::pin(async move {
                Ok(Value::map()
                    .with("version", info.version)
                    .with("session", info.session)
                    .with("project", project)
                    .with("pid", std::process::id() as i64)
                    .with("started", info.started_wall))
            })
        }),
    );
}
