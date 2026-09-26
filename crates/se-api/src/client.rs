//! One API client session, shared by every transport (Unix socket, WebSocket).

use crate::auth::{Auth, Scope};
use se_hub::{Bus, Hub, any_match};
use se_proto::wire::{ClientMsg, PROTOCOL_VERSION, ServerMsg, StateEntry, Subscription};
use se_proto::{Command, Id, Op, Origin, Value};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct ClientCfg {
    /// Unix-socket clients are trusted by file permissions (no token needed).
    pub trusted: bool,
    pub default_origin: Origin,
    pub name: String,
}

/// Drive a client: `inbox` yields decoded client messages, `out` receives server messages.
pub async fn serve(hub: Arc<Hub>, auth: Arc<Auth>, cfg: ClientCfg, mut inbox: mpsc::Receiver<ClientMsg>, out: mpsc::Sender<ServerMsg>) {
    let mut scope: Option<Scope> = if cfg.trusted { Some(Scope::Full) } else { None };
    let mut sub = Subscription::default();
    let mut own: HashMap<Id, Option<u64>> = HashMap::new();
    let mut bus = hub.subscribe();
    let mut sig_tick = tokio::time::interval(std::time::Duration::from_millis(50));
    sig_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut sig_cache: (u64, Vec<usize>) = (u64::MAX, Vec::new());
    let mut last_sig = std::time::Instant::now();
    loop {
        tokio::select! {
            msg = inbox.recv() => {
                let Some(msg) = msg else { break };
                if let ClientMsg::Hello { token, .. } = &msg {
                    if scope.is_none() {
                        scope = token.as_deref().and_then(|t| auth.check(t));
                        if scope.is_none() {
                            let _ = out.send(ServerMsg::Error { msg: "unauthorized".into() }).await;
                            break;
                        }
                    }
                    let info = hub.info.read().clone();
                    let _ = out.send(ServerMsg::Welcome { version: PROTOCOL_VERSION, engine: info.version, session: info.session, now: se_clock::now(), pid: std::process::id() }).await;
                    continue;
                }
                let Some(sc) = scope.clone() else {
                    let _ = out.send(ServerMsg::Error { msg: "send hello with a token first".into() }).await;
                    break;
                };
                if !handle(&hub, &sc, &cfg, msg, &mut sub, &mut own, &out).await {
                    break;
                }
            }
            b = bus.recv() => {
                match b {
                    Ok(b) => {
                        if !forward(&b, &sub, &mut own, &out).await { break; }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        tracing::debug!(client = %cfg.name, "lagged {n} bus messages; resyncing");
                        if !resync(&hub, &sub, &out).await { break; }
                    }
                    Err(_) => break,
                }
            }
            _ = sig_tick.tick(), if !sub.signals.is_empty() => {
                let hz = sub.signal_hz.unwrap_or(20.0).clamp(1.0, 120.0);
                if last_sig.elapsed().as_secs_f32() < 1.0 / hz - 0.001 { continue; }
                last_sig = std::time::Instant::now();
                let snap = hub.snapshot.load();
                if sig_cache.0 != snap.signal_names.len() as u64 {
                    sig_cache.0 = snap.signal_names.len() as u64;
                    sig_cache.1 = snap.signal_names.iter().enumerate().filter(|(_, n)| any_match(&sub.signals, n)).map(|(i, _)| i).collect();
                }
                let values = sig_cache.1.iter().filter_map(|&i| Some((snap.signal_names.get(i)?.clone(), *snap.signals.get(i)?))).collect();
                if out.send(ServerMsg::Signals { ts: snap.now, values }).await.is_err() { break; }
            }
        }
        if sub.signal_hz.is_some() {
            let hz = sub.signal_hz.unwrap_or(20.0).clamp(1.0, 120.0);
            let want = std::time::Duration::from_secs_f32(1.0 / hz);
            if sig_tick.period() != want && !sub.signals.is_empty() {
                sig_tick = tokio::time::interval(want);
                sig_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                sig_cache.0 = u64::MAX;
            }
        }
    }
}

async fn resync(hub: &Hub, sub: &Subscription, out: &mpsc::Sender<ServerMsg>) -> bool {
    let mut changes = Vec::new();
    for p in &sub.state {
        for e in hub.get(p, false).await {
            changes.push((e.address, e.value));
        }
    }
    changes.is_empty() || out.send(ServerMsg::State { changes }).await.is_ok()
}

