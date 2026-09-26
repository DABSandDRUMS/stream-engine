//! Settings support (§15.6, §19): API endpoints, paired device tokens (persisted), full-token
//! rotation (the token itself is never served over the API: read-scoped clients can query),
//! token rotation, and write-only secret entry (YouTube key, relay secret) for the UI.

use crate::daemon::Ctx;
use se_api::auth::{Auth, Scope};
use se_proto::{Event, Op, Origin, Value};
use se_store::secrets;
use std::sync::Arc;

/// Secrets the UI may set (never read back — only whether they are present).
pub const SETTABLE: &[(&str, &str)] =
    &[(secrets::names::YOUTUBE_KEY, "YouTube Data API key"), (secrets::names::RELAY_SECRET, "Cloudflare relay shared secret")];

const DEVICES_NS: &str = "api.devices";

fn scope_name(s: &Scope) -> String {
    match s {
        Scope::Full => "full".into(),
        Scope::ReadOnly => "read".into(),
        Scope::Patch(id) => format!("patch:{id}"),
        Scope::Mod => "mod".into(),
    }
}

fn parse_scope(s: &str) -> Option<Scope> {
    match s {
        "full" => Some(Scope::Full),
        "read" | "readonly" => Some(Scope::ReadOnly),
        "mod" => Some(Scope::Mod),
        _ => None,
    }
}

pub fn start(ctx: &Ctx, auth: Arc<Auth>) {
    // restore paired devices
    match ctx.db.kv_list(DEVICES_NS) {
        Ok(list) => {
            for (name, v) in list {
                let (Some(tok), Some(scope)) = (v.get_path("token").and_then(Value::as_str), v.get_path("scope").and_then(Value::as_str).and_then(parse_scope))
                else {
                    continue;
                };
                auth.add(tok, &name, scope);
            }
        }
        Err(e) => tracing::warn!("paired devices: {e:#}"),
    }
    {
        let (auth, opts) = (auth.clone(), ctx.opts.clone());
        let sock = opts.socket.clone().unwrap_or_else(se_proto::wire::default_socket_path);
        ctx.hub.register_query(
            "api.info",
            Arc::new(move |_, _| {
                let auth = auth.clone();
                let (http, osc, sock) = (opts.http, opts.osc, sock.clone());
                Box::pin(async move {
                    let devices: Vec<Value> = auth.devices().into_iter().map(|(n, s)| Value::map().with("name", n).with("scope", scope_name(&s))).collect();
                    Ok(Value::map()
                        .with("socket", sock.display().to_string())
                        .with("http", format!("http://{http}"))
                        .with("ws", format!("ws://{http}/ws"))
                        .with("osc", format!("udp://{osc}"))
                        .with("token_set", secrets::get(secrets::names::API_TOKEN).ok().flatten().is_some_and(|t| !t.is_empty()))
                        .with("devices", devices))
                })
            }),
        );
    }
    ctx.hub.register_query(
        "secrets.status",
        Arc::new(|_, _| {
            Box::pin(async move {
                let mut out = Vec::new();
                for (name, label) in SETTABLE {
                    let set = secrets::get(name).map(|v| v.is_some_and(|s| !s.is_empty()));
                    out.push(
                        Value::map()
                            .with("name", *name)
                            .with("label", *label)
                            .with("set", set.as_ref().is_ok_and(|b| *b))
                            .with("error", set.err().map(|e| Value::Str(format!("{e:#}"))).unwrap_or_default()),
                    );
                }
                Ok(Value::List(out))
            })
        }),
    );
    for prefix in ["api", "secrets"] {
        let (ctx, auth) = (ctx.clone(), auth.clone());
        let mut rx = ctx.hub.route_actions(prefix);
        tokio::spawn(async move {
            while let Some(c) = rx.recv().await {
                let Op::Action { name, args } = &c.op else { continue };
                if let Err(e) = handle(&ctx, &auth, name, args) {
                    ctx.hub.log("error", "api", format!("{name}: {e}"));
                }
            }
        });
    }
}

fn handle(ctx: &Ctx, auth: &Auth, name: &str, args: &Value) -> Result<(), String> {
    let s = |k: &str| args.get_path(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty());
    match name {
        "api.token.rotate" => {
            let t = secrets::random_token();
            secrets::set(secrets::names::API_TOKEN, &t).map_err(|e| format!("{e:#}"))?;
            auth.set_full(Some(t));
            ctx.hub.log("info", "api", "API token rotated; reconnect WebSocket/OSC clients with the new token");
        }
        "api.device.add" => {
            let dev = s("name").ok_or("needs `name`")?;
            let tok = s("token").ok_or("needs `token`")?;
            if tok.len() < 24 {
                return Err("token must be at least 24 characters".into());
            }
            let scope_s = s("scope").unwrap_or("read");
            let scope = parse_scope(scope_s).ok_or_else(|| format!("unknown scope `{scope_s}` (full | read | mod)"))?;
            auth.remove_named(dev);
            auth.add(tok, dev, scope);
            ctx.db.kv_set(DEVICES_NS, dev, &Value::map().with("token", tok).with("scope", scope_s)).map_err(|e| format!("{e:#}"))?;
        }
        "api.device.remove" => {
            let dev = s("name").ok_or("needs `name`")?;
            auth.remove_named(dev);
            ctx.db.kv_del(DEVICES_NS, dev).map_err(|e| format!("{e:#}"))?;
        }
        "secrets.set" | "secrets.delete" => {
            let key = s("name").ok_or("needs `name`")?;
            if !SETTABLE.iter().any(|(n, _)| *n == key) {
                return Err(format!("`{key}` cannot be set from the UI"));
            }
            if name == "secrets.set" {
                secrets::set(key, s("value").ok_or("needs `value`")?).map_err(|e| format!("{e:#}"))?;
            } else {
                secrets::delete(key).map_err(|e| format!("{e:#}"))?;
            }
            ctx.hub.emit(Event::new("secrets.changed", Origin::System, Value::map().with("name", key)));
        }
        other => return Err(format!("unknown action {other}")),
    }
    Ok(())
}
