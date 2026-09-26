//! Omarchy desktop integration done by the engine (§16.3): keep the machine awake from
//! `preshow` until `offline`, restoring the user's previous idle setting afterwards.

use crate::daemon::Ctx;
use se_hub::Bus;
use se_proto::Value;
use std::process::Command;

const PREV_KEY: &str = "omarchy.idle.previous";

fn status() -> Option<bool> {
    let out = Command::new("omarchy").args(["toggle", "idle", "status"]).output().ok()?;
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).ok()?;
    v.get("enabled")?.as_bool()
}

fn set(stay_awake: bool) -> bool {
    Command::new("omarchy").args(["toggle", "idle", if stay_awake { "stay-awake" } else { "allow-idle" }]).status().map(|s| s.success()).unwrap_or(false)
}

pub fn start_idle_inhibit(ctx: &Ctx) {
    if which("omarchy").is_none() {
        tracing::info!("omarchy not found; idle inhibition handled by window rules only");
        return;
    }
    let hub = ctx.hub.clone();
    let ctx = ctx.clone();
    tokio::spawn(async move {
        let mut bus = ctx.hub.subscribe();
        while let Ok(b) = bus.recv().await {
            let Bus::Event(e) = &*b else { continue };
            if e.ty != "mode.changed" {
                continue;
            }
            let to = e.payload.get_path("to").and_then(Value::as_str).unwrap_or("");
            let from = e.payload.get_path("from").and_then(Value::as_str).unwrap_or("");
            if from == "offline" && to != "offline" {
                if let Some(prev) = status()
                    && ctx.db.kv_get("engine", PREV_KEY).ok().flatten().is_none()
                {
                    let _ = ctx.db.kv_set("engine", PREV_KEY, &Value::Bool(prev));
                }
                let ok = set(true);
                ctx.hub.publish("system.idle_inhibited", Value::Bool(ok));
                tracing::info!("idle inhibited for the show ({ok})");
            } else if to == "offline" {
                restore_idle(&ctx).await;
            }
        }
    });
    // reflect current state for preflight
    let on = status().unwrap_or(false);
    hub.publish("system.idle_inhibited", Value::Bool(on));
}

pub async fn restore_idle(ctx: &Ctx) {
    if let Ok(Some(prev)) = ctx.db.kv_get("engine", PREV_KEY) {
        set(prev.truthy());
        let _ = ctx.db.kv_del("engine", PREV_KEY);
        ctx.hub.publish("system.idle_inhibited", Value::Bool(prev.truthy()));
        tracing::info!("idle setting restored (stay-awake = {})", prev.truthy());
    }
}

fn which(bin: &str) -> Option<std::path::PathBuf> {
    std::env::var_os("PATH")?.to_str()?.split(':').map(|d| std::path::Path::new(d).join(bin)).find(|p| p.exists())
}