async fn forward(b: &Bus, sub: &Subscription, own: &mut HashMap<Id, Option<u64>>, out: &mpsc::Sender<ServerMsg>) -> bool {
    let msg = match b {
        Bus::Changes(c) if !sub.state.is_empty() => {
            let changes: Vec<(String, Value)> = c.iter().filter(|(a, _)| any_match(&sub.state, a)).cloned().collect();
            if changes.is_empty() {
                return true;
            }
            ServerMsg::State { changes }
        }
        Bus::Event(e) if any_match(&sub.events, &e.ty) => ServerMsg::Event { event: e.clone() },
        Bus::Trace(t) if sub.trace => ServerMsg::Trace { req: None, records: t.clone() },
        Bus::Log { level, target, msg, ts } if sub.logs => ServerMsg::Log { level: level.clone(), target: target.clone(), msg: msg.clone(), ts: *ts },
        Bus::Ack { id, ok, error } => match own.remove(id) {
            Some(req) => ServerMsg::Ack { req, id: *id, ok: *ok, error: error.clone() },
            None => return true,
        },
        _ => return true,
    };
    out.send(msg).await.is_ok()
}

async fn handle(
    hub: &Hub,
    scope: &Scope,
    cfg: &ClientCfg,
    msg: ClientMsg,
    sub: &mut Subscription,
    own: &mut HashMap<Id, Option<u64>>,
    out: &mpsc::Sender<ServerMsg>,
) -> bool {
    let reply = match msg {
        ClientMsg::Hello { .. } => return true,
        ClientMsg::Cmd { req, cmd } => submit(hub, scope, cfg, req, cmd, own),
        ClientMsg::CmdText { req, text } => match Op::parse(&text) {
            Ok(op) => submit(hub, scope, cfg, req, Command::new(cfg.default_origin, op), own),
            Err(e) => Some(ServerMsg::Ack { req, id: 0, ok: false, error: Some(e.to_string()) }),
        },
        ClientMsg::Subscribe { sub: s } => {
            *sub = s;
            if !sub.state.is_empty() && !resync(hub, sub, out).await {
                return false;
            }
            None
        }
        ClientMsg::Get { req, pattern, meta } => {
            let entries: Vec<StateEntry> = hub.get(&pattern, meta).await;
            Some(ServerMsg::Values { req, entries })
        }
        ClientMsg::Explain { req, address } => Some(ServerMsg::Explain { req, provenance: hub.explain(&address).await }),
        ClientMsg::Trace { req, id } => Some(ServerMsg::Trace { req: Some(req), records: hub.trace(id).await }),
        ClientMsg::Query { req, name, args } => {
            if !scope.may_query(&name) {
                Some(ServerMsg::Reply { req, value: Value::Null, error: Some("not permitted".into()) })
            } else {
                match hub.query(&name, args).await {
                    Ok(value) => Some(ServerMsg::Reply { req, value, error: None }),
                    Err(e) => Some(ServerMsg::Reply { req, value: Value::Null, error: Some(e) }),
                }
            }
        }
        ClientMsg::Ping { stamp } => Some(ServerMsg::Pong { stamp, now: se_clock::now() }),
    };
    match reply {
        Some(r) => out.send(r).await.is_ok(),
        None => true,
    }
}

fn submit(hub: &Hub, scope: &Scope, cfg: &ClientCfg, req: Option<u64>, mut cmd: Command, own: &mut HashMap<Id, Option<u64>>) -> Option<ServerMsg> {
    if !scope.allows(&cmd.op) {
        return Some(ServerMsg::Ack { req, id: cmd.id, ok: false, error: Some(format!("not permitted: {}", cmd.op.describe())) });
    }
    // Only fully trusted clients may claim an origin; others are `api`.
    if !matches!(scope, Scope::Full) {
        cmd.origin = Origin::Api;
        cmd.priority = None;
    } else if !cfg.trusted && matches!(cmd.origin, Origin::System) {
        cmd.origin = cfg.default_origin;
    }
    if cmd.id == 0 {
        cmd.id = se_proto::next_id();
    }
    own.insert(cmd.id, req);
    if own.len() > 10_000 {
        own.clear();
    }
    hub.command(cmd);
    None
}
